//! Authentication utilities: constant-time credential comparison and IP rate-limiting.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ring::digest::{digest, SHA256};
use subtle::ConstantTimeEq;

/// Verify username and password in constant time to prevent timing side channels.
pub fn verify_credentials(
    expected_user: &str,
    expected_pass: &str,
    user: &str,
    password: &str,
) -> bool {
    let exp_u = digest(&SHA256, expected_user.as_bytes());
    let giv_u = digest(&SHA256, user.as_bytes());
    let exp_p = digest(&SHA256, expected_pass.as_bytes());
    let giv_p = digest(&SHA256, password.as_bytes());

    let u_match = exp_u.as_ref().ct_eq(giv_u.as_ref());
    let p_match = exp_p.as_ref().ct_eq(giv_p.as_ref());
    bool::from(u_match & p_match)
}

struct Attempt {
    failures: u32,
    lockout_until: Option<Instant>,
    last_attempt: Instant,
}

#[derive(Default)]
pub struct RateLimiter {
    attempts: Mutex<HashMap<IpAddr, Attempt>>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Checks whether `ip` is currently locked out.
    /// Returns `Some(remaining)` if locked out, or `None` if allowed.
    pub fn check(&self, ip: IpAddr) -> Option<Duration> {
        let now = Instant::now();
        let mut map = self.attempts.lock().unwrap();
        // Periodically purge entries older than 30 minutes
        if map.len() > 1024 {
            map.retain(|_, att| now.duration_since(att.last_attempt) < Duration::from_secs(1800));
        }

        if let Some(until) = map.get(&ip).and_then(|att| att.lockout_until).filter(|&u| now < u) {
            return Some(until.duration_since(now));
        }
        None
    }

    /// Records a failed authentication attempt and returns the lockout delay applied, if any.
    pub fn record_failure(&self, ip: IpAddr) -> Option<Duration> {
        let now = Instant::now();
        let mut map = self.attempts.lock().unwrap();
        let entry = map.entry(ip).or_insert(Attempt {
            failures: 0,
            lockout_until: None,
            last_attempt: now,
        });

        // If the last failure was more than 15 minutes ago, reset count
        if now.duration_since(entry.last_attempt) > Duration::from_secs(900) {
            entry.failures = 0;
        }

        entry.failures += 1;
        entry.last_attempt = now;

        // Exponential backoff after 3 failed attempts:
        // 4 fails: 2s
        // 5 fails: 4s
        // 6 fails: 8s
        // 7 fails: 16s
        // 8+ fails: 32s (capped at 60s)
        if entry.failures >= 3 {
            let backoff_secs = (1u64 << (entry.failures - 3).min(5)).min(60);
            let duration = Duration::from_secs(backoff_secs);
            entry.lockout_until = Some(now + duration);
            Some(duration)
        } else {
            entry.lockout_until = None;
            None
        }
    }

    /// Records a successful authentication, clearing any failure record for `ip`.
    pub fn record_success(&self, ip: IpAddr) {
        let mut map = self.attempts.lock().unwrap();
        map.remove(&ip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_comparison() {
        assert!(verify_credentials("admin", "secret123", "admin", "secret123"));
        assert!(!verify_credentials("admin", "secret123", "admin", "wrong"));
        assert!(!verify_credentials("admin", "secret123", "root", "secret123"));
        assert!(!verify_credentials("admin", "secret123", "admin", "secret1234"));
        assert!(!verify_credentials("admin", "secret123", "", ""));
    }

    #[test]
    fn rate_limiter_backoff() {
        let limiter = RateLimiter::new();
        let ip: IpAddr = "192.0.2.1".parse().unwrap();

        // First 2 failures: no lockout
        assert_eq!(limiter.check(ip), None);
        assert_eq!(limiter.record_failure(ip), None);
        assert_eq!(limiter.check(ip), None);
        assert_eq!(limiter.record_failure(ip), None);
        assert_eq!(limiter.check(ip), None);

        // 3rd failure: 1s lockout
        let lockout = limiter.record_failure(ip);
        assert!(lockout.is_some());
        assert!(limiter.check(ip).is_some());

        // Success resets lockout
        limiter.record_success(ip);
        assert_eq!(limiter.check(ip), None);
    }
}
