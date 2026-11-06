//! Windows session backend: DXGI Desktop Duplication capture, SendInput, WASAPI loopback audio.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::sync::{broadcast, watch};
use tracing::{debug, info, warn};
use windows::Win32::Foundation::{HMODULE, HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_RESOURCE_MISC_SHARED, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTPUT_DESC, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1,
    IDXGIOutputDuplication, IDXGIResource,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
use windows::core::Interface;

use crate::frame::{CursorEvent, CursorShape, Frame, FrameData, FrameReceiver, Rect, Source};
use crate::input::{Input, InputBackend};

pub struct WinSession {
    pub size: (u32, u32),
    pub source: WinSource,
    pub frames: FrameReceiver,
    pub cursor: watch::Receiver<Option<CursorEvent>>,
    pub input: WinInput,
}

pub struct WinSource {
    refresh: std::sync::atomic::AtomicU32,
}

impl Source for WinSource {
    fn resize(&self, width: u32, height: u32) {
        debug!(
            width,
            height, "Windows session uses physical display resolution"
        );
    }

    fn set_refresh(&self, hz: u32) -> bool {
        self.refresh.swap(hz, Ordering::Relaxed) != hz
    }
}

pub fn start(adapter_index: usize, fps: u32) -> Result<WinSession> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let adapter = factory
            .EnumAdapters1(adapter_index as u32)
            .or_else(|_| factory.EnumAdapters1(0))
            .context("enumerating DXGI adapters")?;

        let output = adapter.EnumOutputs(0).context("enumerating DXGI outputs")?;
        let output1: IDXGIOutput1 = output.cast()?;
        let desc = output.GetDesc()?;

        let w = (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left).unsigned_abs();
        let h = (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top).unsigned_abs();

        let mut device = None;
        let mut context = None;
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            7, // D3D11_SDK_VERSION
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .context("creating D3D11 device")?;

        let device = device.expect("device created");
        let context = context.expect("context created");

        let dupl = output1
            .DuplicateOutput(&device)
            .context("DuplicateOutput")?;

        let (tx, rx) = crate::frame::latest_frame();
        let (_cursor_tx, cursor_rx) = watch::channel(None);

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
        let mut staging = None;
        device.CreateTexture2D(&staging_desc, None, Some(&mut staging))?;
        let staging = staging.expect("staging texture created");

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);

        let ctx = CaptureCtx {
            dupl,
            context,
            staging,
            w,
            h,
            fps,
            tx,
            stop: thread_stop,
        };

        std::thread::Builder::new()
            .name("win-capture".into())
            .spawn(move || {
                capture_loop(ctx);
            })?;

        Ok(WinSession {
            size: (w, h),
            source: WinSource {
                refresh: std::sync::atomic::AtomicU32::new(fps),
            },
            frames: rx,
            cursor: cursor_rx,
            input: WinInput {
                width: w,
                height: h,
            },
        })
    }
}

struct CaptureCtx {
    dupl: IDXGIOutputDuplication,
    context: ID3D11DeviceContext,
    staging: ID3D11Texture2D,
    w: u32,
    h: u32,
    fps: u32,
    tx: crate::frame::FrameSender,
    stop: Arc<AtomicBool>,
}

