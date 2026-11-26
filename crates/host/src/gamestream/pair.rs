//! GameStream pairing (adapted from Sunshine `src/nvhttp.cpp`, commit 224c6be, GPLv3).
//!
//! The client shows a PIN; the person types it on the host (`glidedesk-host pair <PIN>`).
//! Four requests to `/pair`, keyed by the client's `uniqueid`:
//!
//! 1. `getservercert` (salt, client certificate): held until the PIN is entered; the AES key
//!    is `SHA-256(salt ‖ PIN)[..16]`; answers with our certificate.
//! 2. `clientchallenge`: AES-ECB(challenge). Answer AES-ECB(SHA-256(challenge ‖ our
//!    certificate's signature ‖ server secret) ‖ server challenge).
//! 3. `serverchallengeresp`: AES-ECB(SHA-256(server challenge ‖ client certificate's
//!    signature ‖ client secret)), kept. Answer server secret ‖ RSA signature of it.
//! 4. `clientpairingsecret`: client secret ‖ RSA signature of it. The client is paired when
//!    the signature checks with its certificate and the hash of step 3 matches (otherwise
//!    someone in the middle changed the certificates).
//!
//! Then `pairchallenge` over HTTPS with the client certificate confirms it.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit, generic_array::GenericArray};
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::oneshot;
use tracing::{info, warn};

use super::identity::{self, Clients, Identity};

/// How long a client waits for the PIN to be typed on the host.
pub const PIN_TIMEOUT: Duration = Duration::from_secs(180);
/// Steps 2 to 4 must follow within this time.
const SESSION_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    GotCert,
    ClientChallenge,
    ServerChallengeResp,
}

struct Session {
    name: String,
    cert_pem: String,
    cert_der: Vec<u8>,
    key: [u8; 16],
    phase: Phase,
    server_secret: [u8; 16],
    server_challenge: [u8; 16],
    client_hash: Vec<u8>,
    started: Instant,
}

/// A client waiting for its PIN.
struct Pending {
    unique_id: String,
    name: String,
    peer: IpAddr,
    pin: oneshot::Sender<String>,
}

/// What `glidedesk-host pair` lists.
#[derive(Clone, Debug)]
pub struct Waiting {
    pub name: String,
    pub peer: IpAddr,
}

#[derive(Default)]
pub struct Pairing {
    sessions: Mutex<HashMap<String, Session>>,
    pending: Mutex<Vec<Pending>>,
}

