//! System service (`glidedesk-host daemon`, Linux): one root process on the public native
//! port logs clients in with their own system account (PAM, any user), starts that user's
//! headless GNOME session as a systemd unit (`glidedesk-session@USER.service`, see
//! `scripts/systemd/`) and hands the client to the host running in it (`serve
//! --handover`, [`crate::handover`]). Clients see one server on one port, whoever logs in.
//!
//! The client's TLS ends here: the daemon relays the decrypted control stream to the
//! user's host over its Unix socket, and the UDP media, which stays sealed end to end with
//! the session key the host chose, between the client and the host's loopback port
//! ([`Relay`]). The video costs one extra datagram hop on the loopback, no re-encryption.
//! A session outlives its client, as an RDP session on Windows does; the next login of
//! the same user takes it over.
//!
//! Each new session gets one of `--gpus` (a CUDA device for NVENC or a render node for
//! VA-API): the one with the fewest running sessions, or the one pinned to the user. The
//! choice goes to the unit through `/run/glidedesk/sessions/USER.env`.
//!
//! What the daemon does not serve:
//! - RDP. Its NLA (CredSSP over NTLM) proves the password with a challenge the server has
//!   to answer from the password itself or its NT hash, and PAM never hands out either;
//!   a per-user RDP password would be needed. The way in, for later: read the
//!   `Cookie: mstshash=USER` of the client's X.224 request without consuming it
//!   (`MSG_PEEK`) and pass the TCP socket itself to that user's running host
//!   (`SCM_RIGHTS`), which runs NLA with its own `--password`, as gnome-remote-desktop's
//!   system daemon hands sockets to the user's session. Until then RDP, like the web
//!   client and Moonlight, is off in daemon sessions and keeps running from a plain
//!   `serve` in a user's session.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use glidedesk_proto::crypto::{Direction, PacketKey};
use glidedesk_proto::{ClientMsg, ServerMsg, read_msg, write_msg};
use tokio::io::AsyncWrite;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tracing::{debug, info, warn};

use crate::auth::RateLimiter;
use crate::handover;

/// One of the GPUs sessions are spread over.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GpuSlot {
    /// CUDA device index (NVENC).
    Cuda(usize),
    /// DRM render node (VA-API).
    RenderNode(String),
}

/// `--gpus` entries: a CUDA device index or a `/dev/dri` render node.
pub fn parse_gpu(s: &str) -> Result<GpuSlot, String> {
    if let Ok(i) = s.parse() {
        return Ok(GpuSlot::Cuda(i));
    }
    if s.starts_with("/dev/dri/") && !s.contains(char::is_whitespace) {
        return Ok(GpuSlot::RenderNode(s.to_owned()));
    }
    Err(format!(
        "{s:?} is neither a CUDA device index nor a /dev/dri render node"
    ))
}

/// `--pin-gpu USER=INDEX`.
pub fn parse_pin(s: &str) -> Result<(String, usize), String> {
    let (user, index) = s.split_once('=').ok_or("expected USER=INDEX")?;
    if !valid_user_name(user) {
        return Err(format!("{user:?} is not a user name"));
    }
    let index = index
        .parse()
        .map_err(|_| format!("{index:?} is not an index into --gpus"))?;
    Ok((user.to_owned(), index))
}

pub struct Options {
    /// Native protocol address (TCP and UDP).
    pub bind: SocketAddr,
    /// TLS identity (`cert.pem`, `key.pem`).
    pub state_dir: PathBuf,
    pub gpus: Vec<GpuSlot>,
    /// Users whose sessions always get the GPU at this index of `gpus`.
    pub pins: Vec<(String, usize)>,
    /// The template unit of a user's session: `NAME@USER.service`.
    pub session_unit: String,
}

/// Accounts below this are system accounts (the usual `UID_MIN`): they get no session.
const MIN_UID: u32 = 1000;
/// How long a new session may take until its host takes clients.
const START_TIMEOUT: Duration = Duration::from_secs(45);
/// The GPU of each session, for its unit's `EnvironmentFile=`.
const SESSIONS_DIR: &str = "/run/glidedesk/sessions";

