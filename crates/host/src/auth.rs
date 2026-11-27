//! Credential checks for the native and web front ends (RDP's go through NLA in IronRDP):
//! comparison in constant time, and a growing wait for addresses that keep failing.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ring::digest::{SHA256, digest};
use subtle::ConstantTimeEq;

/// With `--auth system`: the account whose system password logs in (the user the host runs
/// as), instead of the password given to the host.
static SYSTEM_USER: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Logins are checked against the system's own password of `user` from now on (PAM on
/// Linux, `LogonUser` on Windows).
pub fn use_system_accounts(user: String) {
    let _ = SYSTEM_USER.set(user);
}

/// Logins are checked against the system's accounts.
pub fn system_accounts() -> bool {
    SYSTEM_USER.get().is_some()
}

/// Whether `user` and `password` are the expected ones, in time that does not depend on how
/// much of them matches (both sides are hashed to the same length first).
///
/// With [`use_system_accounts`], the system checks the password of the host's own account
/// instead (`user` must be that account, or empty); the expected values are then unused.
pub fn verify_credentials(
    expected_user: &str,
    expected_pass: &str,
    user: &str,
    password: &str,
) -> bool {
    if let Some(account) = SYSTEM_USER.get() {
        if !user.is_empty() && user != account {
            return false;
        }
        return verify_system_password(account, password);
    }
    let h = |s: &str| digest(&SHA256, s.as_bytes());
    let u = h(expected_user).as_ref().ct_eq(h(user).as_ref());
    let p = h(expected_pass).as_ref().ct_eq(h(password).as_ref());
    bool::from(u & p)
}

/// Whether `password` is the system password of `user` (PAM on Linux, `LogonUser` on
/// Windows). Only root can check accounts other than its own on Linux, as
/// `glidedesk-host daemon` does.
pub fn verify_system_password(user: &str, password: &str) -> bool {
    // PAM and LogonUser can take seconds (a failure is delayed on purpose): off the async
    // workers where the runtime allows it.
    let check = || system::verify(user, password);
    match tokio::runtime::Handle::try_current() {
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(check)
        }
        _ => check(),
    }
}

#[cfg(target_os = "linux")]
mod system {
    //! PAM through libpam (`/etc/pam.d/glidedesk`, see `scripts/pam.d/glidedesk`). A
    //! process that is not root can check only its own user's password (pam_unix's
    //! `unix_chkpwd` helper), which is what `serve --auth system` needs; the root daemon
    //! checks any account.

    use std::ffi::{CStr, CString, c_char, c_int, c_void};

    const PAM_SUCCESS: c_int = 0;
    const PAM_PROMPT_ECHO_OFF: c_int = 1;
    const PAM_PROMPT_ECHO_ON: c_int = 2;
    const PAM_CONV_ERR: c_int = 19;
    const PAM_BUF_ERR: c_int = 5;
    /// The PAM service glidedesk logs in through.
    const SERVICE: &str = "glidedesk";

    #[repr(C)]
    struct PamMessage {
        msg_style: c_int,
        msg: *const c_char,
    }

    #[repr(C)]
    struct PamResponse {
        resp: *mut c_char,
        resp_retcode: c_int,
    }

    #[repr(C)]
    struct PamConv {
        conv: Option<
            unsafe extern "C" fn(
                c_int,
                *mut *const PamMessage,
                *mut *mut PamResponse,
                *mut c_void,
            ) -> c_int,
        >,
        appdata_ptr: *mut c_void,
    }

    #[link(name = "pam")]
    unsafe extern "C" {
        fn pam_start(
            service: *const c_char,
            user: *const c_char,
            conv: *const PamConv,
            handle: *mut *mut c_void,
        ) -> c_int;
        fn pam_authenticate(handle: *mut c_void, flags: c_int) -> c_int;
        fn pam_acct_mgmt(handle: *mut c_void, flags: c_int) -> c_int;
        fn pam_end(handle: *mut c_void, status: c_int) -> c_int;
        fn pam_strerror(handle: *mut c_void, errnum: c_int) -> *const c_char;
    }

