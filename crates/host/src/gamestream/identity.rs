//! The host's GameStream identity and the Moonlight clients paired with it.
//!
//! Moonlight pins the certificate it receives while pairing and signs with its own RSA key,
//! so the host keeps one RSA-2048 key and self-signed certificate (as Sunshine does,
//! `src/httpcommon.cpp`), a unique id, and the certificate of every paired client. All live
//! in `<state dir>/gamestream/`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use ring::signature::{self, RsaKeyPair};
use sha2::{Digest, Sha256};
use tracing::info;

pub struct Identity {
    /// PEM, as sent to clients while pairing.
    pub cert_pem: String,
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
    /// The signature bits of our certificate (part of the pairing hashes).
    pub cert_signature: Vec<u8>,
    signer: RsaKeyPair,
    /// Hex id the host reports in `/serverinfo`; stable across restarts.
    pub unique_id: String,
}

impl Identity {
    /// Loads the identity from `dir`, creating it on first use.
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let (key_path, cert_path, id_path) = (
            dir.join("key.pem"),
            dir.join("cert.pem"),
            dir.join("uniqueid"),
        );
        if !key_path.exists() || !cert_path.exists() {
            info!("Creating the GameStream identity (RSA-2048)");
            let (key_pem, cert_pem) = generate()?;
            write_private(&key_path, key_pem.as_bytes())?;
            std::fs::write(&cert_path, cert_pem)?;
        }
        if !id_path.exists() {
            std::fs::write(&id_path, hex(&glidedesk_proto::random::<8>()))?;
        }
        let key_pem = std::fs::read_to_string(&key_path)?;
        let cert_pem = std::fs::read_to_string(&cert_path)?;
        let key_der = pem_body(&key_pem, "PRIVATE KEY").context("key.pem")?;
        let cert_der = pem_body(&cert_pem, "CERTIFICATE").context("cert.pem")?;
        let signer =
            RsaKeyPair::from_pkcs8(&key_der).map_err(|e| anyhow::anyhow!("GameStream key: {e}"))?;
        let cert_signature = certificate_signature(&cert_der)?;
        Ok(Self {
            cert_pem,
            cert_der,
            key_der,
            cert_signature,
            signer,
            unique_id: std::fs::read_to_string(&id_path)?.trim().to_owned(),
        })
    }

    /// RSA PKCS#1 v1.5 signature with SHA-256.
    pub fn sign(&self, data: &[u8]) -> Vec<u8> {
        let mut sig = vec![0u8; self.signer.public().modulus_len()];
        self.signer
            .sign(
                &signature::RSA_PKCS1_SHA256,
                &ring::rand::SystemRandom::new(),
                data,
                &mut sig,
            )
            .expect("RSA signing failed");
        sig
    }
}

/// A fresh RSA-2048 key (PKCS#8 PEM) and a self-signed certificate for it (PEM).
fn generate() -> Result<(String, String)> {
    use rsa::pkcs8::{EncodePrivateKey, LineEnding};
    let key = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048)?;
    let key_pem = key.to_pkcs8_pem(LineEnding::LF)?.to_string();
    let pair = rcgen::KeyPair::from_pem_and_sign_algo(&key_pem, &rcgen::PKCS_RSA_SHA256)?;
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new())?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "glidedesk GameStream host");
    params.not_before = rcgen::date_time_ymd(2026, 1, 1);
    params.not_after = rcgen::date_time_ymd(2046, 1, 1);
    params.serial_number = Some(rcgen::SerialNumber::from_slice(&glidedesk_proto::random::<
        16,
    >()));
    let cert = params.self_signed(&pair)?;
    Ok((key_pem, cert.pem()))
}

#[cfg(unix)]
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?
        .write_all(data)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    std::fs::write(path, data)?;
    Ok(())
}

/// The DER inside the first PEM block with this label.
pub fn pem_body(pem: &str, label: &str) -> Result<Vec<u8>> {
    let (_, block) = x509_parser::pem::parse_x509_pem(pem.as_bytes())
        .map_err(|e| anyhow::anyhow!("bad PEM: {e}"))?;
    if block.label != label {
        bail!("expected a {label} PEM block, got {}", block.label);
    }
    Ok(block.contents)
}

