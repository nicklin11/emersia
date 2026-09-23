# ADR 0004: Stack — Rust host, Godot 4 client, Tauri GUI, GPL-3.0

- Status: accepted
- Date: 2026-09-23

## Context

Emersia must be Linux-first (niri/sway/hyprland), later Windows/macOS, with a
Quest client — built mostly by one developer, and legally armored so the
project can never become proprietary (the point is to obsolete a paid app).

## Decision

| Layer | Choice | Notes |
|---|---|---|
| Host engine (`emersia-daemon`) | **Rust** | capture/encode/transport/input pipeline; ALVR proves Rust viable for Quest streaming; toolchain available |
| Control client (`emersia` CLI) | **Rust**, same workspace | MVP control surface over the local socket |
| Quest client | **Godot 4** + OpenXR | Linux-first editor (4.7.x), GPL-compatible, Android export; small plugin for MediaCodec HW decode (M2) |
| Companion desktop app | **Tauri** (M4) | Rust host reuse, system webview, tiny vs Electron, cross-platform |
| Transport | **Custom UDP/RTP-style** | carries ADR 0003 forward; WebRTC deferred to a remote-access milestone |
| Encoders | ffmpeg/libs: **VAAPI, NVENC, AMF, QSV** + H.264/HEVC (AV1 where available) | |
| License | **GPL-3.0-or-later** | copyleft keeps forks free |

### Rejected alternatives

- **Unity client** — mature Quest ecosystem, but proprietary and not
  Linux-first; conflicts with the project's licensing goal.
- **Native C++/Kotlin client** — max latency control, disproportionate cost
  for a solo MVP; revisit only if Godot decode latency proves unacceptable.
- **Electron GUI** — separate JS stack, heavy runtime, poor fit for a
  Rust-first project. **Qt** — Rust bindings (cxx-qt) are the weak link.
- **GStreamer shell-pipeline product** (libworkspaceVR approach) — great for
  experiments, weak as a maintained product core; GStreamer may still be used
  internally where it earns its place.
- **Go host** — GC pauses and weaker low-level media/FFI story for this loop.

## Consequences

- One language (Rust) spans host, CLI and GUI; Godot stays isolated behind the
  wire protocol, so a future native client swap remains possible.
- Android/Godot build support must be provisioned for CI once M2 starts.
- GPL-3.0: contributors' changes stay free; linking GPL host code into
  proprietary stacks is prohibited (intended).