    /// Answers PAM's prompts: the password for hidden ones, nothing for the rest. PAM frees
    /// the answers with `free`, so they are allocated with `calloc` and `strdup`.
    unsafe extern "C" fn conversation(
        n: c_int,
        messages: *mut *const PamMessage,
        responses: *mut *mut PamResponse,
        password: *mut c_void,
    ) -> c_int {
        if n <= 0 || messages.is_null() || responses.is_null() {
            return PAM_CONV_ERR;
        }
        // SAFETY: PAM passes `n` messages and a place for the answer array; `password` is
        // the CString owned by `verify` for the length of pam_authenticate.
        unsafe {
            let out = libc::calloc(n as usize, std::mem::size_of::<PamResponse>()) as *mut PamResponse;
            if out.is_null() {
                return PAM_BUF_ERR;
            }
            for i in 0..n as usize {
                // Linux-PAM passes an array of pointers (`messages[i]`).
                let m = *messages.add(i);
                if m.is_null() {
                    continue;
                }
                match (*m).msg_style {
                    PAM_PROMPT_ECHO_OFF | PAM_PROMPT_ECHO_ON => {
                        (*out.add(i)).resp = libc::strdup(password as *const c_char);
                    }
                    _ => {}
                }
            }
            *responses = out;
        }
        PAM_SUCCESS
    }

    pub fn verify(user: &str, password: &str) -> bool {
        let (Ok(service), Ok(cuser), Ok(cpass)) = (
            CString::new(SERVICE),
            CString::new(user),
            CString::new(password),
        ) else {
            return false;
        };
        let conv = PamConv {
            conv: Some(conversation),
            appdata_ptr: cpass.as_ptr() as *mut c_void,
        };
        let mut handle: *mut c_void = std::ptr::null_mut();
        // SAFETY: valid C strings and conversation that outlive the PAM transaction, which
        // ends with pam_end on every path.
        unsafe {
            let r = pam_start(service.as_ptr(), cuser.as_ptr(), &conv, &mut handle);
            if r != PAM_SUCCESS {
                tracing::warn!(code = r, "pam_start failed (is /etc/pam.d/glidedesk there?)");
                return false;
            }
            let mut r = pam_authenticate(handle, 0);
            if r == PAM_SUCCESS {
                r = pam_acct_mgmt(handle, 0);
            }
            if r != PAM_SUCCESS {
                let why = pam_strerror(handle, r);
                if !why.is_null() {
                    tracing::debug!(error = %CStr::from_ptr(why).to_string_lossy(), "PAM refused");
                }
            }
            pam_end(handle, r);
            r == PAM_SUCCESS
        }
    }
}

#[cfg(windows)]
mod system {
    //! `LogonUserW` with a network logon: it checks the password and creates no session.

    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{LOGON32_LOGON_NETWORK, LOGON32_PROVIDER_DEFAULT, LogonUserW};
    use windows::core::PCWSTR;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    pub fn verify(user: &str, password: &str) -> bool {
        // DOMAIN\user or user@domain as given; a plain name is a local account.
        let (domain, name) = match user.split_once('\\') {
            Some((d, n)) => (d.to_owned(), n.to_owned()),
            None if user.contains('@') => (String::new(), user.to_owned()),
            None => (".".to_owned(), user.to_owned()),
        };
        let (name, password) = (wide(&name), wide(password));
        let domain = (!domain.is_empty()).then(|| wide(&domain));
        let mut token = HANDLE::default();
        // SAFETY: NUL-terminated strings that outlive the call; the token is closed.
        unsafe {
            let ok = LogonUserW(
                PCWSTR(name.as_ptr()),
                domain
                    .as_ref()
                    .map_or(PCWSTR::null(), |d| PCWSTR(d.as_ptr())),
                PCWSTR(password.as_ptr()),
                LOGON32_LOGON_NETWORK,
                LOGON32_PROVIDER_DEFAULT,
                &mut token,
            )
            .is_ok();
            if ok {
                let _ = CloseHandle(token);
            }
            ok
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod system {
    pub fn verify(_user: &str, _password: &str) -> bool {
        tracing::warn!("System accounts are not supported on this system");
        false
    }
}

/// Failures allowed before the first wait.
const FREE_FAILURES: u32 = 2;
/// The wait doubles from 1 s with each failure, up to this.
const MAX_WAIT: Duration = Duration::from_secs(60);
/// Failures older than this are forgotten.
const FORGET: Duration = Duration::from_secs(15 * 60);
/// Addresses remembered at most; beyond it the oldest go.
const MAX_ENTRIES: usize = 4096;

struct Attempt {
    failures: u32,
    locked_until: Option<Instant>,
    last: Instant,
}

/// Failed logins per client address. IPv6 clients count per /64, the block one host
/// usually has.
#[derive(Default)]
pub struct RateLimiter {
    attempts: Mutex<HashMap<IpAddr, Attempt>>,
}

fn key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let s = v6.segments();
                IpAddr::V6(std::net::Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
            }
        },
        v4 => v4,
    }
}

