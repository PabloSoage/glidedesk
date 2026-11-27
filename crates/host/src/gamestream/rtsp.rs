//! The RTSP handshake that sets a Moonlight stream up (TCP 48010, one request per connection,
//! answered and closed; Sunshine `src/rtsp.cpp`, moonlight-common-c `RtspConnection.c`, commits
//! in THIRD_PARTY.md).
//!
//! `OPTIONS`; `DESCRIBE` (codecs, encryption, audio layouts); `SETUP` of audio, video and
//! control (their ports, and the ping payload and connect data the client echoes on them);
//! `ANNOUNCE` with the client's stream settings (SDP); `PLAY`. Plain RTSP only: the host
//! answers `/launch` with an `rtsp://` URL, so clients do not encrypt it.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tracing::{debug, info, warn};

use super::http;
use super::stream::{Settings, Stream};

/// Encryption flags (moonlight-common-c `SS_ENC_*`).
const SS_ENC_CONTROL_V2: u32 = 0x01;
const SS_ENC_VIDEO: u32 = 0x02;
const SS_ENC_AUDIO: u32 = 0x04;

/// Opus layouts for `DESCRIBE`, as Sunshine lists them (`src/audio.cpp` `stream_configs`):
/// channels, streams, coupled streams and the channel mapping, normal then high quality for
/// stereo, 5.1 and 7.1. GFE's normal-quality surround mappings are rotated, so Moonlight
/// expects them rotated back (Sunshine `cmd_describe`).
const SURROUND_PARAMS: [&str; 6] = [
    "21101",
    "21101",
    "642012453",
    "660012345",
    "85301245673",
    "88001234567",
];

pub async fn run(stream: Arc<Stream>, bind: SocketAddr) -> Result<()> {
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    info!(%bind, "RTSP for Moonlight");
    loop {
        let (tcp, peer) = listener.accept().await?;
        let stream = Arc::clone(&stream);
        tokio::spawn(async move {
            if let Err(e) = handle(tcp, peer, &stream).await {
                debug!(%peer, error = format!("{e:#}"), "RTSP request failed");
            }
        });
    }
}

async fn handle(mut tcp: tokio::net::TcpStream, peer: SocketAddr, stream: &Stream) -> Result<()> {
    let req = http::read_request(&mut tcp).await?;
    let cseq = req.header("CSeq").unwrap_or("0").to_owned();
    debug!(%peer, method = req.method, target = req.path, "RTSP");
    let (status, headers, body) = match stream.launch() {
        Some(l) if l.client.to_canonical() == peer.ip().to_canonical() => {
            answer(&req, &l, stream, !is_private(peer.ip()))
        }
        _ => ("503 Service Unavailable", Vec::new(), String::new()),
    };
    let mut out = format!("RTSP/1.0 {status}\r\nCSeq: {cseq}\r\n");
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    out.push_str(&body);
    tcp.write_all(out.as_bytes()).await?;
    tcp.shutdown().await.ok();
    Ok(())
}

type Answer = (&'static str, Vec<(&'static str, String)>, String);

fn answer(
    req: &http::Request,
    launch: &super::stream::Launch,
    stream: &Stream,
    remote: bool,
) -> Answer {
    let ok = |headers, body| ("200 OK", headers, body);
    match req.method.as_str() {
        "OPTIONS" | "PLAY" => ok(Vec::new(), String::new()),
        "DESCRIBE" => ok(Vec::new(), describe(stream.hevc, remote)),
        "SETUP" => {
            let ports = stream.ports;
            let (port, extra) = if req.path.contains("streamid=audio") {
                (ports.audio, ("X-SS-Ping-Payload", ping(launch)))
            } else if req.path.contains("streamid=video") {
                (ports.video, ("X-SS-Ping-Payload", ping(launch)))
            } else if req.path.contains("streamid=control") {
                (
                    ports.control,
                    ("X-SS-Connect-Data", launch.connect_data.to_string()),
                )
            } else {
                return ("404 Not Found", Vec::new(), String::new());
            };
            ok(
                vec![
                    ("Session", "DEADBEEFCAFE;timeout = 90".into()),
                    ("Transport", format!("server_port={port}")),
                    extra,
                ],
                String::new(),
            )
        }
        "ANNOUNCE" => {
            let sdp = String::from_utf8_lossy(&req.body);
            match Settings::from_sdp(&sdp).and_then(|s| stream.start_video(s)) {
                Ok(()) => ok(Vec::new(), String::new()),
                Err(e) => {
                    warn!(error = format!("{e:#}"), "Moonlight stream refused");
                    ("400 Bad Request", Vec::new(), String::new())
                }
            }
        }
        _ => ("404 Not Found", Vec::new(), String::new()),
    }
}

