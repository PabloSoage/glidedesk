//! Desktop integration on Linux (and other Unix systems): dialogs, saved passwords and the
//! `.glidedesk` file type, through the usual desktop tools rather than libraries, so the
//! client builds and runs without them.
//!
//! - Dialogs: `zenity` (GNOME and most desktops), else `kdialog` (KDE), else the terminal
//!   the client was started from. With none of them a question is answered no.
//! - Passwords: the desktop's Secret Service keyring (GNOME Keyring, KWallet) through
//!   `secret-tool` (package `libsecret-tools`), under `service=glidedesk host=<host>
//!   user=<user>`.
//! - File type: `register_files` writes a desktop entry and a MIME type for `.glidedesk`
//!   under `~/.local/share` and makes glidedesk their default application.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A program on the `PATH`.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// A graphical session to show dialogs in.
fn graphical() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some()
}

fn interactive_terminal() -> bool {
    // SAFETY: isatty only looks at the descriptor.
    unsafe { libc::isatty(0) == 1 && libc::isatty(2) == 1 }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dialogs {
    Zenity,
    Kdialog,
    Terminal,
    None,
}

fn dialogs() -> Dialogs {
    if graphical() {
        if which("zenity").is_some() {
            return Dialogs::Zenity;
        }
        if which("kdialog").is_some() {
            return Dialogs::Kdialog;
        }
    }
    if interactive_terminal() {
        Dialogs::Terminal
    } else {
        Dialogs::None
    }
}

/// Runs a dialog; its standard output (without the final newline) if it was accepted.
fn run(cmd: &mut Command) -> Option<String> {
    let out = cmd.stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    while s.ends_with('\n') || s.ends_with('\r') {
        s.pop();
    }
    Some(s)
}

/// An error the user should see. On a terminal it is already on stderr.
pub fn show_error(text: &str) {
    eprintln!("{text}");
    if interactive_terminal() || !graphical() {
        return;
    }
    if which("zenity").is_some() {
        let _ = Command::new("zenity")
            .args(["--error", "--title=glidedesk", "--no-markup", "--text"])
            .arg(text)
            .stdin(Stdio::null())
            .spawn();
    } else if which("kdialog").is_some() {
        let _ = Command::new("kdialog")
            .args(["--title", "glidedesk", "--error"])
            .arg(text)
            .stdin(Stdio::null())
            .spawn();
    }
}

/// A yes/no question; no unless the user says yes.
pub fn confirm(text: &str) -> bool {
    match dialogs() {
        Dialogs::Zenity => run(Command::new("zenity").args([
            "--question",
            "--title=glidedesk",
            "--no-markup",
            "--default-cancel",
            "--icon-name=dialog-warning",
            "--width=520",
            "--text",
            text,
        ]))
        .is_some(),
        Dialogs::Kdialog => run(Command::new("kdialog").args([
            "--title",
            "glidedesk",
            "--warningyesno",
            text,
        ]))
        .is_some(),
        Dialogs::Terminal => {
            eprint!("{text}\n[y/N] ");
            let _ = std::io::stderr().flush();
            let mut line = String::new();
            if std::io::stdin().lock().read_line(&mut line).is_err() {
                return false;
            }
            matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
        }
        Dialogs::None => {
            tracing::error!(
                "{text}\nNo dialog program (zenity, kdialog) and no terminal to ask in: \
                 answered no. Pass --fingerprint to trust a server without asking."
            );
            false
        }
    }
}

// ---------------------------------------------------------------------------------------
// Passwords

fn secret_tool() -> bool {
    which("secret-tool").is_some()
}

/// The saved user and password for `host` (the given user's, when there is one).
fn saved(host: &str, user: Option<&str>) -> Option<(String, String)> {
    if !secret_tool() {
        return None;
    }
    let out = Command::new("secret-tool")
        .args(["search", "--unlock", "service", "glidedesk", "host", host])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_search(&String::from_utf8_lossy(&out.stdout))
        .into_iter()
        .find(|(u, _)| user.is_none_or(|w| w == u))
}

/// `secret-tool search` output: one block per item, `key = value` lines.
fn parse_search(out: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let (mut user, mut secret) = (None::<String>, None::<String>);
    let mut flush = |user: &mut Option<String>, secret: &mut Option<String>| {
        if let (Some(u), Some(s)) = (user.take(), secret.take()) {
            found.push((u, s));
        }
        *user = None;
        *secret = None;
    };
    for line in out.lines() {
        if line.starts_with('[') {
            flush(&mut user, &mut secret);
            continue;
        }
        let Some((k, v)) = line.split_once(" = ") else {
            continue;
        };
        match k.trim() {
            "attribute.user" => user = Some(v.to_owned()),
            "secret" => secret = Some(v.to_owned()),
            _ => {}
        }
    }
    flush(&mut user, &mut secret);
    found
}

fn save(host: &str, user: &str, password: &str) {
    if !secret_tool() {
        return;
    }
    let child = Command::new("secret-tool")
        .args(["store", "--label"])
        .arg(format!("glidedesk: {user}@{host}"))
        .args(["service", "glidedesk", "host", host, "user", user])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return };
    if let Some(mut stdin) = child.stdin.take() {
        // Without a newline: secret-tool stores what it reads.
        let _ = stdin.write_all(password.as_bytes());
    }
    match child.wait() {
        Ok(s) if s.success() => tracing::info!(host, user, "Password saved in the keyring"),
        _ => tracing::warn!(host, "Could not save the password in the keyring"),
    }
}

