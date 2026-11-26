//! GameStream front end, for Moonlight clients (desktop, Android, iOS, TVs, Switch, Vita…).
//!
//! The protocol is the one NVIDIA GameStream used and Sunshine keeps (Sunshine `src/nvhttp.cpp`,
//! moonlight-common-c). What is here so far:
//!
//! - HTTP on 47989 and HTTPS on 47984 (the client presents its certificate; only paired
//!   clients get past `/serverinfo` and `/pair`);
//! - pairing with a PIN ([`pair`]), typed on the host with `glidedesk-host pair <PIN>`;
//! - `/serverinfo` and `/applist` (the host's [`crate::apps`]).
//!
//! Streaming (`/launch`, RTSP, video, audio, input) comes next.

pub mod http;
pub mod identity;
pub mod pair;

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use tokio::net::TcpListener;
use tracing::{debug, info, warn};

use http::{Request, Xml};
use identity::{Clients, Identity};
use pair::Pairing;

pub const HTTP_PORT: u16 = 47989;
pub const HTTPS_PORT: u16 = 47984;
/// The GameStream server version Moonlight checks (generation 7: SHA-256 pairing), and the
/// GeForce Experience version, as Sunshine reports them (`src/nvhttp.h`).
const APP_VERSION: &str = "7.1.431.-1";
const GFE_VERSION: &str = "3.23.0.74";
/// Codec bits of `ServerCodecModeSupport` (moonlight-common-c `Limelight.h`).
const SCM_H264: u32 = 0x1;
const SCM_HEVC: u32 = 0x100;

pub struct Front {
    pub http_bind: SocketAddr,
    pub https_bind: SocketAddr,
    pub state_dir: PathBuf,
    pub password: String,
    pub auth_limiter: Arc<crate::auth::RateLimiter>,
    pub hevc: bool,
}

struct State {
    id: Identity,
    clients: Clients,
    pairing: Pairing,
    front: Front,
    host_name: String,
}

pub async fn run(front: Front) -> Result<()> {
    let dir = front.state_dir.join("gamestream");
    let id = Identity::load_or_create(&dir)?;
    let clients = Clients::load(&dir)?;
    let tls = Arc::new(tls_config(&id)?);
    let http = TcpListener::bind(front.http_bind)
        .await
        .with_context(|| format!("binding {}", front.http_bind))?;
    let https = TcpListener::bind(front.https_bind)
        .await
        .with_context(|| format!("binding {}", front.https_bind))?;
    info!(
        http = %front.http_bind,
        https = %front.https_bind,
        "Listening for Moonlight (GameStream)"
    );
    let state = Arc::new(State {
        id,
        clients,
        pairing: Pairing::default(),
        front,
        host_name: host_name(),
    });
    let acceptor = tokio_rustls::TlsAcceptor::from(tls);
    loop {
        tokio::select! {
            r = http.accept() => {
                let (tcp, peer) = r?;
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    let local = tcp.local_addr().map(|a| a.ip()).unwrap_or(peer.ip());
                    if let Err(e) = serve(tcp, peer, local, None, &state).await {
                        debug!(%peer, error = format!("{e:#}"), "GameStream HTTP request failed");
                    }
                });
            }
            r = https.accept() => {
                let (tcp, peer) = r?;
                let (state, acceptor) = (Arc::clone(&state), acceptor.clone());
                tokio::spawn(async move {
                    let local = tcp.local_addr().map(|a| a.ip()).unwrap_or(peer.ip());
                    let r = async {
                        let tls = acceptor.accept(tcp).await.context("TLS handshake")?;
                        let cert = tls
                            .get_ref()
                            .1
                            .peer_certificates()
                            .and_then(|c| c.first())
                            .map(|c| c.to_vec());
                        serve(tls, peer, local, Some(cert), &state).await
                    }
                    .await;
                    if let Err(e) = r {
                        debug!(%peer, error = format!("{e:#}"), "GameStream HTTPS request failed");
                    }
                });
            }
        }
    }
}