fn ping(launch: &super::stream::Launch) -> String {
    String::from_utf8_lossy(&launch.ping_payload).into_owned()
}

/// Addresses on the client's own network or a private tunnel: loopback, link-local, private
/// IPv4 ranges, shared address space (100.64/10, used by some VPNs) and IPv6 unique local.
fn is_private(ip: std::net::IpAddr) -> bool {
    match ip.to_canonical() {
        std::net::IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || (a == 100 && (64..128).contains(&b))
        }
        std::net::IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback() || first & 0xfe00 == 0xfc00 || first & 0xffc0 == 0xfe80
        }
    }
}

/// Sunshine feature flags (moonlight-common-c `LI_FF_*`): touch and pen events (1),
/// controller arrival, touchpad and motion events (2). Without them Moonlight turns touch
/// into mouse input and sends no motion.
pub const FEATURE_FLAGS: u32 = 0x01 | 0x02;

/// The `DESCRIBE` SDP: the Sunshine feature flags; encryption of the control stream
/// (always), video and audio (offered: the client opts in; asked for from outside private
/// networks); reference frame invalidation; HEVC if the encoder makes it (the VPS prefix is
/// how GameStream says so); and the Opus layouts.
fn describe(hevc: bool, remote: bool) -> String {
    let mut s = format!("a=x-ss-general.featureFlags:{FEATURE_FLAGS}\r\n");
    let supported = SS_ENC_CONTROL_V2 | SS_ENC_VIDEO | SS_ENC_AUDIO;
    let requested = if remote { supported } else { SS_ENC_CONTROL_V2 };
    s.push_str(&format!(
        "a=x-ss-general.encryptionSupported:{supported}\r\na=x-ss-general.encryptionRequested:{requested}\r\n"
    ));
    // Lost frames are invalidated rather than answered with a keyframe where the encoder can
    // (NVENC); otherwise the host sends a keyframe anyway.
    s.push_str("a=x-nv-video[0].refPicInvalidation:1\r\n");
    if hevc {
        s.push_str("sprop-parameter-sets=AAAAAU\r\n");
    }
    for p in SURROUND_PARAMS {
        s.push_str(&format!("a=fmtp:97 surround-params={p}\r\n"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_lists_codecs_and_audio() {
        let d = describe(true, false);
        assert!(d.contains("sprop-parameter-sets=AAAAAU"));
        assert!(d.contains("a=x-ss-general.encryptionSupported:7"));
        assert!(d.contains("a=x-ss-general.encryptionRequested:1"));
        assert!(describe(true, true).contains("a=x-ss-general.encryptionRequested:7"));
        // Moonlight looks for "a=fmtp:97 surround-params=<channels>" and reads the rest.
        assert_eq!(d.matches("surround-params=6").count(), 2);
        assert!(!describe(false, false).contains("AAAAAU"));
    }

    #[test]
    fn private_networks() {
        for ip in [
            "192.168.1.5",
            "10.8.0.2",
            "100.100.1.1",
            "fd00::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(is_private(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "100.200.1.1", "2a00:1450::1"] {
            assert!(!is_private(ip.parse().unwrap()), "{ip}");
        }
    }
}
