//! Virtual evdev devices shared by the uinput-based emulation paths.
//!
//! The wlroots backend uses them for keyboard, wheel and touchpad input that
//! a compositor must treat as a real device. The uinput backend also routes
//! pointer motion and buttons through them, so it works without any display
//! server protocol (for example on Android).

use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, EventType, InputEvent, KeyCode, PropType,
    RelativeAxisCode, UinputAbsSetup, uinput::VirtualDevice,
};
use input_event::{TOUCHPAD_AXIS_MAX, TouchpadAxisEvent};
use std::collections::HashSet;
use std::io;

/// Every key code, as the wlroots backend has always declared.
#[cfg(wlroots)]
pub(crate) fn all_keys(_: u16) -> bool {
    true
}

/// Keyboard keys and mouse buttons only.
///
/// Android classifies a device by its declared codes. `BTN_MISC` and the
/// joystick, gamepad and digitizer ranges would add a gamepad class to the
/// device, so they are left out.
#[cfg(uinput)]
pub(crate) fn keyboard_and_mouse_keys(code: u16) -> bool {
    !(0x100..=0x10f).contains(&code) && !(0x118..=0x15f).contains(&code)
}

#[cfg(wlroots)]
pub(crate) fn create_uinput_keyboard(
    name: &str,
    declare_key: fn(u16) -> bool,
) -> io::Result<VirtualDevice> {
    let mut keys = AttributeSet::<KeyCode>::new();
    for code in (1..=0x2ff).filter(|code| declare_key(*code)) {
        keys.insert(KeyCode::new(code));
    }

    VirtualDevice::builder()?
        .name(name)
        .with_keys(&keys)?
        .with_relative_axes(&AttributeSet::from_iter([
            RelativeAxisCode::REL_X,
            RelativeAxisCode::REL_Y,
            RelativeAxisCode::REL_WHEEL,
            RelativeAxisCode::REL_HWHEEL,
            // declaring the hi-res axes is what makes libinput treat this as
            // a high-resolution wheel instead of quantizing to whole clicks
            RelativeAxisCode::REL_WHEEL_HI_RES,
            RelativeAxisCode::REL_HWHEEL_HI_RES,
        ]))?
        .build()
}

/// Replays raw touchpad frames onto a virtual multitouch device.
///
/// No gesture interpretation happens here on purpose: the compositor's own
/// libinput sees a normal multitouch touchpad and performs recognition
/// itself, which is what makes remote pinch / 3-finger / 4-finger gestures
/// behave identically to local ones.
pub(crate) struct UInputTouchpad {
    device: Option<VirtualDevice>,
    /// description the current device was built from, so a touchpad with
    /// different geometry replaces it rather than reusing a wrong mapping
    built_from: Option<(u16, u16, u8, i32)>,
    /// slot most recently selected by `ABS_MT_SLOT`
    current_slot: i32,
    /// slots holding a live tracking id, used to lift stuck fingers
    active_slots: HashSet<i32>,
}

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_ABS: u16 = 3;
const SYN_REPORT: u16 = 0;

