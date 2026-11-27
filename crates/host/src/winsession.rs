//! Windows session: the console desktop through DXGI Desktop Duplication, input through
//! `SendInput`. Audio is in `winaudio`, the clipboard in `winclip`.
//!
//! The primary monitor is served at its own size, or with `--virtual-display` a virtual
//! monitor that follows the client (`winvdisplay`). Duplication is lost on a mode change, a
//! full-screen exclusive application or a desktop switch (UAC, lock screen); the capture
//! thread moves to the input desktop and duplicates the output again until it works. A user
//! program cannot capture or drive the secure desktop: the service can (`winservice`).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::sync::watch;
use tracing::{debug, info, warn};
use windows::Win32::Foundation::{GENERIC_ALL, HANDLE, HMODULE, LUID, RECT};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME, DXGI_OUTPUT_DESC, IDXGIAdapter1, IDXGIDevice,
    IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
};
use windows::Win32::Security::SE_INC_BASE_PRIORITY_NAME;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, GetUserObjectInformationW,
    OpenInputDesktop, SetThreadDesktop, UOI_NAME,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE,
    MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
    MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY, VK_PAUSE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    XBUTTON1, XBUTTON2,
};
use windows::core::Interface;

use crate::frame::{
    CursorEvent, CursorShape, Frame, FrameData, FrameReceiver, FrameSender, Recycler, Source,
};
use crate::input::{Input, InputBackend};
use crate::wincuda::D3dToCuda;
use crate::wind3d::TexturePool;
use crate::winvdisplay::{self, Control};

pub struct WinSession {
    pub size: (u32, u32),
    pub source: WinSource,
    pub frames: FrameReceiver,
    pub cursor: watch::Receiver<Option<CursorEvent>>,
    pub input: WinInput,
    /// The virtual display, with `--virtual-display` and the driver: removed at the end.
    pub display: Option<Arc<Control>>,
}

/// Without a virtual display the desktop has the monitor's size: requests to change it are
/// ignored. With one, the virtual monitor follows the client.
pub struct WinSource {
    display: Option<Arc<Control>>,
}

impl Source for WinSource {
    fn resize(&self, width: u32, height: u32) {
        match &self.display {
            Some(d) => d.resize(width, height),
            None => debug!(
                width,
                height, "The Windows desktop keeps the monitor's size"
            ),
        }
    }

    fn set_refresh(&self, hz: u32) -> bool {
        self.display.as_ref().is_some_and(|d| d.set_refresh(hz))
    }

    fn client_arrived(&self) {
        if let Some(d) = &self.display {
            d.arrived();
        }
    }

    fn client_left(&self) {
        if let Some(d) = &self.display {
            d.left();
        }
    }
}

/// The monitor being captured, in virtual desktop coordinates (input maps onto it).
type Bounds = Arc<Mutex<RECT>>;

/// Which display the capture follows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum Target {
    /// The primary monitor.
    #[default]
    Primary,
    /// A display by its GDI name (`\\.\DISPLAY3`): the virtual one.
    Named(String),
    /// None for now: the virtual display is being replaced, or was removed.
    Nothing,
}

/// The display to capture, and a count of its changes (the capture duplicates again when it
/// moves on).
#[derive(Default)]
pub(crate) struct Follow {
    generation: u64,
    target: Target,
}

pub(crate) type Followed = Arc<Mutex<Follow>>;

/// Points the capture at another display.
pub(crate) fn follow(f: &Followed, target: Target) {
    let mut f = f.lock().unwrap();
    if f.target != target {
        f.target = target;
        f.generation += 1;
    }
}

/// The output of the primary monitor (the one at the desktop origin) and its adapter.
fn primary_output() -> Result<(IDXGIAdapter1, IDXGIOutput1)> {
    let mut first = None;
    let found = each_output(|adapter, output, desc| {
        let r = desc.DesktopCoordinates;
        if r.left == 0 && r.top == 0 {
            return true;
        }
        first.get_or_insert((adapter.clone(), output.clone()));
        false
    })?;
    found.or(first).context(
        "no monitor is attached to the desktop (a PC without one needs a virtual display: \
         --virtual-display)",
    )
}

/// The output whose GDI name is `name`, and its adapter.
fn named_output(name: &str) -> Result<(IDXGIAdapter1, IDXGIOutput1)> {
    each_output(|_, _, desc| {
        let len = desc
            .DeviceName
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(desc.DeviceName.len());
        String::from_utf16_lossy(&desc.DeviceName[..len]).eq_ignore_ascii_case(name)
    })?
    .with_context(|| format!("display {name} is not attached to the desktop (yet)"))
}