/// One request. `tls` is `None` over HTTP, and the client's certificate (if any) over HTTPS.
async fn serve<S>(
    mut s: S,
    peer: SocketAddr,
    local: IpAddr,
    tls: Option<Option<Vec<u8>>>,
    state: &State,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let req = http::read_request(&mut s).await?;
    debug!(%peer, method = req.method, path = req.path, https = tls.is_some(), "GameStream request");
    if req.path == "/glidedesk/pin" {
        let (status, text) = enter_pin(&req, peer, state);
        return http::respond(&mut s, status, "text/plain; charset=utf-8", text.as_bytes()).await;
    }
    let paired = match &tls {
        Some(Some(cert)) => state.clients.find(cert),
        _ => None,
    };
    let xml = match (req.path.as_str(), &tls) {
        ("/serverinfo", _) => server_info(state, local, paired.is_some()),
        ("/pair", None) => pair_http(&req, peer, state).await,
        (_, Some(_)) if paired.is_none() => Xml::error(
            401,
            "The client is not authorized. Certificate verification failed.",
        ),
        ("/pair", Some(_)) if req.arg("phrase") == Some("pairchallenge") => {
            Xml::ok().put("paired", 1)
        }
        ("/unpair", Some(Some(cert))) => {
            state.clients.remove(cert);
            info!(device = paired.map(|c| c.name), "Moonlight client unpaired");
            Xml::ok()
        }
        ("/applist", Some(_)) => app_list(state),
        ("/unpair", None) => {
            // A client that failed to pair tidies up over HTTP.
            if let Some(uid) = req.arg("uniqueid") {
                state.pairing.cancel(uid);
            }
            Xml::ok()
        }
        _ => Xml::error(404, "Not found"),
    };
    http::respond(
        &mut s,
        "200 OK",
        "application/xml; charset=utf-8",
        xml.render().as_bytes(),
    )
    .await
}

fn server_info(state: &State, local: IpAddr, paired: bool) -> Xml {
    let mut codecs = SCM_H264;
    if state.front.hevc {
        codecs |= SCM_HEVC;
    }
    // Moonlight expects an IPv4 LocalIP; for IPv6 Sunshine sends 127.0.0.1, which it ignores.
    let local_ip = match local {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map_or_else(|| "127.0.0.1".to_owned(), |v4| v4.to_string()),
        IpAddr::V4(v4) => v4.to_string(),
    };
    Xml::ok()
        .put("hostname", &state.host_name)
        .put("appversion", APP_VERSION)
        .put("GfeVersion", GFE_VERSION)
        .put("uniqueid", &state.id.unique_id)
        .put("HttpsPort", state.front.https_bind.port())
        .put("ExternalPort", state.front.http_bind.port())
        .put(
            "MaxLumaPixelsHEVC",
            if state.front.hevc { "1869449984" } else { "0" },
        )
        // A placeholder Moonlight knows to ignore (no wake-on-LAN for now).
        .put("mac", "00:00:00:00:00:00")
        .put("LocalIP", local_ip)
        .put("ServerCodecModeSupport", codecs)
        .put("PairStatus", u8::from(paired))
        .put("currentgame", 0)
        .put("state", "SUNSHINE_SERVER_FREE")
}

fn app_list(state: &State) -> Xml {
    crate::apps::load(&state.front.state_dir)
        .iter()
        .fold(Xml::ok(), |xml, app| {
            xml.child(
                "App",
                &[
                    ("IsHdrSupported", "0".into()),
                    ("AppTitle", app.name.clone()),
                    ("ID", app.id.to_string()),
                ],
            )
        })
}

/// `/pair` over HTTP: the four steps of [`pair`].
async fn pair_http(req: &Request, peer: SocketAddr, state: &State) -> Xml {
    let Some(uid) = req.arg("uniqueid") else {
        return Xml::error(400, "Missing uniqueid parameter");
    };
    let fail = |e: anyhow::Error| {
        warn!(%peer, error = format!("{e:#}"), "Pairing failed");
        Xml::error(400, format!("{e:#}")).put("paired", 0)
    };
    let hex_arg = |name: &str| {
        req.arg(name)
            .and_then(identity::unhex)
            .with_context(|| format!("bad {name}"))
    };
    if req.arg("phrase") == Some("getservercert") {
        let name = req.arg("devicename").unwrap_or("Moonlight");
        let r = state
            .pairing
            .get_server_cert(
                &state.id,
                uid,
                name,
                req.arg("salt").unwrap_or_default(),
                req.arg("clientcert").unwrap_or_default(),
                peer.ip(),
            )
            .await;
        return match r {
            Ok(pem) => Xml::ok()
                .put("paired", 1)
                .put("plaincert", identity::hex(pem.as_bytes())),
            Err(e) => fail(e),
        };
    }
    if req.arg("phrase") == Some("pairchallenge") {
        return Xml::error(400, "pairchallenge goes over HTTPS").put("paired", 0);
    }
    let r = if req.query.contains_key("clientchallenge") {
        hex_arg("clientchallenge")
            .and_then(|c| state.pairing.client_challenge(&state.id, uid, &c))
            .map(|r| {
                Xml::ok()
                    .put("paired", 1)
                    .put("challengeresponse", identity::hex(&r))
            })
    } else if req.query.contains_key("serverchallengeresp") {
        hex_arg("serverchallengeresp")
            .and_then(|c| state.pairing.server_challenge_resp(&state.id, uid, &c))
            .map(|r| {
                Xml::ok()
                    .put("paired", 1)
                    .put("pairingsecret", identity::hex(&r))
            })
    } else if req.query.contains_key("clientpairingsecret") {
        hex_arg("clientpairingsecret")
            .and_then(|c| state.pairing.client_pairing_secret(&state.clients, uid, &c))
            .map(|paired| Xml::ok().put("paired", u8::from(paired)))
    } else {
        Err(anyhow::anyhow!("Invalid pairing request"))
    };
    r.unwrap_or_else(fail)
}

