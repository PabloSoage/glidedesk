//! A virtual monitor at the client's size (`serve --virtual-display`), through the SudoVDA
//! indirect display driver (IddCx, MIT), which is installed separately: a PC without a
//! monitor can be served, and the desktop follows the client's size and refresh rate.
//!
//! The driver is driven by `DeviceIoControl` on its device interface: add a monitor with a
//! preferred mode, remove it, choose the GPU that renders it. It removes every monitor it
//! made when nobody has spoken to it for its watchdog's timeout (3 s by default), so a host
//! that dies leaves nothing behind; the worker thread here pings it in between.
//!
//! A monitor's modes are fixed when it is added (the preferred one, a few scaled from it and
//! common sizes), so a new size is first tried as a mode change and otherwise the monitor is
//! removed and added again. It is removed a while after the client left, and comes back when
//! another one takes over the session.
//!
//! Adapted from SudoVDA `Common/Include/sudovda-ioctl.h` (commit a4b09fa, MIT: control codes,
//! structures, protocol version) and Apollo `third-party/sudovda/sudovda.h` and
//! `src/platform/windows/virtual_display.cpp` (commit adc5c5a, GPL-3.0: opening the device,
//! the watchdog ping, finding the new monitor's GDI name, setting its mode).

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tracing::{debug, info, warn};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_Interface_List_SizeW,
    CM_Get_Device_Interface_ListW, CR_SUCCESS,
};
use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER,
    DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS,
    QueryDisplayConfig,
};
use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE, LUID};
use windows::Win32::Graphics::Gdi::{
    CDS_UPDATEREGISTRY, ChangeDisplaySettingsExW, DEVMODEW, DISP_CHANGE_SUCCESSFUL,
    DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::core::{GUID, Owned, PCWSTR};

use crate::winsession::{Followed, Target};

/// SudoVDA's device interface (`SUVDA_INTERFACE_GUID`).
const INTERFACE: GUID = GUID::from_u128(0xe5bcc234_1e0c_418a_a0d4_ef8b7501414d);

/// `CTL_CODE(FILE_DEVICE_UNKNOWN, function, METHOD_BUFFERED, FILE_ANY_ACCESS)`.
const fn ctl_code(function: u32) -> u32 {
    (0x22 << 16) | (function << 2)
}

const IOCTL_ADD: u32 = ctl_code(0x800);
const IOCTL_REMOVE: u32 = ctl_code(0x801);
const IOCTL_SET_RENDER_ADAPTER: u32 = ctl_code(0x802);
const IOCTL_GET_WATCHDOG: u32 = ctl_code(0x803);
const IOCTL_PING: u32 = ctl_code(0x888);
const IOCTL_GET_PROTOCOL_VERSION: u32 = ctl_code(0x8ff);

/// The protocol version these structures follow (`VDAProtocolVersion`: 0.2.1).
const PROTOCOL: (u8, u8) = (0, 2);

/// Our monitor: always the same, so that Windows remembers where it goes and its settings.
const MONITOR: GUID = GUID::from_u128(0x6c1e5a2e_8d4b_4f7a_9b3e_2f0d7c9a41d5);
const MONITOR_NAME: &[u8] = b"glidedesk";
const MONITOR_SERIAL: &[u8] = b"glidedesk-1";

/// `VIRTUAL_DISPLAY_ADD_PARAMS`.
#[repr(C)]
struct AddParams {
    width: u32,
    height: u32,
    /// Hz (the driver takes values under 1000 as Hz, others as mHz).
    refresh_rate: u32,
    monitor_guid: GUID,
    /// NUL-terminated, at most 13 characters.
    device_name: [u8; 14],
    serial_number: [u8; 14],
}

/// `VIRTUAL_DISPLAY_ADD_OUT`.
#[repr(C)]
#[derive(Default)]
struct AddOut {
    adapter_luid: LUID,
    target_id: u32,
}

/// `VIRTUAL_DISPLAY_REMOVE_PARAMS`.
#[repr(C)]
struct RemoveParams {
    monitor_guid: GUID,
}

/// `VIRTUAL_DISPLAY_SET_RENDER_ADAPTER_PARAMS`.
#[repr(C)]
struct RenderAdapterParams {
    adapter_luid: LUID,
}

/// `VIRTUAL_DISPLAY_GET_WATCHDOG_OUT`, in seconds.
#[repr(C)]
#[derive(Default)]
struct WatchdogOut {
    timeout: u32,
    countdown: u32,
}

/// `SUVDA_PROTOCAL_VERSION` (the last field is a C++ `bool`).
#[repr(C)]
#[derive(Default)]
struct ProtocolVersion {
    major: u8,
    minor: u8,
    incremental: u8,
    test_build: u8,
}

/// Whether a driver speaking `driver` takes our requests: same major version, and a minor
/// one at least ours (SudoVDA's `isProtocolCompatible`).
fn compatible(driver: (u8, u8)) -> bool {
    driver.0 == PROTOCOL.0 && driver.1 >= PROTOCOL.1
}

/// `name` as a NUL-terminated field of 14 bytes.
fn field(name: &[u8]) -> [u8; 14] {
    let mut out = [0u8; 14];
    let n = name.len().min(13);
    out[..n].copy_from_slice(&name[..n]);
    out
}

/// How often to ping a watchdog of `timeout` seconds (a third of it, as Apollo does); `None`
/// when it is off.
fn ping_interval(timeout: u32) -> Option<Duration> {
    (timeout > 0).then(|| Duration::from_millis(u64::from(timeout) * 1000 / 3).max(MIN_PING))
}

const MIN_PING: Duration = Duration::from_millis(100);
/// How long a new monitor may take to show up among the active displays.
const APPEAR: Duration = Duration::from_secs(3);
/// Sizes asked for in a burst (a window being dragged) are applied once they settle.
const SETTLE: Duration = Duration::from_millis(300);
/// The monitor stays this long after the client left, for a reconnection.
const LINGER: Duration = Duration::from_secs(30);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn from_wide(s: &[u16]) -> String {
    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..end])
}