fn create_uinput_touchpad(
    width_mm: u16,
    height_mm: u16,
    slots: u8,
    pressure_max: i32,
) -> io::Result<VirtualDevice> {
    // libinput expresses gesture thresholds in millimetres, so the
    // resolution has to reflect the *source* touchpad's physical size even
    // though the axis range itself is normalized.
    let res_x = (TOUCHPAD_AXIS_MAX / width_mm.max(1) as i32).max(1);
    let res_y = (TOUCHPAD_AXIS_MAX / height_mm.max(1) as i32).max(1);

    let position = |resolution: i32| AbsInfo::new(0, 0, TOUCHPAD_AXIS_MAX, 0, 0, resolution);

    let mut keys = AttributeSet::<KeyCode>::new();
    keys.insert(KeyCode::BTN_LEFT);
    keys.insert(KeyCode::BTN_TOUCH);
    // the BTN_TOOL_* bits are how libinput learns the finger count
    keys.insert(KeyCode::BTN_TOOL_FINGER);
    keys.insert(KeyCode::BTN_TOOL_DOUBLETAP);
    keys.insert(KeyCode::BTN_TOOL_TRIPLETAP);
    keys.insert(KeyCode::BTN_TOOL_QUADTAP);
    keys.insert(KeyCode::BTN_TOOL_QUINTTAP);

    let mut props = AttributeSet::<PropType>::new();
    props.insert(PropType::POINTER);
    // without BUTTONPAD libinput treats the device as a touchscreen
    props.insert(PropType::BUTTONPAD);

    let mut builder = VirtualDevice::builder()?
        .name("Lan Mouse uinput touchpad")
        .with_keys(&keys)?
        .with_properties(&props)?
        .with_absolute_axis(&UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_X,
            position(res_x),
        ))?
        .with_absolute_axis(&UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_Y,
            position(res_y),
        ))?
        .with_absolute_axis(&UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_MT_SLOT,
            AbsInfo::new(0, 0, slots.max(1) as i32 - 1, 0, 0, 0),
        ))?
        .with_absolute_axis(&UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_MT_POSITION_X,
            position(res_x),
        ))?
        .with_absolute_axis(&UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_MT_POSITION_Y,
            position(res_y),
        ))?
        .with_absolute_axis(&UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_MT_TRACKING_ID,
            AbsInfo::new(0, 0, 0xffff, 0, 0, 0),
        ))?;

    if pressure_max > 0 {
        let pressure = AbsInfo::new(0, 0, pressure_max, 0, 0, 0);
        builder = builder
            .with_absolute_axis(&UinputAbsSetup::new(
                AbsoluteAxisCode::ABS_PRESSURE,
                pressure,
            ))?
            .with_absolute_axis(&UinputAbsSetup::new(
                AbsoluteAxisCode::ABS_MT_PRESSURE,
                pressure,
            ))?;
    }

    builder.build()
}

impl UInputTouchpad {
    pub(crate) fn new() -> Self {
        Self {
            device: None,
            built_from: None,
            current_slot: 0,
            active_slots: HashSet::new(),
        }
    }

    pub(crate) fn describe(
        &mut self,
        width_mm: u16,
        height_mm: u16,
        slots: u8,
        pressure_max: i32,
    ) -> io::Result<()> {
        let description = (width_mm, height_mm, slots, pressure_max);
        if self.built_from == Some(description) && self.device.is_some() {
            // same touchpad as last capture - keep the device so the
            // compositor does not see a hotplug on every crossing
            return Ok(());
        }

        // drop the old device first, otherwise both linger until the
        // replacement is built
        self.device = None;
        self.active_slots.clear();
        self.current_slot = 0;
        self.device = Some(create_uinput_touchpad(
            width_mm,
            height_mm,
            slots,
            pressure_max,
        )?);
        self.built_from = Some(description);
        log::info!("created virtual touchpad ({width_mm}x{height_mm}mm, {slots} slots)");
        Ok(())
    }

    /// Replay one whole frame in a single write.
    ///
    /// The kernel timestamps an input packet once, when its first event is
    /// processed, and holds that stamp until the `SYN_REPORT`. Writing a
    /// frame in one call keeps it a single packet, which is what libinput
    /// measures velocity against.
    pub(crate) fn emit_frame(&mut self, count: u8, frame: &[TouchpadAxisEvent]) -> io::Result<()> {
        let mut events = Vec::with_capacity(count as usize);

        for e in frame.iter().take(count as usize) {
            // track slot state so a lost finger-up can be repaired on release
            if e.kind == EV_ABS {
                if e.code == AbsoluteAxisCode::ABS_MT_SLOT.0 {
                    self.current_slot = e.value;
                } else if e.code == AbsoluteAxisCode::ABS_MT_TRACKING_ID.0 {
                    if e.value < 0 {
                        self.active_slots.remove(&self.current_slot);
                    } else {
                        self.active_slots.insert(self.current_slot);
                    }
                }
            }
            events.push(InputEvent::new(e.kind, e.code, e.value));
        }

        let Some(device) = self.device.as_mut() else {
            // frames can outrun the description after a restart; dropping
            // them is preferable to guessing the geometry
            return Ok(());
        };
        device.emit(&events)
    }