pub fn forget(host: &str) {
    if secret_tool() {
        let _ = Command::new("secret-tool")
            .args(["clear", "service", "glidedesk", "host", host])
            .stdin(Stdio::null())
            .status();
    }
}

/// Reads a line from the terminal without echo.
fn read_hidden(prompt: &str) -> Option<String> {
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    // SAFETY: termios is plain data; the settings are restored before returning.
    let old = unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut t) != 0 {
            return None;
        }
        let old = t;
        t.c_lflag &= !libc::ECHO;
        libc::tcsetattr(0, libc::TCSANOW, &t);
        old
    };
    let mut line = String::new();
    let r = std::io::stdin().lock().read_line(&mut line);
    // SAFETY: as above.
    unsafe { libc::tcsetattr(0, libc::TCSANOW, &old) };
    eprintln!();
    r.ok()?;
    Some(line.trim_end_matches(['\n', '\r']).to_owned())
}

/// Asks for the user (unless given) and the password; also whether to save them.
fn ask(host: &str, user: Option<&str>) -> Option<(String, String, bool)> {
    match dialogs() {
        Dialogs::Zenity => {
            let (user, password) = match user {
                Some(u) => {
                    let p = run(Command::new("zenity").args([
                        "--password",
                        "--title",
                        format!("Sign in to {host} as {u}").as_str(),
                    ]))?;
                    (u.to_owned(), p)
                }
                None => {
                    // "user|password"
                    let s = run(Command::new("zenity").args([
                        "--password",
                        "--username",
                        "--title",
                        format!("Sign in to {host}").as_str(),
                    ]))?;
                    let (u, p) = s.split_once('|')?;
                    (u.to_owned(), p.to_owned())
                }
            };
            let keep = secret_tool()
                && run(Command::new("zenity").args([
                    "--question",
                    "--title=glidedesk",
                    "--no-markup",
                    "--text",
                    format!("Save the password for {user}@{host} in the keyring?").as_str(),
                ]))
                .is_some();
            Some((user, password, keep))
        }
        Dialogs::Kdialog => {
            let user = match user {
                Some(u) => u.to_owned(),
                None => run(Command::new("kdialog").args([
                    "--title",
                    "glidedesk",
                    "--inputbox",
                    format!("User on {host}").as_str(),
                ]))?,
            };
            let password = run(Command::new("kdialog").args([
                "--title",
                "glidedesk",
                "--password",
                format!("Password for {user}@{host}").as_str(),
            ]))?;
            let keep = secret_tool()
                && run(Command::new("kdialog").args([
                    "--title",
                    "glidedesk",
                    "--yesno",
                    format!("Save the password for {user}@{host} in the keyring?").as_str(),
                ]))
                .is_some();
            Some((user, password, keep))
        }
        Dialogs::Terminal => {
            let user = match user {
                Some(u) => u.to_owned(),
                None => {
                    eprint!("User on {host}: ");
                    let _ = std::io::stderr().flush();
                    let mut line = String::new();
                    std::io::stdin().lock().read_line(&mut line).ok()?;
                    line.trim().to_owned()
                }
            };
            let password = read_hidden(&format!("Password for {user}@{host}: "))?;
            let keep = secret_tool() && {
                eprint!("Save it in the keyring? [y/N] ");
                let _ = std::io::stderr().flush();
                let mut line = String::new();
                std::io::stdin().lock().read_line(&mut line).is_ok()
                    && matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
            };
            Some((user, password, keep))
        }
        Dialogs::None => None,
    }
}

