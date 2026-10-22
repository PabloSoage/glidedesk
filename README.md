# glidedesk

<img src="assets/logo/glidedesk-512.png" alt="" width="96" align="right">

A remote desktop for Linux that aims to feel like Windows RDP, or better:

- **The whole session is GPU-rendered**: each user gets a headless GNOME Shell (Wayland)
  composited on the GPU, with no physical monitor, dummy plug or virtual display driver.
- **The desktop follows the client window**: resize the window and the remote resolution
  changes.
- **Hardware video end to end**: NVENC on NVIDIA, VA-API on AMD and Intel (x264 on the CPU
  when there is no usable GPU), low-latency settings, frames encoded as soon as the
  compositor produces them, up to the client's refresh rate (144 Hz and more).
- **Two ways in, at the same time**:
  - **Standard RDP** (port 3389): the Windows Remote Desktop client (`mstsc`), Microsoft's
    Windows App (macOS, iOS, Android) and FreeRDP/Remmina work without installing anything.
  - **glidedesk's own low-latency protocol** (port 3391): video over UDP with Reed-Solomon
    FEC (a lost packet never stalls the picture), HEVC or H.264, everything else over TLS.
    Used by the glidedesk clients for Windows, Linux, Android and Termux.
- **No fixed bandwidth**: frames are encoded at a constant quality, so a still desktop costs
  almost nothing; only the ceiling a frame may reach follows the link, from the measured
  delay (it drops as soon as frames queue, and climbs back while the link is clean).

**Status: early development (v0.1).** Tested on Ubuntu 24.04 (GNOME 46) with NVIDIA GPUs.

## Which client?

| | `mstsc` / Windows App / FreeRDP (RDP) | glidedesk clients (native protocol) |
|---|---|---|
| Install | nothing (built into Windows) | one program (Windows, Linux, Android, Termux) |
| Transport | TCP (TLS) | video over UDP with FEC, the rest over TLS |
| On a lossy link (Wi-Fi, mobile) | a lost packet stalls everything until it is resent | the picture keeps going |
| Codec | H.264 4:2:0 | HEVC (where the client decodes it in hardware) or H.264 |
| Refresh rate | the server's (`--refresh`, default 120) | the client's screen (60, 120, 144 Hz...) |
| Desktop follows the window | FreeRDP: yes. `mstsc`: not yet | yes |
| Sound | yes | yes (desktop and Termux clients) |
| Clipboard (text) | yes | yes (desktop client) |
| Pointer | drawn by the client | drawn by the client |

Use RDP when you cannot install anything; use the glidedesk client for the lowest latency,
especially over Wi-Fi or the Internet.

## Download

