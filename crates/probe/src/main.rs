//! Input-to-photon latency probe for remote desktop clients.
//!
//! Runs on the client machine. The remote session shows `tools/flash.py`, whose top-left
//! square turns white while a mouse button is held. The probe finds the client window,
//! presses the button over that square with `SendInput`, and polls the screen pixel until it
//! turns white; then releases and waits for black. The time from the press to the white
//! pixel is the round trip: client input → network → server input → app → compositor →
//! capture → encode → network → decode → present. It does not include the monitor's own
//! scan-out, so it compares clients and servers, not absolute glass-to-glass latency.

use clap::Parser;

#[derive(Parser, Debug)]
#[command(version, about = "Input-to-photon latency probe (Windows)")]
struct Args {
    /// Substring of the client window title (mstsc: the host name; Moonlight: "Moonlight").
    #[arg(long)]
    window: String,
    /// Samples to take.
    #[arg(long, default_value_t = 100)]
    samples: usize,
    /// Point inside the remote flash square, relative to the window's client area.
    #[arg(long, default_value_t = 40)]
    x: i32,
    #[arg(long, default_value_t = 40)]
    y: i32,
    /// Give up on a sample after this many milliseconds.
    #[arg(long, default_value_t = 1000)]
    timeout_ms: u64,
    /// Write every sample (milliseconds) to this CSV file.
    #[arg(long)]
    csv: Option<std::path::PathBuf>,
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    win::run(Args::parse())
}

#[cfg(not(windows))]
fn main() {
    let _ = Args::parse();
    eprintln!("glidedesk-probe runs on Windows clients");
    std::process::exit(2);
}

