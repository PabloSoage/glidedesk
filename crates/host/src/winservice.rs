//! The host as a Windows service (ADR-0006, phase 2), so that the lock screen, the sign-in
//! screen and UAC prompts are seen and used too.
//!
//! The service runs as SYSTEM in session 0, which has no desktop. It starts the host itself
//! (`service helper`) in the console session with a copy of its own token moved to that
//! session, on `winsta0\default`: as SYSTEM the helper can open the secure desktop and
//! follows it for capture and input (`winsession::follow_input_desktop`). When the console
//! session changes (signing out, switching users, a Remote Desktop connection taking the
//! console) the helper is stopped and started again in the new one; if it exits, it is
//! started again, waiting longer each time it fails quickly. A job object ends the helper
//! with the service. Ctrl+Alt+Del cannot be sent: SendInput does not make the secure
//! attention sequence (`SendSAS` would, with the policy that allows services to).
//!
//! The `serve` arguments are kept in `%ProgramData%\glidedesk\service.args`, one per line
//! (they hold the password), and both processes log to `%ProgramData%\glidedesk\host.log`;
//! both files are readable only by SYSTEM and administrators.
//!
//! Adapted from Sunshine `tools/sunshinesvc.cpp` (commit 224c6be): the token duplicated into
//! the console session, the desktop, the inherited log handle, the job object and the restart
//! on console session changes.

use std::ffi::c_void;
use std::fs::File;
use std::io::Write as _;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use tracing::{debug, error, info, warn};
use windows::Win32::Foundation::{
    ERROR_CALL_NOT_IMPLEMENTED, ERROR_FAILED_SERVICE_CONTROLLER_CONNECT, ERROR_NOT_ALL_ASSIGNED,
    ERROR_SERVICE_EXISTS, ERROR_SERVICE_NOT_ACTIVE, GetLastError, HANDLE, HLOCAL, LUID,
    LocalFree, NO_ERROR, WAIT_OBJECT_0,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    AdjustTokenPrivileges, DuplicateTokenEx, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW,
    PSECURITY_DESCRIPTOR, SE_INC_BASE_PRIORITY_NAME, SE_PRIVILEGE_ENABLED, SECURITY_ATTRIBUTES,
    SecurityImpersonation, SetTokenInformation, TOKEN_ADJUST_PRIVILEGES, TOKEN_ALL_ACCESS,
    TOKEN_DUPLICATE, TOKEN_PRIVILEGES, TOKEN_QUERY, TokenPrimary, TokenSessionId,
};
use windows::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateFileW, FILE_APPEND_DATA, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_WRITE,
    FILE_SHARE_READ,
};
use windows::Win32::System::Console::{STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows::Win32::System::Services::{
    ChangeServiceConfig2W, ChangeServiceConfigW, ControlService, CreateServiceW, DeleteService,
    ENUM_SERVICE_TYPE,
    OpenSCManagerW, OpenServiceW, QueryServiceStatus, RegisterServiceCtrlHandlerExW,
    SC_ACTION, SC_ACTION_RESTART, SC_HANDLE, SC_MANAGER_CONNECT, SC_MANAGER_CREATE_SERVICE,
    SERVICE_ACCEPT_PRESHUTDOWN, SERVICE_ACCEPT_SESSIONCHANGE, SERVICE_ACCEPT_STOP,
    SERVICE_ALL_ACCESS, SERVICE_AUTO_START, SERVICE_CONFIG_DESCRIPTION,
    SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_PRESHUTDOWN,
    SERVICE_CONTROL_SESSIONCHANGE, SERVICE_CONTROL_STOP, SERVICE_DESCRIPTIONW, SERVICE_ERROR,
    SERVICE_ERROR_NORMAL, SERVICE_FAILURE_ACTIONSW, SERVICE_NO_CHANGE, SERVICE_RUNNING,
    SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_HANDLE,
    SERVICE_STOP_PENDING, SERVICE_STOPPED, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
    SetServiceStatus, StartServiceCtrlDispatcherW, StartServiceW,
};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CreateEventW, CreateProcessAsUserW, GetCurrentProcess,
    GetExitCodeProcess, HIGH_PRIORITY_CLASS, INFINITE, OpenProcessToken, PROCESS_INFORMATION,
    ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOW, SetEvent, TerminateProcess,
    WaitForMultipleObjects, WaitForSingleObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    WTS_CONSOLE_CONNECT, WTS_CONSOLE_DISCONNECT, WTS_SESSION_LOGOFF, WTS_SESSION_LOGON,
};
use windows::core::{Owned, PCWSTR, PWSTR, w};