    /// Lift every finger still marked down.
    ///
    /// Events travel over UDP, so a dropped `ABS_MT_TRACKING_ID = -1` would
    /// otherwise leave a phantom contact that blocks all further input on
    /// the receiving machine.
    pub(crate) fn release(&mut self) -> io::Result<()> {
        let Some(device) = self.device.as_mut() else {
            return Ok(());
        };
        if self.active_slots.is_empty() {
            return Ok(());
        }

        let mut events = Vec::new();
        for slot in std::mem::take(&mut self.active_slots) {
            events.push(InputEvent::new(
                EV_ABS,
                AbsoluteAxisCode::ABS_MT_SLOT.0,
                slot,
            ));
            events.push(InputEvent::new(
                EV_ABS,
                AbsoluteAxisCode::ABS_MT_TRACKING_ID.0,
                -1,
            ));
        }
        events.push(InputEvent::new(EV_KEY, KeyCode::BTN_TOUCH.0, 0));
        events.push(InputEvent::new(EV_KEY, KeyCode::BTN_TOOL_FINGER.0, 0));
        events.push(InputEvent::new(EV_KEY, KeyCode::BTN_TOOL_DOUBLETAP.0, 0));
        events.push(InputEvent::new(EV_KEY, KeyCode::BTN_TOOL_TRIPLETAP.0, 0));
        events.push(InputEvent::new(EV_KEY, KeyCode::BTN_TOOL_QUADTAP.0, 0));
        events.push(InputEvent::new(EV_KEY, KeyCode::BTN_TOOL_QUINTTAP.0, 0));
        events.push(InputEvent::new(EV_SYN, SYN_REPORT, 0));
        device.emit(&events)
    }
}

/// Writes input events to a virtual device.
pub(crate) trait EventSink: Send {
    /// Writes `events` as one report, terminated by `SYN_REPORT`.
    fn emit(&mut self, events: &[InputEvent]) -> io::Result<()>;

    /// The Caps Lock LED state the system wrote since the last call, if any.
    fn caps_lock_led(&mut self) -> io::Result<Option<bool>> {
        Ok(None)
    }
}

impl EventSink for VirtualDevice {
    fn emit(&mut self, events: &[InputEvent]) -> io::Result<()> {
        VirtualDevice::emit(self, events)
    }
}

/// Keyboard, mouse buttons, wheel and relative motion on one virtual device.
pub(crate) struct UInputKeyboard {
    device: Box<dyn EventSink>,
    raw_modifier_keys: HashSet<u32>,
    synthetic_modifiers: u32,
    /// leftover high-resolution scroll, per axis, in 1/120 detent units
    ///
    /// A high-resolution wheel reports one detent as several fractional
    /// events. Without carrying the remainder between them, each fraction has
    /// to be rounded to a whole click, which multiplies one detent into
    /// several.
    scroll_remainder: [i32; 2],
    /// sub-pixel pointer motion carried to the next event, per axis
    #[cfg(uinput)]
    motion_remainder: [f64; 2],
    /// Caps Lock state of the sending machine
    #[cfg(uinput)]
    caps_lock_wanted: bool,
    /// Caps Lock state of this system, from its LED writes; unknown until the first
    #[cfg(uinput)]
    caps_lock_actual: Option<bool>,
}

const MOD_SHIFT: u32 = 1 << 0;
#[cfg(uinput)]
const MOD_LOCK: u32 = 1 << 1;
#[cfg(uinput)]
const KEY_CAPSLOCK: u16 = 58;
const MOD_CTRL: u32 = 1 << 2;
const MOD_ALT: u32 = 1 << 3;
const MOD_SUPER: u32 = 1 << 6;
const FORWARDED_MODIFIERS: u32 = MOD_SHIFT | MOD_CTRL | MOD_ALT | MOD_SUPER;

fn modifier_for_key(key: u32) -> u32 {
    match key {
        42 | 54 => MOD_SHIFT,
        29 | 97 => MOD_CTRL,
        56 | 100 => MOD_ALT,
        // Caps Lock is mapped to Super in the Niri configuration on both machines.
        58 | 125 | 126 => MOD_SUPER,
        _ => 0,
    }
}

