# Emersia

**Free & open-source VR workspace for Meta Quest — unlimited full-resolution
virtual monitors, first-class on Linux Wayland.**

Emersia streams your desktop into a Quest headset as as many virtual monitors
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

Early development (milestone **M0 — Foundation**). The roadmap lives in the
[Emersia project board](https://github.com/users/nicklin11/projects).

### Planned milestones

- **M0** Foundation — repo, CI, ADRs, project board + workflow automation
- **M1** Linux host engine (Rust) — Wayland capture → VAAPI/NVENC encode →
  RTP/UDP stream → `uinput` input injection, niri/sway/hyprland first
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
host/     Rust workspace: emersia-daemon (streaming engine) + emersia (CLI)
client/   Godot 4 OpenXR Quest client        (from M2)
gui/      Tauri companion app                (from M4)
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
