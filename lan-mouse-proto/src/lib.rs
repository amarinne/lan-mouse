use input_event::{
    Event as InputEvent, KeyboardEvent, MAX_FRAME_EVENTS, PointerEvent, TouchpadAxisEvent,
    TouchpadEvent,
};
use num_enum::{IntoPrimitive, TryFromPrimitive, TryFromPrimitiveError};
use paste::paste;
use std::{
    fmt::{Debug, Display, Formatter},
    mem::size_of,
};
use thiserror::Error;

/// defines the maximum size an encoded event can take up
/// this is currently the touchpad frame event
/// type: u8, time: u32, count: u8, then `count` * (kind: u16, code: u16, value: i32)
///
/// A whole evdev frame travels in one datagram. Splitting a frame across
/// datagrams, as an event-sized buffer forces, multiplies the packet rate by
/// the number of events in a frame and makes every single loss corrupt a
/// frame instead of dropping a sample.
pub const MAX_EVENT_SIZE: usize = size_of::<u8>()
    + size_of::<u32>()
    + size_of::<u8>()
    + MAX_FRAME_EVENTS * (2 * size_of::<u16>() + size_of::<i32>());

/// error type for protocol violations
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// event type does not exist
    #[error("invalid event id: `{0}`")]
    InvalidEventId(#[from] TryFromPrimitiveError<EventType>),
    /// position type does not exist
    #[error("invalid event id: `{0}`")]
    InvalidPosition(#[from] TryFromPrimitiveError<Position>),
    /// touchpad frame claims more events than one datagram can hold
    #[error("touchpad frame carries {0} events, max is {max}", max = MAX_FRAME_EVENTS)]
    FrameTooLong(u8),
}

/// Position of a client
#[derive(Clone, Copy, Debug, TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum Position {
    Left,
    Right,
    Top,
    Bottom,
}

impl Display for Position {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let pos = match self {
            Position::Left => "left",
            Position::Right => "right",
            Position::Top => "top",
            Position::Bottom => "bottom",
        };
        write!(f, "{pos}")
    }
}

/// main lan-mouse protocol event type
#[derive(Clone, Copy, Debug)]
pub enum ProtoEvent {
    /// notify a client that the cursor entered its region at the given position
    /// [`ProtoEvent::Ack`] with the same serial is used for synchronization between devices
    Enter(Position),
    /// notify a client that the cursor left its region
    /// [`ProtoEvent::Ack`] with the same serial is used for synchronization between devices
    Leave(u32),
    /// acknowledge of an [`ProtoEvent::Enter`] or [`ProtoEvent::Leave`] event
    Ack(u32),
    /// Input event
    Input(InputEvent),
    /// Ping event for tracking unresponsive clients.
    /// A client has to respond with [`ProtoEvent::Pong`].
    Ping,
    /// Response to [`ProtoEvent::Ping`], true if emulation is enabled / available
    Pong(bool),
}

impl Display for ProtoEvent {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtoEvent::Enter(s) => write!(f, "Enter({s})"),
            ProtoEvent::Leave(s) => write!(f, "Leave({s})"),
            ProtoEvent::Ack(s) => write!(f, "Ack({s})"),
            ProtoEvent::Input(e) => write!(f, "{e}"),
            ProtoEvent::Ping => write!(f, "ping"),
            ProtoEvent::Pong(alive) => {
                write!(
                    f,
                    "pong: {}",
                    if *alive { "alive" } else { "not available" }
                )
            }
        }
    }
}

#[derive(TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum EventType {
    PointerMotion,
    PointerButton,
    PointerAxis,
    PointerAxisValue120,
    KeyboardKey,
    KeyboardModifiers,
    Ping,
    Pong,
    Enter,
    Leave,
    Ack,
    /// appended after `Ack` so the discriminants of all pre-existing
    /// events stay stable on the wire
    TouchpadDevice,
    TouchpadFrame,
}