Prebuilt binaries are on the [releases page](https://github.com/PabloSoage/glidedesk/releases):

| File | What |
|---|---|
| `glidedesk-host-linux-x86_64` | the server (Ubuntu 24.04 / GNOME 46, x86-64) |
| `glidedesk-session.sh` | starts the headless GNOME session the server runs in |
| `glidedesk-windows-x86_64.exe` | desktop client for Windows |
| `glidedesk-linux-x86_64` | desktop client for Linux (Wayland or X11) |
| `glidedesk-android.apk` | Android client (RDP or native protocol) |
| `glidedesk-x11-termux-aarch64` | client for Termux with termux-x11 (Android, arm64) |
| `glidedesk-x11-linux-x86_64` | the same small X11 client for Linux machines without a GPU |

## Server

Requirements: Ubuntu 24.04 with GNOME 46 (other distributions with GNOME 46+ should work), and
for hardware encoding the NVIDIA driver (570 or later) or Mesa / Intel media VA-API drivers.

```sh
sudo apt install gnome-shell pipewire wireplumber dbus-daemon ffmpeg
chmod +x glidedesk-host-linux-x86_64 glidedesk-session.sh
# Your Linux user name and a password for remote clients (not your login password,
# unless you want it to be):
export GLIDEDESK_PASSWORD='choose-a-password'
./glidedesk-session.sh -- ./glidedesk-host-linux-x86_64 serve
```

`glidedesk-session.sh` starts a private headless GNOME Shell on its own D-Bus session (it does
not touch a desktop session you may already have) and runs the server next to it. Clients log
in with your Linux user name and `GLIDEDESK_PASSWORD`.

Open the ports if a firewall is active: **3389/tcp** (RDP) and **3391/tcp + 3391/udp**
(native protocol), e.g. `sudo ufw allow 3389/tcp && sudo ufw allow 3391`.

Useful options of `serve` (`--help` lists them all):

| Option | Default | |
|---|---|---|
| `--encoder auto\|nvenc\|vaapi\|software` | `auto` | NVENC if there is an NVIDIA GPU, else VA-API, else x264 |
| `--max-bitrate BPS` | none | a ceiling for metered or slow links (e.g. `8000000` for mobile data); without it the link decides |
| `--refresh HZ` | 120 | refresh rate for RDP clients (native clients use their own screen's) |
| `--bind`, `--native-bind` | `0.0.0.0:3389`, `0.0.0.0:3391` | addresses; `--no-native` turns the native protocol off |
| `--avc444` | off | full-chroma AVC444 for RDP clients (experimental: `mstsc` shows it blurred) |
| `--x11 :0` | | serve an existing X11 display instead of a GNOME session (see Termux below) |

The TLS certificate is self-signed, created in `~/.config/glidedesk/` on first start.

If the "Stop screen sharing" button in GNOME's top bar is pressed, the server starts a new
session by itself; clients stay connected.

## Clients

### Windows: Remote Desktop (`mstsc`)

`mstsc /v:<server>`, user name = your Linux user, password = `GLIDEDESK_PASSWORD`. Accept the
self-signed certificate the first time. Full screen works; resizing the `mstsc` window does
not change the remote resolution yet (FreeRDP's `/dynamic-resolution` does).

### Windows and Linux: `glidedesk`

```sh
glidedesk <server> --user <user>             # native protocol (UDP), the default
glidedesk <server> --user <user> --rdp       # RDP instead
glidedesk <server> --user <user> --fullscreen --width 1920 --height 1080
```

On Windows it asks for the password with the standard Windows sign-in dialog and can remember
it (Credential Manager); a wrong password is forgotten. A shortcut with those arguments is all
you need. On Linux give it with `--password` or the `GLIDEDESK_PASSWORD` variable. The window
menu (right click on the title bar) has **Full screen** and **Show log**; the log is in
`%LOCALAPPDATA%\glidedesk\glidedesk.log` (Linux: `~/.local/state/glidedesk/glidedesk.log`).

The remote desktop takes the window's size and follows it when you resize. Decoding is in
software (OpenH264) for now.

### Android

Install `glidedesk-android.apk`, enter the server, user and password, and choose **Low
latency** (native protocol, hardware HEVC/H.264 decoding, the phone's refresh rate) or plain
RDP. Touch moves the pointer; the keyboard button opens the on-screen keyboard.

### Termux (Android) with termux-x11

A small client that draws into an X11 display, for phones and tablets where you already use
[Termux](https://termux.dev) and [termux-x11](https://github.com/termux/termux-x11):

```sh
pkg install x11-repo && pkg install termux-x11-nightly pulseaudio
pulseaudio --start --exit-idle-time=-1
termux-x11 :0 &        # then open the Termux:X11 app
chmod +x glidedesk-x11-termux-aarch64
DISPLAY=:0 GLIDEDESK_PASSWORD='...' ./glidedesk-x11-termux-aarch64 --user <user> <server>
```

It fills the screen (`--width`/`--height` for a window), plays sound through PulseAudio
(`--mute` to turn it off), and takes `--rdp` too. The same program runs on any X11 desktop
(`glidedesk-x11-linux-x86_64`).

## Serving an X11 display (Termux, X11 desktops)

The server can also share an existing X11 display instead of starting a GNOME session: an
XFCE desktop in termux-x11 on a phone, or a Linux desktop running on X.

```sh
# Termux: build it on the device (no prebuilt binary yet)
pkg install rust clang ffmpeg pulseaudio
cargo build --release -p glidedesk-host --no-default-features --features x11
GLIDEDESK_PASSWORD='...' target/release/glidedesk-host serve --x11 :0 --encoder software
```

The display keeps its own size; input goes through XTEST, sound through PulseAudio.

## Building from source

```sh
# Server (Ubuntu 24.04):
sudo apt install libpipewire-0.3-dev libspa-0.2-dev libclang-dev clang libegl-dev libgbm-dev \
  libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libavfilter-dev \
  libavdevice-dev libswresample-dev libva-dev libasound2-dev pkg-config
cargo build --release -p glidedesk-host --features x11
# Desktop client (Windows or Linux):
cargo build --release -p glidedesk-desktop
# Termux client, cross-compiled:
cargo ndk -t arm64-v8a build --release -p glidedesk-x11
# Android app (Gradle; builds the Rust core with cargo-ndk):
cd android && ./gradlew assembleRelease
```

For development, `scripts/dev-serve.sh` restarts the server and its session in the background
(log in `$XDG_RUNTIME_DIR/glidedesk-serve.log`) and `scripts/dev-session.sh` runs any command
next to a private headless GNOME Shell (it is the release's `glidedesk-session.sh`).

## Design

```
clients: mstsc / Windows App / FreeRDP          glidedesk (Windows, Linux, Android, Termux)
              |  RDP (TLS, EGFX H.264)                  |  TLS control + UDP video with FEC
              v                                         v
glidedesk-host: RDP front end (IronRDP)  |  native front end
                encoder: NVENC (colour conversion in CUDA), or VA-API / x264 (FFmpeg)
                rate control: constant quality under a ceiling that follows the link
                capture: Mutter ScreenCast RecordVirtual -> PipeWire -> CUDA (zero copy)
                input, clipboard: Mutter RemoteDesktop
                session: gnome-shell --headless (Wayland, rendered on the GPU through GBM)
```

The last client to connect, on either protocol, gets the session; the previous one is told
why it was disconnected.

## License

GPL-3.0-or-later. See [THIRD_PARTY.md](THIRD_PARTY.md) for bundled and derived code.
