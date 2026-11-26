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

use std::path::Path;

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

    #[test]
    fn splits_commands() {
        assert_eq!(
            split_command(r#"code --new-window "/home/me/My Project" it\'s ''"#),
            vec!["code", "--new-window", "/home/me/My Project", "it's", ""]
        );
    }
}