impl ProtoEvent {
    fn event_type(&self) -> EventType {
        match self {
            ProtoEvent::Input(e) => match e {
                InputEvent::Pointer(p) => match p {
                    PointerEvent::Motion { .. } => EventType::PointerMotion,
                    PointerEvent::Button { .. } => EventType::PointerButton,
                    PointerEvent::Axis { .. } => EventType::PointerAxis,
                    PointerEvent::AxisDiscrete120 { .. } => EventType::PointerAxisValue120,
                },
                InputEvent::Keyboard(k) => match k {
                    KeyboardEvent::Key { .. } => EventType::KeyboardKey,
                    KeyboardEvent::Modifiers { .. } => EventType::KeyboardModifiers,
                },
                InputEvent::Touchpad(t) => match t {
                    TouchpadEvent::Device { .. } => EventType::TouchpadDevice,
                    TouchpadEvent::Frame { .. } => EventType::TouchpadFrame,
                },
            },
            ProtoEvent::Ping => EventType::Ping,
            ProtoEvent::Pong(_) => EventType::Pong,
            ProtoEvent::Enter(_) => EventType::Enter,
            ProtoEvent::Leave(_) => EventType::Leave,
            ProtoEvent::Ack(_) => EventType::Ack,
        }
    }
}

impl TryFrom<[u8; MAX_EVENT_SIZE]> for ProtoEvent {
    type Error = ProtocolError;

    fn try_from(buf: [u8; MAX_EVENT_SIZE]) -> Result<Self, Self::Error> {
        let mut buf = &buf[..];
        let event_type = decode_u8(&mut buf)?;
        match EventType::try_from(event_type)? {
            EventType::PointerMotion => {
                Ok(Self::Input(InputEvent::Pointer(PointerEvent::Motion {
                    time: decode_u32(&mut buf)?,
                    dx: decode_f64(&mut buf)?,
                    dy: decode_f64(&mut buf)?,
                })))
            }
            EventType::PointerButton => {
                Ok(Self::Input(InputEvent::Pointer(PointerEvent::Button {
                    time: decode_u32(&mut buf)?,
                    button: decode_u32(&mut buf)?,
                    state: decode_u32(&mut buf)?,
                })))
            }
            EventType::PointerAxis => Ok(Self::Input(InputEvent::Pointer(PointerEvent::Axis {
                time: decode_u32(&mut buf)?,
                axis: decode_u8(&mut buf)?,
                value: decode_f64(&mut buf)?,
            }))),
            EventType::PointerAxisValue120 => Ok(Self::Input(InputEvent::Pointer(
                PointerEvent::AxisDiscrete120 {
                    axis: decode_u8(&mut buf)?,
                    value: decode_i32(&mut buf)?,
                },
            ))),
            EventType::KeyboardKey => Ok(Self::Input(InputEvent::Keyboard(KeyboardEvent::Key {
                time: decode_u32(&mut buf)?,
                key: decode_u32(&mut buf)?,
                state: decode_u8(&mut buf)?,
            }))),
            EventType::KeyboardModifiers => Ok(Self::Input(InputEvent::Keyboard(
                KeyboardEvent::Modifiers {
                    depressed: decode_u32(&mut buf)?,
                    latched: decode_u32(&mut buf)?,
                    locked: decode_u32(&mut buf)?,
                    group: decode_u32(&mut buf)?,
                },
            ))),
            EventType::Ping => Ok(Self::Ping),
            EventType::Pong => Ok(Self::Pong(decode_u8(&mut buf)? != 0)),
            EventType::Enter => Ok(Self::Enter(decode_u8(&mut buf)?.try_into()?)),
            EventType::Leave => Ok(Self::Leave(decode_u32(&mut buf)?)),
            EventType::Ack => Ok(Self::Ack(decode_u32(&mut buf)?)),
            EventType::TouchpadDevice => {
                Ok(Self::Input(InputEvent::Touchpad(TouchpadEvent::Device {
                    width_mm: decode_u16(&mut buf)?,
                    height_mm: decode_u16(&mut buf)?,
                    slots: decode_u8(&mut buf)?,
                    pressure_max: decode_i32(&mut buf)?,
                })))
            }
            EventType::TouchpadFrame => {
                let time = decode_u32(&mut buf)?;
                let count = decode_u8(&mut buf)?;
                if count as usize > MAX_FRAME_EVENTS {
                    return Err(ProtocolError::FrameTooLong(count));
                }
                let mut events = [TouchpadAxisEvent::default(); MAX_FRAME_EVENTS];
                for event in events.iter_mut().take(count as usize) {
                    event.kind = decode_u16(&mut buf)?;
                    event.code = decode_u16(&mut buf)?;
                    event.value = decode_i32(&mut buf)?;
                }
                Ok(Self::Input(InputEvent::Touchpad(TouchpadEvent::Frame {
                    time,
                    count,
                    events,
                })))
            }
        }
    }
}