/// Calls `pick` on the outputs attached to the desktop until it returns true; that output.
fn each_output(
    mut pick: impl FnMut(&IDXGIAdapter1, &IDXGIOutput1, &DXGI_OUTPUT_DESC) -> bool,
) -> Result<Option<(IDXGIAdapter1, IDXGIOutput1)>> {
    // SAFETY: DXGI enumeration; the interfaces returned are reference counted.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        for a in 0.. {
            let adapter = match factory.EnumAdapters1(a) {
                Ok(adapter) => adapter,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(e.into()),
            };
            for o in 0.. {
                let Ok(output) = adapter.EnumOutputs(o) else {
                    break;
                };
                let desc = output.GetDesc()?;
                if !desc.AttachedToDesktop.as_bool() {
                    continue;
                }
                let output: IDXGIOutput1 = output.cast()?;
                if pick(&adapter, &output, &desc) {
                    return Ok(Some((adapter, output)));
                }
            }
        }
    }
    Ok(None)
}

fn find_output(target: &Target) -> Result<(IDXGIAdapter1, IDXGIOutput1)> {
    match target {
        Target::Primary => primary_output(),
        Target::Named(name) => named_output(name),
        Target::Nothing => bail!("no display to capture yet"),
    }
}

/// The DXGI adapter of `ctx`'s GPU, where the virtual display should be drawn for the
/// copy-free path.
fn cuda_adapter(ctx: &Arc<cudarc::driver::CudaContext>) -> Option<LUID> {
    // SAFETY: DXGI enumeration as above.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        for a in 0.. {
            let adapter = factory.EnumAdapters1(a).ok()?;
            if D3dToCuda::new(Arc::clone(ctx), &adapter).is_ok() {
                return adapter.GetDesc1().ok().map(|d| d.AdapterLuid);
            }
        }
    }
    None
}

/// A D3D11 device on `adapter` for duplication and the copies.
fn create_device(adapter: &IDXGIAdapter1) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    // SAFETY: creates a device on the adapter that owns the output.
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .context("creating the D3D11 device")?;
    }
    let device = device.context("no D3D11 device")?;
    // The GPU thread of this device ahead of others (Sunshine does the same); needs
    // administrator rights or SYSTEM.
    // SAFETY: plain DXGI calls on a live device.
    if let Err(e) = unsafe {
        device
            .cast::<IDXGIDevice>()
            .and_then(|d| d.SetGPUThreadPriority(7))
    } {
        debug!(%e, "GPU thread priority not raised");
    }
    Ok((device, context.context("no D3D11 context")?))
}

/// `cuda` is the NVENC encoder's context, if NVENC encodes. `virtual_display` is the mode
/// (width, height, Hz) of a virtual monitor to serve instead of the physical ones.
pub fn start(
    cuda: Option<Arc<cudarc::driver::CudaContext>>,
    virtual_display: Option<(u32, u32, u32)>,
) -> Result<WinSession> {
    // Real pixels: a process that is not DPI aware sees the desktop scaled down (and its
    // input would land scaled too).
    // SAFETY: no preconditions; it fails harmlessly if the awareness was already set.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    raise_gpu_priority();
    let follow = Followed::default();
    let display = virtual_display.and_then(|mode| {
        let driver = match winvdisplay::Driver::open() {
            Ok(Some(driver)) => driver,
            Ok(None) => {
                warn!(
                    "--virtual-display: the SudoVDA driver is not installed; serving the monitors"
                );
                return None;
            }
            Err(e) => {
                warn!(
                    error = format!("{e:#}"),
                    "--virtual-display: cannot use the driver; serving the monitors"
                );
                return None;
            }
        };
        if let Some(luid) = cuda.as_ref().and_then(cuda_adapter)
            && let Err(e) = driver.set_render_adapter(luid)
        {
            warn!(
                error = format!("{e:#}"),
                "The virtual display is drawn on another GPU"
            );
        }
        Control::start(driver, mode, Arc::clone(&follow))
            .map_err(|e| {
                warn!(
                    error = format!("{e:#}"),
                    "No virtual display; serving the monitors"
                )
            })
            .ok()
    });
    // A new virtual monitor takes a moment to reach DXGI.
    let patience = if display.is_some() {
        Duration::from_secs(5)
    } else {
        Duration::ZERO
    };
    let since = Instant::now();
    let target = follow.lock().unwrap().target.clone();
    let (adapter, _) = loop {
        match find_output(&target) {
            Ok(found) => break found,
            Err(_) if since.elapsed() < patience => {
                std::thread::sleep(Duration::from_millis(100))
            }
            Err(e) => return Err(e),
        }
    };
    let mut cap = Capture::new(&adapter, cuda, Arc::clone(&follow))?;
    // The first duplication here, so that a desktop that cannot be captured is an error
    // at start rather than a black stream.
    loop {
        match cap.duplicate() {
            Ok(()) => break,
            Err(_) if since.elapsed() < patience => {
                std::thread::sleep(Duration::from_millis(100))
            }
            Err(e) => return Err(e.context("duplicating the desktop")),
        }
    }
    let r = *cap.bounds.lock().unwrap();
    let size = ((r.right - r.left) as u32, (r.bottom - r.top) as u32);
    info!(
        width = size.0,
        height = size.1,
        "Capturing the {}",
        if display.is_some() {
            "virtual display"
        } else {
            "primary monitor"
        }
    );

    let (tx, rx) = crate::frame::latest_frame();
    let (cursor_tx, cursor_rx) = watch::channel(None);
    let bounds = Arc::clone(&cap.bounds);
    std::thread::Builder::new()
        .name("win-capture".into())
        .spawn(move || cap.run(&tx, &cursor_tx))?;
    Ok(WinSession {
        size,
        source: WinSource {
            display: display.clone(),
        },
        frames: rx,
        cursor: cursor_rx,
        input: WinInput { bounds },
        display,
    })
}

