use std::fmt::{self, Display};

pub mod error;
pub mod scancode;

#[cfg(all(unix, feature = "libei", not(target_os = "macos")))]
mod libei;

// FIXME
pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;
pub const BTN_BACK: u32 = 0x113;
pub const BTN_FORWARD: u32 = 0x114;

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum PointerEvent {
    /// relative motion event
    Motion { time: u32, dx: f64, dy: f64 },
    /// mouse button event
    Button { time: u32, button: u32, state: u32 },
    /// axis event, scroll event for touchpads
    Axis { time: u32, axis: u8, value: f64 },
    /// discrete axis event, scroll event for mice - 120 = one scroll tick
    AxisDiscrete120 { axis: u8, value: i32 },
}

/// upper bound of the normalized touchpad position axes.
///
/// Touchpads differ wildly in native resolution, so [`TouchpadEvent::Frame`]
/// carries positions rescaled into `0..=TOUCHPAD_AXIS_MAX` instead of raw
/// device units. The receiver rebuilds a device over the same range and
/// derives a resolution from the physical size in [`TouchpadEvent::Device`],
/// which is what libinput needs: its gesture thresholds are expressed in mm.
pub const TOUCHPAD_AXIS_MAX: i32 = 0xffff;

/// Touchpad events, forwarded as raw evdev frames.
///
/// Gestures are deliberately *not* interpreted on the capturing machine.
/// Multi-finger state is replayed onto a virtual multitouch device on the
/// receiving machine so that its own libinput performs gesture recognition -
/// pinch, swipe, tap and kinetic scrolling then behave exactly as they would
/// for a directly attached touchpad, including compositor-level gestures that
/// are never forwarded to Wayland clients.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum TouchpadEvent {
    /// Description of the captured touchpad.
    ///
    /// Sent once each time capture begins, before any [`TouchpadEvent::Frame`],
    /// so the receiver can create a matching virtual device.
    Device {
        width_mm: u16,
        height_mm: u16,
        slots: u8,
        pressure_max: i32,
    },
    /// A single evdev event belonging to the frame currently being built.
    ///
    /// `kind` and `code` are raw evdev type/code values. As on the kernel
    /// interface, an `EV_SYN` / `SYN_REPORT` event terminates a frame and
    /// makes it visible to the receiving compositor.
    Frame {
        time: u32,
        kind: u16,
        code: u16,
        value: i32,
    },
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum KeyboardEvent {
    /// a key press / release event
    Key { time: u32, key: u32, state: u8 },
    /// modifiers changed state
    Modifiers {
        depressed: u32,
        latched: u32,
        locked: u32,
        group: u32,
    },
}

#[derive(PartialEq, Debug, Clone, Copy)]
pub enum Event {
    /// pointer event (motion / button / axis)
    Pointer(PointerEvent),
    /// keyboard events (key / modifiers)
    Keyboard(KeyboardEvent),
    /// raw touchpad frames, replayed onto a virtual multitouch device
    Touchpad(TouchpadEvent),
}

impl Display for PointerEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PointerEvent::Motion { time: _, dx, dy } => write!(f, "motion({dx},{dy})"),
            PointerEvent::Button {
                time: _,
                button,
                state,
            } => {
                let str = match *button {
                    BTN_LEFT => Some("left"),
                    BTN_RIGHT => Some("right"),
                    BTN_MIDDLE => Some("middle"),
                    BTN_FORWARD => Some("forward"),
                    BTN_BACK => Some("back"),
                    _ => None,
                };
                if let Some(button) = str {
                    write!(f, "button({button}, {state})")
                } else {
                    write!(f, "button({button}, {state}")
                }
            }
            PointerEvent::Axis {
                time: _,
                axis,
                value,
            } => write!(f, "scroll({axis}, {value})"),
            PointerEvent::AxisDiscrete120 { axis, value } => {
                write!(f, "scroll-120 ({axis}, {value})")
            }
        }
    }
}

impl Display for KeyboardEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyboardEvent::Key {
                time: _,
                key,
                state,
            } => {
                let scan = scancode::Linux::try_from(*key);
                if let Ok(scan) = scan {
                    write!(f, "key({scan:?}, {state})")
                } else {
                    write!(f, "key({key}, {state})")
                }
            }
            KeyboardEvent::Modifiers {
                depressed: mods_depressed,
                latched: mods_latched,
                locked: mods_locked,
                group,
            } => write!(
                f,
                "modifiers({mods_depressed},{mods_latched},{mods_locked},{group})"
            ),
        }
    }
}

impl Display for TouchpadEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TouchpadEvent::Device {
                width_mm,
                height_mm,
                slots,
                pressure_max,
            } => write!(
                f,
                "touchpad({width_mm}x{height_mm}mm, {slots} slots, pressure {pressure_max})"
            ),
            TouchpadEvent::Frame {
                time: _,
                kind,
                code,
                value,
            } => write!(f, "touchpad-frame({kind:#x}, {code:#x}, {value})"),
        }
    }
}

impl Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Event::Pointer(p) => write!(f, "{p}"),
            Event::Keyboard(k) => write!(f, "{k}"),
            Event::Touchpad(t) => write!(f, "{t}"),
        }
    }
}
