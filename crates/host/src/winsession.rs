//! Windows session: the console desktop through DXGI Desktop Duplication, input through
//! `SendInput`. Audio is in `winaudio`, the clipboard in `winclip`.
//!
//! The primary monitor is served at its own size (no virtual display yet). Duplication is
//! lost on a mode change, a full-screen exclusive application or a desktop switch (UAC,
//! lock screen); the capture thread duplicates the output again until it works. A user
//! program cannot capture or drive the secure desktop: that needs the service (ADR-0006).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::sync::watch;
use tracing::{debug, info, warn};
use windows::Win32::Foundation::{GENERIC_ALL, HANDLE, HMODULE, RECT};
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
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1,
    IDXGIOutputDuplication,
};
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

pub struct WinSession {
    pub size: (u32, u32),
    pub source: WinSource,
    pub frames: FrameReceiver,
    pub cursor: watch::Receiver<Option<CursorEvent>>,
    pub input: WinInput,
}

/// The desktop has the monitor's size: requests to change it are ignored.
pub struct WinSource;

impl Source for WinSource {
    fn resize(&self, width: u32, height: u32) {
        debug!(
            width,
            height, "The Windows desktop keeps the monitor's size"
        );
    }
}

/// The monitor being captured, in virtual desktop coordinates (input maps onto it).
type Bounds = Arc<Mutex<RECT>>;

/// The output of the primary monitor (the one at the desktop origin) and its adapter.
fn primary_output() -> Result<(IDXGIAdapter1, IDXGIOutput1)> {
    // SAFETY: DXGI enumeration; the interfaces returned are reference counted.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut first = None;
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
                let r = desc.DesktopCoordinates;
                let output: IDXGIOutput1 = output.cast()?;
                if r.left == 0 && r.top == 0 {
                    return Ok((adapter, output));
                }
                first.get_or_insert((adapter.clone(), output));
            }
        }
        first.context(
            "no monitor is attached to the desktop (a PC without one needs a virtual display)",
        )
    }
}

/// `cuda` is the NVENC encoder's context, if NVENC encodes.
pub fn start(cuda: Option<Arc<cudarc::driver::CudaContext>>) -> Result<WinSession> {
    // Real pixels: a process that is not DPI aware sees the desktop scaled down (and its
    // input would land scaled too).
    // SAFETY: no preconditions; it fails harmlessly if the awareness was already set.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let (adapter, output) = primary_output()?;
    // SAFETY: plain DXGI call on a live interface.
    if let Ok(d) = unsafe { adapter.GetDesc1() } {
        let name = String::from_utf16_lossy(&d.Description);
        debug!(adapter = name.trim_end_matches('\0'), "Display adapter");
    }
    let mut device = None;
    let mut context = None;
    // SAFETY: creates a device on the adapter that owns the output.
    unsafe {
        D3D11CreateDevice(
            &adapter,
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
    let gpu = cuda.and_then(|ctx| {
        D3dToCuda::new(ctx, &adapter)
            .inspect(|_| info!("The desktop goes to NVENC without leaving the GPU"))
            .map_err(|e| {
                info!(
                    reason = format!("{e:#}"),
                    "The desktop goes to NVENC through system memory"
                )
            })
            .ok()
    });
    let mut cap = Capture {
        device: device.context("no D3D11 device")?,
        context: context.context("no D3D11 context")?,
        output,
        dupl: None,
        staging: None,
        buffers: Recycler::default(),
        gpu,
        bounds: Arc::new(Mutex::new(RECT::default())),
        shape: None,
        visible: true,
    };
    // The first duplication here, so that a desktop that cannot be captured is an error
    // at start rather than a black stream.
    cap.duplicate().context("duplicating the desktop")?;
    let r = *cap.bounds.lock().unwrap();
    let size = ((r.right - r.left) as u32, (r.bottom - r.top) as u32);
    info!(
        width = size.0,
        height = size.1,
        "Capturing the primary monitor"
    );

    let (tx, rx) = crate::frame::latest_frame();
    let (cursor_tx, cursor_rx) = watch::channel(None);
    let bounds = Arc::clone(&cap.bounds);
    std::thread::Builder::new()
        .name("win-capture".into())
        .spawn(move || cap.run(&tx, &cursor_tx))?;
    Ok(WinSession {
        size,
        source: WinSource,
        frames: rx,
        cursor: cursor_rx,
        input: WinInput { bounds },
    })
}

struct Capture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output: IDXGIOutput1,
    dupl: Option<IDXGIOutputDuplication>,
    /// CPU-readable copy of the desktop, and its size.
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    /// System-memory buffers the frames are copied into, reused.
    buffers: Recycler,
    /// The GPU path, when the desktop is on the NVENC GPU.
    gpu: Option<D3dToCuda>,
    bounds: Bounds,
    shape: Option<CursorShape>,
    visible: bool,
}

/// How long to wait for a frame before checking again (a still desktop sends none).
const FRAME_WAIT_MS: u32 = 100;
/// Between attempts to duplicate the desktop again.
const RETRY: Duration = Duration::from_millis(250);

impl Capture {
    fn duplicate(&mut self) -> Result<()> {
        self.dupl = None;
        let desktop = follow_input_desktop();
        // SAFETY: plain DXGI calls on live interfaces.
        unsafe {
            let desc = self.output.GetDesc()?;
            *self.bounds.lock().unwrap() = desc.DesktopCoordinates;
            let dupl = self
                .output
                .DuplicateOutput(&self.device)
                .with_context(|| format!("on the input desktop {desktop:?}"))?;
            self.dupl = Some(dupl);
        }
        Ok(())
    }

    fn run(mut self, frames: &FrameSender, cursor: &watch::Sender<Option<CursorEvent>>) {
        let mut failing_since: Option<Instant> = None;
        loop {
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
                if !self.read_gpu(&tex, frames) {
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
/// screen and UAC prompts); duplication and SendInput only work there. Returns its name.
fn follow_input_desktop() -> Option<String> {
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
    fn absolute_units() {
        assert_eq!(absolute(0, 0, 1920), 17);
        assert_eq!(absolute(1919, 0, 1920), 65518);
        assert_eq!(absolute(-10, 0, 1920), 0);
        // Second monitor to the left of the primary one.
        assert_eq!(absolute(0, -1920, 3840), 32776);
    }
}