struct Capture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// The adapter the device is on.
    luid: LUID,
    dupl: Option<IDXGIOutputDuplication>,
    /// CPU-readable copy of the desktop, and its size.
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    /// System-memory buffers the frames are copied into, reused.
    buffers: Recycler,
    /// The GPU path, when the desktop is on the NVENC GPU.
    gpu: Option<D3dToCuda>,
    /// The GPU path for AMF and QSV: desktop copies in D3D11 textures (`wind3d`).
    d3d: Option<TexturePool>,
    cuda: Option<Arc<cudarc::driver::CudaContext>>,
    /// The display to capture, and the generation of it duplicated last.
    follow: Followed,
    seen: u64,
    bounds: Bounds,
    shape: Option<CursorShape>,
    visible: bool,
}

/// How long to wait for a frame before checking again (a still desktop sends none).
const FRAME_WAIT_MS: u32 = 100;
/// Between attempts to duplicate the desktop again.
const RETRY: Duration = Duration::from_millis(250);

impl Capture {
    fn new(
        adapter: &IDXGIAdapter1,
        cuda: Option<Arc<cudarc::driver::CudaContext>>,
        follow: Followed,
    ) -> Result<Self> {
        let (device, context) = create_device(adapter)?;
        let mut cap = Self {
            device,
            context,
            luid: LUID::default(),
            dupl: None,
            staging: None,
            buffers: Recycler::default(),
            gpu: None,
            d3d: None,
            cuda,
            follow,
            seen: 0,
            bounds: Arc::new(Mutex::new(RECT::default())),
            shape: None,
            visible: true,
        };
        cap.adapter_ready(adapter);
        Ok(cap)
    }

    /// Notes the device's adapter and sets up the GPU path for it.
    fn adapter_ready(&mut self, adapter: &IDXGIAdapter1) {
        // SAFETY: plain DXGI call on a live interface.
        if let Ok(d) = unsafe { adapter.GetDesc1() } {
            let name = String::from_utf16_lossy(&d.Description);
            debug!(adapter = name.trim_end_matches('\0'), "Display adapter");
            self.luid = d.AdapterLuid;
        }
        self.staging = None;
        self.gpu = self.cuda.clone().and_then(|ctx| {
            D3dToCuda::new(ctx, adapter)
                .inspect(|_| info!("The desktop goes to NVENC without leaving the GPU"))
                .map_err(|e| {
                    info!(
                        reason = format!("{e:#}"),
                        "The desktop goes to NVENC through system memory"
                    )
                })
                .ok()
        });
        // AMF and QSV read the desktop's textures on the encoder thread (on this device:
        // a new one makes the encoder set up its frames again).
        self.d3d = (self.gpu.is_none() && crate::wind3d::enabled()).then(|| {
            crate::wind3d::protect(&self.device);
            TexturePool::default()
        });
    }

