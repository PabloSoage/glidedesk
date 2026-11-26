//! The apps a client can open instead of the whole desktop.
//!
//! One list for every front end (GameStream's `/applist`, the native protocol, RDP's
//! alternate shell, the web page), read from `<state dir>/apps.conf`:
//!
//! ```text
//! # Name = command line (run in the session)
//! Firefox = firefox --kiosk
//! Terminal = gnome-terminal
//! ```
//!
//! "Desktop" (id 1) is always first and opens the desktop as before.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::sync::broadcast;
use tracing::{info, warn};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct App {
    /// Stable while the name stays (Moonlight caches ids).
    pub id: u32,
    pub name: String,
    /// `None` for the desktop.
    pub command: Option<String>,
}

pub const DESKTOP_ID: u32 = 1;

/// The desktop plus the apps in `apps.conf` (missing file: only the desktop).
pub fn load(state_dir: &Path) -> Vec<App> {
    let text = std::fs::read_to_string(state_dir.join("apps.conf")).unwrap_or_default();
    parse(&text)
}

pub fn parse(text: &str) -> Vec<App> {
    let mut apps = vec![App {
        id: DESKTOP_ID,
        name: "Desktop".into(),
        command: None,
    }];
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, command)) = line.split_once('=') else {
            tracing::warn!(line, "apps.conf: expected `Name = command`");
            continue;
        };
        let (name, command) = (name.trim(), command.trim());
        if name.is_empty() || command.is_empty() || apps.iter().any(|a| a.name == name) {
            continue;
        }
        apps.push(App {
            id: id_for(name),
            name: name.to_owned(),
            command: Some(command.to_owned()),
        });
    }
    apps
}

/// An app that ended: its name, and why when it was not a normal exit.
#[derive(Clone, Debug)]
pub struct Exited {
    pub name: String,
    pub reason: Option<String>,
}

/// A process that exits this soon after starting cleanly is taken for a launcher that handed
/// over to another process (as Sunshine does): the app is not reported as ended.
const LAUNCHER_GRACE: Duration = Duration::from_secs(5);

/// Starts apps in the session and reports when they end. One instance of each app: opening
/// a running one again just reuses it.
pub struct Runner {
    state_dir: PathBuf,
    running: Mutex<Vec<(String, u32)>>,
    exited: broadcast::Sender<Exited>,
    grace: Duration,
}

impl Runner {
    pub fn new(state_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            state_dir,
            running: Mutex::default(),
            exited: broadcast::channel(8).0,
            grace: LAUNCHER_GRACE,
        })
    }

    pub fn list(&self) -> Vec<App> {
        load(&self.state_dir)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Exited> {
        self.exited.subscribe()
    }

    /// Opens the app called `name` (the desktop needs nothing).
    pub fn open(self: &Arc<Self>, name: &str) -> Result<()> {
        let app = self
            .list()
            .into_iter()
            .find(|a| a.name == name)
            .with_context(|| format!("there is no app called {name:?} on this host"))?;
        let Some(command) = app.command else {
            return Ok(());
        };
        if self
            .running
            .lock()
            .unwrap()
            .iter()
            .any(|(n, _)| *n == app.name)
        {
            info!(app = app.name, "App already running");
            return Ok(());
        }
        let words = split_command(&command);
        let Some((program, args)) = words.split_first() else {
            bail!("the command of {name:?} is empty");
        };
        let mut child = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("starting {program:?}"))?;
        let pid = child.id();
        info!(app = app.name, pid, "App started");
        self.running.lock().unwrap().push((app.name.clone(), pid));
        #[cfg(windows)]
        std::thread::spawn(move || maximize_windows_of(pid));
        let this = Arc::clone(self);
        let (started, grace) = (Instant::now(), self.grace);
        std::thread::Builder::new()
            .name("app-wait".into())
            .spawn(move || {
                let status = child.wait();
                this.running.lock().unwrap().retain(|&(_, p)| p != pid);
                let quick_clean =
                    started.elapsed() < grace && status.as_ref().is_ok_and(|s| s.success());
                if quick_clean {
                    info!(
                        app = app.name,
                        "App launcher handed over; not watching it further"
                    );
                    return;
                }
                let reason = match status {
                    Ok(s) if s.success() => None,
                    Ok(s) => Some(format!("it exited with {s}")),
                    Err(e) => Some(e.to_string()),
                };
                info!(app = app.name, ?reason, "App ended");
                let _ = this.exited.send(Exited {
                    name: app.name,
                    reason,
                });
            })?;
        Ok(())
    }
}