#[cfg(windows)]
mod win {
    use std::io::Write;
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, bail};
    use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT};
    use windows::Win32::Graphics::Gdi::{
        BitBlt, ClientToScreen, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject,
        GetDC, GetPixel, ReleaseDC, SRCCOPY, SelectObject,
    };
    use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
    use windows::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEINPUT,
        SendInput,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClientRect, GetWindowTextW, IsWindowVisible, SetCursorPos,
        SetForegroundWindow,
    };
    use windows::core::BOOL;

    use super::Args;

    fn find_window(needle: &str) -> Option<HWND> {
        struct Search {
            needle: String,
            found: Option<HWND>,
        }
        unsafe extern "system" fn cb(hwnd: HWND, lp: LPARAM) -> BOOL {
            // SAFETY: lp is the &mut Search passed below, alive for the enumeration.
            let s = unsafe { &mut *(lp.0 as *mut Search) };
            let mut buf = [0u16; 512];
            // SAFETY: plain Win32 queries on a window handle from EnumWindows.
            let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
            let title = String::from_utf16_lossy(&buf[..n as usize]);
            if unsafe { IsWindowVisible(hwnd) }.as_bool() && title.contains(&s.needle) {
                s.found = Some(hwnd);
                return BOOL(0);
            }
            BOOL(1)
        }
        let mut s = Search {
            needle: needle.to_owned(),
            found: None,
        };
        // SAFETY: the callback only touches `s` through the pointer we pass.
        let _ = unsafe { EnumWindows(Some(cb), LPARAM(&mut s as *mut Search as isize)) };
        s.found
    }

    /// Reads one screen pixel through a 1×1 BitBlt (works with DWM composition).
    struct Pixel {
        screen: windows::Win32::Graphics::Gdi::HDC,
        mem: windows::Win32::Graphics::Gdi::HDC,
        bmp: windows::Win32::Graphics::Gdi::HBITMAP,
        at: POINT,
    }

    impl Pixel {
        fn new(at: POINT) -> Self {
            // SAFETY: standard GDI setup for a 1×1 memory bitmap.
            unsafe {
                let screen = GetDC(None);
                let mem = CreateCompatibleDC(Some(screen));
                let bmp = CreateCompatibleBitmap(screen, 1, 1);
                SelectObject(mem, bmp.into());
                Self {
                    screen,
                    mem,
                    bmp,
                    at,
                }
            }
        }

        /// Luma-ish brightness 0..255.
        fn brightness(&self) -> u32 {
            // SAFETY: DCs created in `new`.
            unsafe {
                let _ = BitBlt(
                    self.mem,
                    0,
                    0,
                    1,
                    1,
                    Some(self.screen),
                    self.at.x,
                    self.at.y,
                    SRCCOPY,
                );
                let c = GetPixel(self.mem, 0, 0).0;
                ((c & 0xff) + ((c >> 8) & 0xff) + ((c >> 16) & 0xff)) / 3
            }
        }
    }

    impl Drop for Pixel {
        fn drop(&mut self) {
            // SAFETY: releasing what `new` created.
            unsafe {
                let _ = DeleteObject(self.bmp.into());
                let _ = DeleteDC(self.mem);
                ReleaseDC(None, self.screen);
            }
        }
    }

    fn mouse(flags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS) {
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dwFlags: flags,
                    ..Default::default()
                },
            },
        };
        // SAFETY: one well-formed INPUT.
        unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
    }

    fn wait_for(px: &Pixel, white: bool, timeout: Duration) -> Option<Duration> {
        let start = Instant::now();
        while start.elapsed() < timeout {
            let b = px.brightness();
            if (white && b > 200) || (!white && b < 55) {
                return Some(start.elapsed());
            }
            std::hint::spin_loop();
        }
        None
    }

    pub fn run(args: Args) -> Result<()> {
        // SAFETY: process-wide settings, set before any window or DC use.
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            timeBeginPeriod(1);
        }
        let hwnd = find_window(&args.window)
            .with_context(|| format!("no visible window with \"{}\"", args.window))?;
        let mut rect = RECT::default();
        let mut origin = POINT::default();
        // SAFETY: valid window handle.
        unsafe {
            GetClientRect(hwnd, &mut rect)?;
            let _ = ClientToScreen(hwnd, &mut origin);
            let _ = SetForegroundWindow(hwnd);
        }
        let at = POINT {
            x: origin.x + args.x,
            y: origin.y + args.y,
        };
        println!(
            "window client {}x{} at {},{}; probing {},{}",
            rect.right, rect.bottom, origin.x, origin.y, at.x, at.y
        );
        // SAFETY: moves the pointer over the probe square.
        unsafe { SetCursorPos(at.x, at.y)? };
        std::thread::sleep(Duration::from_millis(500));
        let px = Pixel::new(at);
        if px.brightness() > 55 {
            mouse(MOUSEEVENTF_LEFTUP);
            if wait_for(&px, false, Duration::from_secs(2)).is_none() {
                bail!(
                    "the probe square is not black: is tools/flash.py full screen in the session?"
                );
            }
        }

        let timeout = Duration::from_millis(args.timeout_ms);
        let mut samples = Vec::with_capacity(args.samples);
        let mut lost = 0;
        for i in 0..args.samples {
            mouse(MOUSEEVENTF_LEFTDOWN);
            let down = wait_for(&px, true, timeout);
            mouse(MOUSEEVENTF_LEFTUP);
            let up = wait_for(&px, false, timeout);
            match down {
                Some(d) => samples.push(d.as_secs_f64() * 1000.0),
                None => lost += 1,
            }
            if up.is_none() {
                lost += 1;
            }
            // Desynchronise from the frame clock so samples cover the whole frame interval.
            let jitter = 20 + (i * 7919 % 37) as u64;
            std::thread::sleep(Duration::from_millis(jitter));
        }
        // SAFETY: pairs with timeBeginPeriod above.
        unsafe { timeEndPeriod(1) };

        if let Some(path) = &args.csv {
            let mut f = std::fs::File::create(path)?;
            writeln!(f, "ms")?;
            for s in &samples {
                writeln!(f, "{s:.2}")?;
            }
        }
        if samples.is_empty() {
            bail!("no sample completed ({lost} timed out)");
        }
        let mut sorted = samples.clone();
        sorted.sort_by(f64::total_cmp);
        let pct = |p: f64| sorted[((sorted.len() - 1) as f64 * p).round() as usize];
        println!(
            "samples {} (lost {lost}): min {:.1} ms, median {:.1} ms, p95 {:.1} ms, max {:.1} ms",
            sorted.len(),
            sorted[0],
            pct(0.5),
            pct(0.95),
            sorted[sorted.len() - 1]
        );
        Ok(())
    }
}