    fn duplicate(&mut self) -> Result<()> {
        self.dupl = None;
        let target = {
            let f = self.follow.lock().unwrap();
            self.seen = f.generation;
            f.target.clone()
        };
        // On the input desktop first: what is attached and what can be duplicated depends
        // on it (Sunshine syncs the thread desktop before enumerating too).
        let desktop = follow_input_desktop();
        let (adapter, output) = find_output(&target)?;
        // SAFETY: plain DXGI call on a live interface.
        let luid = unsafe { adapter.GetDesc1()? }.AdapterLuid;
        if luid != self.luid {
            // The display moved to another GPU (a virtual display drawn elsewhere).
            info!("The display is on another adapter: new D3D11 device");
            (self.device, self.context) = create_device(&adapter)?;
            self.adapter_ready(&adapter);
        }
        // SAFETY: plain DXGI calls on live interfaces.
        unsafe {
            let desc = output.GetDesc()?;
            *self.bounds.lock().unwrap() = desc.DesktopCoordinates;
            let dupl = output
                .DuplicateOutput(&self.device)
                .with_context(|| format!("on the input desktop {desktop:?}"))?;
            self.dupl = Some(dupl);
        }
        Ok(())
    }

    fn run(mut self, frames: &FrameSender, cursor: &watch::Sender<Option<CursorEvent>>) {
        let mut failing_since: Option<Instant> = None;
        loop {
            if self.dupl.is_some() && self.follow.lock().unwrap().generation != self.seen {
                debug!("The display to capture changed");
                self.dupl = None;
            }
            if self.dupl.is_none() {
                match self.duplicate() {
                    Ok(()) => {
                        if let Some(t) = failing_since.take() {
                            info!(after = ?t.elapsed(), "Desktop duplication is back");
                        }
                    }
                    Err(e) => {
                        if failing_since.is_none() {
                            warn!(
                                error = format!("{e:#}"),
                                "Cannot duplicate the desktop; retrying"
                            );
                            failing_since = Some(Instant::now());
                        }
                        if cursor.receiver_count() == 0 {
                            return;
                        }
                        std::thread::sleep(RETRY);
                        continue;
                    }
                }
            }
            if let Err(e) = self.next(frames, cursor) {
                let lost = e.code() == DXGI_ERROR_ACCESS_LOST;
                if lost {
                    info!(
                        "Desktop duplication lost (mode change, full-screen application or desktop switch)"
                    );
                } else {
                    warn!(%e, "AcquireNextFrame failed");
                }
                self.dupl = None;
                if cursor.receiver_count() == 0 {
                    return;
                }
            }
        }
    }