impl From<ProtoEvent> for ([u8; MAX_EVENT_SIZE], usize) {
    fn from(event: ProtoEvent) -> Self {
        let mut buf = [0u8; MAX_EVENT_SIZE];
        let mut len = 0usize;
        {
            let mut buf = &mut buf[..];
            let buf = &mut buf;
            let len = &mut len;
            encode_u8(buf, len, event.event_type() as u8);
            match event {
                ProtoEvent::Input(event) => match event {
                    InputEvent::Pointer(p) => match p {
                        PointerEvent::Motion { time, dx, dy } => {
                            encode_u32(buf, len, time);
                            encode_f64(buf, len, dx);
                            encode_f64(buf, len, dy);
                        }
                        PointerEvent::Button {
                            time,
                            button,
                            state,
                        } => {
                            encode_u32(buf, len, time);
                            encode_u32(buf, len, button);
                            encode_u32(buf, len, state);
                        }
                        PointerEvent::Axis { time, axis, value } => {
                            encode_u32(buf, len, time);
                            encode_u8(buf, len, axis);
                            encode_f64(buf, len, value);
                        }
                        PointerEvent::AxisDiscrete120 { axis, value } => {
                            encode_u8(buf, len, axis);
                            encode_i32(buf, len, value);
                        }
                    },
                    InputEvent::Touchpad(t) => match t {
                        TouchpadEvent::Device {
                            width_mm,
                            height_mm,
                            slots,
                            pressure_max,
                        } => {
                            encode_u16(buf, len, width_mm);
                            encode_u16(buf, len, height_mm);
                            encode_u8(buf, len, slots);
                            encode_i32(buf, len, pressure_max);
                        }
                        TouchpadEvent::Frame {
                            time,
                            count,
                            events,
                        } => {
                            let count = count.min(MAX_FRAME_EVENTS as u8);
                            encode_u32(buf, len, time);
                            encode_u8(buf, len, count);
                            for event in events.iter().take(count as usize) {
                                encode_u16(buf, len, event.kind);
                                encode_u16(buf, len, event.code);
                                encode_i32(buf, len, event.value);
                            }
                        }
                    },
                    InputEvent::Keyboard(k) => match k {
                        KeyboardEvent::Key { time, key, state } => {
                            encode_u32(buf, len, time);
                            encode_u32(buf, len, key);
                            encode_u8(buf, len, state);
                        }
                        KeyboardEvent::Modifiers {
                            depressed,
                            latched,
                            locked,
                            group,
                        } => {
                            encode_u32(buf, len, depressed);
                            encode_u32(buf, len, latched);
                            encode_u32(buf, len, locked);
                            encode_u32(buf, len, group);
                        }
                    },
                },
                ProtoEvent::Ping => {}
                ProtoEvent::Pong(alive) => encode_u8(buf, len, alive as u8),
                ProtoEvent::Enter(pos) => encode_u8(buf, len, pos as u8),
                ProtoEvent::Leave(serial) => encode_u32(buf, len, serial),
                ProtoEvent::Ack(serial) => encode_u32(buf, len, serial),
            }
        }
        (buf, len)
    }
}

macro_rules! decode_impl {
    ($t:ty) => {
        paste! {
            fn [<decode_ $t>](data: &mut &[u8]) -> Result<$t, ProtocolError> {
                let (int_bytes, rest) = data.split_at(size_of::<$t>());
                *data = rest;
                Ok($t::from_be_bytes(int_bytes.try_into().unwrap()))
            }
        }
    };
}

decode_impl!(u8);
decode_impl!(u16);
decode_impl!(u32);
decode_impl!(i32);
decode_impl!(f64);

macro_rules! encode_impl {
    ($t:ty) => {
        paste! {
            fn [<encode_ $t>](buf: &mut &mut [u8], amt: &mut usize, n: $t) {
                let src = n.to_be_bytes();
                let data = std::mem::take(buf);
                let (int_bytes, rest) = data.split_at_mut(size_of::<$t>());
                int_bytes.copy_from_slice(&src);
                *amt += size_of::<$t>();
                *buf = rest
            }
        }
    };
}