impl Pairing {
    /// Step 1: waits for the PIN, then returns our certificate (PEM) for the client.
    pub async fn get_server_cert(
        &self,
        id: &Identity,
        unique_id: &str,
        name: &str,
        salt_hex: &str,
        client_cert_hex: &str,
        peer: IpAddr,
    ) -> Result<String> {
        let salt = identity::unhex(salt_hex)
            .filter(|s| s.len() == 16)
            .context("bad salt")?;
        let cert_pem = identity::unhex(client_cert_hex)
            .and_then(|b| String::from_utf8(b).ok())
            .context("bad client certificate")?;
        let cert_der = identity::pem_body(&cert_pem, "CERTIFICATE")?;
        identity::certificate_signature(&cert_der)?;
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().unwrap();
            pending.retain(|p| p.unique_id != unique_id && !p.pin.is_closed());
            pending.push(Pending {
                unique_id: unique_id.to_owned(),
                name: name.to_owned(),
                peer,
                pin: tx,
            });
        }
        self.sessions.lock().unwrap().remove(unique_id);
        info!(
            device = name,
            %peer,
            "A Moonlight client wants to pair: run `glidedesk-host pair <PIN>` with the PIN it shows"
        );
        let pin = match tokio::time::timeout(PIN_TIMEOUT, rx).await {
            Ok(Ok(pin)) => pin,
            _ => {
                self.cancel(unique_id);
                bail!("no PIN was entered");
            }
        };
        let key = aes_key(&salt, &pin);
        self.sessions.lock().unwrap().insert(
            unique_id.to_owned(),
            Session {
                name: name.to_owned(),
                cert_pem,
                cert_der,
                key,
                phase: Phase::GotCert,
                server_secret: glidedesk_proto::random(),
                server_challenge: glidedesk_proto::random(),
                client_hash: Vec::new(),
                started: Instant::now(),
            },
        );
        Ok(id.cert_pem.clone())
    }

    /// Clients waiting for a PIN.
    pub fn waiting(&self) -> Vec<Waiting> {
        let mut pending = self.pending.lock().unwrap();
        pending.retain(|p| !p.pin.is_closed());
        pending
            .iter()
            .map(|p| Waiting {
                name: p.name.clone(),
                peer: p.peer,
            })
            .collect()
    }

    /// Hands a PIN to the waiting client whose name contains `device` (any client when
    /// there is only one). Returns its name.
    pub fn enter_pin(&self, pin: &str, device: Option<&str>) -> Result<String> {
        if pin.len() < 4 || pin.len() > 16 || !pin.bytes().all(|b| b.is_ascii_digit()) {
            bail!("the PIN is 4 or more digits");
        }
        let mut pending = self.pending.lock().unwrap();
        pending.retain(|p| !p.pin.is_closed());
        let matches: Vec<usize> = pending
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                device.is_none_or(|d| p.name.to_lowercase().contains(&d.to_lowercase()))
            })
            .map(|(i, _)| i)
            .collect();
        match matches.as_slice() {
            [] => bail!("no Moonlight client is waiting for a PIN"),
            [i] => {
                let p = pending.remove(*i);
                let name = p.name.clone();
                p.pin
                    .send(pin.to_owned())
                    .map_err(|_| anyhow::anyhow!("the client stopped waiting"))?;
                Ok(name)
            }
            _ => bail!("several clients are waiting: say which one with --device"),
        }
    }

    /// Forgets a pairing in progress (the client gave up, or failed).
    pub fn cancel(&self, unique_id: &str) {
        self.pending
            .lock()
            .unwrap()
            .retain(|p| p.unique_id != unique_id);
        self.sessions.lock().unwrap().remove(unique_id);
    }

    /// Runs `f` on the session in `expected` phase; any error ends the pairing.
    fn step<T>(
        &self,
        unique_id: &str,
        expected: Phase,
        f: impl FnOnce(&mut Session) -> Result<T>,
    ) -> Result<T> {
        let mut sessions = self.sessions.lock().unwrap();
        sessions.retain(|_, s| s.started.elapsed() < SESSION_TIMEOUT);
        let Some(s) = sessions.get_mut(unique_id) else {
            bail!("no pairing in progress for this client");
        };
        let r = if s.phase == expected {
            f(s)
        } else {
            Err(anyhow::anyhow!("pairing step out of order"))
        };
        if r.is_err() {
            sessions.remove(unique_id);
        }
        r
    }

    /// Step 2.
    pub fn client_challenge(&self, id: &Identity, unique_id: &str, enc: &[u8]) -> Result<Vec<u8>> {
        self.step(unique_id, Phase::GotCert, |s| {
            let challenge = ecb(&s.key, enc, false)?;
            let mut h = Sha256::new();
            h.update(&challenge);
            h.update(&id.cert_signature);
            h.update(s.server_secret);
            let mut plain = h.finalize().to_vec();
            plain.extend_from_slice(&s.server_challenge);
            s.phase = Phase::ClientChallenge;
            ecb(&s.key, &plain, true)
        })
    }

    /// Step 3.
    pub fn server_challenge_resp(
        &self,
        id: &Identity,
        unique_id: &str,
        enc: &[u8],
    ) -> Result<Vec<u8>> {
        self.step(unique_id, Phase::ClientChallenge, |s| {
            s.client_hash = ecb(&s.key, enc, false)?;
            s.phase = Phase::ServerChallengeResp;
            let mut out = s.server_secret.to_vec();
            out.extend(id.sign(&s.server_secret));
            Ok(out)
        })
    }

    /// Step 4: whether the client is now paired (and stored).
    pub fn client_pairing_secret(
        &self,
        clients: &Clients,
        unique_id: &str,
        data: &[u8],
    ) -> Result<bool> {
        let r = self.step(unique_id, Phase::ServerChallengeResp, |s| {
            if data.len() <= 16 {
                bail!("client pairing secret too short");
            }
            let (secret, sig) = data.split_at(16);
            let mut h = Sha256::new();
            h.update(s.server_challenge);
            h.update(identity::certificate_signature(&s.cert_der)?);
            h.update(secret);
            let hash = h.finalize();
            let same = s.client_hash.len() == hash.len()
                && bool::from(s.client_hash.as_slice().ct_eq(hash.as_slice()));
            let signed = identity::verify_with_certificate(&s.cert_der, secret, sig);
            if !(same && signed) {
                warn!(
                    device = s.name,
                    same, signed, "Pairing failed: wrong PIN or a changed certificate"
                );
                return Ok(false);
            }
            clients.add(&s.name, &s.cert_pem, s.cert_der.clone())?;
            info!(device = s.name, "Moonlight client paired");
            Ok(true)
        });
        self.sessions.lock().unwrap().remove(unique_id);
        r
    }
}