/// The opened driver.
pub struct Driver {
    device: Owned<HANDLE>,
    ping: Option<Duration>,
}

// SAFETY: a plain kernel handle; it is only used from one thread at a time (the caller's
// until the worker takes it).
unsafe impl Send for Driver {}

impl Driver {
    /// Opens the driver; `None` when it is not installed.
    pub fn open() -> Result<Option<Self>> {
        let mut len = 0u32;
        // SAFETY: `len` outlives the call; the GUID is a constant.
        let cr = unsafe {
            CM_Get_Device_Interface_List_SizeW(
                &mut len,
                &INTERFACE,
                PCWSTR::null(),
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            )
        };
        if cr != CR_SUCCESS {
            bail!("CM_Get_Device_Interface_List_SizeW failed: {cr:?}");
        }
        // An empty list is a single NUL.
        if len <= 1 {
            return Ok(None);
        }
        let mut list = vec![0u16; len as usize];
        // SAFETY: the buffer has the length the previous call asked for.
        let cr = unsafe {
            CM_Get_Device_Interface_ListW(
                &INTERFACE,
                PCWSTR::null(),
                &mut list,
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            )
        };
        if cr != CR_SUCCESS {
            bail!("CM_Get_Device_Interface_ListW failed: {cr:?}");
        }
        let Some(path) = list.split(|&c| c == 0).find(|p| !p.is_empty()) else {
            return Ok(None);
        };
        let path: Vec<u16> = path.iter().copied().chain(Some(0)).collect();
        // SAFETY: `path` is NUL-terminated and outlives the call. Opened for synchronous I/O
        // (Apollo opens it overlapped but calls it synchronously).
        let device = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .context("opening the SudoVDA device")?;
        // SAFETY: the handle was just opened and is ours.
        let device = unsafe { Owned::new(device) };
        let mut driver = Self { device, ping: None };

        let mut version = ProtocolVersion::default();
        driver
            .ioctl(IOCTL_GET_PROTOCOL_VERSION, None::<&()>, Some(&mut version))
            .context("asking the SudoVDA driver its protocol version")?;
        info!(
            version = format!(
                "{}.{}.{}{}",
                version.major,
                version.minor,
                version.incremental,
                if version.test_build != 0 { " (test)" } else { "" }
            ),
            "SudoVDA virtual display driver"
        );
        if !compatible((version.major, version.minor)) {
            bail!(
                "the SudoVDA driver speaks protocol {}.{}, this host {}.{}: update the driver",
                version.major,
                version.minor,
                PROTOCOL.0,
                PROTOCOL.1
            );
        }
        let mut watchdog = WatchdogOut::default();
        match driver.ioctl(IOCTL_GET_WATCHDOG, None::<&()>, Some(&mut watchdog)) {
            Ok(_) => {
                debug!(timeout_s = watchdog.timeout, "SudoVDA watchdog");
                driver.ping = ping_interval(watchdog.timeout);
            }
            // Ping anyway, at the default timeout's pace.
            Err(e) => {
                debug!(%e, "Cannot read the SudoVDA watchdog");
                driver.ping = ping_interval(3);
            }
        }
        Ok(Some(driver))
    }