/// A name `useradd` accepts by default; safe in a unit instance name and a file name.
fn valid_user_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && !name.starts_with(['-', '.'])
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// The GPU for a new session: the pinned one, else the least used (the first of those).
fn pick_slot(loads: &[usize], pinned: Option<usize>) -> Option<usize> {
    if let Some(i) = pinned
        && i < loads.len()
    {
        return Some(i);
    }
    loads
        .iter()
        .enumerate()
        .min_by_key(|&(i, &n)| (n, i))
        .map(|(i, _)| i)
}

/// The environment file of a session on GPU `index`.
fn session_env(index: usize, slot: &GpuSlot) -> String {
    match slot {
        GpuSlot::Cuda(device) => format!("GLIDEDESK_GPU_SLOT={index}\nGLIDEDESK_GPU={device}\n"),
        GpuSlot::RenderNode(node) => format!(
            "GLIDEDESK_GPU_SLOT={index}\nGLIDEDESK_ENCODER=vaapi\nGLIDEDESK_RENDER_NODE={node}\n"
        ),
    }
}

/// The GPU index written by [`session_env`].
fn slot_of(env: &str) -> Option<usize> {
    env.lines()
        .find_map(|l| l.strip_prefix("GLIDEDESK_GPU_SLOT="))?
        .trim()
        .parse()
        .ok()
}

struct Account {
    name: String,
    uid: u32,
}

/// The system account `name`, if there is one.
fn account(name: &str) -> Option<Account> {
    let cname = std::ffi::CString::new(name).ok()?;
    let mut pwd = std::mem::MaybeUninit::<libc::passwd>::zeroed();
    let mut buf: Vec<libc::c_char> = vec![0; 16 * 1024];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: a NUL-terminated name, and a passwd record and buffer that outlive the call;
    // `result` is either null or points to `pwd`, filled in by getpwnam_r.
    unsafe {
        let r = libc::getpwnam_r(
            cname.as_ptr(),
            pwd.as_mut_ptr(),
            buf.as_mut_ptr(),
            buf.len(),
            &mut result,
        );
        if r != 0 || result.is_null() {
            return None;
        }
        Some(Account {
            name: name.to_owned(),
            uid: (*result).pw_uid,
        })
    }
}

