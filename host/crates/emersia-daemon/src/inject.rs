//! Input injection from a paired headset through `/dev/uinput`.
//!
//! Two virtual devices, created on the first input event (so nothing appears in
//! the compositor's device list until a headset actually sends input):
//!
//! - **pointer**: absolute `ABS_X/ABS_Y` + left/right/middle buttons + wheel.
//!   libinput classifies it like a VM tablet and reports absolute motion, which
//!   niri, sway and hyprland map across the *global* layout — verified on niri
//!   26.04, where x=0.25 of a two-monitor layout lands on the left output. The
//!   headset's coordinates are relative to the streamed output, so they are
//!   mapped through `xdg-output` geometry first ([`map_pointer`]).
//! - **keyboard**: evdev key codes 1..=248.
//!
//! Anything held down is released when the last device disconnects, so a
//! dropped Wi-Fi link cannot leave a key or button stuck.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::OpenOptionsExt as _;

use crate::capture::Rect;
use crate::input::{Button, InputEvent};

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const EV_ABS: u16 = 3;
const SYN_REPORT: u16 = 0;
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;
const REL_HWHEEL: u16 = 6;
const REL_WHEEL: u16 = 8;
const REL_WHEEL_HI_RES: u16 = 11;
const REL_HWHEEL_HI_RES: u16 = 12;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
/// Keyboard keys exposed: everything up to `KEY_MICMUTE`.
const KEYBOARD_MAX: u16 = 248;
/// Absolute axis range.
pub const ABS_MAX: i32 = 65_535;
/// Hi-res units per wheel detent.
const DETENT: i32 = 120;

// ioctl numbers from <linux/uinput.h> (_IOW('U', n, T) / _IO('U', n)).
const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
const UI_SET_RELBIT: libc::c_ulong = 0x4004_5566;
const UI_SET_ABSBIT: libc::c_ulong = 0x4004_5567;
const UI_DEV_SETUP: libc::c_ulong = 0x405c_5503;
const UI_ABS_SETUP: libc::c_ulong = 0x401c_5504;
const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
const BUS_VIRTUAL: u16 = 0x06;

#[repr(C)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
struct AbsInfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

#[repr(C)]
struct UinputAbsSetup {
    code: u16,
    absinfo: AbsInfo,
}

/// Map a point on the streamed output (0..=65535 per axis) to the absolute
/// device range over the global layout `bounds`.
pub fn map_pointer(x: u16, y: u16, output: Rect, bounds: Rect) -> (i32, i32) {
    let axis = |n: u16, start: i32, len: i32, b_start: i32, b_len: i32| -> i32 {
        if b_len <= 0 {
            return 0;
        }
        // Position in logical pixels, then normalise over the layout.
        let px = f64::from(start) + f64::from(n) / f64::from(ABS_MAX) * f64::from(len.max(0));
        let v = (px - f64::from(b_start)) / f64::from(b_len) * f64::from(ABS_MAX);
        (v.round() as i32).clamp(0, ABS_MAX)
    };
    (
        axis(x, output.x, output.width, bounds.x, bounds.width),
        axis(y, output.y, output.height, bounds.y, bounds.height),
    )
}

/// One uinput device.
struct Device {
    file: File,
}

impl Device {
    fn ioctl(&self, request: libc::c_ulong, arg: libc::c_ulong) -> std::io::Result<()> {
        // SAFETY: valid fd; each request's argument is either an int or a
        // pointer to a correctly sized, live #[repr(C)] struct (see callers).
        let r = unsafe { libc::ioctl(self.file.as_raw_fd(), request as _, arg) };
        if r < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn create(
        name: &str,
        configure: impl FnOnce(&Device) -> std::io::Result<()>,
    ) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open("/dev/uinput")?;
        let dev = Device { file };
        configure(&dev)?;
        let mut setup = UinputSetup {
            id: InputId {
                bustype: BUS_VIRTUAL,
                vendor: 0x1d6b, // Linux Foundation: virtual device
                product: 0xe3e5,
                version: 1,
            },
            name: [0; 80],
            ff_effects_max: 0,
        };
        let bytes = name.as_bytes();
        setup.name[..bytes.len().min(79)].copy_from_slice(&bytes[..bytes.len().min(79)]);
        dev.ioctl(UI_DEV_SETUP, &setup as *const _ as libc::c_ulong)?;
        dev.ioctl(UI_DEV_CREATE, 0)?;
        Ok(dev)
    }