fn canonical_key_for_modifier(modifier: u32) -> u16 {
    match modifier {
        MOD_SHIFT => 42,
        MOD_CTRL => 29,
        MOD_ALT => 56,
        MOD_SUPER => 125,
        _ => unreachable!("unsupported modifier bit"),
    }
}

impl UInputKeyboard {
    pub(crate) fn new(device: impl EventSink + 'static) -> Self {
        Self {
            device: Box::new(device),
            raw_modifier_keys: HashSet::new(),
            synthetic_modifiers: 0,
            scroll_remainder: [0; 2],
            #[cfg(uinput)]
            motion_remainder: [0.0; 2],
            #[cfg(uinput)]
            caps_lock_wanted: false,
            #[cfg(uinput)]
            caps_lock_actual: None,
        }
    }

    /// Records the sender's Caps Lock state from its locked modifiers.
    #[cfg(uinput)]
    pub(crate) fn set_caps_lock_wanted(&mut self, locked: u32) {
        self.caps_lock_wanted = locked & MOD_LOCK != 0;
    }

    /// Toggles Caps Lock when this system's state differs from the sender's.
    ///
    /// Android also toggles Caps Lock on its own Meta+Alt shortcut, which a
    /// forwarded Super+Alt chord triggers. Call this before each key press so
    /// the next character uses the sender's state.
    #[cfg(uinput)]
    pub(crate) fn sync_caps_lock(&mut self) -> io::Result<()> {
        if let Some(led) = self.device.caps_lock_led()? {
            self.caps_lock_actual = Some(led);
        }
        if self.caps_lock_actual == Some(!self.caps_lock_wanted) {
            let wanted = if self.caps_lock_wanted { "on" } else { "off" };
            log::info!("caps lock: following the sender ({wanted})");
            self.device
                .emit(&[InputEvent::new(EventType::KEY.0, KEY_CAPSLOCK, 1)])?;
            self.device
                .emit(&[InputEvent::new(EventType::KEY.0, KEY_CAPSLOCK, 0)])?;
            // the LED write confirming the toggle arrives later
            self.caps_lock_actual = Some(self.caps_lock_wanted);
        }
        Ok(())
    }

    fn raw_modifier_mask(&self) -> u32 {
        self.raw_modifier_keys
            .iter()
            .fold(0, |mask, key| mask | modifier_for_key(*key))
    }