    /// `DeviceIoControl` with a fixed-size input and output; the bytes returned.
    fn ioctl<I, O>(
        &self,
        code: u32,
        input: Option<&I>,
        output: Option<&mut O>,
    ) -> windows::core::Result<u32> {
        let (in_ptr, in_len) = match input {
            Some(i) => (
                Some(i as *const I as *const c_void),
                std::mem::size_of::<I>() as u32,
            ),
            None => (None, 0),
        };
        let (out_ptr, out_len) = match output {
            Some(o) => (
                Some(o as *mut O as *mut c_void),
                std::mem::size_of::<O>() as u32,
            ),
            None => (None, 0),
        };
        let mut returned = 0u32;
        // SAFETY: the buffers are live for the call and have the sizes given; the handle is
        // synchronous, so the call completes before it returns.
        unsafe {
            DeviceIoControl(
                *self.device,
                code,
                in_ptr,
                in_len,
                out_ptr,
                out_len,
                Some(&mut returned),
                None,
            )?;
        }
        Ok(returned)
    }

    /// Keeps the watchdog from removing the monitor.
    fn ping(&self) {
        if let Err(e) = self.ioctl(IOCTL_PING, None::<&()>, None::<&mut ()>) {
            debug!(%e, "SudoVDA ping failed");
        }
    }

    /// Renders the virtual monitors on this GPU (the encoder's, for the copy-free path).
    pub fn set_render_adapter(&self, luid: LUID) -> Result<()> {
        self.ioctl(
            IOCTL_SET_RENDER_ADAPTER,
            Some(&RenderAdapterParams { adapter_luid: luid }),
            None::<&mut ()>,
        )
        .context("choosing the GPU of the virtual display")?;
        Ok(())
    }

