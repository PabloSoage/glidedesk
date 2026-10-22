# glidedesk

A remote desktop for Linux that aims to feel like Windows RDP, or better:

- **The whole session is GPU-rendered**: each user gets a headless GNOME Shell (Wayland)
  composited on the GPU, with no physical monitor, dummy plug or virtual display driver.
- **The desktop follows the client window**: resize the window and the remote resolution
  changes, as with `mstsc`.
- **Hardware video end to end**: NVENC on NVIDIA, VA-API on AMD and Intel (x264 on the CPU
  when there is no usable GPU), low-latency encoder settings, no B-frames, frames encoded as soon as the compositor
  produces them.
- **Standard clients**: the RDP front end speaks the graphics pipeline extension (H.264
  AVC420/AVC444), so the Windows Remote Desktop client (`mstsc`), Microsoft's Windows App on
  macOS/iOS/Android and FreeRDP/Remmina work without installing anything.
- **A low-latency protocol of its own** for glidedesk's clients: video over UDP with
  Reed-Solomon FEC (no retransmission stalls), HEVC or H.264, everything else over TLS, the
  desktop following the window and the pointer drawn locally.

**Status: early development.** What works today, on Ubuntu 24.04 with GNOME 46 and an NVIDIA
GPU:

| Piece | State |
|---|---|
| Headless GNOME Shell rendered on the GPU (GBM) | works |
| Virtual monitor capture over PipeWire (Mutter `RecordVirtual`) | works |
| Live resize of the virtual monitor | works |
| NVENC H.264 / HEVC encoding | works |
| VA-API encoding (AMD, Intel; through FFmpeg), x264 fallback, `--encoder` | works (tested through Mesa d3d12 in WSL; not yet on AMD/Intel hardware) |
| RDP front end (IronRDP, NLA, EGFX AVC420 and AVC444v2, Display Control, RDP-UDP) | works with `mstsc` and FreeRDP |
| Client-side cursor (shape from Mutter, drawn by the client) | works |
| Zero-copy capture: DMA-BUF → GL → CUDA → NVENC, colour conversion in CUDA | works |
| Keyboard and mouse through Mutter RemoteDesktop | works |
| Clipboard (text, both ways) | works |
| Audio (session sink → rdpsnd, 48 kHz PCM) | works |
| Virtual monitor above 60 Hz (`--refresh`, default 120) | works |
| Native low-latency protocol (TLS control, encrypted UDP video with FEC, port 3391) | works |
| Desktop client `glidedesk` (Windows, Linux: wgpu, OpenH264) | works (tested on Linux) |
| Minimal X11 client `glidedesk-x11` (Termux with termux-x11, X11 without a GPU) | works (tested on Linux) |
| Android client (RDP or native protocol, MediaCodec H.264/HEVC) | works on the emulator |
| Serving an existing X11 display (`serve --x11 :0`: Termux with termux-x11, X11 desktops; DAMAGE capture, XTEST input, PulseAudio) | works (tested on Linux) |
| Adaptive per-region quality, hardware decoding in the desktop client | planned |

## Try the pieces

On the machine with the GPU (Ubuntu 24.04, GNOME 46; NVIDIA driver 570+, or Mesa / Intel media
VA-API drivers):

```sh
sudo apt install libpipewire-0.3-dev libspa-0.2-dev libclang-dev clang libegl-dev libgbm-dev \n  libavcodec-dev libavutil-dev
cargo build --release
# A private headless GNOME Shell on its own D-Bus session, plus a capture + encode test:
scripts/dev-session.sh -- target/release/glidedesk-host capture-test
scripts/dev-session.sh -- target/release/glidedesk-host encode-test --hevc
# RDP server in the background (log in $XDG_RUNTIME_DIR/glidedesk-serve.log):
GLIDEDESK_PASSWORD=... scripts/dev-serve.sh
```

Then connect with `mstsc /v:<host>` as your Linux user name and that password. The
certificate is self-signed (created in `~/.config/glidedesk/`).

## Clients

```sh
# Desktop (Windows or Linux): native protocol, or --rdp
GLIDEDESK_PASSWORD=... cargo run --release -p glidedesk-desktop -- <host> --user <user>
# Termux (Android) with termux-x11: build on the phone (pkg install rust clang) or
# cross-compile, then run it with DISPLAY=:0
cargo ndk -t arm64-v8a build --release -p glidedesk-x11
# Android app: android/ (Gradle; builds the Rust core with cargo-ndk)
```

## Serving an X11 display (Termux, X11 desktops)

```sh
# In Termux: pkg install rust clang ffmpeg pulseaudio termux-x11-nightly, start an X11 desktop
# (e.g. XFCE) on :0, then:
cargo build --release -p glidedesk-host --no-default-features --features x11
GLIDEDESK_PASSWORD=... target/release/glidedesk-host serve --x11 :0 --encoder software
```

The display keeps its own size; input goes through XTEST, sound through PulseAudio.

`dev-session.sh` starts its own session bus, so it does not interfere with a desktop that is
already running.

## Design

```
client (mstsc / Windows App / FreeRDP / Moonlight)
   |  RDP: TLS, EGFX H.264, Display Control, UDP transport
   v
glidedesk-host (one process per session)
   +- RDP front end (IronRDP)
   +- encode: NVENC (colour conversion in CUDA), or VA-API / x264 through FFmpeg (on the CPU)
   +- capture: Mutter ScreenCast RecordVirtual -> PipeWire -> CUDA (NVIDIA) or system memory
   +- input: Mutter RemoteDesktop
   +- session: gnome-shell --headless (Wayland, GPU rendering through GBM)
```

## License

GPL-3.0-or-later. See [THIRD_PARTY.md](THIRD_PARTY.md) for bundled and derived code.