/// The saved credentials for `host`, else asked for (and saved if the user wants).
pub fn credentials(host: &str, user: Option<&str>) -> Option<(String, String)> {
    if let Some(found) = saved(host, user) {
        return Some(found);
    }
    let (user, password, keep) = ask(host, user)?;
    if user.is_empty() {
        return None;
    }
    if keep {
        save(host, &user, &password);
    }
    Some((user, password))
}

// ---------------------------------------------------------------------------------------
// The .glidedesk file type

const MIME: &str = "application/x-glidedesk";
const ICON_PNG: &[u8] = include_bytes!("../../../assets/logo/glidedesk-512.png");

fn data_home() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
}

/// A path as the `Exec` key wants it: quoted, with `"`, `` ` ``, `$` and `\` escaped.
fn exec_quote(p: &Path) -> String {
    let mut s = String::from("\"");
    for c in p.to_string_lossy().chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            s.push('\\');
        }
        s.push(c);
    }
    s.push('"');
    s
}

fn desktop_entry(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=glidedesk\n\
         GenericName=Remote desktop\n\
         Comment=Connect to a glidedesk or RDP server\n\
         Exec={} %f\n\
         Icon=glidedesk\n\
         Terminal=false\n\
         Categories=Network;RemoteAccess;\n\
         MimeType={MIME};\n",
        exec_quote(exe)
    )
}

fn mime_package() -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <mime-info xmlns=\"http://www.freedesktop.org/standards/shared-mime-info\">\n\
         \x20 <mime-type type=\"{MIME}\">\n\
         \x20   <comment>glidedesk connection</comment>\n\
         \x20   <glob pattern=\"*.glidedesk\"/>\n\
         \x20   <sub-class-of type=\"text/plain\"/>\n\
         \x20   <icon name=\"glidedesk\"/>\n\
         \x20 </mime-type>\n\
         </mime-info>\n"
    )
}

/// Makes this program the one that opens `.glidedesk` files (for this user).
pub fn register_files() -> anyhow::Result<String> {
    use anyhow::Context as _;
    let exe = std::env::current_exe().context("finding this program")?;
    let data = data_home().context("no XDG_DATA_HOME or HOME")?;
    let apps = data.join("applications");
    let mime = data.join("mime");
    let icons = data.join("icons/hicolor/512x512/apps");
    for d in [&apps, &mime.join("packages"), &icons] {
        std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
    }
    std::fs::write(apps.join("glidedesk.desktop"), desktop_entry(&exe))?;
    std::fs::write(mime.join("packages/glidedesk.xml"), mime_package())?;
    std::fs::write(icons.join("glidedesk.png"), ICON_PNG)?;
    // The databases are optional: without the tools the files are picked up at the next
    // login or by the desktop's own monitoring.
    let _ = Command::new("update-mime-database").arg(&mime).status();
    let _ = Command::new("update-desktop-database").arg(&apps).status();
    let _ = Command::new("xdg-mime")
        .args(["default", "glidedesk.desktop", MIME])
        .status();
    Ok(format!(
        "glidedesk now opens .glidedesk files ({} and {}).",
        apps.join("glidedesk.desktop").display(),
        mime.join("packages/glidedesk.xml").display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_secret_tool_search() {
        let out = "[/org/freedesktop/secrets/collection/login/12]\n\
                   label = glidedesk: ana@server\n\
                   secret = pa = ss\n\
                   created = 2026-11-27 10:00:00\n\
                   schema = org.freedesktop.Secret.Generic\n\
                   attribute.host = server\n\
                   attribute.service = glidedesk\n\
                   attribute.user = ana\n\
                   [/org/freedesktop/secrets/collection/login/13]\n\
                   label = x\n\
                   attribute.user = bob\n\
                   secret = 2\n";
        assert_eq!(
            parse_search(out),
            vec![
                ("ana".to_owned(), "pa = ss".to_owned()),
                ("bob".to_owned(), "2".to_owned())
            ]
        );
    }

    #[test]
    fn desktop_entry_quotes_the_program() {
        let e = desktop_entry(Path::new("/opt/my $apps/glidedesk"));
        assert!(e.contains("Exec=\"/opt/my \\$apps/glidedesk\" %f\n"));
        assert!(e.contains("MimeType=application/x-glidedesk;"));
        assert!(mime_package().contains("*.glidedesk"));
    }
}
