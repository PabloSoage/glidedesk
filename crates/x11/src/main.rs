//! glidedesk-x11: a minimal client for any X11 display, made for Termux with termux-x11
//! (where Rust targets Android and windowing libraries take the Android path), and usable
//! on Linux machines without a GPU.
//!
//! Native protocol (or `--rdp`), OpenH264 decoding, YUV → BGRX on the CPU and `PutImage`;
//! X11 keyboard and mouse events go out as scan codes and buttons; audio is piped to
//! `pacat` (PulseAudio, as Termux has) when it is installed.

#[cfg(unix)]
mod app;

#[cfg(unix)]
fn main() -> anyhow::Result<()> {
    app::main()
}

#[cfg(not(unix))]
fn main() {
    eprintln!("glidedesk-x11 needs an X11 display (Linux, Termux); use `glidedesk` instead");
}