/// Maximizes the top-level windows of `pid` as they appear (for a few seconds): the app
/// fills the screen instead of floating on the desktop.
#[cfg(windows)]
fn maximize_windows_of(pid: u32) {
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GW_OWNER, GetWindow, GetWindowThreadProcessId, IsWindowVisible, SW_MAXIMIZE,
        SetForegroundWindow, ShowWindow,
    };
    unsafe extern "system" fn each(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
        // SAFETY: lparam points to the (pid, found) pair below for the length of EnumWindows.
        let state = unsafe { &mut *(lparam.0 as *mut (u32, Vec<HWND>)) };
        let mut owner_pid = 0u32;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut owner_pid));
            if owner_pid == state.0
                && IsWindowVisible(hwnd).as_bool()
                && GetWindow(hwnd, GW_OWNER).is_err()
            {
                state.1.push(hwnd);
            }
        }
        true.into()
    }
    let mut done: Vec<isize> = Vec::new();
    for _ in 0..50 {
        let mut state: (u32, Vec<HWND>) = (pid, Vec::new());
        // SAFETY: the callback only reads window properties; `state` outlives the call.
        unsafe {
            let _ = EnumWindows(Some(each), LPARAM(&mut state as *mut _ as isize));
        }
        for hwnd in state.1 {
            if !done.contains(&(hwnd.0 as isize)) {
                // SAFETY: plain window calls on a window handle we just enumerated.
                unsafe {
                    let _ = ShowWindow(hwnd, SW_MAXIMIZE);
                    let _ = SetForegroundWindow(hwnd);
                }
                done.push(hwnd.0 as isize);
            }
        }
        if !done.is_empty() {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    warn!(pid, "No window of the app to maximize");
}

/// FNV-1a of the name, kept positive and away from the desktop's id.
fn id_for(name: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in name.bytes() {
        h = (h ^ u32::from(b)).wrapping_mul(0x0100_0193);
    }
    (h & 0x7fff_ffff).max(2)
}

/// Splits a command line into words: spaces separate, quotes group, `\` escapes.
pub fn split_command(cmd: &str) -> Vec<String> {
    let (mut words, mut cur, mut quote, mut any) = (Vec::new(), String::new(), None, false);
    let mut chars = cmd.chars();
    while let Some(c) = chars.next() {
        match (c, quote) {
            ('\\', _) => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                    any = true;
                }
            }
            ('"' | '\'', None) => {
                quote = Some(c);
                any = true;
            }
            (c, Some(q)) if c == q => quote = None,
            (c, None) if c.is_whitespace() => {
                if any {
                    words.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            (c, _) => {
                cur.push(c);
                any = true;
            }
        }
    }
    if any {
        words.push(cur);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_list() {
        let apps =
            parse("# comment\nFirefox = firefox --kiosk\n\nbad line\nFirefox = again\nTerm=xterm");
        assert_eq!(apps.len(), 3);
        assert_eq!(apps[0].id, DESKTOP_ID);
        assert_eq!(apps[1].name, "Firefox");
        assert_eq!(apps[1].command.as_deref(), Some("firefox --kiosk"));
        assert_eq!(apps[1].id, parse("Firefox = other")[1].id);
        assert_ne!(apps[1].id, apps[2].id);
        assert!(apps.iter().all(|a| a.id > 0 && a.id <= i32::MAX as u32));
    }

    fn runner(conf: &str, grace: Duration) -> (Arc<Runner>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("gd-apps-{}", rand_tag()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("apps.conf"), conf).unwrap();
        let mut r = Runner::new(dir.clone());
        Arc::get_mut(&mut r).unwrap().grace = grace;
        (r, dir)
    }

    fn rand_tag() -> String {
        glidedesk_proto::random::<4>()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// A command that waits ms and exits with code.
    fn waiter(ms: u32, code: i32) -> String {
        if cfg!(windows) {
            format!(
                "powershell -NoProfile -Command \"Start-Sleep -Milliseconds {ms}; exit {code}\""
            )
        } else {
            format!("sh -c \"sleep {}; exit {code}\"", f64::from(ms) / 1000.0)
        }
    }

    #[tokio::test]
    async fn reports_apps_that_end() {
        let conf = format!("Fails = {}\nQuick = {}\n", waiter(300, 3), waiter(0, 0));
        let (r, dir) = runner(&conf, Duration::from_millis(150));
        let mut exits = r.subscribe();
        r.open("Desktop").unwrap();
        assert!(r.open("Nope").is_err());
        r.open("Fails").unwrap();
        // Opening it again while it runs reuses it.
        r.open("Fails").unwrap();
        let e = tokio::time::timeout(Duration::from_secs(20), exits.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(e.name, "Fails");
        assert!(e.reason.unwrap().contains('3'));
        // A clean exit within the grace time is a launcher: nothing is reported.
        let (r, dir2) = runner(&conf, Duration::from_secs(20));
        let mut exits = r.subscribe();
        r.open("Quick").unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(3), exits.recv())
                .await
                .is_err()
        );
        std::fs::remove_dir_all(dir).ok();
        std::fs::remove_dir_all(dir2).ok();
    }

    #[test]
    fn splits_commands() {
        assert_eq!(
            split_command(r#"code --new-window "/home/me/My Project" it\'s ''"#),
            vec!["code", "--new-window", "/home/me/My Project", "it's", ""]
        );
    }
}