fn aes_key(salt: &[u8], pin: &str) -> [u8; 16] {
    let mut h = Sha256::new();
    h.update(salt);
    h.update(pin.as_bytes());
    h.finalize()[..16].try_into().unwrap()
}

/// AES-128-ECB without padding (the data are whole blocks).
fn ecb(key: &[u8; 16], data: &[u8], encrypt: bool) -> Result<Vec<u8>> {
    if data.is_empty() || !data.len().is_multiple_of(16) {
        bail!("{} bytes is not a whole number of AES blocks", data.len());
    }
    let cipher = aes::Aes128::new(GenericArray::from_slice(key));
    let mut out = data.to_vec();
    for block in out.chunks_mut(16) {
        let block = GenericArray::from_mut_slice(block);
        if encrypt {
            cipher.encrypt_block(block);
        } else {
            cipher.decrypt_block(block);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn temp(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "gd-gs-{tag}-{}",
            identity::hex(&glidedesk_proto::random::<4>())
        ))
    }

    /// The client side, as moonlight-qt does it (`app/backend/nvpairingmanager.cpp`).
    async fn pair_as_client(pin_on_host: &str, pin_on_client: &str) -> Result<bool> {
        let (host_dir, client_dir) = (temp("host"), temp("client"));
        let host = Arc::new(Identity::load_or_create(&host_dir)?);
        let client = Identity::load_or_create(&client_dir)?;
        let clients = Clients::load(&host_dir)?;
        let pairing = Arc::new(Pairing::default());
        let peer = IpAddr::from([127, 0, 0, 1]);

        let salt: [u8; 16] = glidedesk_proto::random();
        let key = aes_key(&salt, pin_on_client);
        let step1 = {
            let (pairing, host) = (Arc::clone(&pairing), Arc::clone(&host));
            let cert_hex = identity::hex(client.cert_pem.as_bytes());
            tokio::spawn(async move {
                pairing
                    .get_server_cert(
                        &host,
                        "c1",
                        "Switch",
                        &identity::hex(&salt),
                        &cert_hex,
                        peer,
                    )
                    .await
            })
        };
        while pairing.waiting().is_empty() {
            tokio::task::yield_now().await;
        }
        assert_eq!(pairing.waiting()[0].name, "Switch");
        assert_eq!(pairing.enter_pin(pin_on_host, Some("swi"))?, "Switch");
        let server_cert_pem = step1.await??;
        let server_der = identity::pem_body(&server_cert_pem, "CERTIFICATE")?;

        let challenge: [u8; 16] = glidedesk_proto::random();
        let resp = pairing.client_challenge(&host, "c1", &ecb(&key, &challenge, true)?)?;
        let resp = ecb(&key, &resp, false)?;
        let (server_hash, server_challenge) = resp.split_at(32);

        let client_secret: [u8; 16] = glidedesk_proto::random();
        let mut h = Sha256::new();
        h.update(server_challenge);
        h.update(&client.cert_signature);
        h.update(client_secret);
        let secret =
            pairing.server_challenge_resp(&host, "c1", &ecb(&key, &h.finalize(), true)?)?;
        let (server_secret, server_sig) = secret.split_at(16);
        // What the client checks: the host signed its secret, and knew the PIN.
        if !identity::verify_with_certificate(&server_der, server_secret, server_sig) {
            return Ok(false);
        }
        let mut h = Sha256::new();
        h.update(challenge);
        h.update(identity::certificate_signature(&server_der)?);
        h.update(server_secret);
        let host_knew_pin = h.finalize().as_slice() == server_hash;

        let mut last = client_secret.to_vec();
        last.extend(client.sign(&client_secret));
        let paired = pairing.client_pairing_secret(&clients, "c1", &last)?;
        let stored = clients.find(&client.cert_der).is_some();
        assert_eq!(paired, stored);
        std::fs::remove_dir_all(&host_dir).ok();
        std::fs::remove_dir_all(&client_dir).ok();
        Ok(paired && host_knew_pin)
    }

    #[tokio::test]
    async fn pairs_with_the_right_pin() {
        assert!(pair_as_client("4321", "4321").await.unwrap());
    }

    #[tokio::test]
    async fn refuses_a_wrong_pin() {
        assert!(!pair_as_client("1234", "4321").await.unwrap());
    }

    #[test]
    fn pin_needs_a_waiting_client() {
        let p = Pairing::default();
        assert!(p.enter_pin("1234", None).is_err());
        assert!(p.enter_pin("12a4", None).is_err());
    }
}
