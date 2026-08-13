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

/// Events carried by one [`TouchpadEvent::Frame`].
///
/// A single contact needs four (slot, tracking id, x, y); five fingers plus
/// the tool and button bits and the terminating `SYN_REPORT` stay inside this
/// bound, and anything longer is split across datagrams.
pub const MAX_FRAME_EVENTS: usize = 24;

/// One raw evdev event: type, code and value exactly as the kernel reports
/// them, with positions rescaled to `0..=TOUCHPAD_AXIS_MAX`.
#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
pub struct TouchpadAxisEvent {
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

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
    /// One complete evdev frame: every event up to and including its
    /// `EV_SYN` / `SYN_REPORT`.
    ///
    /// A frame is the unit that means something - a position without the
    /// slot it belongs to, or without the `SYN_REPORT` that ends it, is not
    /// interpretable. Sending each evdev event in its own datagram made a
    /// touchpad cost roughly eight times the packets of a mouse and turned
    /// any single loss into a corrupted frame rather than a skipped sample.
    /// Keeping a frame in one datagram makes loss degrade the way it does on
    /// the pointer path: one sample vanishes and the stream continues.
    ///
    /// Frames longer than [`MAX_FRAME_EVENTS`] are split; only the part
    /// carrying the `SYN_REPORT` ends the frame, which is exactly how the
    /// kernel interface behaves.
    Frame {
        time: u32,
        count: u8,
        events: [TouchpadAxisEvent; MAX_FRAME_EVENTS],
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
                count,
                events,
            } => {
                write!(f, "touchpad-frame({count} events")?;
                for e in events.iter().take(*count as usize) {
                    write!(f, "; {:#x},{:#x},{}", e.kind, e.code, e.value)?;
                }
                write!(f, ")")
            }
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
