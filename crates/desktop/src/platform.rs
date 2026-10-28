//! Desktop integration: the log file, error dialogs and, on Windows, the window menu.
//!
//! The Windows build is a GUI program (no console window), so the log goes to a file and
//! the window's system menu (right click on the title bar, or Alt+Space) opens it.

use std::path::PathBuf;
use std::sync::Mutex;

use tracing_subscriber::fmt::writer::MakeWriterExt;
use winit::event_loop::EventLoopProxy;
use winit::window::Window;

use crate::video::Event;

/// `%LOCALAPPDATA%\glidedesk\glidedesk.log`, or `$XDG_STATE_HOME/glidedesk/glidedesk.log`.
pub fn log_path() -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
    };
    base.unwrap_or_else(std::env::temp_dir)
        .join("glidedesk")
        .join("glidedesk.log")
}

/// Logs to stderr and to the log file (started afresh on each run).
pub fn init_logging() {
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let path = log_path();
    let file = path
        .parent()
        .and_then(|d| std::fs::create_dir_all(d).ok())
        .and_then(|()| std::fs::File::create(&path).ok());
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false);
    match file {
        Some(f) => builder
            .with_writer(std::io::stderr.and(Mutex::new(f)))
            .init(),
        None => builder.with_writer(std::io::stderr).init(),
    }
    // Without a console a panic would vanish: log it and show it.
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("{info}");
        show_error(&format!("glidedesk stopped: {info}"));
        default(info);
    }));
}

/// Tells the user something went wrong: a dialog on Windows (there is no console).
pub fn show_error(text: &str) {
    tracing::error!("{text}");
    #[cfg(windows)]
    win::message_box(text, true);
    #[cfg(not(windows))]
    eprintln!("{text}");
}

/// Plain information (help, version) in a dialog.
#[cfg(windows)]
pub fn show_message(text: &str) {
    win::message_box(text, false);
}

/// Opens the log file with the system's text viewer.
pub fn open_log() {
    let path = log_path();
    #[cfg(windows)]
    win::open(&path);
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(&path).spawn();
    }
}

/// User and password for `host`: saved ones from the Windows Credential Manager, else the
/// Windows sign-in dialog (which can save them). `None` when cancelled or unavailable.
pub fn credentials(host: &str, user: Option<&str>) -> Option<(String, String)> {
    #[cfg(windows)]
    return win::credentials(host, user);
    #[cfg(not(windows))]
    {
        let _ = (host, user);
        None
    }
}

/// The saved password for `host` was refused: forget it so the next start asks again.
pub fn forget_credentials(host: &str) {
    #[cfg(windows)]
    win::forget(host);
    #[cfg(not(windows))]
    let _ = host;
}

/// Adds the glidedesk entries to the window menu (Windows); they come back as events.
pub fn add_menu(window: &Window, proxy: EventLoopProxy<Event>) {
    #[cfg(windows)]
    win::add_menu(window, proxy);
    #[cfg(not(windows))]
    let _ = (window, proxy);
}

