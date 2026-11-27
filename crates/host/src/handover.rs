//! Handing native clients from the root daemon (`glidedesk-host daemon`) to the host in
//! the user's session (`serve --handover`), over a Unix socket in the user's runtime
//! directory (`$XDG_RUNTIME_DIR/glidedesk/handover.sock`).
//!
//! On a new connection the daemon, which has already logged the client in, writes
//! `GLIDEDESK-HANDOVER 1 <client address>\n`; the host answers `OK <UDP port>\n` with the
//! loopback port its media socket is on. The connection then carries the native control
//! messages in plaintext (the daemon holds the client's TLS), starting with the client's
//! `Hello`.
//!
//! Both ends check who is on the other side (`SO_PEERCRED`): the host takes connections
//! from root or its own user only, and the daemon talks only to a socket of the user it
//! logged in (a socket path in a directory the user controls could point anywhere).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

const MAGIC: &str = "GLIDEDESK-HANDOVER";
const VERSION: u32 = 1;
/// Longest header line accepted.
const MAX_LINE: usize = 256;

/// The handover socket of the user `uid`, where systemd's logind puts runtime directories.
pub fn socket_for(uid: u32) -> PathBuf {
    PathBuf::from(format!("/run/user/{uid}/glidedesk/handover.sock"))
}

/// This process's own handover socket.
pub fn own_socket() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => Path::new(&dir).join("glidedesk").join("handover.sock"),
        // SAFETY: getuid cannot fail.
        None => socket_for(unsafe { libc::getuid() }),
    }
}

fn request_line(peer: SocketAddr) -> String {
    format!("{MAGIC} {VERSION} {peer}\n")
}

fn parse_request(line: &str) -> Option<SocketAddr> {
    let mut parts = line.trim_end().split(' ');
    if parts.next()? != MAGIC || parts.next()?.parse::<u32>().ok()? != VERSION {
        return None;
    }
    let peer = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some(peer)
}

fn reply_line(udp_port: u16) -> String {
    format!("OK {udp_port}\n")
}

fn parse_reply(line: &str) -> Option<u16> {
    let port = line.trim_end().strip_prefix("OK ")?.parse().ok()?;
    (port != 0).then_some(port)
}

/// Reads one `\n`-terminated line, byte by byte so nothing after it is consumed.
async fn read_line<R: AsyncRead + Unpin>(r: &mut R) -> Result<String> {
    let mut line = Vec::new();
    loop {
        let b = r.read_u8().await.context("reading the handover header")?;
        if b == b'\n' {
            break;
        }
        if line.len() >= MAX_LINE {
            bail!("handover header too long");
        }
        line.push(b);
    }
    String::from_utf8(line).context("handover header is not text")
}

async fn write_line<W: AsyncWrite + Unpin>(w: &mut W, line: &str) -> Result<()> {
    w.write_all(line.as_bytes()).await?;
    w.flush().await?;
    Ok(())
}

/// Host side: listens on `path`, in a directory only this user can enter.
pub fn listen(path: &Path) -> Result<UnixListener> {
    use std::os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt};
    let dir = path.parent().context("handover socket path")?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    // A socket left by a host that did not stop cleanly.
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && meta.file_type().is_socket()
    {
        std::fs::remove_file(path)?;
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Host side: takes a connection from the daemon; returns the client's address. The
/// stream then carries the native protocol.
pub async fn accept(stream: &mut UnixStream, udp_port: u16) -> Result<SocketAddr> {
    let cred = stream.peer_cred().context("reading the peer's credentials")?;
    // SAFETY: getuid cannot fail.
    let own = unsafe { libc::getuid() };
    if cred.uid() != 0 && cred.uid() != own {
        bail!("handover from uid {}, neither root nor this user", cred.uid());
    }
    let line = read_line(stream).await?;
    let peer = parse_request(&line).with_context(|| format!("bad handover header {line:?}"))?;
    write_line(stream, &reply_line(udp_port)).await?;
    Ok(peer)
}

/// Daemon side: connects to the host of user `uid` for the client at `peer`; returns the
/// stream and the host's loopback UDP port.
pub async fn connect(uid: u32, peer: SocketAddr) -> Result<(UnixStream, u16)> {
    let path = socket_for(uid);
    let mut stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("connecting to {}", path.display()))?;
    let cred = stream.peer_cred().context("reading the peer's credentials")?;
    if cred.uid() != uid {
        bail!("{} belongs to uid {}, not {uid}", path.display(), cred.uid());
    }
    write_line(&mut stream, &request_line(peer)).await?;
    let line = read_line(&mut stream).await?;
    let port = parse_reply(&line).with_context(|| format!("bad handover reply {line:?}"))?;
    Ok((stream, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trip() {
        for peer in ["192.0.2.7:51000", "[2001:db8::1]:3391"] {
            let peer: SocketAddr = peer.parse().unwrap();
            assert_eq!(parse_request(&request_line(peer)), Some(peer));
        }
        assert_eq!(parse_reply(&reply_line(40123)), Some(40123));
    }

    #[test]
    fn bad_headers_are_refused() {
        assert_eq!(parse_request("GLIDEDESK-HANDOVER 2 192.0.2.7:1\n"), None);
        assert_eq!(parse_request("HELLO 1 192.0.2.7:1\n"), None);
        assert_eq!(parse_request("GLIDEDESK-HANDOVER 1 192.0.2.7:1 extra\n"), None);
        assert_eq!(parse_request("GLIDEDESK-HANDOVER 1\n"), None);
        assert_eq!(parse_reply("OK 0\n"), None);
        assert_eq!(parse_reply("NO 5\n"), None);
    }

    #[tokio::test]
    async fn line_reading_stops_at_the_newline() {
        let mut data: &[u8] = b"OK 7\nrest";
        assert_eq!(read_line(&mut data).await.unwrap(), "OK 7");
        assert_eq!(data, b"rest");
        let long = vec![b'x'; MAX_LINE + 1];
        assert!(read_line(&mut long.as_slice()).await.is_err());
    }
}