    pub(crate) fn emit_key(&mut self, key: u32, state: u8) -> io::Result<()> {
        let code = u16::try_from(key).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "keyboard keycode exceeds u16")
        })?;

        let modifier = modifier_for_key(key);
        match state {
            1 => {
                if modifier != 0 {
                    self.raw_modifier_keys.insert(key);
                }
            }
            0 => {
                self.raw_modifier_keys.remove(&key);
            }
            _ => {}
        }

        self.device
            .emit(&[InputEvent::new(EventType::KEY.0, code, i32::from(state))])
    }

    pub(crate) fn reconcile_modifiers(&mut self, depressed: u32) -> io::Result<()> {
        let desired = depressed & FORWARDED_MODIFIERS;
        let raw = self.raw_modifier_mask();
        let missing = desired & !(raw | self.synthetic_modifiers);
        let stale = self.synthetic_modifiers & !desired;
        let mut events = Vec::new();

        for modifier in [MOD_SHIFT, MOD_CTRL, MOD_ALT, MOD_SUPER] {
            if missing & modifier != 0 {
                events.push(InputEvent::new(
                    EventType::KEY.0,
                    canonical_key_for_modifier(modifier),
                    1,
                ));
                self.synthetic_modifiers |= modifier;
            }
            if stale & modifier != 0 {
                events.push(InputEvent::new(
                    EventType::KEY.0,
                    canonical_key_for_modifier(modifier),
                    0,
                ));
                self.synthetic_modifiers &= !modifier;
            }
        }

        if events.is_empty() {
            Ok(())
        } else {
            self.device.emit(&events)
        }
    }

    /// Replay a scroll event, reporting it the way a physical wheel does.
    ///
    /// `value_120` is in 1/120 of a detent. A high-resolution wheel - the
    /// G502 X among them - splits a single detent into several such events
    /// even in ratcheted mode, so rounding each one to a whole click turns
    /// one detent into three to five and makes precise scrolling impossible.
    ///
    /// The kernel convention is to report both: `REL_WHEEL_HI_RES` carries
    /// every fraction, and `REL_WHEEL` carries whole detents only, with the
    /// remainder held until it completes one. Emitting both in a single
    /// frame is what real mice do, and libinput expects it.
    pub(crate) fn emit_scroll(&mut self, axis: u8, value_120: i32) -> io::Result<()> {
        let (hi_res_code, wheel_code, delta) = match axis {
            // Wayland vertical scroll is positive downward; Linux REL_WHEEL is positive upward.
            0 => (
                RelativeAxisCode::REL_WHEEL_HI_RES,
                RelativeAxisCode::REL_WHEEL,
                -value_120,
            ),
            1 => (
                RelativeAxisCode::REL_HWHEEL_HI_RES,
                RelativeAxisCode::REL_HWHEEL,
                value_120,
            ),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid scroll axis",
                ));
            }
        };

        let remainder = &mut self.scroll_remainder[axis as usize];
        *remainder += delta;
        // truncation toward zero is what we want: a partial detent that
        // reverses direction must not emit a click
        let detents = *remainder / 120;
        *remainder -= detents * 120;

        let mut events = vec![InputEvent::new(EventType::RELATIVE.0, hi_res_code.0, delta)];
        if detents != 0 {
            events.push(InputEvent::new(
                EventType::RELATIVE.0,
                wheel_code.0,
                detents,
            ));
        }

        self.device.emit(&events)
    }

    /// Relative motion in pixels. Fractions accumulate until they make up a
    /// whole count, so slow movement is not lost to rounding.
    #[cfg(uinput)]
    pub(crate) fn emit_motion(&mut self, dx: f64, dy: f64) -> io::Result<()> {
        let mut events = Vec::with_capacity(2);
        for (axis, (delta, code)) in [(dx, RelativeAxisCode::REL_X), (dy, RelativeAxisCode::REL_Y)]
            .into_iter()
            .enumerate()
        {
            let total = self.motion_remainder[axis] + delta;
            let whole = total.trunc();
            self.motion_remainder[axis] = total - whole;
            if whole != 0.0 {
                events.push(InputEvent::new(EventType::RELATIVE.0, code.0, whole as i32));
            }
        }
        if events.is_empty() {
            return Ok(());
        }
        self.device.emit(&events)
    }

    /// `button` is an evdev `BTN_*` code, as every capture backend sends it.
    #[cfg(uinput)]
    pub(crate) fn emit_button(&mut self, button: u32, state: u32) -> io::Result<()> {
        let code = u16::try_from(button).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "pointer button exceeds u16")
        })?;
        self.device.emit(&[InputEvent::new(
            EventType::KEY.0,
            code,
            i32::from(state != 0),
        )])
    }
}

#[cfg(uinput)]
pub(crate) use backend::UinputEmulation;

/// Emulation through `/dev/uinput` alone, with no display server protocol.
///
/// This is the backend for Android, where a process running as the `shell`
/// user may open `/dev/uinput` and the system treats the result as a
/// physical keyboard, mouse and touchpad.
#[cfg(uinput)]
mod backend {
    use super::{
        UInputKeyboard, UInputTouchpad, keyboard_and_mouse_keys, led_keyboard::LedKeyboard,
    };
    use crate::{Emulation, EmulationHandle, error::EmulationError};
    use async_trait::async_trait;
    use input_event::{Event, KeyboardEvent, PointerEvent, TouchpadEvent};
    use std::collections::HashSet;
    use std::io;

    const KEY_CAPSLOCK: u32 = 58;
    const KEY_LEFTMETA: u32 = 125;

    /// Wayland axis units per wheel detent, as libinput reports them.
    const AXIS_UNITS_PER_DETENT: f64 = 15.0;

    pub(crate) struct UinputEmulation {
        input: UInputKeyboard,
        touchpad: UInputTouchpad,
        clients: HashSet<EmulationHandle>,
    }