const NAME: PCWSTR = w!("glidedesk");
const DISPLAY_NAME: PCWSTR = w!("glidedesk host");
const DESCRIPTION: &str = "Remote desktop server: serves the console session, including the \
                           lock screen, the sign-in screen and UAC prompts.";

/// Only SYSTEM and administrators; nothing inherited from the folder.
const PRIVATE: PCWSTR = w!("D:P(A;;FA;;;SY)(A;;FA;;;BA)");

/// A helper that ran this long had started fine; one that exits sooner is restarted later
/// each time, up to the last.
const STABLE: Duration = Duration::from_secs(30);
const RESTART_FIRST: Duration = Duration::from_secs(3);
const RESTART_LAST: Duration = Duration::from_secs(60);

/// Set in the helper (`service helper`).
static HELPER: AtomicBool = AtomicBool::new(false);

pub fn set_helper() {
    HELPER.store(true, Ordering::Relaxed);
}

/// Whether this process is the service's helper, running as SYSTEM.
pub fn is_helper() -> bool {
    HELPER.load(Ordering::Relaxed)
}

fn data_dir() -> PathBuf {
    std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("glidedesk")
}

fn args_path() -> PathBuf {
    data_dir().join("service.args")
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn wide_path(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str().encode_wide().chain(Some(0)).collect()
}

/// The arguments for `serve` from the settings file, one per line.
pub fn helper_args() -> Result<Vec<String>> {
    let path = args_path();
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading the service's arguments from {}", path.display()))?;
    Ok(text.lines().map(str::to_string).collect())
}

/// The arguments, one per line (none may hold a line break).
fn args_file(args: &[String]) -> Result<String> {
    if let Some(a) = args.iter().find(|a| a.contains(['\n', '\r'])) {
        bail!("an argument has a line break: {a:?}");
    }
    let mut text = args.join("\n");
    text.push('\n');
    Ok(text)
}

/// A security descriptor from SDDL, freed on drop.
struct Descriptor(PSECURITY_DESCRIPTOR);

impl Descriptor {
    fn private() -> Result<Self> {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: a constant SDDL string; `sd` receives a LocalAlloc'd descriptor.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PRIVATE,
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
        }
        .context("building the security descriptor")?;
        Ok(Self(sd))
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe {
            LocalFree(Some(HLOCAL(self.0.0)));
        }
    }
}

/// Creates `path` anew, readable only by SYSTEM and administrators (an existing file would
/// keep its own permissions, so it is removed first). `inherit`: child processes get the
/// handle.
fn create_private(path: &Path, access: u32, inherit: bool) -> Result<File> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("replacing {}", path.display())),
    }
    let sd = Descriptor::private()?;
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0.0,
        bInheritHandle: inherit.into(),
    };
    let name = wide_path(path);
    // SAFETY: `name` is NUL-terminated; `sa` and the descriptor outlive the call.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            access,
            FILE_SHARE_READ,
            Some(&sa),
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .with_context(|| format!("creating {}", path.display()))?;
    // SAFETY: a file handle just opened and owned by nobody else.
    Ok(unsafe { File::from_raw_handle(handle.0) })
}