/// Runs `systemctl` with `args`; its error output on failure.
async fn systemctl(args: &[&str]) -> Result<()> {
    let out = tokio::process::Command::new("systemctl")
        .args(args)
        .output()
        .await
        .context("running systemctl")?;
    if !out.status.success() {
        bail!(
            "systemctl {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

async fn unit_active(unit: &str) -> bool {
    systemctl(&["is-active", "--quiet", unit]).await.is_ok()
}

async fn deny<S: AsyncWrite + Unpin>(stream: &mut S, reason: String) -> Result<()> {
    write_msg(stream, &ServerMsg::Denied(reason.clone())).await?;
    bail!("denied: {reason}")
}

struct Daemon {
    opts: Options,
    tls: tokio_rustls::TlsAcceptor,
    limiter: RateLimiter,
    relay: Arc<Relay>,
    /// Held while a session is checked and started, so that two logins of one user start
    /// it once and GPU loads are counted one start at a time.
    starting: tokio::sync::Mutex<()>,
}

pub async fn run(opts: Options) -> Result<()> {
    // SAFETY: geteuid cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        bail!("the daemon runs as root: it logs in any user and starts their sessions");
    }
    if let Some((user, i)) = opts.pins.iter().find(|(_, i)| *i >= opts.gpus.len()) {
        bail!("--pin-gpu {user}={i}: there are {} --gpus", opts.gpus.len());
    }
    crate::rdp::ensure_identity(&opts.state_dir)?;
    let tls = tokio_rustls::TlsAcceptor::from(crate::native::tls_config(&opts.state_dir)?);
    let tcp = TcpListener::bind(opts.bind)
        .await
        .with_context(|| format!("binding TCP {}", opts.bind))?;
    let relay = Relay::bind(opts.bind)?;
    tokio::spawn(Arc::clone(&relay).from_clients());
    info!(bind = %opts.bind, gpus = ?opts.gpus, "Listening for native clients (system daemon)");
    let daemon = Arc::new(Daemon {
        opts,
        tls,
        limiter: RateLimiter::new(),
        relay,
        starting: tokio::sync::Mutex::new(()),
    });
    loop {
        let (tcp, peer) = tcp.accept().await?;
        let _ = tcp.set_nodelay(true);
        let daemon = Arc::clone(&daemon);
        tokio::spawn(async move {
            info!(%peer, "Native client connecting");
            let r = daemon.client(tcp, peer).await;
            info!(%peer, error = ?r.err().map(|e| format!("{e:#}")), "Native client disconnected");
        });
    }
}

impl Daemon {
    async fn client(&self, tcp: TcpStream, peer: SocketAddr) -> Result<()> {
        let mut tls = self.tls.accept(tcp).await.context("TLS handshake")?;
        let hello: ClientMsg = read_msg(&mut tls).await?;
        let ClientMsg::Hello {
            version,
            user,
            password,
            width,
            height,
            codecs,
            refresh,
        } = hello
        else {
            bail!("expected Hello");
        };
        let ip = peer.ip();
        if let Some(wait) = self.limiter.check(ip) {
            let reason = format!(
                "too many failed attempts; try again in {:.0}s",
                wait.as_secs_f64().max(1.0)
            );
            return deny(&mut tls, reason).await;
        }
        let Some(account) = self.log_in(&user, &password) else {
            match self.limiter.record_failure(ip) {
                Some(wait) => warn!(%peer, user, lockout_secs = wait.as_secs(), "Authentication failed; backoff applied"),
                None => warn!(%peer, user, "Authentication failed"),
            }
            return deny(&mut tls, "wrong user name or password".to_owned()).await;
        };
        self.limiter.record_success(ip);
        info!(%peer, user, "Logged in");

        let (mut host, udp_port) = match self.session(&account, peer).await {
            Ok(h) => h,
            Err(e) => {
                warn!(%peer, user, error = format!("{e:#}"), "No session");
                return deny(&mut tls, format!("could not start the session: {e:#}")).await;
            }
        };
        // The host trusts the daemon's login: it does not need the password.
        let hello = ClientMsg::Hello {
            version,
            user,
            password: String::new(),
            width,
            height,
            codecs,
            refresh,
        };
        write_msg(&mut host, &hello).await?;
        let first: ServerMsg = read_msg(&mut host).await?;
        let route = match &first {
            ServerMsg::Welcome { key, .. } => Some(
                self.relay
                    .add(key, SocketAddr::from((Ipv4Addr::LOCALHOST, udp_port)))?,
            ),
            _ => None,
        };
        write_msg(&mut tls, &first).await?;
        // Refused by the host (protocol version, no codec in common): that was all.
        let Some(_route) = route else {
            return Ok(());
        };
        match tokio::io::copy_bidirectional(&mut tls, &mut host).await {
            Ok(_) => Ok(()),
            // A client that leaves without closing TLS, or a session that ends.
            Err(e) => {
                debug!(%peer, %e, "Relay ended");
                Ok(())
            }
        }
    }

    /// The account of `user` if `password` is its system password and it may have a
    /// session here.
    fn log_in(&self, user: &str, password: &str) -> Option<Account> {
        // Checked first: the name goes into a unit name and a path.
        if !valid_user_name(user) || !crate::auth::verify_system_password(user, password) {
            return None;
        }
        let account = account(user)?;
        if account.uid < MIN_UID {
            warn!(user, uid = account.uid, "System accounts get no session");
            return None;
        }
        Some(account)
    }

    /// Connects to the host of `account`'s session, starting the session first if it is
    /// not running.
    async fn session(
        &self,
        account: &Account,
        peer: SocketAddr,
    ) -> Result<(tokio::net::UnixStream, u16)> {
        let unit = format!("{}@{}.service", self.opts.session_unit, account.name);
        {
            let _starting = self.starting.lock().await;
            if !unit_active(&unit).await {
                self.assign_gpu(&account.name).await?;
                info!(unit, "Starting the session");
                systemctl(&["start", unit.as_str()]).await?;
            }
        }
        // Its host takes clients once GNOME Shell and the encoder are up.
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            match handover::connect(account.uid, peer).await {
                Ok(h) => return Ok(h),
                Err(e) if Instant::now() >= deadline => {
                    return Err(e.context(format!(
                        "the session is not ready after {}s (journalctl -u {unit})",
                        START_TIMEOUT.as_secs()
                    )));
                }
                Err(e) => {
                    if systemctl(&["is-failed", "--quiet", unit.as_str()]).await.is_ok() {
                        return Err(e.context(format!("{unit} failed (journalctl -u {unit})")));
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
        }
    }

    /// Writes the GPU of `user`'s new session; nothing without `--gpus`.
    async fn assign_gpu(&self, user: &str) -> Result<()> {
        let file = Path::new(SESSIONS_DIR).join(format!("{user}.env"));
        if self.opts.gpus.is_empty() {
            let _ = tokio::fs::remove_file(&file).await;
            return Ok(());
        }
        let mut loads = vec![0usize; self.opts.gpus.len()];
        if let Ok(mut dir) = tokio::fs::read_dir(SESSIONS_DIR).await {
            while let Ok(Some(entry)) = dir.next_entry().await {
                let name = entry.file_name();
                let Some(other) = name.to_str().and_then(|n| n.strip_suffix(".env")) else {
                    continue;
                };
                if other == user || !valid_user_name(other) {
                    continue;
                }
                // Files of sessions that ended stay until that user logs in again.
                if !unit_active(&format!("{}@{other}.service", self.opts.session_unit)).await {
                    continue;
                }
                if let Ok(text) = tokio::fs::read_to_string(entry.path()).await
                    && let Some(i) = slot_of(&text)
                    && i < loads.len()
                {
                    loads[i] += 1;
                }
            }
        }
        let pinned = self
            .opts
            .pins
            .iter()
            .find(|(u, _)| u == user)
            .map(|&(_, i)| i);
        let index = pick_slot(&loads, pinned).context("no GPU")?;
        tokio::fs::create_dir_all(SESSIONS_DIR).await?;
        tokio::fs::write(&file, session_env(index, &self.opts.gpus[index])).await?;
        info!(user, gpu = ?self.opts.gpus[index], ?loads, "GPU for the new session");
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------
// UDP relay

/// The public UDP socket: each client's datagrams go to the loopback port of its session's
/// host, and the host's back to the client. A datagram belongs to the session whose key
/// opens it, so a client whose address changes (another network, a NAT that rebinds) is
/// followed as soon as its next hello arrives.
struct Relay {
    socket: Arc<UdpSocket>,
    routes: Mutex<Vec<Arc<Route>>>,
    next_id: AtomicU64,
}

struct Route {
    id: u64,
    key: PacketKey,
    /// Connected to the host's loopback port.
    local: UdpSocket,
    /// Where the client's datagrams come from.
    client: Mutex<Option<SocketAddr>>,
}

/// A route for as long as its client's control connection lasts.
struct RouteGuard {
    relay: Arc<Relay>,
    id: u64,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for RouteGuard {
    fn drop(&mut self) {
        self.task.abort();
        self.relay
            .routes
            .lock()
            .unwrap()
            .retain(|r| r.id != self.id);
    }
}

impl Relay {
    fn bind(addr: SocketAddr) -> Result<Arc<Self>> {
        let s = socket2::Socket::new(
            socket2::Domain::for_address(addr),
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?;
        // A large frame leaves as a burst of hundreds of packets.
        let _ = s.set_send_buffer_size(8 << 20);
        let _ = s.set_recv_buffer_size(1 << 20);
        s.bind(&addr.into())
            .with_context(|| format!("binding UDP {addr}"))?;
        s.set_nonblocking(true)?;
        let socket = UdpSocket::from_std(std::net::UdpSocket::from(s))?;
        Ok(Arc::new(Self {
            socket: Arc::new(socket),
            routes: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(0),
        }))
    }

    /// A route to the host listening on `host` for the session sealed with `key`.
    fn add(self: &Arc<Self>, key: &[u8; 32], host: SocketAddr) -> Result<RouteGuard> {
        let s = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?;
        let _ = s.set_recv_buffer_size(8 << 20);
        s.bind(&SocketAddr::from((Ipv4Addr::LOCALHOST, 0)).into())?;
        s.connect(&host.into())?;
        s.set_nonblocking(true)?;
        let local = UdpSocket::from_std(std::net::UdpSocket::from(s))?;
        let route = Arc::new(Route {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            key: PacketKey::new(key),
            local,
            client: Mutex::new(None),
        });
        self.routes.lock().unwrap().push(Arc::clone(&route));
        let id = route.id;
        let task = tokio::spawn(to_client(Arc::clone(&self.socket), route));
        Ok(RouteGuard {
            relay: Arc::clone(self),
            id,
            task,
        })
    }

    /// Datagrams from clients to their hosts.
    async fn from_clients(self: Arc<Self>) {
        let mut buf = vec![0u8; 2048];
        loop {
            let (n, from) = match self.socket.recv_from(&mut buf).await {
                Ok(r) => r,
                Err(e) => {
                    debug!(%e, "UDP receive failed");
                    continue;
                }
            };
            let packet = &buf[..n];
            let route = {
                let routes = self.routes.lock().unwrap();
                // The route this address used last, then any other.
                routes
                    .iter()
                    .filter(|r| *r.client.lock().unwrap() == Some(from))
                    .chain(routes.iter())
                    .find(|r| r.key.open(Direction::ToServer, packet).is_some())
                    .cloned()
            };
            let Some(route) = route else { continue };
            {
                let mut client = route.client.lock().unwrap();
                if *client != Some(from) {
                    info!(%from, "Native client media path open (relayed)");
                    *client = Some(from);
                }
            }
            if let Err(e) = route.local.send(packet).await {
                debug!(%e, "UDP relay to the host failed");
            }
        }
    }
}

/// Datagrams from a session's host to its client.
async fn to_client(socket: Arc<UdpSocket>, route: Arc<Route>) {
    let mut buf = vec![0u8; 2048];
    loop {
        let n = match route.local.recv(&mut buf).await {
            Ok(n) => n,
            // The host's port is gone for a moment (ICMP unreachable): not a reason to spin.
            Err(e) => {
                debug!(%e, "UDP relay from the host failed");
                tokio::time::sleep(Duration::from_millis(5)).await;
                continue;
            }
        };
        let client = *route.client.lock().unwrap();
        if let Some(client) = client
            && let Err(e) = socket.send_to(&buf[..n], client).await
        {
            debug!(%e, "UDP send to the client failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_names() {
        for ok in ["alice", "bob.smith", "u_1", "a-b"] {
            assert!(valid_user_name(ok), "{ok}");
        }
        for bad in ["", "-x", ".x", "a/b", "a b", "a@b", "ünï", "x".repeat(33).as_str()] {
            assert!(!valid_user_name(bad), "{bad}");
        }
    }

    #[test]
    fn gpus_and_pins_parse() {
        assert_eq!(parse_gpu("1"), Ok(GpuSlot::Cuda(1)));
        assert_eq!(
            parse_gpu("/dev/dri/renderD129"),
            Ok(GpuSlot::RenderNode("/dev/dri/renderD129".into()))
        );
        assert!(parse_gpu("nvidia").is_err());
        assert_eq!(parse_pin("alice=2"), Ok(("alice".into(), 2)));
        assert!(parse_pin("alice").is_err());
        assert!(parse_pin("a/b=1").is_err());
        assert!(parse_pin("alice=x").is_err());
    }

    #[test]
    fn least_used_gpu_first() {
        assert_eq!(pick_slot(&[], None), None);
        assert_eq!(pick_slot(&[0, 0], None), Some(0));
        assert_eq!(pick_slot(&[1, 0, 0], None), Some(1));
        assert_eq!(pick_slot(&[2, 1, 1], None), Some(1));
        // Pinned, even when busier; an index out of range falls back.
        assert_eq!(pick_slot(&[0, 3], Some(1)), Some(1));
        assert_eq!(pick_slot(&[1, 0], Some(5)), Some(1));
    }

    #[test]
    fn session_environment() {
        let env = session_env(1, &GpuSlot::Cuda(3));
        assert_eq!(env, "GLIDEDESK_GPU_SLOT=1\nGLIDEDESK_GPU=3\n");
        assert_eq!(slot_of(&env), Some(1));
        let env = session_env(0, &GpuSlot::RenderNode("/dev/dri/renderD128".into()));
        assert!(env.contains("GLIDEDESK_ENCODER=vaapi\n"));
        assert!(env.contains("GLIDEDESK_RENDER_NODE=/dev/dri/renderD128\n"));
        assert_eq!(slot_of(&env), Some(0));
        assert_eq!(slot_of("GLIDEDESK_GPU=1\n"), None);
    }
}