encode_impl!(u8);
encode_impl!(u16);
encode_impl!(u32);
encode_impl!(i32);
encode_impl!(f64);

#[cfg(test)]
mod tests {
    use super::*;

    /// every encoded event has to fit the fixed-size datagram buffer
    fn roundtrip(event: ProtoEvent) -> ProtoEvent {
        let (buf, len) = <([u8; MAX_EVENT_SIZE], usize)>::from(event);
        assert!(len <= MAX_EVENT_SIZE, "{event} overflows the event buffer");
        ProtoEvent::try_from(buf).expect("decodes")
    }

    #[test]
    fn touchpad_device_roundtrip() {
        let event = ProtoEvent::Input(InputEvent::Touchpad(TouchpadEvent::Device {
            width_mm: 162,
            height_mm: 115,
            slots: 5,
            pressure_max: 511,
        }));
        assert!(matches!(
            roundtrip(event),
            ProtoEvent::Input(InputEvent::Touchpad(TouchpadEvent::Device {
                width_mm: 162,
                height_mm: 115,
                slots: 5,
                pressure_max: 511,
            }))
        ));
    }

    fn frame(events: &[(u16, u16, i32)]) -> TouchpadEvent {
        let mut buf = [TouchpadAxisEvent::default(); MAX_FRAME_EVENTS];
        for (slot, &(kind, code, value)) in buf.iter_mut().zip(events) {
            *slot = TouchpadAxisEvent { kind, code, value };
        }
        TouchpadEvent::Frame {
            time: 0xdead_beef,
            count: events.len() as u8,
            events: buf,
        }
    }

    #[test]
    fn touchpad_frame_roundtrip() {
        // ABS_MT_TRACKING_ID = -1 (finger lifted) must survive as a
        // negative value - a stuck slot is the worst failure mode here
        let sent = frame(&[
            (0x03, 0x2f, 0),  // ABS_MT_SLOT 0
            (0x03, 0x39, -1), // ABS_MT_TRACKING_ID -1
            (0x00, 0x00, 0),  // SYN_REPORT
        ]);
        let event = ProtoEvent::Input(InputEvent::Touchpad(sent));
        match roundtrip(event) {
            ProtoEvent::Input(InputEvent::Touchpad(got)) => assert_eq!(got, sent),
            other => panic!("decoded to {other}"),
        }
    }

    #[test]
    fn a_full_frame_fits_one_datagram() {
        // the whole point of batching: a frame must never be split by the
        // wire buffer, or loss corrupts frames instead of dropping samples
        let events: Vec<(u16, u16, i32)> = (0..MAX_FRAME_EVENTS)
            .map(|i| (0x03, 0x35, i as i32))
            .collect();
        let sent = frame(&events);
        let (_, len) =
            <([u8; MAX_EVENT_SIZE], usize)>::from(ProtoEvent::Input(InputEvent::Touchpad(sent)));
        assert!(len <= MAX_EVENT_SIZE, "{len} > {MAX_EVENT_SIZE}");
        match roundtrip(ProtoEvent::Input(InputEvent::Touchpad(sent))) {
            ProtoEvent::Input(InputEvent::Touchpad(got)) => assert_eq!(got, sent),
            other => panic!("decoded to {other}"),
        }
    }

    #[test]
    fn overlong_frame_is_rejected() {
        let mut buf = [0u8; MAX_EVENT_SIZE];
        buf[0] = EventType::TouchpadFrame.into();
        buf[5] = MAX_FRAME_EVENTS as u8 + 1;
        assert!(matches!(
            ProtoEvent::try_from(buf),
            Err(ProtocolError::FrameTooLong(_))
        ));
    }

    #[test]
    fn appended_event_types_keep_existing_discriminants() {
        // both ends must agree on the wire; appending must not shift
        // the events that existed before touchpad support
        assert_eq!(u8::from(EventType::PointerMotion), 0);
        assert_eq!(u8::from(EventType::Ack), 10);
        assert_eq!(u8::from(EventType::TouchpadDevice), 11);
        assert_eq!(u8::from(EventType::TouchpadFrame), 12);
    }
}
