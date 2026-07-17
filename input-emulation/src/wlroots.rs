use crate::error::EmulationError;

use super::error::{WaylandBindError, WlrootsEmulationCreationError};
use super::{Emulation, EmulationHandle};
use async_trait::async_trait;
use evdev::{
    AttributeSet, EventType, InputEvent, KeyCode, RelativeAxisCode, uinput::VirtualDevice,
};
use input_event::{Event, KeyboardEvent, PointerEvent};
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
        }));
        log::info!("using uinput keyboard injection for compositor shortcut support");

        Ok(Self {
            last_flush_failed: false,
            state: State {
                input_for_client: HashMap::new(),
                qh,
                vpm,
                keyboard,
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
            },
        );
    }

    fn destroy_client(&mut self, handle: EmulationHandle) {
        if let Some(input) = self.input_for_client.remove(&handle) {
            input.pointer.destroy();
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
}

struct UInputKeyboard {
    device: VirtualDevice,
    raw_modifier_keys: HashSet<u32>,
    synthetic_modifiers: u32,
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

    fn emit_scroll(&mut self, axis: u8, value_120: i32) -> io::Result<()> {
        let mut steps = value_120 / 120;
        if steps == 0 && value_120 != 0 {
            steps = value_120.signum();
        }

        let (axis, value) = match axis {
            // Wayland vertical scroll is positive downward; Linux REL_WHEEL is positive upward.
            0 => (RelativeAxisCode::REL_WHEEL, -steps),
            1 => (RelativeAxisCode::REL_HWHEEL, steps),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid scroll axis",
                ));
            }
        };

        self.device
            .emit(&[InputEvent::new(EventType::RELATIVE.0, axis.0, value)])
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
