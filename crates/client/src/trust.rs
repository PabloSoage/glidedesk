//! Trust in the server's self-signed certificate, as SSH trusts host keys: its SHA-256
//! fingerprint is pinned (given by the user, or saved at the first connection), and a server
//! with no pin is accepted only if the platform's `confirm` says so. The check runs during
//! the TLS handshake, before any credentials are sent.

use std::sync::Arc;

use sha2::Digest;
use tracing::info;

/// Asks the user whether to trust a server seen for the first time (its fingerprint).
pub type Confirm = Arc<dyn Fn(&[u8; 32]) -> bool + Send + Sync>;

#[derive(Clone, Default)]
pub struct Trust {
    /// Fingerprint the certificate must have.
    pub pinned: Option<[u8; 32]>,
    /// For a server with no pin; without it any certificate is accepted (and logged).
    pub confirm: Option<Confirm>,
}

impl std::fmt::Debug for Trust {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Trust")
            .field("pinned", &self.pinned.map(|p| hex(&p)))
            .field("confirm", &self.confirm.is_some())
            .finish()
    }
}

impl Trust {
    /// Whether to go on with a server whose certificate has fingerprint `fp`.
    pub fn check(&self, fp: &[u8; 32]) -> Result<(), String> {
        match (self.pinned, &self.confirm) {
            (Some(p), _) if p == *fp => Ok(()),
            (Some(p), _) => Err(format!(
                "the server certificate is not the one trusted before (expected {}, got {}); \
                 if the server got a new certificate, forget the old one first",
                hex(&p),
                hex(fp)
            )),
            (None, Some(confirm)) => {
                if confirm(fp) {
                    Ok(())
                } else {
                    Err("the server certificate was not trusted".into())
                }
            }
            (None, None) => {
                info!(fingerprint = hex(fp), "Server certificate (not pinned)");
                Ok(())
            }
        }
    }
}

/// SHA-256 of a DER certificate.
pub fn fingerprint(der: &[u8]) -> [u8; 32] {
    sha2::Sha256::digest(der).into()
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A fingerprint as written by a user: 64 hex digits, colons and spaces allowed.
pub fn parse_fingerprint(s: &str) -> Result<[u8; 32], String> {
    let digits: String = s.chars().filter(|c| !matches!(c, ':' | ' ')).collect();
    let bytes: Vec<u8> = (0..digits.len())
        .step_by(2)
        .map(|i| {
            digits
                .get(i..i + 2)
                .and_then(|d| u8::from_str_radix(d, 16).ok())
        })
        .collect::<Option<_>>()
        .ok_or_else(|| format!("{s:?} is not a hexadecimal fingerprint"))?;
    bytes
        .try_into()
        .map_err(|b: Vec<u8>| format!("a SHA-256 fingerprint has 32 bytes, {s:?} has {}", b.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fingerprints() {
        let fp = [0xab; 32];
        assert_eq!(parse_fingerprint(&hex(&fp)), Ok(fp));
        let colons = hex(&fp)
            .as_bytes()
            .chunks(2)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect::<Vec<_>>()
            .join(":");
        assert_eq!(parse_fingerprint(&colons), Ok(fp));
        assert!(parse_fingerprint("abc").is_err());
        assert!(parse_fingerprint(&hex(&[1; 31])).is_err());
        assert!(parse_fingerprint(&"zz".repeat(32)).is_err());
    }

    #[test]
    fn checks() {
        let (a, b) = ([1; 32], [2; 32]);
        let pinned = Trust {
            pinned: Some(a),
            confirm: None,
        };
        assert!(pinned.check(&a).is_ok());
        assert!(pinned.check(&b).is_err());
        let refuse = Trust {
            pinned: None,
            confirm: Some(Arc::new(|_| false)),
        };
        assert!(refuse.check(&a).is_err());
        assert!(Trust::default().check(&a).is_ok());
    }
}