/// The service manager, or a service, closed on drop.
fn open_scm(access: u32) -> Result<Owned<SC_HANDLE>> {
    // SAFETY: the local machine's default database.
    let scm = unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), access) }
        .context("opening the service manager (run this as administrator)")?;
    // SAFETY: just opened.
    Ok(unsafe { Owned::new(scm) })
}

fn open_service(scm: &Owned<SC_HANDLE>) -> windows::core::Result<Owned<SC_HANDLE>> {
    // SAFETY: a live handle to the service manager.
    let s = unsafe { OpenServiceW(**scm, NAME, SERVICE_ALL_ACCESS) }?;
    // SAFETY: just opened.
    Ok(unsafe { Owned::new(s) })
}

/// Registers the service (or updates it), saves the arguments and starts it.
pub fn install(args: &[String]) -> Result<()> {
    let text = args_file(args)?;
    let dir = data_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = args_path();
    create_private(&path, FILE_GENERIC_WRITE.0, false)?
        .write_all(text.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;

    let exe = std::env::current_exe().context("finding this program")?;
    let command = wide(&format!("\"{}\" service run", exe.display()));
    let scm = open_scm(SC_MANAGER_CONNECT | SC_MANAGER_CREATE_SERVICE)?;
    // SAFETY: live handle; the strings are NUL-terminated and outlive the call. No account
    // name: LocalSystem.
    let created = unsafe {
        CreateServiceW(
            *scm,
            NAME,
            DISPLAY_NAME,
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            PCWSTR(command.as_ptr()),
            PCWSTR::null(),
            None,
            PCWSTR::null(),
            PCWSTR::null(),
            PCWSTR::null(),
        )
    };
    let service = match created {
        // SAFETY: just created.
        Ok(s) => unsafe { Owned::new(s) },
        Err(e) if e.code() == ERROR_SERVICE_EXISTS.to_hresult() => {
            let s = open_service(&scm).context("opening the installed service")?;
            // The program may have moved; it is restarted below with the new arguments.
            // SAFETY: live handle; strings as above.
            unsafe {
                ChangeServiceConfigW(
                    *s,
                    ENUM_SERVICE_TYPE(SERVICE_NO_CHANGE),
                    SERVICE_AUTO_START,
                    SERVICE_ERROR(SERVICE_NO_CHANGE),
                    PCWSTR(command.as_ptr()),
                    PCWSTR::null(),
                    None,
                    PCWSTR::null(),
                    PCWSTR::null(),
                    PCWSTR::null(),
                    PCWSTR::null(),
                )
            }
            .context("updating the service")?;
            stop(&s)?;
            s
        }
        Err(e) => return Err(e).context("creating the service"),
    };
    configure(&service);
    // SAFETY: live handle; no arguments.
    unsafe { StartServiceW(*service, None) }.context("starting the service")?;
    println!(
        "Service installed and started; it logs to {}.",
        data_dir().join("host.log").display()
    );
    Ok(())
}

/// Description and restart on failure (not essential: failures are only logged).
fn configure(service: &Owned<SC_HANDLE>) {
    let mut description = wide(DESCRIPTION);
    let info = SERVICE_DESCRIPTIONW {
        lpDescription: PWSTR(description.as_mut_ptr()),
    };
    // SAFETY: live handle; `info` and its string outlive the call.
    if let Err(e) = unsafe {
        ChangeServiceConfig2W(
            **service,
            SERVICE_CONFIG_DESCRIPTION,
            Some(&info as *const _ as *const c_void),
        )
    } {
        warn!(%e, "Setting the service's description");
    }
    let mut actions = [SC_ACTION {
        Type: SC_ACTION_RESTART,
        Delay: 5_000,
    }; 3];
    let failure = SERVICE_FAILURE_ACTIONSW {
        dwResetPeriod: 24 * 3600,
        lpRebootMsg: PWSTR::null(),
        lpCommand: PWSTR::null(),
        cActions: actions.len() as u32,
        lpsaActions: actions.as_mut_ptr(),
    };
    // SAFETY: as above.
    if let Err(e) = unsafe {
        ChangeServiceConfig2W(
            **service,
            SERVICE_CONFIG_FAILURE_ACTIONS,
            Some(&failure as *const _ as *const c_void),
        )
    } {
        warn!(%e, "Setting the service's restart on failure");
    }
}

/// Stops the service and waits until it has stopped.
fn stop(service: &Owned<SC_HANDLE>) -> Result<()> {
    let mut status = SERVICE_STATUS::default();
    // SAFETY: live handle; `status` is an out parameter.
    match unsafe { ControlService(**service, SERVICE_CONTROL_STOP, &mut status) } {
        Ok(()) => {}
        Err(e) if e.code() == ERROR_SERVICE_NOT_ACTIVE.to_hresult() => return Ok(()),
        Err(e) => return Err(e).context("stopping the service"),
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        // SAFETY: as above.
        unsafe { QueryServiceStatus(**service, &mut status) }.context("querying the service")?;
        if status.dwCurrentState == SERVICE_STOPPED {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("the service did not stop within 30 s");
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Stops and removes the service, and deletes its arguments (they hold the password).
pub fn uninstall() -> Result<()> {
    let scm = open_scm(SC_MANAGER_CONNECT)?;
    let service = open_service(&scm).context("opening the service (is it installed?)")?;
    stop(&service)?;
    // SAFETY: live handle.
    unsafe { DeleteService(*service) }.context("removing the service")?;
    match std::fs::remove_file(args_path()) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!(%e, "Removing the service's arguments"),
    }
    println!("Service removed.");
    Ok(())
}

/// The service's entry point (`service run`): hands this thread to the service manager until
/// the service stops.
pub fn run() -> Result<()> {
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(NAME.0 as *mut u16),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    // SAFETY: the table ends with an empty entry and outlives the dispatcher, which returns
    // when the service has stopped. The name is only read.
    unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) }.map_err(|e| {
        if e.code() == ERROR_FAILED_SERVICE_CONTROLLER_CONNECT.to_hresult() {
            anyhow!("`service run` is for the service manager: use `service install`")
        } else {
            anyhow!(e).context("starting the service dispatcher")
        }
    })
}

/// What the control handler signals to the service's thread.
struct Events {
    /// Manual reset: stay set once stopping.
    stop: HANDLE,
    /// Auto reset: a console session came or went.
    session: HANDLE,
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    if let Err(e) = serve() {
        error!(error = format!("{e:#}"), "The service failed");
    }
}

unsafe extern "system" fn handler(
    control: u32,
    event: u32,
    _data: *mut c_void,
    context: *mut c_void,
) -> u32 {
    // SAFETY: the context is the `Events` registered with this handler, never freed.
    let events = unsafe { &*(context as *const Events) };
    match control {
        SERVICE_CONTROL_INTERROGATE => NO_ERROR.0,
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_PRESHUTDOWN => {
            // SAFETY: a live event handle.
            unsafe {
                let _ = SetEvent(events.stop);
            }
            NO_ERROR.0
        }
        SERVICE_CONTROL_SESSIONCHANGE => {
            // Locking needs no restart: the helper follows the desktop within its session.
            if matches!(
                event,
                WTS_CONSOLE_CONNECT
                    | WTS_CONSOLE_DISCONNECT
                    | WTS_SESSION_LOGON
                    | WTS_SESSION_LOGOFF
            ) {
                // SAFETY: as above.
                unsafe {
                    let _ = SetEvent(events.session);
                }
            }
            NO_ERROR.0
        }
        _ => ERROR_CALL_NOT_IMPLEMENTED.0,
    }
}

/// Tells the service manager how the service is doing.
struct Status(SERVICE_STATUS_HANDLE);

impl Status {
    fn set(&self, state: SERVICE_STATUS_CURRENT_STATE, accepted: u32, exit_code: u32) {
        let status = SERVICE_STATUS {
            dwServiceType: SERVICE_WIN32_OWN_PROCESS,
            dwCurrentState: state,
            dwControlsAccepted: accepted,
            dwWin32ExitCode: exit_code,
            dwServiceSpecificExitCode: 0,
            dwCheckPoint: 0,
            dwWaitHint: if state == SERVICE_STOP_PENDING {
                10_000
            } else {
                0
            },
        };
        // SAFETY: the handle from RegisterServiceCtrlHandlerExW, valid for the process.
        if let Err(e) = unsafe { SetServiceStatus(self.0, &status) } {
            warn!(%e, "SetServiceStatus failed");
        }
    }
}

fn serve() -> Result<()> {
    // SAFETY: unnamed events with default security.
    let events: &'static Events = unsafe {
        Box::leak(Box::new(Events {
            stop: CreateEventW(None, true, false, PCWSTR::null())?,
            session: CreateEventW(None, false, false, PCWSTR::null())?,
        }))
    };
    // SAFETY: the handler and its context live as long as the process.
    let status = Status(unsafe {
        RegisterServiceCtrlHandlerExW(
            NAME,
            Some(handler),
            Some(events as *const Events as *const c_void),
        )
    }?);
    status.set(SERVICE_START_PENDING, 0, 0);
    let log = match open_log() {
        Ok(log) => log,
        Err(e) => {
            status.set(SERVICE_STOPPED, 0, 1);
            return Err(e);
        }
    };
    info!(version = env!("CARGO_PKG_VERSION"), "Service started");
    status.set(
        SERVICE_RUNNING,
        SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_PRESHUTDOWN | SERVICE_ACCEPT_SESSIONCHANGE,
        0,
    );
    supervise(events, &log, || status.set(SERVICE_STOP_PENDING, 0, 0));
    info!("Service stopped");
    status.set(SERVICE_STOPPED, 0, 0);
    Ok(())
}