/// `POST /glidedesk/pin` from this machine (`glidedesk-host pair`): body
/// `password=…&pin=…[&device=…]`. Without a PIN it lists the clients waiting.
fn enter_pin(req: &Request, peer: SocketAddr, state: &State) -> (&'static str, String) {
    if !peer.ip().is_loopback() || req.method != "POST" {
        return ("403 Forbidden", "only from this machine\n".into());
    }
    let limiter = &state.front.auth_limiter;
    if let Some(wait) = limiter.check(peer.ip()) {
        return (
            "429 Too Many Requests",
            format!(
                "too many failed attempts; try again in {}s\n",
                wait.as_secs().max(1)
            ),
        );
    }
    let form = http::parse_query(&String::from_utf8_lossy(&req.body));
    let password = form.get("password").map(String::as_str).unwrap_or_default();
    if !crate::auth::verify_credentials("", &state.front.password, "", password) {
        limiter.record_failure(peer.ip());
        return ("403 Forbidden", "wrong password\n".into());
    }
    limiter.record_success(peer.ip());
    match form.get("pin").filter(|p| !p.is_empty()) {
        None => {
            let list: String = state
                .pairing
                .waiting()
                .iter()
                .map(|w| format!("{} ({})\n", w.name, w.peer))
                .collect();
            ("200 OK", list)
        }
        Some(pin) => match state
            .pairing
            .enter_pin(pin, form.get("device").map(String::as_str))
        {
            Ok(name) => ("200 OK", format!("PIN sent to {name}\n")),
            Err(e) => ("409 Conflict", format!("{e:#}\n")),
        },
    }
}

/// `glidedesk-host pair`: hands the PIN to the running host.
pub async fn send_pin(
    port: u16,
    password: &str,
    pin: Option<&str>,
    device: Option<&str>,
) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let enc = |s: &str| -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    };
    let mut body = format!("password={}", enc(password));
    if let Some(pin) = pin {
        body.push_str(&format!("&pin={}", enc(pin)));
    }
    if let Some(device) = device {
        body.push_str(&format!("&device={}", enc(device)));
    }
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .context("is the host running with GameStream on?")?;
    s.write_all(
        format!(
            "POST /glidedesk/pin HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    )
    .await?;
    let mut out = String::new();
    s.read_to_string(&mut out).await?;
    let (head, text) = out.split_once("\r\n\r\n").unwrap_or((&out, ""));
    if !head.starts_with("HTTP/1.1 200") {
        anyhow::bail!("{}", text.trim());
    }
    Ok(text.to_owned())
}

fn host_name() -> String {
    let name = std::env::var(if cfg!(windows) {
        "COMPUTERNAME"
    } else {
        "HOSTNAME"
    })
    .ok()
    .or_else(|| {
        std::fs::read_to_string("/etc/hostname")
            .ok()
            .map(|s| s.trim().to_owned())
    })
    .filter(|s| !s.is_empty());
    name.unwrap_or_else(|| "glidedesk".into())
}

fn tls_config(id: &Identity) -> Result<rustls::ServerConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(AnyClientCert(Arc::clone(&provider)));
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            vec![CertificateDer::from(id.cert_der.clone())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(id.key_der.clone())),
        )?;
    Ok(config)
}

/// Asks for a client certificate and accepts any: whether it belongs to a paired client is
/// decided per request (as Sunshine does), so unpaired clients still reach `/serverinfo`.
#[derive(Debug)]
struct AnyClientCert(Arc<rustls::crypto::CryptoProvider>);

impl ClientCertVerifier for AnyClientCert {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn client_auth_mandatory(&self) -> bool {
        false
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