    /// Waits for the next update and hands over the image and the pointer it carries.
    fn next(
        &mut self,
        frames: &FrameSender,
        cursor: &watch::Sender<Option<CursorEvent>>,
    ) -> windows::core::Result<()> {
        let dupl = self.dupl.clone().expect("duplicated");
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource = None;
        // SAFETY: the frame is released before the next acquire.
        match unsafe { dupl.AcquireNextFrame(FRAME_WAIT_MS, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(()),
            Err(e) => return Err(e),
        }
        let r = (|| {
            if info.PointerShapeBufferSize > 0 {
                self.pointer_shape(&dupl, info.PointerShapeBufferSize, cursor);
            }
            if info.LastMouseUpdateTime != 0 {
                let visible = info.PointerPosition.Visible.as_bool();
                if visible != self.visible {
                    self.visible = visible;
                    let ev = match (&self.shape, visible) {
                        (Some(s), true) => Some(CursorEvent::Shape(s.clone())),
                        _ => Some(CursorEvent::Hidden),
                    };
                    cursor.send_replace(ev);
                }
            }
            // Pointer-only updates carry no new image.
            if info.LastPresentTime != 0
                && let Some(res) = resource
            {
                let tex: ID3D11Texture2D = res.cast()?;
                if !self.read_gpu(&tex, frames) && !self.read_d3d(&tex, frames) {
                    self.read(&tex, frames)?;
                }
            }
            Ok(())
        })();
        // SAFETY: matches the successful acquire above.
        unsafe { dupl.ReleaseFrame()? };
        r
    }

    /// Hands the desktop texture to the encoder without leaving the GPU, when it is on the
    /// encoder's. False when the frame must go through system memory instead.
    fn read_gpu(&mut self, tex: &ID3D11Texture2D, frames: &FrameSender) -> bool {
        let Some(gpu) = self.gpu.as_mut() else {
            return false;
        };
        let t = Instant::now();
        match gpu.copy(&self.device, &self.context, tex) {
            Ok(Some(buf)) => {
                debug!(
                    read_ms = t.elapsed().as_secs_f64() * 1e3,
                    "Desktop read (GPU)"
                );
                frames.send(Frame {
                    width: buf.width,
                    height: buf.height,
                    data: FrameData::Gpu(buf),
                    damage: None,
                    captured: Instant::now(),
                    recycle: None,
                });
            }
            // Every buffer is with the encoder: this image is skipped, a newer one follows.
            Ok(None) => {}
            Err(e) => {
                warn!(
                    error = format!("{e:#}"),
                    "Copying the desktop on the GPU failed; going through system memory"
                );
                self.gpu = None;
                return false;
            }
        }
        true
    }

    /// Hands a GPU copy of the desktop texture to AMF or QSV. False when the frame must go
    /// through system memory instead (no such encoder, or the path failed).
    fn read_d3d(&mut self, tex: &ID3D11Texture2D, frames: &FrameSender) -> bool {
        if !crate::wind3d::enabled() {
            self.d3d = None;
        }
        let Some(pool) = self.d3d.as_mut() else {
            return false;
        };
        match pool.copy(&self.device, &self.context, tex) {
            Ok(Some(t)) => {
                frames.send(Frame {
                    width: t.width,
                    height: t.height,
                    data: FrameData::D3d(t),
                    damage: None,
                    captured: Instant::now(),
                    recycle: None,
                });
            }
            // Every texture is with the encoder: this image is skipped, a newer one follows.
            Ok(None) => {}
            Err(e) => {
                warn!(error = format!("{e:#}"), "Copying the desktop on the GPU failed");
                crate::wind3d::give_up();
                self.d3d = None;
                return false;
            }
        }
        true
    }

    /// Copies the desktop texture to system memory.
    fn read(&mut self, tex: &ID3D11Texture2D, frames: &FrameSender) -> windows::core::Result<()> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: plain D3D11 calls; the mapping is read within its lifetime and unmapped.
        unsafe {
            tex.GetDesc(&mut desc);
            let (w, h) = (desc.Width, desc.Height);
            if !matches!(self.staging, Some((_, sw, sh)) if (sw, sh) == (w, h)) {
                let staging_desc = D3D11_TEXTURE2D_DESC {
                    Width: w,
                    Height: h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                };
                let mut t = None;
                self.device
                    .CreateTexture2D(&staging_desc, None, Some(&mut t))?;
                self.staging = t.map(|t| (t, w, h));
            }
            let Some((staging, _, _)) = &self.staging else {
                return Ok(());
            };
            let t = Instant::now();
            self.context.CopyResource(staging, tex);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            // Waits for the copy.
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let stride = mapped.RowPitch;
            let len = (stride * h) as usize;
            let mut data = self.buffers.take(len);
            data.extend_from_slice(std::slice::from_raw_parts(mapped.pData as *const u8, len));
            self.context.Unmap(staging, 0);
            debug!(read_ms = t.elapsed().as_secs_f64() * 1e3, "Desktop read");
            frames.send(Frame {
                width: w,
                height: h,
                data: FrameData::Cpu { data, stride },
                damage: None,
                captured: Instant::now(),
                recycle: Some(self.buffers.clone()),
            });
        }
        Ok(())
    }

    fn pointer_shape(
        &mut self,
        dupl: &IDXGIOutputDuplication,
        size: u32,
        cursor: &watch::Sender<Option<CursorEvent>>,
    ) {
        let mut buf = vec![0u8; size as usize];
        let mut used = 0;
        let mut info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        // SAFETY: the buffer has the size DXGI asked for.
        let r = unsafe {
            dupl.GetFramePointerShape(size, buf.as_mut_ptr().cast(), &mut used, &mut info)
        };
        if let Err(e) = r {
            debug!(%e, "GetFramePointerShape failed");
            return;
        }
        let Some(shape) = convert_shape(&buf, &info) else {
            debug!(kind = info.Type, "Unknown pointer shape type");
            return;
        };
        if self.visible {
            cursor.send_replace(Some(CursorEvent::Shape(shape.clone())));
        }
        self.shape = Some(shape);
    }
}

/// Moves the calling thread to the desktop that has the input now (it changes with the lock
/// screen and UAC prompts); duplication, SendInput and mode changes only work there. Returns
/// its name. Only SYSTEM can open the secure desktop (`Winlogon`): run as the service, the host
/// follows it there (Sunshine `src/platform/windows/misc.cpp`, `syncThreadDesktop`).
pub(crate) fn follow_input_desktop() -> Option<String> {
    // SAFETY: the desktop handle is closed after the thread switched to it (the thread keeps
    // its own reference); the name buffer outlives the call.
    unsafe {
        let desk = OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS_FLAGS(GENERIC_ALL.0),
        )
        .ok()?;
        let switched = SetThreadDesktop(desk).is_ok();
        let mut name = [0u16; 64];
        let mut len = 0;
        let named = GetUserObjectInformationW(
            HANDLE(desk.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            (name.len() * 2) as u32,
            Some(&mut len),
        )
        .is_ok();
        let _ = CloseDesktop(desk);
        let name =
            named.then(|| String::from_utf16_lossy(&name[..(len as usize / 2).saturating_sub(1)]));
        debug!(?name, switched, "Input desktop");
        name
    }
}