    impl UinputEmulation {
        pub(crate) fn new() -> io::Result<Self> {
            let device = LedKeyboard::new(
                "Lan Mouse uinput keyboard and mouse",
                keyboard_and_mouse_keys,
            )?;
            log::info!("created uinput keyboard and mouse");
            Ok(Self {
                input: UInputKeyboard::new(device),
                touchpad: UInputTouchpad::new(),
                clients: HashSet::new(),
            })
        }
    }

    /// The capturing machines map Caps Lock to Super in xkb, so the raw
    /// scan code they send is Caps Lock. A receiver without that xkb option
    /// would toggle Caps Lock instead, so the key is sent as Super here.
    fn remap_key(key: u32) -> u32 {
        if key == KEY_CAPSLOCK {
            KEY_LEFTMETA
        } else {
            key
        }
    }

    #[async_trait]
    impl Emulation for UinputEmulation {
        async fn consume(
            &mut self,
            event: Event,
            handle: EmulationHandle,
        ) -> Result<(), EmulationError> {
            if !self.clients.contains(&handle) {
                return Ok(());
            }
            match event {
                Event::Pointer(PointerEvent::Motion { dx, dy, .. }) => {
                    self.input.emit_motion(dx, dy)?
                }
                Event::Pointer(PointerEvent::Button { button, state, .. }) => {
                    self.input.emit_button(button, state)?
                }
                Event::Pointer(PointerEvent::Axis { axis, value, .. }) => {
                    let value_120 = (value * 120.0 / AXIS_UNITS_PER_DETENT).round() as i32;
                    self.input.emit_scroll(axis, value_120)?
                }
                Event::Pointer(PointerEvent::AxisDiscrete120 { axis, value }) => {
                    self.input.emit_scroll(axis, value)?
                }
                Event::Keyboard(KeyboardEvent::Key { key, state, .. }) => {
                    let key = remap_key(key);
                    if state == 1 && super::modifier_for_key(key) == 0 {
                        self.input.sync_caps_lock()?;
                    }
                    self.input.emit_key(key, state)?
                }
                Event::Keyboard(KeyboardEvent::Modifiers {
                    depressed, locked, ..
                }) => {
                    self.input.set_caps_lock_wanted(locked);
                    self.input.reconcile_modifiers(depressed)?
                }
                Event::Touchpad(TouchpadEvent::Device {
                    width_mm,
                    height_mm,
                    slots,
                    pressure_max,
                }) => self
                    .touchpad
                    .describe(width_mm, height_mm, slots, pressure_max)?,
                Event::Touchpad(TouchpadEvent::Frame { count, events, .. }) => {
                    self.touchpad.emit_frame(count, &events)?
                }
            }
            Ok(())
        }

        async fn create(&mut self, handle: EmulationHandle) {
            self.clients.insert(handle);
        }

        async fn destroy(&mut self, handle: EmulationHandle) {
            self.clients.remove(&handle);
            // a peer that leaves mid-gesture must not leave fingers down
            if let Err(e) = self.touchpad.release() {
                log::warn!("failed to release virtual touchpad contacts: {e}");
            }
        }

        async fn terminate(&mut self) {}
    }
}

#[cfg(all(test, uinput))]
mod tests {
    use super::keyboard_and_mouse_keys;

    #[test]
    fn android_device_declares_keys_and_mouse_buttons_only() {
        // KEY_A, BTN_LEFT, BTN_TASK, KEY_OK
        for code in [30, 0x110, 0x117, 0x160] {
            assert!(keyboard_and_mouse_keys(code), "{code:#x} must be declared");
        }
        // BTN_0, BTN_JOYSTICK, BTN_SOUTH (gamepad), BTN_TOUCH
        for code in [0x100, 0x120, 0x130, 0x14a] {
            assert!(!keyboard_and_mouse_keys(code), "{code:#x} must be omitted");
        }
    }
}