/// The log, shared by the service and the helper: the previous one is kept as
/// `host.old.log`. This process's standard output and error go there too.
fn open_log() -> Result<File> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("host.log");
    let _ = std::fs::rename(&path, dir.join("host.old.log"));
    // Appends only: the writes of both processes go to the end.
    let log = create_private(&path, FILE_APPEND_DATA.0, true)?;
    let handle = HANDLE(log.as_raw_handle());
    // SAFETY: a live file handle, kept open for the life of the service. Rust's standard
    // output looks the handle up on every write, so the log subscriber writes there now.
    unsafe {
        SetStdHandle(STD_OUTPUT_HANDLE, handle)?;
        SetStdHandle(STD_ERROR_HANDLE, handle)?;
    }
    Ok(log)
}

/// Keeps a helper running in the console session until the service is stopped.
fn supervise(events: &Events, log: &File, stopping: impl Fn()) {
    let mut delay = Duration::ZERO;
    loop {
        // SAFETY: a live event handle.
        if unsafe { WaitForSingleObject(events.stop, delay.as_millis() as u32) } == WAIT_OBJECT_0 {
            stopping();
            return;
        }
        // SAFETY: no preconditions.
        let session = unsafe { WTSGetActiveConsoleSessionId() };
        if session == u32::MAX {
            // Between sessions: no console session for a moment.
            delay = Duration::from_secs(1);
            continue;
        }
        let helper = match Helper::start(session, log) {
            Ok(h) => h,
            Err(e) => {
                warn!(
                    error = format!("{e:#}"),
                    session, "Cannot start the host in the console session"
                );
                delay = later(delay);
                continue;
            }
        };
        info!(session, pid = helper.pid, "Host started in the console session");
        let started = Instant::now();
        match helper.wait(events, session) {
            Outcome::Stop => {
                stopping();
                helper.end();
                return;
            }
            Outcome::SessionChanged => {
                info!("The console session changed: starting the host there");
                helper.end();
                delay = Duration::from_millis(500);
            }
            Outcome::Exited(code) => {
                delay = if started.elapsed() >= STABLE {
                    RESTART_FIRST
                } else {
                    later(delay)
                };
                warn!(
                    code,
                    again_in_s = delay.as_secs(),
                    "The host exited; starting it again"
                );
            }
        }
    }
}