/// A DXGI pointer shape as an RDP pointer (BGRA, rows bottom-up, straight alpha). Pixels
/// that invert the screen (masked and monochrome cursors) become black, the usual
/// approximation for a cursor drawn by the client.
fn convert_shape(buf: &[u8], info: &DXGI_OUTDUPL_POINTER_SHAPE_INFO) -> Option<CursorShape> {
    let (w, pitch) = (info.Width as usize, info.Pitch as usize);
    let kind = info.Type as i32;
    let h = if kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 {
        info.Height as usize / 2
    } else {
        info.Height as usize
    };
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        let dst_row = &mut out[(h - 1 - y) * w * 4..][..w * 4];
        for x in 0..w {
            let px: [u8; 4] = if kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 {
                buf.get(y * pitch + x * 4..y * pitch + x * 4 + 4)?
                    .try_into()
                    .ok()?
            } else if kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 {
                let p: [u8; 4] = buf
                    .get(y * pitch + x * 4..y * pitch + x * 4 + 4)?
                    .try_into()
                    .ok()?;
                // Mask 0: the colour replaces the screen. Mask 0xff: XOR with the screen.
                match (p[3], p[..3] == [0, 0, 0]) {
                    (0, _) => [p[0], p[1], p[2], 0xff],
                    (_, true) => [0, 0, 0, 0],
                    (_, false) => [0, 0, 0, 0xff],
                }
            } else if kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 {
                let bit = |row: usize| {
                    (buf.get(row * pitch + x / 8).copied().unwrap_or(0) >> (7 - x % 8)) & 1
                };
                match (bit(y), bit(y + h)) {
                    (0, 0) => [0, 0, 0, 0xff],
                    (0, _) => [0xff, 0xff, 0xff, 0xff],
                    (_, 0) => [0, 0, 0, 0],
                    _ => [0, 0, 0, 0xff],
                }
            } else {
                return None;
            };
            dst_row[x * 4..x * 4 + 4].copy_from_slice(&px);
        }
    }
    Some(CursorShape {
        width: w as u16,
        height: h as u16,
        hot_x: info.HotSpot.x as u16,
        hot_y: info.HotSpot.y as u16,
        bgra_bottom_up: out,
    })
}

/// Set-1 scan code of an evdev key code, and whether it takes the E0 prefix (the inverse of
/// `input::scancode_to_evdev`, plus the keys only USB keyboards have).
fn evdev_to_scancode(code: u32) -> Option<(u16, bool)> {
    Some(match code {
        1..=0x58 => (code as u16, false),
        89 => (0x73, false),                              // KEY_RO
        92 => (0x79, false),                              // KEY_HENKAN
        93 => (0x70, false),                              // KEY_KATAKANAHIRAGANA
        94 => (0x7b, false),                              // KEY_MUHENKAN
        117 => (0x59, false),                             // KEY_KPEQUAL
        124 => (0x7d, false),                             // KEY_YEN
        183..=193 => (0x64 + (code - 183) as u16, false), // KEY_F13..KEY_F23
        194 => (0x76, false),                             // KEY_F24
        96 => (0x1c, true),                               // KEY_KPENTER
        97 => (0x1d, true),                               // KEY_RIGHTCTRL
        98 => (0x35, true),                               // KEY_KPSLASH
        99 => (0x37, true),                               // KEY_SYSRQ
        100 => (0x38, true),                              // KEY_RIGHTALT
        102 => (0x47, true),                              // KEY_HOME
        103 => (0x48, true),                              // KEY_UP
        104 => (0x49, true),                              // KEY_PAGEUP
        105 => (0x4b, true),                              // KEY_LEFT
        106 => (0x4d, true),                              // KEY_RIGHT
        107 => (0x4f, true),                              // KEY_END
        108 => (0x50, true),                              // KEY_DOWN
        109 => (0x51, true),                              // KEY_PAGEDOWN
        110 => (0x52, true),                              // KEY_INSERT
        111 => (0x53, true),                              // KEY_DELETE
        113 => (0x20, true),                              // KEY_MUTE
        114 => (0x2e, true),                              // KEY_VOLUMEDOWN
        115 => (0x30, true),                              // KEY_VOLUMEUP
        125 => (0x5b, true),                              // KEY_LEFTMETA
        126 => (0x5c, true),                              // KEY_RIGHTMETA
        127 => (0x5d, true),                              // KEY_COMPOSE
        163 => (0x19, true),                              // KEY_NEXTSONG
        164 => (0x22, true),                              // KEY_PLAYPAUSE
        165 => (0x10, true),                              // KEY_PREVIOUSSONG
        166 => (0x24, true),                              // KEY_STOPCD
        _ => return None,
    })
}

