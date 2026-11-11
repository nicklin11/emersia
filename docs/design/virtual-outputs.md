# Virtual outputs: niri, sway, hyprland

Status: design (M3 input) · Issue: #76

M3 wants monitors that exist only in the headset. A compositor only lays out
windows on outputs it knows about, so a "virtual monitor" has to be an output
the compositor believes in — or we stream something other than an output.

## What each compositor offers

| | Runtime virtual output | How | Capture by Emersia |
|---|---|---|---|
| **hyprland** | yes | `hyprctl output create headless [NAME]`, `hyprctl output remove NAME`; mode/position via `hyprctl keyword monitor NAME,2560x1440@60,auto,1` | normal wlr-screencopy of `NAME` |
| **sway** | yes | `swaymsg create_output` → `HEADLESS-1`; size via `swaymsg output HEADLESS-1 mode 2560x1440@60Hz`; remove with `swaymsg output HEADLESS-1 unplug`. On a DRM session the headless backend must be enabled (`WLR_BACKENDS=drm,libinput,headless`) | normal wlr-screencopy |
| **niri** | **no** (26.04) | — | — |

## niri options

1. **Kernel-forced connector (works today, no niri change).** Light up an
   unused GPU connector with an EDID override, and niri sees a real monitor:
   `drm.edid_firmware=HDMI-A-2:edid/2560x1440.bin video=HDMI-A-2:e` on the
   kernel command line, EDID blob in `/usr/lib/firmware/edid/` (and in the
   initramfs if the GPU driver loads early). Costs: a reboot to change, one
   output per spare connector, and the GPU scans it out to nothing (small
   power cost). Fixed count, but zero code.
2. **Dummy plug / EDID emulator** — same effect in hardware, hot-pluggable,
   a few dollars per monitor.
3. **Window casting instead of outputs.** niri can screencast a single window
   (xdg-desktop-portal-gnome / PipeWire). Each headset screen could be one
   window rather than one output; niri keeps tiling on the real monitor while
   the headset shows windows at their own size. This is a different product
   shape (no "move window to virtual monitor 3") but needs nothing from niri.
4. **Upstream headless outputs in niri.** The clean answer; niri's backend is
   Smithay, which supports headless outputs, so the work is mostly
   configuration/IPC surface. Worth proposing upstream once M3 has a concrete
   API to ask for.
5. **Nested compositor** (sway/cage headless inside niri, streamed by us):
   works, but windows live in a separate session — rejected for daily use.

## Recommendation

- **Now (M2):** mirror real outputs on every compositor — done.
- **M3 on sway/hyprland:** the daemon creates/removes headless outputs over
  IPC on demand (`emersia screens add 2560x1440`), streams each as its own
  session stream, and removes them on disconnect so a crash leaves no ghost
  monitors.
- **M3 on niri:** support option 1/2 out of the box (they are just outputs —
  the daemon needs hot-plug handling only), offer window casting (3) via the
  portal backend that #35 is likely to add anyway, and open the upstream
  discussion (4). Do not block niri users on upstream.

## What the daemon needs regardless

- Output hot-plug: track `wl_output` add/remove and keep streams keyed by
  output name, not index.
- One encoder + stream per output (multiple `send_frame` streams per peer,
  distinguished by a stream id in the frame header — a protocol bump).
- A compositor adapter trait (`create_virtual`, `remove_virtual`) with
  sway/hyprland implementations and a niri implementation that reports
  "unsupported" with a pointer to this document.
