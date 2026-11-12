# Emersia

**Free & open-source VR workspace for Meta Quest — unlimited full-resolution
virtual monitors, first-class on Linux Wayland.**

Emersia streams your desktop into a Quest headset with as many virtual monitors
as you want, at your monitor's full resolution, with keyboard and mouse
control — with **no subscription, no monitor caps, no telemetry**. It exists to
make paid VR-desktop apps obsolete.

| | Immersed Free | Immersed Pro | **Emersia** |
|---|---|---|---|
| Price | Free | ≥ $5.99/mo | **Free forever (GPL-3.0)** |
| Virtual monitors | 3, capped 1440×900 | up to 5 | **unlimited, full res** |
| Linux Wayland (niri/sway/hyprland) | limited / X11-era | limited | **first-class** |
| Source available | no | no | **yes** |

## Status

**M0 is complete; M1 (Linux host) is functionally complete; M2 (Quest client)
is underway — screen mirroring to a Quest works today.** Live work is tracked on
the [project board](https://github.com/users/nicklin11/projects/4).

- Host: direct Wayland capture (`ext-image-copy-capture`, `wlr-screencopy`
  fallback), GPU colour conversion + hardware H.264/HEVC (VAAPI/NVENC/AMF/QSV),
  forward-secret encrypted UDP, desktop audio, cursor compositing,
  pairing/revocation, headset input via `uinput`, presence announcements so a
  headset follows the PC to a new IP, systemd user service. Verified on niri and (headless, in CI) sway.
- Quest client: Rust core with NDK MediaCodec hardware decode into a Horizon
  OS panel, keyframe recovery, pointer/keyboard input, on-headset latency
  readout. Measured ≈ 16 ms capture → decoded at 60 fps
  ([ADR 0007](docs/adr/0007-quest-client-native-core.md)).
- Several monitors at once: `emersia start DP-1 HDMI-A-1` shows each output in
  its own Horizon OS panel. On sway/hyprland, `emersia virtual add 2560x1440`
  creates a headset-only monitor (niri: see
  [virtual outputs](docs/design/virtual-outputs.md) for the EDID / dummy-plug
  route).
- Next: the immersive OpenXR view (curved screens placed in 3D), zero-copy
  capture.

### Companion app

`cd gui && cargo run --release` — a tray icon that turns red whenever your
screen is shared (tooltip: what, with how many headsets), plus a window with
live status, Start/Stop, paired headsets with Revoke, and in-headset pairing
with the 6-digit code. Closing the window keeps the tray indicator.

### Try it: mirror your screen to a Quest

Install the host (`scripts/install.sh`), open Emersia on the headset, and on
the PC run:

```sh
emersia pair headset    # compare the 6-digit code with the headset, confirm on both
```

The headset then mirrors your desktop. For development over USB instead:

```sh
scripts/quest-mirror.sh --output DP-1     # builds, installs, pairs, streams
```

The desktop appears as a floating panel. Point at it with a controller or
hand and pinch/trigger to click, use the thumbstick to scroll; a Bluetooth
keyboard paired to the headset types into the host. The daemon injects this
through `/dev/uinput` (your user needs the `input` group; `--no-input` turns it
off). Desktop audio plays in the headset too (`--no-audio` turns it off), and
the mouse pointer is drawn into the stream (`--no-cursor`). With a GPU that
encodes HEVC, `--codec hevc` gives sharper text at the same bitrate; the Quest
decodes it in hardware. Needs the Android SDK/NDK and a Rust
toolchain with `aarch64-linux-android` (see `client/android/build.sh`). If a
host firewall is active, allow the stream port (UDP 47800 by default); the
script prints the exact `ufw` command when it detects one.

### Planned milestones

- **M0** Foundation — complete: repo, CI, ADRs, project board + workflow automation
- **M1** Linux host engine (Rust) — in progress: Wayland capture → runtime
  encoder → RTP/UDP stream → `uinput` input injection, niri/sway/hyprland first
- **M2** Quest client MVP (Godot 4 + OpenXR) — hardware-decoded, crisp
  virtual monitor, controller-driven input
- **M3** Virtual workspace — up to 5+ monitors, layouts, audio, full keyboard
- **M4** Companion desktop app (Tauri) — pairing, consent & status UI
- **M5** Wayland breadth & hardware matrix (GNOME/KDE portals, X11, NVIDIA…)
- **M6** Windows host · **M7** macOS host
- **M8** Polish & release · **M9** Collaboration (stretch)

## Platform support (target)

| Host OS | Status |
|---|---|
| Linux — niri, sway, hyprland (direct Wayland capture) | 🎯 M1 (first) |
| Linux — GNOME/KDE (portal fallback), X11 | M5 |
| Windows 10/11 | M6 |
| macOS | M7 |

Headset: Meta Quest 2 / 3 / 3S / Pro (OpenXR).

## Repository layout

```
host/     Rust workspace: emersia-daemon (streaming engine), emersia (CLI),
          emersia-transport (wire protocol shared with the client), emersia-protocol
client/   Quest client: Rust core (decode/transport) + Android panel shell (M2)
gui/      Tauri companion app: tray sharing indicator, status, devices, pairing
docs/     ADRs and design documents
scripts/  development & validation tooling
```

## Development

Trunk-based development: no direct commits to `main`. Work happens via
feature branches → Conventional Commits → CI → pull request → squash merge.
See [CONTRIBUTING.md](CONTRIBUTING.md).

Architecture decisions live in [`docs/adr/`](docs/adr/) — numbered once,
never rewritten in place.

## License

[GPL-3.0-or-later](LICENSE).