/// The Unicode code point of a keysym from `input::unicode_keysym`.
fn keysym_char(sym: u32) -> Option<char> {
    match sym {
        0x20..=0x7e | 0xa0..=0xff => char::from_u32(sym),
        _ if sym & 0xff00_0000 == 0x0100_0000 => char::from_u32(sym & 0x00ff_ffff),
        _ => None,
    }
}

pub struct WinInput {
    bounds: Bounds,
}

fn key_input(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn mouse_input(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send(inputs: &[INPUT]) -> Result<()> {
    // SAFETY: the structures are fully initialised.
    let inject = || unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    let mut n = inject();
    if n == 0 {
        // The input desktop changed under this thread: follow it and try again.
        follow_input_desktop();
        n = inject();
    }
    if n as usize != inputs.len() {
        // UIPI: windows of elevated programs do not take input from a normal one.
        bail!("SendInput injected {n} of {} events", inputs.len());
    }
    Ok(())
}

/// `D3DKMT_SCHEDULINGPRIORITYCLASS`: high, and realtime (which needs
/// SeIncreaseBasePriorityPrivilege: administrators and SYSTEM have it).
const GPU_PRIORITY_HIGH: i32 = 4;
const GPU_PRIORITY_REALTIME: i32 = 5;

#[link(name = "gdi32")]
unsafe extern "system" {
    /// gdi32's export (d3dkmthk.h); a negative NTSTATUS on failure.
    fn D3DKMTSetProcessSchedulingPriorityClass(process: *mut std::ffi::c_void, priority: i32)
    -> i32;
}

/// Asks the GPU scheduler to run this process's work (capture copies, colour conversion,
/// encoding) ahead of the desktop's: a game that keeps the GPU busy would otherwise delay
/// every frame. Realtime where the process may, else high. Realtime needs
/// SeIncreaseBasePriorityPrivilege, which administrators and SYSTEM hold but must enable
/// first (Sunshine `src/platform/windows/display_base.cpp` does both); the service's helper
/// runs as SYSTEM, so it gets realtime.
///
/// `GLIDEDESK_GPU_PRIORITY=high` stops at high (Sunshine does so on NVIDIA with hardware
/// scheduling, where realtime has frozen the encoder), `normal` leaves it alone.
fn raise_gpu_priority() {
    if let Err(e) = crate::winservice::enable_own_privilege(SE_INC_BASE_PRIORITY_NAME) {
        debug!(
            error = format!("{e:#}"),
            "SeIncreaseBasePriorityPrivilege not enabled"
        );
    }
    let classes: &[(i32, &str)] = match std::env::var("GLIDEDESK_GPU_PRIORITY").as_deref() {
        Ok("normal") => return,
        Ok("high") => &[(GPU_PRIORITY_HIGH, "high")],
        _ => &[
            (GPU_PRIORITY_REALTIME, "realtime"),
            (GPU_PRIORITY_HIGH, "high"),
        ],
    };
    // GetCurrentProcess's pseudo handle.
    let me = -1isize as *mut std::ffi::c_void;
    for &(class, name) in classes {
        // SAFETY: the pseudo handle of this process and a valid priority class.
        let status = unsafe { D3DKMTSetProcessSchedulingPriorityClass(me, class) };
        if status >= 0 {
            info!(priority = name, "GPU scheduling priority");
            return;
        }
        debug!(
            priority = name,
            status = format!("{status:#x}"),
            "GPU priority not granted"
        );
    }
}

/// A coordinate of the virtual desktop as SendInput's absolute units (0..=65535 across it).
fn absolute(pos: i32, origin: i32, extent: i32) -> i32 {
    let extent = extent.max(1);
    ((f64::from(pos - origin) + 0.5) * 65536.0 / f64::from(extent)).clamp(0.0, 65535.0) as i32
}

#[async_trait::async_trait]
impl InputBackend for WinInput {
    async fn apply(&self, ev: &Input) -> Result<()> {
        let up = |down: bool| {
            if down {
                KEYBD_EVENT_FLAGS(0)
            } else {
                KEYEVENTF_KEYUP
            }
        };
        match *ev {
            // Pause has no plain make code (E1 1D 45): send it as a virtual key.
            Input::Key(119, down) => send(&[key_input(VK_PAUSE, 0, up(down))]),
            Input::Key(code, down) => {
                let Some((scan, extended)) = evdev_to_scancode(code) else {
                    debug!(code, "No scan code for this key");
                    return Ok(());
                };
                let mut flags = KEYEVENTF_SCANCODE | up(down);
                if extended {
                    flags |= KEYEVENTF_EXTENDEDKEY;
                }
                send(&[key_input(VIRTUAL_KEY(0), scan, flags)])
            }
            Input::Keysym(sym, down) => {
                let Some(ch) = keysym_char(sym) else {
                    debug!(sym, "Keysym without a character");
                    return Ok(());
                };
                let mut units = [0u16; 2];
                let inputs: Vec<INPUT> = ch
                    .encode_utf16(&mut units)
                    .iter()
                    .map(|&u| key_input(VIRTUAL_KEY(0), u, KEYEVENTF_UNICODE | up(down)))
                    .collect();
                send(&inputs)
            }
            Input::Motion(x, y) => {
                let r = *self.bounds.lock().unwrap();
                // SAFETY: GetSystemMetrics has no preconditions.
                let (vx, vy, vw, vh) = unsafe {
                    (
                        GetSystemMetrics(SM_XVIRTUALSCREEN),
                        GetSystemMetrics(SM_YVIRTUALSCREEN),
                        GetSystemMetrics(SM_CXVIRTUALSCREEN),
                        GetSystemMetrics(SM_CYVIRTUALSCREEN),
                    )
                };
                let (px, py) = (r.left + x as i32, r.top + y as i32);
                send(&[mouse_input(
                    absolute(px, vx, vw),
                    absolute(py, vy, vh),
                    0,
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                )])
            }
            Input::Button(btn, down) => {
                let (flags, data) = match (btn, down) {
                    (0x110, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                    (0x110, false) => (MOUSEEVENTF_LEFTUP, 0),
                    (0x111, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                    (0x111, false) => (MOUSEEVENTF_RIGHTUP, 0),
                    (0x112, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                    (0x112, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                    (0x113, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
                    (0x113, false) => (MOUSEEVENTF_XUP, XBUTTON1),
                    (0x114, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
                    (0x114, false) => (MOUSEEVENTF_XUP, XBUTTON2),
                    _ => return Ok(()),
                };
                send(&[mouse_input(0, 0, u32::from(data), flags)])
            }
            Input::Scroll { axis, steps } => {
                // Windows: positive is up for the wheel and right for the horizontal one.
                let (flags, delta) = if axis == 0 {
                    (MOUSEEVENTF_WHEEL, -steps * 120)
                } else {
                    (MOUSEEVENTF_HWHEEL, steps * 120)
                };
                send(&[mouse_input(0, 0, delta as u32, flags)])
            }
            // Touch injection is not used yet: the forwarder turns fingers into the pointer.
            Input::TouchDown { .. } | Input::TouchMotion { .. } | Input::TouchUp { .. } => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::scancode_to_evdev;

    #[test]
    fn scancodes_round_trip() {
        for code in 1..=127u32 {
            if let Some((scan, ext)) = evdev_to_scancode(code)
                && let Some(back) = scancode_to_evdev(scan as u8, ext)
            {
                assert_eq!(back, code, "scan code {scan:#x} extended {ext}");
            }
        }
        assert_eq!(evdev_to_scancode(103), Some((0x48, true))); // Up
        assert_eq!(evdev_to_scancode(30), Some((0x1e, false))); // A
    }

    #[test]
    fn keysyms_to_chars() {
        assert_eq!(keysym_char(0x61), Some('a'));
        assert_eq!(keysym_char(0xf1), Some('ñ'));
        assert_eq!(keysym_char(0x0100_20ac), Some('€'));
        assert_eq!(keysym_char(0x0101_f600), Some('😀'));
        assert_eq!(keysym_char(0xff0d), None);
    }

    #[test]
    fn following_a_display() {
        let f = Followed::default();
        assert_eq!(f.lock().unwrap().target, Target::Primary);
        follow(&f, Target::Named(r"\\.\DISPLAY3".into()));
        follow(&f, Target::Named(r"\\.\DISPLAY3".into()));
        assert_eq!(f.lock().unwrap().generation, 1, "the same display is no change");
        follow(&f, Target::Nothing);
        assert_eq!(f.lock().unwrap().generation, 2);
    }

    #[test]
    fn absolute_units() {
        assert_eq!(absolute(0, 0, 1920), 17);
        assert_eq!(absolute(1919, 0, 1920), 65518);
        assert_eq!(absolute(-10, 0, 1920), 0);
        // Second monitor to the left of the primary one.
        assert_eq!(absolute(0, -1920, 3840), 32776);
    }
}
