//! Who may connect to the phone server: its TLS identity, created on first use as the host
//! does (`crates/host/src/rdp.rs`, `ensure_identity`), the user name and password set in
//! the app, and a pause after repeated failed logins from one address.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// The certificate and key in `dir` (`cert.pem`, `key.pem`), created there if missing; and
/// the certificate's SHA-256 fingerprint, which clients show before trusting it.
pub fn identity(dir: &Path) -> Result<(Arc<rustls::ServerConfig>, [u8; 32])> {
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    if !cert.exists() || !key.exists() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let ck = rcgen::generate_simple_self_signed(vec!["glidedesk".to_owned()])?;
        std::fs::write(&cert, ck.cert.pem()).context("writing cert.pem")?;
        std::fs::write(&key, ck.signing_key.serialize_pem()).context("writing key.pem")?;
        // The app's files are private to it already; the key more so.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600))?;
        }
        tracing::info!(path = %cert.display(), "Created a self-signed certificate");
    }
    let cert_pem = std::fs::read(&cert).context("reading cert.pem")?;
    let key_pem = std::fs::read(&key).context("reading key.pem")?;
    let certs = CertificateDer::pem_slice_iter(&cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("reading cert.pem: {e:?}"))?;
    let first = certs.first().context("cert.pem has no certificate")?;
    let fingerprint: [u8; 32] = Sha256::digest(first.as_ref()).into();
    let key = PrivateKeyDer::from_pem_slice(&key_pem)
        .map_err(|e| anyhow::anyhow!("reading key.pem: {e:?}"))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    Ok((Arc::new(config), fingerprint))
}

/// The login matches the expected one. Both sides are hashed first, so the comparison
/// takes the same time whatever the lengths and contents.
pub fn credentials_match(
    expected_user: &str,
    expected_pass: &str,
    user: &str,
    pass: &str,
) -> bool {
    let h = |s: &str| Sha256::digest(s.as_bytes());
    let u = h(expected_user).as_slice().ct_eq(h(user).as_slice());
    let p = h(expected_pass).as_slice().ct_eq(h(pass).as_slice());
    bool::from(u & p)
}

/// Failed logins allowed before the pauses start.
const FREE_FAILURES: u32 = 3;
/// Longest pause.
const MAX_WAIT: Duration = Duration::from_secs(60);
/// Failures older than this are forgotten.
const FORGET: Duration = Duration::from_secs(15 * 60);
/// Addresses remembered at most.
const MAX_ENTRIES: usize = 1024;

/// Failed logins per address: after a few, each failure doubles the pause (1 s up to a
/// minute), like the host's `auth::RateLimiter`.
#[derive(Default)]
pub struct Limiter {
    attempts: Mutex<HashMap<IpAddr, (u32, Instant, Option<Instant>)>>,
}

impl Limiter {
    /// How long `ip` must still wait, if it must.
    pub fn check(&self, ip: IpAddr, now: Instant) -> Option<Duration> {
        let map = self.attempts.lock().unwrap();
        let until = map.get(&ip)?.2?;
        (now < until).then(|| until - now)
    }

    pub fn failure(&self, ip: IpAddr, now: Instant) {
        let mut map = self.attempts.lock().unwrap();
        if map.len() >= MAX_ENTRIES {
            map.retain(|_, a| now.saturating_duration_since(a.1) < FORGET);
            if map.len() >= MAX_ENTRIES {
                map.clear();
            }
        }
        let a = map.entry(ip).or_insert((0, now, None));
        if now.saturating_duration_since(a.1) > FORGET {
            a.0 = 0;
        }
        a.0 += 1;
        a.1 = now;
        a.2 = a
            .0
            .checked_sub(FREE_FAILURES + 1)
            .map(|n| now + Duration::from_secs(1u64 << n.min(6)).min(MAX_WAIT));
    }

    pub fn success(&self, ip: IpAddr) {
        self.attempts.lock().unwrap().remove(&ip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials() {
        assert!(credentials_match("phone", "secret", "phone", "secret"));
        assert!(!credentials_match("phone", "secret", "phone", "secret!"));
        assert!(!credentials_match("phone", "secret", "other", "secret"));
        assert!(!credentials_match("phone", "secret", "", ""));
    }

    #[test]
    fn limiter_pauses_after_repeated_failures() {
        let l = Limiter::default();
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        let t = Instant::now();
        for _ in 0..FREE_FAILURES {
            l.failure(ip, t);
        }
        assert_eq!(l.check(ip, t), None);
        l.failure(ip, t);
        assert_eq!(l.check(ip, t), Some(Duration::from_secs(1)));
        l.failure(ip, t);
        assert_eq!(l.check(ip, t), Some(Duration::from_secs(2)));
        assert_eq!(l.check(ip, t + Duration::from_secs(3)), None);
        l.success(ip);
        l.failure(ip, t);
        assert_eq!(l.check(ip, t), None);
    }

    #[test]
    fn identity_is_created_once() {
        let dir = std::env::temp_dir().join(format!("glidedesk-id-test-{}", std::process::id()));
        let (_, a) = identity(&dir).unwrap();
        let (_, b) = identity(&dir).unwrap();
        assert_eq!(a, b);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
