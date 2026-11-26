# Third-party code

| Component | Where | License |
|---|---|---|
| NVIDIA Video Codec SDK header `nvEncodeAPI.h` 13.0, from FFmpeg's [nv-codec-headers](https://github.com/FFmpeg/nv-codec-headers) n13.0.19.0 | `crates/nvenc-sys/include/` | MIT (notice in the file) |
| [IronRDP](https://github.com/Devolutions/IronRDP) | dependency | MIT or Apache-2.0 |
| [pipewire-rs](https://gitlab.freedesktop.org/pipewire/pipewire-rs) | dependency | MIT |
| [zbus](https://github.com/dbus2/zbus) | dependency | MIT |
| [cudarc](https://github.com/coreylowman/cudarc) | dependency | MIT or Apache-2.0 |
| [FFmpeg](https://ffmpeg.org) (libavcodec, libavutil) | host encoders (system library); desktop client decoders, linked statically into the Windows build (vcpkg, decoders only, no GPL parts) | LGPL-2.1-or-later |
| [str0m](https://github.com/algesten/str0m) (WebRTC) | host dependency (web front end) | MIT |
| [opus](https://github.com/SpaceManiac/opus-rs) bindings, libopus | host dependency (web front end sound) | MIT or Apache-2.0; libopus BSD-3-Clause |
| `crates/desktop/src/probe.hevc`: a 256×144 HEVC test frame made with FFmpeg's `testsrc2` | desktop client | generated, no license |

Code adapted from [Sunshine](https://github.com/LizardByte/Sunshine) or
[Moonlight](https://github.com/moonlight-stream) (GPL-3.0) is listed here with the source file
and commit, and marked in the file header.

| Our file | Adapted from | Commit |
|---|---|---|
| `crates/host/src/gamestream/pair.rs` (GameStream pairing) | Sunshine `src/nvhttp.cpp`, `src/crypto.cpp` | `224c6be` |
| `crates/host/src/gamestream/mod.rs` (`/serverinfo`, `/applist`, version strings, codec bits) | Sunshine `src/nvhttp.cpp`, `src/nvhttp.h`; moonlight-common-c `src/Limelight.h` | `224c6be`; `b4ee1ce` |
| `crates/host/src/gamestream/identity.rs` (RSA-2048 self-signed identity) | Sunshine `src/httpcommon.cpp` | `224c6be` |

The native protocol's reference-frame invalidation follows Sunshine `src/nvenc/nvenc_base.cpp` and
moonlight-common-c `src/ControlStream.c` (design, no code copied).