/// A uinput keyboard and mouse that also declares a Caps Lock LED.
///
/// The system writes its Caps Lock state to the LED, and the write arrives
/// on this device's descriptor. `evdev` cannot declare LEDs, so the device
/// is created with the uinput ioctls directly.
#[cfg(uinput)]
pub(crate) mod led_keyboard {
    use super::EventSink;
    use evdev::InputEvent;
    use std::fs::{File, OpenOptions};
    use std::io::{self, Read, Write};
    use std::mem::size_of;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    const EV_SYN: u16 = 0x00;
    const EV_KEY: u16 = 0x01;
    const EV_REL: u16 = 0x02;
    const EV_LED: u16 = 0x11;
    const LED_CAPSL: u16 = 0x01;
    /// REL_X, REL_Y, REL_HWHEEL, REL_WHEEL, REL_WHEEL_HI_RES, REL_HWHEEL_HI_RES
    const RELATIVE_AXES: [u16; 6] = [0x00, 0x01, 0x06, 0x08, 0x0b, 0x0c];

    // linux/uinput.h; the ioctl encoding is the same on x86_64 and aarch64
    const UI_DEV_CREATE: u64 = 0x5501;
    const UI_DEV_SETUP: u64 = 0x405c_5503;
    const UI_SET_EVBIT: u64 = 0x4004_5564;
    const UI_SET_KEYBIT: u64 = 0x4004_5565;
    const UI_SET_RELBIT: u64 = 0x4004_5566;
    const UI_SET_LEDBIT: u64 = 0x4004_5569;

    #[repr(C)]
    struct UinputSetup {
        bustype: u16,
        vendor: u16,
        product: u16,
        version: u16,
        name: [u8; 80],
        ff_effects_max: u32,
    }

    pub(crate) struct LedKeyboard {
        file: File,
    }

    fn ioctl(file: &File, request: u64, argument: libc::c_ulong) -> io::Result<()> {
        // SAFETY: a uinput ioctl on an open /dev/uinput descriptor; `argument`
        // is an integer or points to a live `UinputSetup`
        if unsafe { libc::ioctl(file.as_raw_fd(), request as _, argument) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    impl LedKeyboard {
        pub(crate) fn new(name: &str, declare_key: fn(u16) -> bool) -> io::Result<Self> {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open("/dev/uinput")?;
            for event_type in [EV_KEY, EV_REL, EV_LED] {
                ioctl(&file, UI_SET_EVBIT, event_type.into())?;
            }
            for code in (1..=0x2ffu16).filter(|code| declare_key(*code)) {
                ioctl(&file, UI_SET_KEYBIT, code.into())?;
            }
            for axis in RELATIVE_AXES {
                ioctl(&file, UI_SET_RELBIT, axis.into())?;
            }
            ioctl(&file, UI_SET_LEDBIT, LED_CAPSL.into())?;

            // the identity evdev gives its virtual devices
            let mut setup = UinputSetup {
                bustype: 0x03,
                vendor: 0x1234,
                product: 0x5678,
                version: 0x111,
                name: [0; 80],
                ff_effects_max: 0,
            };
            let bytes = name.as_bytes();
            let length = bytes.len().min(setup.name.len() - 1);
            setup.name[..length].copy_from_slice(&bytes[..length]);
            ioctl(
                &file,
                UI_DEV_SETUP,
                &setup as *const UinputSetup as libc::c_ulong,
            )?;
            ioctl(&file, UI_DEV_CREATE, 0)?;
            Ok(Self { file })
        }
    }

    fn raw_event(kind: u16, code: u16, value: i32) -> libc::input_event {
        libc::input_event {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_: kind,
            code,
            value,
        }
    }

    impl EventSink for LedKeyboard {
        fn emit(&mut self, events: &[InputEvent]) -> io::Result<()> {
            let mut raw: Vec<libc::input_event> = events
                .iter()
                .map(|e| raw_event(e.event_type().0, e.code(), e.value()))
                .collect();
            raw.push(raw_event(EV_SYN, 0, 0));
            // SAFETY: input_event is plain old data; the slice covers `raw` exactly
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    raw.as_ptr().cast::<u8>(),
                    raw.len() * size_of::<libc::input_event>(),
                )
            };
            self.file.write_all(bytes)
        }

        fn caps_lock_led(&mut self) -> io::Result<Option<bool>> {
            let mut latest = None;
            let mut buffer = [0u8; 32 * size_of::<libc::input_event>()];
            loop {
                let length = match self.file.read(&mut buffer) {
                    Ok(length) => length,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e),
                };
                let (records, _) =
                    buffer[..length].as_chunks::<{ size_of::<libc::input_event>() }>();
                for chunk in records {
                    // SAFETY: the kernel writes whole input_event records
                    let event: libc::input_event =
                        unsafe { std::ptr::read_unaligned(chunk.as_ptr().cast()) };
                    if event.type_ == EV_LED && event.code == LED_CAPSL {
                        latest = Some(event.value != 0);
                    }
                }
                if length == 0 {
                    break;
                }
            }
            Ok(latest)
        }
    }
}