/// The signature bits of a DER certificate.
pub fn certificate_signature(der: &[u8]) -> Result<Vec<u8>> {
    let (_, cert) = x509_parser::parse_x509_certificate(der)
        .map_err(|e| anyhow::anyhow!("bad certificate: {e}"))?;
    Ok(cert.signature_value.data.to_vec())
}

/// Checks an RSA PKCS#1 v1.5 SHA-256 signature made with the key of a DER certificate.
pub fn verify_with_certificate(cert_der: &[u8], data: &[u8], sig: &[u8]) -> bool {
    let Ok((_, cert)) = x509_parser::parse_x509_certificate(cert_der) else {
        return false;
    };
    let key = cert.public_key().subject_public_key.data.to_vec();
    signature::UnparsedPublicKey::new(&signature::RSA_PKCS1_2048_8192_SHA256, key)
        .verify(data, sig)
        .is_ok()
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Paired Moonlight clients: one `<sha256 of the certificate>.pem` per client in
/// `clients/`, with a `name:` line before the PEM block.
pub struct Clients {
    dir: PathBuf,
    list: Mutex<Vec<Client>>,
}

#[derive(Clone, Debug)]
pub struct Client {
    pub name: String,
    pub cert_der: Vec<u8>,
}

impl Clients {
    pub fn load(dir: &Path) -> Result<Self> {
        let dir = dir.join("clients");
        std::fs::create_dir_all(&dir)?;
        let mut list = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().is_none_or(|e| e != "pem") {
                continue;
            }
            let text = std::fs::read_to_string(&path)?;
            let name = text
                .lines()
                .find_map(|l| l.strip_prefix("name: "))
                .unwrap_or("Moonlight")
                .to_owned();
            match pem_body(&text, "CERTIFICATE") {
                Ok(cert_der) => list.push(Client { name, cert_der }),
                Err(e) => tracing::warn!(path = %path.display(), %e, "Ignoring a paired client"),
            }
        }
        Ok(Self {
            dir,
            list: Mutex::new(list),
        })
    }

    pub fn find(&self, cert_der: &[u8]) -> Option<Client> {
        self.list
            .lock()
            .unwrap()
            .iter()
            .find(|c| c.cert_der == cert_der)
            .cloned()
    }

    pub fn add(&self, name: &str, cert_pem: &str, cert_der: Vec<u8>) -> Result<()> {
        let name: String = name.chars().filter(|c| !c.is_control()).take(64).collect();
        let file = self.dir.join(format!("{}.pem", fingerprint(&cert_der)));
        std::fs::write(&file, format!("name: {name}\n{}", cert_pem.trim_start()))?;
        let mut list = self.list.lock().unwrap();
        list.retain(|c| c.cert_der != cert_der);
        list.push(Client { name, cert_der });
        Ok(())
    }

    pub fn remove(&self, cert_der: &[u8]) {
        let _ = std::fs::remove_file(self.dir.join(format!("{}.pem", fingerprint(cert_der))));
        self.list.lock().unwrap().retain(|c| c.cert_der != cert_der);
    }
}

pub fn fingerprint(der: &[u8]) -> String {
    hex(&Sha256::digest(der)).to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_created_once_and_signs() {
        let dir =
            std::env::temp_dir().join(format!("gd-gs-id-{}", hex(&glidedesk_proto::random::<4>())));
        let a = Identity::load_or_create(&dir).unwrap();
        let b = Identity::load_or_create(&dir).unwrap();
        assert_eq!(a.cert_der, b.cert_der);
        assert_eq!(a.unique_id, b.unique_id);
        assert_eq!(a.cert_signature.len(), 256);
        let sig = a.sign(b"hello");
        assert!(verify_with_certificate(&a.cert_der, b"hello", &sig));
        assert!(!verify_with_certificate(&a.cert_der, b"hellO", &sig));

        let clients = Clients::load(&dir).unwrap();
        clients
            .add("Switch", &a.cert_pem, a.cert_der.clone())
            .unwrap();
        let again = Clients::load(&dir).unwrap();
        assert_eq!(again.find(&a.cert_der).unwrap().name, "Switch");
        again.remove(&a.cert_der);
        assert!(Clients::load(&dir).unwrap().find(&a.cert_der).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn hex_round_trip() {
        assert_eq!(hex(&[0, 0xab, 0x10]), "00AB10");
        assert_eq!(unhex("00ab10").unwrap(), vec![0, 0xab, 0x10]);
        assert!(unhex("0").is_none());
        assert!(unhex("zz").is_none());
    }
}
