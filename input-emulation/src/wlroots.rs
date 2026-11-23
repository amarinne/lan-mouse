use crate::error::EmulationError;

use super::error::{WaylandBindError, WlrootsEmulationCreationError};
use super::{Emulation, EmulationHandle};
use crate::uinput::{UInputKeyboard, UInputTouchpad, all_keys, create_uinput_keyboard};
use async_trait::async_trait;
use input_event::{Event, KeyboardEvent, PointerEvent, TouchpadEvent};
use std::collections::HashMap;
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

        let keyboard = Arc::new(Mutex::new(UInputKeyboard::new(create_uinput_keyboard(
            "Lan Mouse uinput keyboard and scroll",
            all_keys,
        )?)));
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