#[cfg(all(test, uinput, target_os = "linux"))]
mod led_tests {
    use super::{EventSink, keyboard_and_mouse_keys, led_keyboard::LedKeyboard};
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::mem::size_of;
    use std::time::Duration;

    /// The event node of the input device with this name.
    fn event_node(name: &str) -> Option<String> {
        fs::read_dir("/sys/class/input")
            .ok()?
            .flatten()
            .find_map(|entry| {
                let node = entry.file_name().into_string().ok()?;
                let device_name = fs::read_to_string(entry.path().join("device/name")).ok()?;
                (node.starts_with("event") && device_name.trim() == name)
                    .then(|| format!("/dev/input/{node}"))
            })
    }

    #[test]
    #[ignore = "creates a device through /dev/uinput and writes to its event node"]
    fn reads_caps_lock_led_written_by_the_system() {
        let name = "Lan Mouse caps lock LED test";
        let mut keyboard = LedKeyboard::new(name, keyboard_and_mouse_keys).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let node = event_node(name).expect("event node for the test device");
        let mut device = OpenOptions::new().write(true).open(node).unwrap();
        for value in [1, 0] {
            let events = [
                libc::input_event {
                    time: libc::timeval {
                        tv_sec: 0,
                        tv_usec: 0,
                    },
                    type_: 0x11,
                    code: 0x01,
                    value,
                },
                libc::input_event {
                    time: libc::timeval {
                        tv_sec: 0,
                        tv_usec: 0,
                    },
                    type_: 0,
                    code: 0,
                    value: 0,
                },
            ];
            // SAFETY: input_event is plain old data
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    events.as_ptr().cast::<u8>(),
                    2 * size_of::<libc::input_event>(),
                )
            };
            device.write_all(bytes).unwrap();
            std::thread::sleep(Duration::from_millis(50));
            assert_eq!(keyboard.caps_lock_led().unwrap(), Some(value == 1));
        }
        assert_eq!(keyboard.caps_lock_led().unwrap(), None, "no new LED writes");
    }
}

#[cfg(all(test, uinput, target_os = "android"))]
mod android_led_tests {
    use super::{EventSink, keyboard_and_mouse_keys, led_keyboard::LedKeyboard};
    use std::process::Command;
    use std::time::Duration;

    fn led_after(keyboard: &mut LedKeyboard, wait: Duration) -> Option<bool> {
        std::thread::sleep(wait);
        keyboard.caps_lock_led().unwrap()
    }

    /// Android must report its Caps Lock state to the device, or the receiver
    /// cannot undo the Meta+Alt shortcut. Toggles Caps Lock twice.
    #[test]
    #[ignore = "creates a keyboard on an Android device and toggles Caps Lock twice"]
    fn android_writes_caps_lock_led() {
        let mut keyboard =
            LedKeyboard::new("Lan Mouse caps lock LED test", keyboard_and_mouse_keys).unwrap();
        let initial = led_after(&mut keyboard, Duration::from_secs(2)).expect("initial LED state");
        let toggle = || {
            let status = Command::new("input")
                .args(["keycombination", "KEYCODE_META_LEFT", "KEYCODE_ALT_LEFT"])
                .status()
                .unwrap();
            assert!(status.success());
        };
        toggle();
        assert_eq!(
            led_after(&mut keyboard, Duration::from_secs(1)),
            Some(!initial)
        );
        toggle();
        assert_eq!(
            led_after(&mut keyboard, Duration::from_secs(1)),
            Some(initial)
        );
    }
}
