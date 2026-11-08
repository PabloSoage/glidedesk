//! Credential checks for the native and web front ends (RDP's go through NLA in IronRDP):
//! comparison in constant time, and a growing wait for addresses that keep failing.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ring::digest::{SHA256, digest};
use subtle::ConstantTimeEq;

/// Whether `user` and `password` are the expected ones, in time that does not depend on how
/// much of them matches (both sides are hashed to the same length first).
pub fn verify_credentials(
    expected_user: &str,
    expected_pass: &str,
    user: &str,
    password: &str,
) -> bool {
    let h = |s: &str| digest(&SHA256, s.as_bytes());
    let u = h(expected_user).as_ref().ct_eq(h(user).as_ref());
    let p = h(expected_pass).as_ref().ct_eq(h(password).as_ref());
    bool::from(u & p)
}

/// Failures allowed before the first wait.
const FREE_FAILURES: u32 = 2;
/// The wait doubles from 1 s with each failure, up to this.
const MAX_WAIT: Duration = Duration::from_secs(60);
/// Failures older than this are forgotten.
const FORGET: Duration = Duration::from_secs(15 * 60);
/// Addresses remembered at most; beyond it the oldest go.
const MAX_ENTRIES: usize = 4096;

struct Attempt {
    failures: u32,
    locked_until: Option<Instant>,
    last: Instant,
}

/// Failed logins per client address. IPv6 clients count per /64, the block one host
/// usually has.
#[derive(Default)]
pub struct RateLimiter {
    attempts: Mutex<HashMap<IpAddr, Attempt>>,
}

fn key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let s = v6.segments();
                IpAddr::V6(std::net::Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
            }
        },
        v4 => v4,
    }
}

/// The wait after `failures` failures.
fn wait_after(failures: u32) -> Option<Duration> {
    let n = failures.checked_sub(FREE_FAILURES + 1)?;
    Some(Duration::from_secs(1u64 << n.min(6)).min(MAX_WAIT))
}

impl RateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// How long `ip` must still wait before trying again, if it must.
    pub fn check(&self, ip: IpAddr) -> Option<Duration> {
        let now = Instant::now();
        let map = self.attempts.lock().unwrap();
        let until = map.get(&key(ip))?.locked_until?;
        (now < until).then(|| until - now)
    }

    /// Records a failed login; returns the wait it imposes, if any.
    pub fn record_failure(&self, ip: IpAddr) -> Option<Duration> {
        let now = Instant::now();
        let mut map = self.attempts.lock().unwrap();
        if map.len() >= MAX_ENTRIES {
            map.retain(|_, a| now.duration_since(a.last) < FORGET);
            if map.len() >= MAX_ENTRIES
                && let Some(oldest) = map.iter().min_by_key(|(_, a)| a.last).map(|(k, _)| *k)
            {
                map.remove(&oldest);
            }
        }
        let a = map.entry(key(ip)).or_insert(Attempt {
            failures: 0,
            locked_until: None,
            last: now,
        });
        if now.duration_since(a.last) > FORGET {
            a.failures = 0;
        }
        a.failures += 1;
        a.last = now;
        let wait = wait_after(a.failures);
        a.locked_until = wait.map(|w| now + w);
        wait
    }

    /// A successful login clears the address's failures.
    pub fn record_success(&self, ip: IpAddr) {
        self.attempts.lock().unwrap().remove(&key(ip));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials() {
        assert!(verify_credentials(
            "admin",
            "secret123",
            "admin",
            "secret123"
        ));
        assert!(!verify_credentials("admin", "secret123", "admin", "wrong"));
        assert!(!verify_credentials(
            "admin",
            "secret123",
            "root",
            "secret123"
        ));
        assert!(!verify_credentials(
            "admin",
            "secret123",
            "admin",
            "secret1234"
        ));
        assert!(!verify_credentials("admin", "secret123", "", ""));
    }

    #[test]
    fn waits_grow_to_a_minute() {
        let waits: Vec<_> = (1..=10)
            .map(|n| wait_after(n).map(|d| d.as_secs()))
            .collect();
        assert_eq!(
            waits,
            [
                None,
                None,
                Some(1),
                Some(2),
                Some(4),
                Some(8),
                Some(16),
                Some(32),
                Some(60),
                Some(60)
            ]
        );
    }

    #[test]
    fn limiter() {
        let limiter = RateLimiter::new();
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        assert_eq!(limiter.record_failure(ip), None);
        assert_eq!(limiter.record_failure(ip), None);
        assert_eq!(limiter.check(ip), None);
        assert_eq!(limiter.record_failure(ip), Some(Duration::from_secs(1)));
        assert!(limiter.check(ip).is_some());
        // Another address is not affected.
        assert_eq!(limiter.check("192.0.2.2".parse().unwrap()), None);
        limiter.record_success(ip);
        assert_eq!(limiter.check(ip), None);
    }

    #[test]
    fn ipv6_counts_per_64() {
        let limiter = RateLimiter::new();
        for i in 1..=3 {
            limiter.record_failure(format!("2001:db8:1:2::{i}").parse().unwrap());
        }
        assert!(limiter.check("2001:db8:1:2::99".parse().unwrap()).is_some());
        assert_eq!(limiter.check("2001:db8:1:3::1".parse().unwrap()), None);
        // An IPv4 client on a dual-stack socket is its IPv4 address.
        limiter.record_failure("::ffff:192.0.2.7".parse().unwrap());
        assert!(
            limiter
                .attempts
                .lock()
                .unwrap()
                .contains_key(&"192.0.2.7".parse().unwrap())
        );
    }
}
