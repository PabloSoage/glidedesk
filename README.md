# glidedesk

A remote desktop for Linux that aims to feel like Windows RDP, or better:

- **The whole session is GPU-rendered**: each user gets a headless GNOME Shell (Wayland)
  composited on the GPU, with no physical monitor, dummy plug or virtual display driver.
- **The desktop follows the client window**: resize the window and the remote resolution
  changes, as with `mstsc`.
- **Hardware video end to end**: NVENC H.264 (and HEVC for the low-latency client),
  low-latency encoder settings, no B-frames, frames encoded as soon as the compositor
  produces them.
- **Standard clients**: the RDP front end speaks the graphics pipeline extension (H.264
  AVC420/AVC444), so the Windows Remote Desktop client (`mstsc`), Microsoft's Windows App on
  macOS/iOS/Android and FreeRDP/Remmina work without installing anything. A
  Moonlight-compatible front end for the lowest latency (HEVC, high refresh rates) is
  planned.

**Status: early development.** What works today, on Ubuntu 24.04 with GNOME 46 and an NVIDIA
GPU:

| Piece | State |
|---|---|
| Headless GNOME Shell rendered on the GPU (GBM) | works |
| Virtual monitor capture over PipeWire (Mutter `RecordVirtual`) | works |
| Live resize of the virtual monitor | works |
| NVENC H.264 / HEVC encoding | works |
| RDP front end (IronRDP, NLA, EGFX AVC420 and AVC444v2, Display Control, RDP-UDP) | works with `mstsc` and FreeRDP |
| Client-side cursor (shape from Mutter, drawn by the client) | works |
| Zero-copy capture: DMA-BUF → GL → CUDA → NVENC, colour conversion in CUDA | works |
| Keyboard and mouse through Mutter RemoteDesktop | works |
| HEVC / AV1 low-latency client (Moonlight protocol), adaptive per-region quality | planned |

## Try the pieces

On the machine with the GPU (Ubuntu 24.04, GNOME 46, NVIDIA driver 570+):

```sh
sudo apt install libpipewire-0.3-dev libspa-0.2-dev libclang-dev clang libegl-dev libgbm-dev
cargo build --release
# A private headless GNOME Shell on its own D-Bus session, plus a capture + encode test:
scripts/dev-session.sh -- target/release/glidedesk-host capture-test
scripts/dev-session.sh -- target/release/glidedesk-host encode-test --hevc
# RDP server in the background (log in $XDG_RUNTIME_DIR/glidedesk-serve.log):
GLIDEDESK_PASSWORD=... scripts/dev-serve.sh
```

Then connect with `mstsc /v:<host>` as your Linux user name and that password. The
certificate is self-signed (created in `~/.config/glidedesk/`).

`dev-session.sh` starts its own session bus, so it does not interfere with a desktop that is
already running.

## Design

```
client (mstsc / Windows App / FreeRDP / Moonlight)
   |  RDP: TLS, EGFX H.264, Display Control, UDP transport
   v
glidedesk-host (one process per session)
   +- RDP front end (IronRDP)
   +- encode: NVENC, colour conversion in CUDA
   +- capture: Mutter ScreenCast RecordVirtual -> PipeWire -> CUDA
   +- input: Mutter RemoteDesktop
   +- session: gnome-shell --headless (Wayland, GPU rendering through GBM)
```

## License

GPL-3.0-or-later. See [THIRD_PARTY.md](THIRD_PARTY.md) for bundled and derived code.