    /// Adds our monitor with this preferred mode; its GDI name (`\\.\DISPLAY3`) once Windows
    /// shows it.
    fn add(&self, width: u32, height: u32, hz: u32) -> Result<String> {
        let params = AddParams {
            width,
            height,
            refresh_rate: hz,
            monitor_guid: MONITOR,
            device_name: field(MONITOR_NAME),
            serial_number: field(MONITOR_SERIAL),
        };
        let mut out = AddOut::default();
        self.ioctl(IOCTL_ADD, Some(&params), Some(&mut out))
            .with_context(|| format!("adding a virtual display of {width}x{height} at {hz} Hz"))?;
        let start = Instant::now();
        loop {
            if let Some(name) = gdi_name(out.adapter_luid, out.target_id) {
                debug!(
                    display = name.as_str(),
                    target = out.target_id,
                    after_ms = start.elapsed().as_millis() as u64,
                    "Virtual display active"
                );
                return Ok(name);
            }
            if start.elapsed() > APPEAR {
                // Left to the watchdog otherwise; remove it now rather than leave it half set up.
                self.remove();
                bail!(
                    "the virtual display did not become active (is the display topology \
                     set to extend?)"
                );
            }
            // The watchdog runs on while we wait.
            self.ping();
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Removes our monitor, if it is there.
    fn remove(&self) {
        let params = RemoveParams {
            monitor_guid: MONITOR,
        };
        match self.ioctl(IOCTL_REMOVE, Some(&params), None::<&mut ()>) {
            Ok(_) => debug!("Virtual display removed"),
            // Not found: there was none.
            Err(e) => debug!(%e, "Removing the virtual display"),
        }
    }
}

/// The GDI name of the active display on this target, from the display configuration.
fn gdi_name(adapter: LUID, target: u32) -> Option<String> {
    let (mut n_paths, mut n_modes) = (0u32, 0u32);
    // SAFETY: plain out parameters.
    unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut n_paths, &mut n_modes) }
        .ok()
        .ok()?;
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); n_paths as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); n_modes as usize];
    // SAFETY: the arrays have the lengths passed in.
    unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut n_paths,
            paths.as_mut_ptr(),
            &mut n_modes,
            modes.as_mut_ptr(),
            None,
        )
    }
    .ok()
    .ok()?;
    paths.truncate(n_paths as usize);
    let path = paths
        .iter()
        .find(|p| p.targetInfo.id == target && p.targetInfo.adapterId == adapter)?;
    let mut name = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
            size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
            adapterId: path.sourceInfo.adapterId,
            id: path.sourceInfo.id,
        },
        ..Default::default()
    };
    // SAFETY: the header starts a structure of the size it gives.
    let r = unsafe { DisplayConfigGetDeviceInfo(&mut name.header) };
    (r == 0).then(|| from_wide(&name.viewGdiDeviceName))
}

/// Sets the mode of the display called `gdi`, unless it has it already. Fails when the
/// monitor does not offer it.
fn set_mode(gdi: &str, width: u32, height: u32, hz: u32) -> Result<()> {
    let name = wide(gdi);
    let size = std::mem::size_of::<DEVMODEW>() as u16;
    let mut current = DEVMODEW {
        dmSize: size,
        ..Default::default()
    };
    // SAFETY: `name` is NUL-terminated; `current` says its size.
    let known = unsafe {
        EnumDisplaySettingsW(PCWSTR(name.as_ptr()), ENUM_CURRENT_SETTINGS, &mut current)
    }
    .as_bool();
    let now = (
        current.dmPelsWidth,
        current.dmPelsHeight,
        current.dmDisplayFrequency,
    );
    if known && now == (width, height, hz) {
        return Ok(());
    }
    let mode = DEVMODEW {
        dmSize: size,
        dmFields: DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY,
        dmPelsWidth: width,
        dmPelsHeight: height,
        dmDisplayFrequency: hz,
        ..Default::default()
    };
    // SAFETY: as above; `mode` outlives the call.
    let r = unsafe {
        ChangeDisplaySettingsExW(
            PCWSTR(name.as_ptr()),
            Some(&mode),
            None,
            CDS_UPDATEREGISTRY,
            None,
        )
    };
    if r != DISP_CHANGE_SUCCESSFUL {
        bail!("{gdi} does not take {width}x{height} at {hz} Hz ({r:?})");
    }
    Ok(())
}

enum Request {
    Mode { width: u32, height: u32, hz: u32 },
    Arrived,
    Left,
    Stop,
}