#[cfg(windows)]
mod win {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::sync::OnceLock;

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::Security::Credentials::{
        CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CREDUI_FLAGS_ALWAYS_SHOW_UI,
        CREDUI_FLAGS_DO_NOT_PERSIST, CREDUI_FLAGS_GENERIC_CREDENTIALS,
        CREDUI_FLAGS_SHOW_SAVE_CHECK_BOX, CREDUI_INFOW, CREDUI_MAX_USERNAME_LENGTH, CredDeleteW,
        CredFree, CredReadW, CredUIPromptForCredentialsW, CredWriteW,
    };
    use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass, ShellExecuteW};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, GetSystemMenu, MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MF_SEPARATOR,
        MF_STRING, MessageBoxW, SW_SHOWNORMAL, WM_SYSCOMMAND,
    };
    use winit::event_loop::EventLoopProxy;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use winit::window::Window;

    use crate::video::Event;

    // System menu command ids: below 0xF000, low four bits clear (Windows uses them).
    const ID_FULLSCREEN: usize = 0x1000;
    const ID_LOG: usize = 0x1010;

    static PROXY: OnceLock<EventLoopProxy<Event>> = OnceLock::new();

    fn wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(Some(0)).collect()
    }

    pub fn message_box(text: &str, error: bool) {
        let (text, title) = (wide(OsStr::new(text)), wide(OsStr::new("glidedesk")));
        let icon = if error {
            MB_ICONERROR
        } else {
            MB_ICONINFORMATION
        };
        // SAFETY: both strings are NUL-terminated and outlive the call.
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                text.as_ptr(),
                title.as_ptr(),
                MB_OK | icon,
            );
        }
    }

    pub fn open(path: &Path) {
        let (verb, file) = (wide(OsStr::new("open")), wide(path.as_os_str()));
        // SAFETY: NUL-terminated strings that outlive the call; no parameters or directory.
        unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                verb.as_ptr(),
                file.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            );
        }
    }

    fn target(host: &str) -> Vec<u16> {
        wide(OsStr::new(&format!("glidedesk:{host}")))
    }

    fn from_wide(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    pub fn credentials(host: &str, user: Option<&str>) -> Option<(String, String)> {
        let target = target(host);
        let mut cred: *mut CREDENTIALW = std::ptr::null_mut();
        // SAFETY: `target` is NUL-terminated; a returned credential is read, then freed.
        unsafe {
            if CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut cred) != 0 {
                let c = &*cred;
                let blob =
                    std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize);
                let pass: Vec<u16> = blob
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| u16::from_le_bytes(*b))
                    .collect();
                let mut name = Vec::new();
                let mut p = c.UserName;
                while !p.is_null() && *p != 0 {
                    name.push(*p);
                    p = p.add(1);
                }
                CredFree(cred.cast());
                let saved = String::from_utf16_lossy(&name);
                if user.is_none_or(|u| u == saved) {
                    return Some((saved, String::from_utf16_lossy(&pass)));
                }
            }
        }

        let mut name = [0u16; CREDUI_MAX_USERNAME_LENGTH as usize + 1];
        for (d, s) in name.iter_mut().zip(user.unwrap_or("").encode_utf16()) {
            *d = s;
        }
        // CREDUI_MAX_PASSWORD_LENGTH (not in windows-sys).
        let mut pass = [0u16; 256 + 1];
        let message = wide(OsStr::new(&format!("Sign in to {host}")));
        let caption = wide(OsStr::new("glidedesk"));
        let info = CREDUI_INFOW {
            cbSize: std::mem::size_of::<CREDUI_INFOW>() as u32,
            hwndParent: std::ptr::null_mut(),
            pszMessageText: message.as_ptr(),
            pszCaptionText: caption.as_ptr(),
            hbmBanner: std::ptr::null_mut(),
        };
        let mut save = 1;
        // SAFETY: the buffers have the sizes given; the strings outlive the call.
        let r = unsafe {
            CredUIPromptForCredentialsW(
                &info,
                target.as_ptr(),
                std::ptr::null_mut(),
                0,
                name.as_mut_ptr(),
                name.len() as u32,
                pass.as_mut_ptr(),
                pass.len() as u32,
                &mut save,
                CREDUI_FLAGS_GENERIC_CREDENTIALS
                    | CREDUI_FLAGS_SHOW_SAVE_CHECK_BOX
                    | CREDUI_FLAGS_DO_NOT_PERSIST
                    | CREDUI_FLAGS_ALWAYS_SHOW_UI,
            )
        };
        if r != 0 {
            return None;
        }
        let (user, password) = (from_wide(&name), from_wide(&pass));
        pass.fill(0);
        if save != 0 {
            let mut blob: Vec<u8> = password.encode_utf16().flat_map(u16::to_le_bytes).collect();
            let mut uname = wide(OsStr::new(&user));
            let mut tname = target.clone();
            let c = CREDENTIALW {
                Flags: 0,
                Type: CRED_TYPE_GENERIC,
                TargetName: tname.as_mut_ptr(),
                Comment: std::ptr::null_mut(),
                LastWritten: Default::default(),
                CredentialBlobSize: blob.len() as u32,
                CredentialBlob: blob.as_mut_ptr(),
                Persist: CRED_PERSIST_LOCAL_MACHINE,
                AttributeCount: 0,
                Attributes: std::ptr::null_mut(),
                TargetAlias: std::ptr::null_mut(),
                UserName: uname.as_mut_ptr(),
            };
            // SAFETY: every pointer in `c` refers to a live buffer for the call.
            unsafe { CredWriteW(&c, 0) };
            blob.fill(0);
        }
        Some((user, password))
    }

    pub fn forget(host: &str) {
        let target = target(host);
        // SAFETY: `target` is NUL-terminated.
        unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) };
    }

    pub fn add_menu(window: &Window, proxy: EventLoopProxy<Event>) {
        let Ok(handle) = window.window_handle() else {
            return;
        };
        let RawWindowHandle::Win32(h) = handle.as_raw() else {
            return;
        };
        let hwnd = h.hwnd.get() as HWND;
        let _ = PROXY.set(proxy);
        let full = wide(OsStr::new("Full screen"));
        let log = wide(OsStr::new("Show log"));
        // SAFETY: `hwnd` is the live window; the menu copies the strings.
        unsafe {
            let menu = GetSystemMenu(hwnd, 0);
            if menu.is_null() {
                return;
            }
            AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
            AppendMenuW(menu, MF_STRING, ID_FULLSCREEN, full.as_ptr());
            AppendMenuW(menu, MF_STRING, ID_LOG, log.as_ptr());
            SetWindowSubclass(hwnd, Some(subclass), 1, 0);
        }
    }

    unsafe extern "system" fn subclass(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _id: usize,
        _data: usize,
    ) -> LRESULT {
        if msg == WM_SYSCOMMAND {
            let event = match wparam & 0xFFF0 {
                ID_FULLSCREEN => Some(Event::ToggleFullscreen),
                ID_LOG => Some(Event::ShowLog),
                _ => None,
            };
            if let Some(e) = event {
                if let Some(p) = PROXY.get() {
                    let _ = p.send_event(e);
                }
                return 0;
            }
        }
        // SAFETY: forwarding the message unchanged to the next handler.
        unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
    }
}