/// The next wait after a quick failure: doubled, between the first and the last.
fn later(delay: Duration) -> Duration {
    (delay * 2).clamp(RESTART_FIRST, RESTART_LAST)
}

enum Outcome {
    Stop,
    SessionChanged,
    Exited(u32),
}

/// The host started in a console session.
struct Helper {
    process: Owned<HANDLE>,
    pid: u32,
    /// Ends the helper when the service ends, even if it is killed.
    _job: Option<Owned<HANDLE>>,
}

impl Helper {
    fn start(session: u32, log: &File) -> Result<Self> {
        let token = session_token(session)?;
        let job = kill_on_close_job()
            .map_err(|e| warn!(error = format!("{e:#}"), "No job object for the host"))
            .ok();
        let exe = std::env::current_exe().context("finding this program")?;
        let mut command = wide(&format!("\"{}\" service helper", exe.display()));
        // Its own folder rather than System32, where services start.
        let dir = exe.parent().map(wide_path);
        let mut desktop = wide(r"winsta0\default");
        let out = HANDLE(log.as_raw_handle());
        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            dwFlags: STARTF_USESTDHANDLES,
            hStdOutput: out,
            hStdError: out,
            ..Default::default()
        };
        let mut info = PROCESS_INFORMATION::default();
        // SAFETY: the buffers outlive the call; the token is a primary token for the
        // session. Handles are inherited: the log is the only inheritable one this process
        // opens. Suspended until it is in the job.
        unsafe {
            CreateProcessAsUserW(
                Some(*token),
                PCWSTR::null(),
                Some(PWSTR(command.as_mut_ptr())),
                None,
                None,
                true,
                CREATE_NO_WINDOW | CREATE_SUSPENDED | HIGH_PRIORITY_CLASS,
                None,
                dir.as_ref().map_or(PCWSTR::null(), |d| PCWSTR(d.as_ptr())),
                &startup,
                &mut info,
            )
        }
        .context("CreateProcessAsUserW")?;
        // SAFETY: the handles were just created for us.
        let (process, thread) = unsafe { (Owned::new(info.hProcess), Owned::new(info.hThread)) };
        if let Some(job) = &job
            // SAFETY: live handles.
            && let Err(e) = unsafe { AssignProcessToJobObject(**job, *process) }
        {
            warn!(%e, "The host is not in the service's job");
        }
        // SAFETY: the suspended main thread of the new process.
        unsafe { ResumeThread(*thread) };
        Ok(Self {
            process,
            pid: info.dwProcessId,
            _job: job,
        })
    }

    fn wait(&self, events: &Events, session: u32) -> Outcome {
        loop {
            let handles = [events.stop, *self.process, events.session];
            // SAFETY: live handles.
            let r = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
            match r.0.wrapping_sub(WAIT_OBJECT_0.0) {
                0 => return Outcome::Stop,
                1 => {
                    let mut code = 0u32;
                    // SAFETY: a live process handle.
                    let _ = unsafe { GetExitCodeProcess(*self.process, &mut code) };
                    return Outcome::Exited(code);
                }
                // SAFETY: no preconditions.
                2 if unsafe { WTSGetActiveConsoleSessionId() } != session => {
                    return Outcome::SessionChanged;
                }
                2 => debug!("Session event; the console session is the same"),
                _ => {
                    error!("Waiting for the host failed; stopping");
                    return Outcome::Stop;
                }
            }
        }
    }

    /// Ends the helper (its virtual display goes with the driver's watchdog).
    fn end(self) {
        // SAFETY: a live process handle.
        unsafe {
            if let Err(e) = TerminateProcess(*self.process, 1) {
                debug!(%e, "TerminateProcess");
            }
            WaitForSingleObject(*self.process, 5_000);
        }
    }
}

