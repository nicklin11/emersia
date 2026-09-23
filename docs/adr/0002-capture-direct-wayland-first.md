# ADR 0002: Direct Wayland capture first, portal fallback, uinput for input

- Status: accepted
- Date: 2026-09-23

## Context

The host must capture screen content and inject keyboard/pointer input across
Wayland compositors — niri (primary target), sway, hyprland — plus GNOME/KDE,
X11, and eventually Windows/macOS.

Verified on the target compositor (niri 26.04, its wiki "Screencasting" page):

1. Portal + PipeWire screencast is the primary documented path (requires
   `xdg-desktop-portal-gnome`, a full niri *session*, D-Bus, PipeWire).
2. niri also implements the direct protocols `ext-image-copy-capture` and the
   older `wlr-screencopy`.
3. sway and hyprland implement `wlr-screencopy` (hyprland also its own
   screencopy flavour); hyprland protocol headers are present on the dev box.

Portal capture works everywhere but has real costs: permission dialogs,
another desktop-DE dependency, PipeWire graph overhead, and extra copies —
all painful for a low-latency VR streaming loop.

Input is harder: `wlr-virtual-pointer` / `virtual-keyboard` are not universal
(niri does not provide them), so compositor-specific virtual input cannot be
the primary mechanism.

## Decision

- **Capture order:** direct Wayland protocols first
  (`ext-image-copy-capture` → `wlr-screencopy`), **portal/PipeWire as
  automatic fallback**, X11 (XCB/PipeWire) last.
- **Input:** kernel-level **`uinput`** virtual keyboard+pointer as the primary
  injection mechanism — compositor-agnostic (niri, sway, hyprland, GNOME, KDE),
  zero protocol dependency. Wayland virtual-input protocols may be used as an
  optimization later where available.
- The core engine (daemon) must have **no desktop-portal/DE hard dependency**:
  it runs standalone in any Wayland compositor session; the portal path is
  optional degradation for locked-down desktops.

## Consequences

- Capture works natively in niri/sway/hyprland with dmabuf zero-copy paths
  and no permission-dialog round trip on the hot path (portal dialog appears
  only when the fallback is engaged).
- `uinput` requires write access to `/dev/uinput` (group membership or a udev
  rule) — documented setup step in M1.
- Windows/macOS hosts implement their own capture/input backends behind the
  same engine interface (DXGI/SendInput, ScreenCaptureKit/CGEvent) — M6/M7.
- Blocking-out / privacy features of compositors (e.g. niri
  `block-out-from "screencast"`) are honored by the portal path but not by
  direct `wlr-screencopy` — a known trade-off, mitigated by the explicit
  "you are sharing" indicator (M4).
