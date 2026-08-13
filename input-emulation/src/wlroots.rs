use crate::error::EmulationError;

use super::error::{WaylandBindError, WlrootsEmulationCreationError};
use super::{Emulation, EmulationHandle};
use async_trait::async_trait;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, EventType, InputEvent, KeyCode, PropType,
    RelativeAxisCode, UinputAbsSetup, uinput::VirtualDevice,
};
use input_event::{
    Event, KeyboardEvent, PointerEvent, TOUCHPAD_AXIS_MAX, TouchpadAxisEvent, TouchpadEvent,
};
use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::{Arc, Mutex};
use wayland_client::backend::WaylandError;
use wayland_client::protocol::wl_pointer::{Axis, ButtonState};
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::wl_registry,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1 as VpManager,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1 as Vp,
};

struct State {
    input_for_client: HashMap<EmulationHandle, VirtualInput>,
    qh: QueueHandle<Self>,
    vpm: VpManager,
    keyboard: Arc<Mutex<UInputKeyboard>>,
    touchpad: Arc<Mutex<UInputTouchpad>>,
}

pub(crate) struct WlrootsEmulation {
    last_flush_failed: bool,
    state: State,
    queue: EventQueue<State>,
}

impl WlrootsEmulation {
    pub(crate) fn new() -> Result<Self, WlrootsEmulationCreationError> {
        let conn = Connection::connect_to_env()?;
        let (globals, queue) = registry_queue_init::<State>(&conn)?;
        let qh = queue.handle();

        let vpm: VpManager = globals
            .bind(&qh, 1..=1, ())
            .map_err(|e| WaylandBindError::new(e, "wlr-virtual-pointer-unstable-v1"))?;

        let keyboard = Arc::new(Mutex::new(UInputKeyboard {
            device: create_uinput_keyboard()?,
            raw_modifier_keys: HashSet::new(),
            synthetic_modifiers: 0,
            scroll_remainder: [0; 2],
        }));
        log::info!("using uinput keyboard injection for compositor shortcut support");

        Ok(Self {
            last_flush_failed: false,
            state: State {
                input_for_client: HashMap::new(),
                qh,
                vpm,
                keyboard,
                // built lazily: the geometry only arrives with the first
                // TouchpadEvent::Device from a peer that captures one
                touchpad: Arc::new(Mutex::new(UInputTouchpad::new())),
            },
            queue,
        })
    }
}

fn create_uinput_keyboard() -> io::Result<VirtualDevice> {
    let mut keys = AttributeSet::<KeyCode>::new();
    for code in 1..=0x2ff {
        keys.insert(KeyCode::new(code));
    }

    VirtualDevice::builder()?
        .name("Lan Mouse uinput keyboard and scroll")
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

impl State {
    fn add_client(&mut self, client: EmulationHandle) {
        let pointer: Vp = self.vpm.create_virtual_pointer(None, &self.qh, ());
        self.input_for_client.insert(
            client,
            VirtualInput {
                pointer,
                keyboard: self.keyboard.clone(),
                touchpad: self.touchpad.clone(),
            },
        );
    }

    fn destroy_client(&mut self, handle: EmulationHandle) {
        if let Some(input) = self.input_for_client.remove(&handle) {
            input.pointer.destroy();
            // a peer that vanishes mid-gesture must not leave fingers down
            if let Ok(mut touchpad) = input.touchpad.lock() {
                if let Err(e) = touchpad.release() {
                    log::warn!("failed to release virtual touchpad contacts: {e}");
                }
            }
        }
    }
}

#[async_trait]
impl Emulation for WlrootsEmulation {
    async fn consume(
        &mut self,
        event: Event,
        handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        if let Some(virtual_input) = self.state.input_for_client.get(&handle) {
            if self.last_flush_failed {
                match self.queue.flush() {
                    Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                        log::warn!("can't keep up, discarding event: ({handle}) - {event:?}");
                        return Ok(());
                    }
                    _ => {}
                }
            }

            virtual_input.consume_event(event)?;

            match self.queue.flush() {
                Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                    self.last_flush_failed = true;
                    log::warn!("can't keep up, discarding event: ({handle}) - {event:?}");
                }
                Err(WaylandError::Protocol(e)) => panic!("wayland protocol violation: {e}"),
                Ok(()) => self.last_flush_failed = false,
                Err(e) => Err(e)?,
            }
        }
        Ok(())
    }

    async fn create(&mut self, handle: EmulationHandle) {
        self.state.add_client(handle);
        if let Err(e) = self.queue.flush() {
            log::error!("{e}");
        }
    }

    async fn destroy(&mut self, handle: EmulationHandle) {
        self.state.destroy_client(handle);
        if let Err(e) = self.queue.flush() {
            log::error!("{e}");
        }
    }

    async fn terminate(&mut self) {}
}