    fn emit(&mut self, events: &[(u16, u16, i32)]) -> std::io::Result<()> {
        let mut buf = Vec::with_capacity((events.len() + 1) * 24);
        for &(ty, code, value) in events.iter().chain([(EV_SYN, SYN_REPORT, 0)].iter()) {
            // struct input_event: timeval (kernel fills it), type, code, value.
            buf.extend_from_slice(&[0u8; std::mem::size_of::<libc::timeval>()]);
            buf.extend_from_slice(&ty.to_ne_bytes());
            buf.extend_from_slice(&code.to_ne_bytes());
            buf.extend_from_slice(&value.to_ne_bytes());
        }
        self.file.write_all(&buf)
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        let _ = self.ioctl(UI_DEV_DESTROY, 0);
    }
}

fn button_code(button: Button) -> u16 {
    match button {
        Button::Left => BTN_LEFT,
        Button::Right => BTN_RIGHT,
        Button::Middle => BTN_MIDDLE,
    }
}

/// Owns the virtual devices and what is currently held down.
#[derive(Default)]
pub struct Injector {
    pointer: Option<Device>,
    keyboard: Option<Device>,
    held: BTreeSet<u16>,
    /// Hi-res scroll not yet worth a legacy detent, per axis.
    wheel_rest: (i32, i32),
    failed: bool,
}

impl Injector {
    fn ensure_devices(&mut self) -> std::io::Result<()> {
        if self.pointer.is_none() {
            self.pointer = Some(Device::create("Emersia headset pointer", |d| {
                d.ioctl(UI_SET_EVBIT, EV_KEY.into())?;
                for b in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE] {
                    d.ioctl(UI_SET_KEYBIT, b.into())?;
                }
                d.ioctl(UI_SET_EVBIT, EV_REL.into())?;
                for r in [REL_WHEEL, REL_HWHEEL, REL_WHEEL_HI_RES, REL_HWHEEL_HI_RES] {
                    d.ioctl(UI_SET_RELBIT, r.into())?;
                }
                d.ioctl(UI_SET_EVBIT, EV_ABS.into())?;
                for axis in [ABS_X, ABS_Y] {
                    d.ioctl(UI_SET_ABSBIT, axis.into())?;
                    let setup = UinputAbsSetup {
                        code: axis,
                        absinfo: AbsInfo {
                            value: 0,
                            minimum: 0,
                            maximum: ABS_MAX,
                            fuzz: 0,
                            flat: 0,
                            resolution: 0,
                        },
                    };
                    d.ioctl(UI_ABS_SETUP, &setup as *const _ as libc::c_ulong)?;
                }
                Ok(())
            })?);
        }
        if self.keyboard.is_none() {
            self.keyboard = Some(Device::create("Emersia headset keyboard", |d| {
                d.ioctl(UI_SET_EVBIT, EV_KEY.into())?;
                for k in 1..=KEYBOARD_MAX {
                    d.ioctl(UI_SET_KEYBIT, k.into())?;
                }
                Ok(())
            })?);
            // libinput needs a moment to open a new device before its first
            // event, or that event is lost.
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
        Ok(())
    }