/// The wait after `failures` failures.
fn wait_after(failures: u32) -> Option<Duration> {
    let n = failures.checked_sub(FREE_FAILURES + 1)?;
    Some(Duration::from_secs(1u64 << n.min(6)).min(MAX_WAIT))
}

impl RateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// How long `ip` must still wait before trying again, if it must.
    pub fn check(&self, ip: IpAddr) -> Option<Duration> {
        let now = Instant::now();
        let map = self.attempts.lock().unwrap();
        let until = map.get(&key(ip))?.locked_until?;
        (now < until).then(|| until - now)
    }

    /// Records a failed login; returns the wait it imposes, if any.
    pub fn record_failure(&self, ip: IpAddr) -> Option<Duration> {
        let now = Instant::now();
        let mut map = self.attempts.lock().unwrap();
        if map.len() >= MAX_ENTRIES {
            map.retain(|_, a| now.duration_since(a.last) < FORGET);
            if map.len() >= MAX_ENTRIES
                && let Some(oldest) = map.iter().min_by_key(|(_, a)| a.last).map(|(k, _)| *k)
            {
                map.remove(&oldest);
            }
        }
        let a = map.entry(key(ip)).or_insert(Attempt {
            failures: 0,
            locked_until: None,
            last: now,
        });
        if now.duration_since(a.last) > FORGET {
            a.failures = 0;
        }
        a.failures += 1;
        a.last = now;
        let wait = wait_after(a.failures);
        a.locked_until = wait.map(|w| now + w);
        wait
    }

    /// A successful login clears the address's failures.
    pub fn record_success(&self, ip: IpAddr) {
        self.attempts.lock().unwrap().remove(&key(ip));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials() {
        assert!(verify_credentials(
            "admin",
            "secret123",
            "admin",
            "secret123"
        ));
        assert!(!verify_credentials("admin", "secret123", "admin", "wrong"));
        assert!(!verify_credentials(
            "admin",
            "secret123",
            "root",
            "secret123"
        ));
        assert!(!verify_credentials(
            "admin",
            "secret123",
            "admin",
            "secret1234"
        ));
        assert!(!verify_credentials("admin", "secret123", "", ""));
    }

    #[test]
    fn waits_grow_to_a_minute() {
        let waits: Vec<_> = (1..=10)
            .map(|n| wait_after(n).map(|d| d.as_secs()))
            .collect();
        assert_eq!(
            waits,
            [
                None,
                None,
                Some(1),
                Some(2),
                Some(4),
                Some(8),
                Some(16),
                Some(32),
                Some(60),
                Some(60)
            ]
        );
    }

    #[test]
    fn limiter() {
        let limiter = RateLimiter::new();
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        assert_eq!(limiter.record_failure(ip), None);
        assert_eq!(limiter.record_failure(ip), None);
        assert_eq!(limiter.check(ip), None);
        assert_eq!(limiter.record_failure(ip), Some(Duration::from_secs(1)));
        assert!(limiter.check(ip).is_some());
        // Another address is not affected.
        assert_eq!(limiter.check("192.0.2.2".parse().unwrap()), None);
        limiter.record_success(ip);
        assert_eq!(limiter.check(ip), None);
    }

    #[test]
    fn ipv6_counts_per_64() {
        let limiter = RateLimiter::new();
        for i in 1..=3 {
            limiter.record_failure(format!("2001:db8:1:2::{i}").parse().unwrap());
        }
        assert!(limiter.check("2001:db8:1:2::99".parse().unwrap()).is_some());
        assert_eq!(limiter.check("2001:db8:1:3::1".parse().unwrap()), None);
        // An IPv4 client on a dual-stack socket is its IPv4 address.
        limiter.record_failure("::ffff:192.0.2.7".parse().unwrap());
        assert!(
            limiter
                .attempts
                .lock()
                .unwrap()
                .contains_key(&"192.0.2.7".parse().unwrap())
        );
    }
}