struct VirtualInput {
    pointer: Vp,
    keyboard: Arc<Mutex<UInputKeyboard>>,
    touchpad: Arc<Mutex<UInputTouchpad>>,
}

/// Replays raw touchpad frames onto a virtual multitouch device.
///
/// No gesture interpretation happens here on purpose: the compositor's own
/// libinput sees a normal multitouch touchpad and performs recognition
/// itself, which is what makes remote pinch / 3-finger / 4-finger gestures
/// behave identically to local ones.
struct UInputTouchpad {
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
    fn new() -> Self {
        Self {
            device: None,
            built_from: None,
            current_slot: 0,
            active_slots: HashSet::new(),
        }
    }

    fn describe(
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
    fn emit_frame(&mut self, count: u8, frame: &[TouchpadAxisEvent]) -> io::Result<()> {
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
    fn release(&mut self) -> io::Result<()> {
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

struct UInputKeyboard {
    device: VirtualDevice,
    raw_modifier_keys: HashSet<u32>,
    synthetic_modifiers: u32,
    /// leftover high-resolution scroll, per axis, in 1/120 detent units
    ///
    /// A high-resolution wheel reports one detent as several fractional
    /// events. Without carrying the remainder between them, each fraction has
    /// to be rounded to a whole click, which multiplies one detent into
    /// several.
    scroll_remainder: [i32; 2],
}

const MOD_SHIFT: u32 = 1 << 0;
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
    fn raw_modifier_mask(&self) -> u32 {
        self.raw_modifier_keys
            .iter()
            .fold(0, |mask, key| mask | modifier_for_key(*key))
    }

    fn emit_key(&mut self, key: u32, state: u8) -> io::Result<()> {
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

    fn reconcile_modifiers(&mut self, depressed: u32) -> io::Result<()> {
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
    fn emit_scroll(&mut self, axis: u8, value_120: i32) -> io::Result<()> {
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

        let mut events = vec![InputEvent::new(
            EventType::RELATIVE.0,
            hi_res_code.0,
            delta,
        )];
        if detents != 0 {
            events.push(InputEvent::new(
                EventType::RELATIVE.0,
                wheel_code.0,
                detents,
            ));
        }

        self.device.emit(&events)
    }
}

impl VirtualInput {
    fn consume_event(&self, event: Event) -> io::Result<()> {
        match event {
            Event::Pointer(e) => {
                match e {
                    PointerEvent::Motion { time, dx, dy } => self.pointer.motion(time, dx, dy),
                    PointerEvent::Button {
                        time,
                        button,
                        state,
                    } => {
                        let state: ButtonState = state.try_into().map_err(|_| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "invalid pointer button state",
                            )
                        })?;
                        self.pointer.button(time, button, state);
                    }
                    PointerEvent::Axis { time, axis, value } => {
                        let axis: Axis = (axis as u32).try_into().map_err(|_| {
                            io::Error::new(io::ErrorKind::InvalidInput, "invalid pointer axis")
                        })?;
                        self.pointer.axis(time, axis, value);
                        self.pointer.frame();
                    }
                    PointerEvent::AxisDiscrete120 { axis, value } => {
                        let mut keyboard = self
                            .keyboard
                            .lock()
                            .map_err(|_| io::Error::other("uinput device lock poisoned"))?;
                        keyboard.emit_scroll(axis, value)?;
                    }
                }
                self.pointer.frame();
            }
            Event::Keyboard(KeyboardEvent::Key { key, state, .. }) => {
                let mut keyboard = self
                    .keyboard
                    .lock()
                    .map_err(|_| io::Error::other("uinput keyboard lock poisoned"))?;
                keyboard.emit_key(key, state)?;
            }
            Event::Keyboard(KeyboardEvent::Modifiers { depressed, .. }) => {
                let mut keyboard = self
                    .keyboard
                    .lock()
                    .map_err(|_| io::Error::other("uinput keyboard lock poisoned"))?;
                keyboard.reconcile_modifiers(depressed)?;
            }
            Event::Touchpad(t) => {
                let mut touchpad = self
                    .touchpad
                    .lock()
                    .map_err(|_| io::Error::other("uinput touchpad lock poisoned"))?;
                match t {
                    TouchpadEvent::Device {
                        width_mm,
                        height_mm,
                        slots,
                        pressure_max,
                    } => touchpad.describe(width_mm, height_mm, slots, pressure_max)?,
                    TouchpadEvent::Frame {
                        time: _,
                        count,
                        events,
                    } => touchpad.emit_frame(count, &events)?,
                }
            }
        }

        Ok(())
    }
}

delegate_noop!(State: Vp);
delegate_noop!(State: VpManager);

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut State,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
    }
}