/// A copy of this process's SYSTEM token, moved to `session` (Sunshine's
/// `DuplicateTokenForSession`), with the privilege for realtime GPU priority enabled.
fn session_token(session: u32) -> Result<Owned<HANDLE>> {
    let mut own = HANDLE::default();
    // SAFETY: this process's pseudo handle; `own` receives a new handle.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE, &mut own) }
        .context("opening the service's token")?;
    // SAFETY: just opened.
    let own = unsafe { Owned::new(own) };
    let mut token = HANDLE::default();
    // SAFETY: live handle; `token` receives a new handle.
    unsafe {
        DuplicateTokenEx(
            *own,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut token,
        )
    }
    .context("duplicating the service's token")?;
    // SAFETY: just created.
    let token = unsafe { Owned::new(token) };
    // SAFETY: live handle; the value is a u32 session id. Needs SeTcbPrivilege (SYSTEM).
    unsafe {
        SetTokenInformation(
            *token,
            TokenSessionId,
            &session as *const u32 as *const c_void,
            std::mem::size_of::<u32>() as u32,
        )
    }
    .context("moving the token to the console session")?;
    if let Err(e) = enable_privilege(*token, SE_INC_BASE_PRIORITY_NAME) {
        debug!(error = format!("{e:#}"), "SeIncreaseBasePriorityPrivilege");
    }
    Ok(token)
}