/// The virtual monitor, run by a worker thread; the capture follows it through `Followed`.
pub struct Control {
    tx: Mutex<mpsc::Sender<Request>>,
    refresh: AtomicU32,
    /// Whether the session has a client, as last told to the worker (it starts as if one
    /// were there, so that a host nobody connects to lets the monitor go too).
    client: AtomicBool,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Control {
    /// Adds the monitor at this mode and points the capture at it.
    pub fn start(
        driver: Driver,
        (width, height, hz): (u32, u32, u32),
        follow: Followed,
    ) -> Result<Arc<Self>> {
        // A monitor left over by a host that was killed with the watchdog off.
        driver.remove();
        let name = driver.add(width, height, hz)?;
        if let Err(e) = set_mode(&name, width, height, hz) {
            warn!(error = format!("{e:#}"), "Keeping the virtual display's mode");
        }
        info!(display = name.as_str(), width, height, hz, "Virtual display");
        crate::winsession::follow(&follow, Target::Named(name.clone()));
        let (tx, rx) = mpsc::channel();
        let mut worker = Worker {
            driver,
            follow,
            current: Some(Mode {
                name,
                width,
                height,
                hz,
            }),
            wanted: (width, height, hz),
        };
        let thread = std::thread::Builder::new()
            .name("virtual-display".into())
            .spawn(move || worker.run(&rx))?;
        Ok(Arc::new(Self {
            tx: Mutex::new(tx),
            refresh: AtomicU32::new(hz),
            client: AtomicBool::new(true),
            worker: Mutex::new(Some(thread)),
        }))
    }

    fn send(&self, r: Request) {
        let _ = self.tx.lock().unwrap().send(r);
    }

    /// A client asks for this size (so there is one).
    pub fn resize(&self, width: u32, height: u32) {
        self.client.store(true, Ordering::Relaxed);
        self.send(Request::Mode {
            width,
            height,
            hz: self.refresh.load(Ordering::Relaxed),
        });
    }

    /// True when the rate changed (the caller then resizes to apply it).
    pub fn set_refresh(&self, hz: u32) -> bool {
        self.refresh.swap(hz, Ordering::Relaxed) != hz
    }

    /// Called for every frame with a client: only the change reaches the worker.
    pub fn arrived(&self) {
        if !self.client.swap(true, Ordering::Relaxed) {
            self.send(Request::Arrived);
        }
    }

    /// Called for every frame without a client, as above.
    pub fn left(&self) {
        if self.client.swap(false, Ordering::Relaxed) {
            self.send(Request::Left);
        }
    }

    /// Removes the monitor and waits for the worker to end.
    pub fn shutdown(&self) {
        self.send(Request::Stop);
        if let Some(t) = self.worker.lock().unwrap().take() {
            let _ = t.join();
        }
    }
}

struct Mode {
    name: String,
    width: u32,
    height: u32,
    hz: u32,
}

struct Worker {
    driver: Driver,
    follow: Followed,
    current: Option<Mode>,
    wanted: (u32, u32, u32),
}

impl Worker {
    fn run(&mut self, rx: &mpsc::Receiver<Request>) {
        let ping = self.driver.ping.unwrap_or(Duration::from_secs(1));
        let mut due: Option<Instant> = None;
        let mut left: Option<Instant> = None;
        let mut pinged = Instant::now();
        loop {
            let mut wait = ping.saturating_sub(pinged.elapsed());
            if let Some(t) = due {
                wait = wait.min(t.saturating_duration_since(Instant::now()));
            }
            if let Some(t) = left
                && self.current.is_some()
            {
                wait = wait.min((t + LINGER).saturating_duration_since(Instant::now()));
            }
            match rx.recv_timeout(wait) {
                Ok(Request::Mode { width, height, hz }) => {
                    self.wanted = (width, height, hz);
                    due = Some(Instant::now() + SETTLE);
                    left = None;
                }
                Ok(Request::Arrived) => {
                    left = None;
                    if self.current.is_none() {
                        due = Some(Instant::now());
                    }
                }
                Ok(Request::Left) => {
                    left.get_or_insert_with(Instant::now);
                }
                Ok(Request::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            // The thread that changes display modes must be on the desktop that has the input.
            if due.is_some_and(|t| Instant::now() >= t) {
                due = None;
                crate::winsession::follow_input_desktop();
                self.apply();
            }
            if let Some(t) = left
                && t.elapsed() >= LINGER
                && self.current.take().is_some()
            {
                info!("The client left: removing the virtual display");
                crate::winsession::follow(&self.follow, Target::Nothing);
                self.driver.remove();
            }
            // Without a watchdog this only paces the loop.
            if pinged.elapsed() >= ping {
                if self.driver.ping.is_some() {
                    self.driver.ping();
                }
                pinged = Instant::now();
            }
        }
        crate::winsession::follow(&self.follow, Target::Nothing);
        self.driver.remove();
    }

    /// Gives the monitor the wanted mode: a mode change if it offers it, else a new monitor.
    fn apply(&mut self) {
        let (width, height, hz) = self.wanted;
        if let Some(cur) = &mut self.current {
            if (cur.width, cur.height, cur.hz) == (width, height, hz) {
                return;
            }
            match set_mode(&cur.name, width, height, hz) {
                Ok(()) => {
                    info!(width, height, hz, "Virtual display mode changed");
                    (cur.width, cur.height, cur.hz) = (width, height, hz);
                    return;
                }
                Err(e) => debug!(error = format!("{e:#}"), "Adding the virtual display again"),
            }
        }
        if self.current.take().is_some() {
            crate::winsession::follow(&self.follow, Target::Nothing);
            self.driver.remove();
        }
        // The removal takes a moment to reach Windows; once more if adding fails.
        let added = self.driver.add(width, height, hz).or_else(|e| {
            debug!(error = format!("{e:#}"), "Adding the virtual display; trying again");
            std::thread::sleep(Duration::from_millis(500));
            self.driver.add(width, height, hz)
        });
        match added {
            Ok(name) => {
                if let Err(e) = set_mode(&name, width, height, hz) {
                    warn!(error = format!("{e:#}"), "Keeping the virtual display's mode");
                }
                info!(display = name.as_str(), width, height, hz, "Virtual display");
                crate::winsession::follow(&self.follow, Target::Named(name.clone()));
                self.current = Some(Mode {
                    name,
                    width,
                    height,
                    hz,
                });
            }
            Err(e) => warn!(error = format!("{e:#}"), "No virtual display"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_codes() {
        // CTL_CODE(0x22, function, 0, 0) as in sudovda-ioctl.h.
        assert_eq!(IOCTL_ADD, 0x0022_2000);
        assert_eq!(IOCTL_REMOVE, 0x0022_2004);
        assert_eq!(IOCTL_SET_RENDER_ADAPTER, 0x0022_2008);
        assert_eq!(IOCTL_GET_WATCHDOG, 0x0022_200c);
        assert_eq!(IOCTL_PING, 0x0022_2220);
        assert_eq!(IOCTL_GET_PROTOCOL_VERSION, 0x0022_23fc);
    }

    #[test]
    fn layouts_match_the_driver() {
        assert_eq!(std::mem::size_of::<AddParams>(), 56);
        assert_eq!(std::mem::size_of::<AddOut>(), 12);
        assert_eq!(std::mem::size_of::<RemoveParams>(), 16);
        assert_eq!(std::mem::size_of::<RenderAdapterParams>(), 8);
        assert_eq!(std::mem::size_of::<WatchdogOut>(), 8);
        assert_eq!(std::mem::size_of::<ProtocolVersion>(), 4);
    }

    #[test]
    fn protocol_compatibility() {
        assert!(compatible((0, 2)));
        assert!(compatible((0, 3)));
        assert!(!compatible((0, 1)));
        assert!(!compatible((1, 2)));
    }

    #[test]
    fn name_fields() {
        let f = field(b"glidedesk");
        assert_eq!(&f[..10], b"glidedesk\0");
        // Truncated to 13 characters and still terminated.
        let f = field(b"a-name-longer-than-fourteen");
        assert_eq!(f[13], 0);
        assert_eq!(&f[..13], b"a-name-longer");
    }

    #[test]
    fn ping_pace() {
        assert_eq!(ping_interval(0), None);
        assert_eq!(ping_interval(3), Some(Duration::from_secs(1)));
        assert_eq!(ping_interval(30), Some(Duration::from_secs(10)));
    }

    #[test]
    fn wide_names() {
        let mut buf = [0u16; 32];
        for (i, c) in r"\\.\DISPLAY3".encode_utf16().enumerate() {
            buf[i] = c;
        }
        assert_eq!(from_wide(&buf), r"\\.\DISPLAY3");
    }
}