    /// Inject a batch. `layout` is `(streamed output, global bounds)`; pointer
    /// motion is dropped without it.
    pub fn apply(&mut self, events: &[InputEvent], layout: Option<(Rect, Rect)>) {
        if self.failed {
            return;
        }
        if let Err(e) = self.ensure_devices() {
            eprintln!("emersia-daemon: input disabled, cannot use /dev/uinput: {e}");
            eprintln!("  (add your user to the `input` group, or pass --no-input)");
            self.failed = true;
            return;
        }
        let mut pointer = Vec::new();
        let mut keys = Vec::new();
        for event in events {
            match *event {
                InputEvent::Pointer { x, y } => {
                    if let Some((output, bounds)) = layout {
                        let (ax, ay) = map_pointer(x, y, output, bounds);
                        pointer.push((EV_ABS, ABS_X, ax));
                        pointer.push((EV_ABS, ABS_Y, ay));
                    }
                }
                InputEvent::Button { button, down } => {
                    let code = button_code(button);
                    self.track(code, down);
                    pointer.push((EV_KEY, code, i32::from(down)));
                }
                InputEvent::Scroll { dx, dy } => {
                    if dy != 0 {
                        pointer.push((EV_REL, REL_WHEEL_HI_RES, -i32::from(dy)));
                        self.wheel_rest.1 += -i32::from(dy);
                        let detents = self.wheel_rest.1 / DETENT;
                        if detents != 0 {
                            self.wheel_rest.1 -= detents * DETENT;
                            pointer.push((EV_REL, REL_WHEEL, detents));
                        }
                    }
                    if dx != 0 {
                        pointer.push((EV_REL, REL_HWHEEL_HI_RES, i32::from(dx)));
                        self.wheel_rest.0 += i32::from(dx);
                        let detents = self.wheel_rest.0 / DETENT;
                        if detents != 0 {
                            self.wheel_rest.0 -= detents * DETENT;
                            pointer.push((EV_REL, REL_HWHEEL, detents));
                        }
                    }
                }
                InputEvent::Key { code, down } => {
                    if code <= KEYBOARD_MAX {
                        self.track(code, down);
                        keys.push((EV_KEY, code, i32::from(down)));
                    }
                }
            }
        }
        if !pointer.is_empty() {
            if let Some(dev) = self.pointer.as_mut() {
                if let Err(e) = dev.emit(&pointer) {
                    eprintln!("emersia-daemon: pointer injection failed: {e}");
                }
            }
        }
        if !keys.is_empty() {
            if let Some(dev) = self.keyboard.as_mut() {
                if let Err(e) = dev.emit(&keys) {
                    eprintln!("emersia-daemon: key injection failed: {e}");
                }
            }
        }
    }

    fn track(&mut self, code: u16, down: bool) {
        if down {
            self.held.insert(code);
        } else {
            self.held.remove(&code);
        }
    }

    /// Release everything held down (device disconnected).
    pub fn release_all(&mut self) {
        if self.held.is_empty() {
            return;
        }
        let held = std::mem::take(&mut self.held);
        let (buttons, keys): (Vec<u16>, Vec<u16>) = held
            .into_iter()
            .partition(|c| (BTN_LEFT..=BTN_MIDDLE).contains(c));
        let up = |codes: Vec<u16>| -> Vec<(u16, u16, i32)> {
            codes.into_iter().map(|c| (EV_KEY, c, 0)).collect()
        };
        if let (Some(dev), false) = (self.pointer.as_mut(), buttons.is_empty()) {
            let _ = dev.emit(&up(buttons));
        }
        if let (Some(dev), false) = (self.keyboard.as_mut(), keys.is_empty()) {
            let _ = dev.emit(&up(keys));
        }
        eprintln!("emersia-daemon: released input held by a disconnected device");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEFT: Rect = Rect {
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
    };
    const RIGHT: Rect = Rect {
        x: 1920,
        y: 0,
        width: 1920,
        height: 1080,
    };

    #[test]
    fn maps_streamed_output_into_global_layout() {
        let bounds = LEFT.union(RIGHT);
        // Centre of the right-hand output is 3/4 across the layout.
        assert_eq!(map_pointer(32768, 32768, RIGHT, bounds), (49152, 32768));
        assert_eq!(map_pointer(0, 0, RIGHT, bounds), (32768, 0));
        assert_eq!(map_pointer(65535, 65535, RIGHT, bounds), (65535, 65535));
        assert_eq!(map_pointer(65535, 0, LEFT, bounds), (32768, 0));
    }

    #[test]
    fn single_output_is_identity() {
        assert_eq!(map_pointer(1234, 54321, LEFT, LEFT), (1234, 54321));
    }

    #[test]
    fn degenerate_layout_does_not_panic() {
        let empty = Rect::default();
        assert_eq!(map_pointer(100, 100, empty, empty), (0, 0));
    }

    #[test]
    fn ioctl_struct_sizes_match_the_kernel_abi() {
        assert_eq!(std::mem::size_of::<UinputSetup>(), 92);
        assert_eq!(std::mem::size_of::<UinputAbsSetup>(), 28);
    }
}