/// A job that kills its processes when its last handle closes.
fn kill_on_close_job() -> Result<Owned<HANDLE>> {
    // SAFETY: an unnamed job with default security.
    let job = unsafe { Owned::new(CreateJobObjectW(None, PCWSTR::null())?) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    // Breakaway allowed: what the host starts may outlive it.
    limits.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
    // SAFETY: live handle; the structure matches the class and outlives the call.
    unsafe {
        SetInformationJobObject(
            *job,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const c_void,
            std::mem::size_of_val(&limits) as u32,
        )
    }?;
    Ok(job)
}

/// Enables a privilege the token holds (administrators and SYSTEM hold some disabled).
pub fn enable_privilege(token: HANDLE, name: PCWSTR) -> Result<()> {
    let mut luid = LUID::default();
    // SAFETY: `name` is a constant privilege name; `luid` is an out parameter.
    unsafe { LookupPrivilegeValueW(PCWSTR::null(), name, &mut luid) }?;
    let privileges = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    // SAFETY: live token opened with TOKEN_ADJUST_PRIVILEGES; no previous state asked for.
    unsafe { AdjustTokenPrivileges(token, false, Some(&privileges), 0, None, None) }?;
    // It succeeds without enabling a privilege the account does not hold.
    // SAFETY: no preconditions.
    if unsafe { GetLastError() } == ERROR_NOT_ALL_ASSIGNED {
        bail!("the account does not hold it");
    }
    Ok(())
}

/// [`enable_privilege`] on this process's token.
pub fn enable_own_privilege(name: PCWSTR) -> Result<()> {
    let mut token = HANDLE::default();
    // SAFETY: this process's pseudo handle; `token` receives a new handle.
    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
    }?;
    // SAFETY: just opened.
    let token = unsafe { Owned::new(token) };
    enable_privilege(*token, name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_round_trip() {
        let args: Vec<String> = ["serve", "--user", "me", "--password", "a b \"c\""]
            .map(String::from)
            .into();
        let text = args_file(&args).unwrap();
        assert_eq!(text.lines().map(str::to_string).collect::<Vec<_>>(), args);
        assert!(args_file(&["serve".to_string(), "two\nlines".to_string()]).is_err());
    }

    #[test]
    fn restart_waits() {
        assert_eq!(later(Duration::ZERO), RESTART_FIRST);
        assert_eq!(later(RESTART_FIRST), RESTART_FIRST * 2);
        assert_eq!(later(RESTART_LAST), RESTART_LAST);
    }
}