fn capture_loop(ctx: CaptureCtx) {
    let timeout_ms = 1000 / ctx.fps.max(1);
    while !ctx.stop.load(Ordering::Relaxed) {
        let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut desktop_resource = None;
        let res = unsafe {
            ctx.dupl
                .AcquireNextFrame(timeout_ms, &mut frame_info, &mut desktop_resource)
        };
        match res {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => continue,
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                warn!("DXGI Desktop Duplication access lost (UAC or desktop switch)");
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(e) => {
                warn!(%e, "AcquireNextFrame failed");
                break;
            }
        }

        let Some(resource) = desktop_resource else {
            let _ = unsafe { ctx.dupl.ReleaseFrame() };
            continue;
        };

        let tex: ID3D11Texture2D = match resource.cast() {
            Ok(t) => t,
            Err(_) => {
                let _ = unsafe { ctx.dupl.ReleaseFrame() };
                continue;
            }
        };

        unsafe {
            ctx.context.CopyResource(&ctx.staging, &tex);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            if ctx
                .context
                .Map(&ctx.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .is_ok()
            {
                let stride = mapped.RowPitch;
                let data_slice = std::slice::from_raw_parts(
                    mapped.pData as *const u8,
                    (stride * ctx.h) as usize,
                );
                let cpu_data = data_slice.to_vec();
                ctx.context.Unmap(&ctx.staging, 0);

                ctx.tx.send(Frame {
                    width: ctx.w,
                    height: ctx.h,
                    data: FrameData::Cpu {
                        data: cpu_data,
                        stride,
                    },
                    damage: None,
                    captured: Instant::now(),
                });
            }
            let _ = ctx.dupl.ReleaseFrame();
        }
    }
}

pub struct WinInput {
    width: u32,
    height: u32,
}

#[async_trait::async_trait]
impl InputBackend for WinInput {
    async fn apply(&self, ev: &Input) -> anyhow::Result<()> {
        match *ev {
            Input::Key(code, down) => {
                let flags = KEYEVENTF_SCANCODE
                    | if down {
                        KEYBD_EVENT_FLAGS(0)
                    } else {
                        KEYEVENTF_KEYUP
                    };
                let input = INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: VIRTUAL_KEY(0),
                            wScan: code as u16,
                            dwFlags: flags,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                };
                unsafe {
                    SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
                }
            }
            Input::Keysym(sym, down) => {
                // Unicode character injection
                if (sym & 0x0100_0000) != 0 || (sym <= 0xffff) {
                    let ch = (sym & 0xffff) as u16;
                    let flags = KEYEVENTF_UNICODE
                        | if down {
                            KEYBD_EVENT_FLAGS(0)
                        } else {
                            KEYEVENTF_KEYUP
                        };
                    let input = INPUT {
                        r#type: INPUT_KEYBOARD,
                        Anonymous: INPUT_0 {
                            ki: KEYBDINPUT {
                                wVk: VIRTUAL_KEY(0),
                                wScan: ch,
                                dwFlags: flags,
                                time: 0,
                                dwExtraInfo: 0,
                            },
                        },
                    };
                    unsafe {
                        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
                    }
                }
            }
            Input::Motion(x, y) => {
                let norm_x = ((x / self.width.max(1) as f64) * 65535.0) as i32;
                let norm_y = ((y / self.height.max(1) as f64) * 65535.0) as i32;
                let input = INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 {
                        mi: MOUSEINPUT {
                            dx: norm_x,
                            dy: norm_y,
                            mouseData: 0,
                            dwFlags: MOUSEEVENTF_MOVE
                                | MOUSEEVENTF_ABSOLUTE
                                | MOUSEEVENTF_VIRTUALDESK,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                };
                unsafe {
                    SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
                }
            }
            Input::Button(btn, down) => {
                let flags = match (btn, down) {
                    (0x110, true) => MOUSEEVENTF_LEFTDOWN,
                    (0x110, false) => MOUSEEVENTF_LEFTUP,
                    (0x111, true) => MOUSEEVENTF_RIGHTDOWN,
                    (0x111, false) => MOUSEEVENTF_RIGHTUP,
                    (0x112, true) => MOUSEEVENTF_MIDDLEDOWN,
                    (0x112, false) => MOUSEEVENTF_MIDDLEUP,
                    _ => return Ok(()),
                };
                let input = INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 {
                        mi: MOUSEINPUT {
                            dx: 0,
                            dy: 0,
                            mouseData: 0,
                            dwFlags: flags,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                };
                unsafe {
                    SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
                }
            }
            Input::Scroll { axis, steps } => {
                let (flags, data) = if axis == 0 {
                    (MOUSEEVENTF_WHEEL, (steps * -120) as u32)
                } else {
                    (MOUSEEVENTF_HWHEEL, (steps * 120) as u32)
                };
                let input = INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 {
                        mi: MOUSEINPUT {
                            dx: 0,
                            dy: 0,
                            mouseData: data,
                            dwFlags: flags,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                };
                unsafe {
                    SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
                }
            }
        }
        Ok(())
    }
}
