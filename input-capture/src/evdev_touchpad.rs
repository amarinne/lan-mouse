//! Raw touchpad capture.
//!
//! Touchpad gestures are never interpreted here. While a client is active the
//! local touchpad is grabbed with `EVIOCGRAB` and its evdev frames are
//! forwarded verbatim, so the *receiving* machine's libinput performs gesture
//! recognition on a virtual multitouch device. That is the only way to move
//! compositor-level gestures across the wire: a compositor such as niri
//! consumes 3- and 4-finger swipes internally and never forwards them to any
//! Wayland client, so no client-side capture can observe them.
//!
//! The grab is held only while capture is active, so local touchpad behaviour
//! is untouched the rest of the time - and because a grab dies with the
//! process, a crash restores the local touchpad automatically.

use std::{
    collections::VecDeque,
    io,
    path::PathBuf,
    task::{Context, Poll},
};

use evdev::{AbsoluteAxisCode, Device, EventStream, EventType, KeyCode, PropType};
use input_event::{TOUCHPAD_AXIS_MAX, TouchpadEvent};

/// Linear map from a device axis onto `0..=TOUCHPAD_AXIS_MAX`.
#[derive(Clone, Copy)]
struct AxisScale {
    min: i32,
    span: i32,
    size_mm: u16,
}

impl AxisScale {
    fn new(min: i32, max: i32, resolution: i32) -> Self {
        let span = (max - min).max(1);
        // resolution is in units per mm; without it the source size is
        // unknown and a plausible default beats a degenerate one
        let size_mm = if resolution > 0 {
            (span / resolution).clamp(1, u16::MAX as i32) as u16
        } else {
            100
        };
        Self { min, span, size_mm }
    }

    fn normalize(&self, value: i32) -> i32 {
        let offset = (value - self.min).clamp(0, self.span) as i64;
        (offset * TOUCHPAD_AXIS_MAX as i64 / self.span as i64) as i32
    }
}

/// One touchpad node that *might* be the live one.
struct Candidate {
    name: String,
    path: PathBuf,
    stream: EventStream,
    x: AxisScale,
    y: AxisScale,
    slots: u8,
    pressure_max: i32,
    grabbed: bool,
}

impl Candidate {
    fn description(&self) -> TouchpadEvent {
        TouchpadEvent::Device {
            width_mm: self.x.size_mm,
            height_mm: self.y.size_mm,
            slots: self.slots,
            pressure_max: self.pressure_max,
        }
    }

    fn grab(&mut self) {
        if self.grabbed {
            return;
        }
        match self.stream.device_mut().grab() {
            Ok(()) => self.grabbed = true,
            // not fatal: pointer and keyboard capture still work, only
            // gestures from this node stay local
            Err(e) => log::warn!("could not grab {} ({:?}): {e}", self.name, self.path),
        }
    }

    fn ungrab(&mut self) {
        if !self.grabbed {
            return;
        }
        if let Err(e) = self.stream.device_mut().ungrab() {
            log::warn!("could not ungrab {} ({:?}): {e}", self.name, self.path);
        }
        self.grabbed = false;
    }

    /// Poll one event, rescaled onto the normalized axes.
    fn poll_frame(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<TouchpadEvent>> {
        let event = match self.stream.poll_event(cx) {
            Poll::Ready(Ok(event)) => event,
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Pending => return Poll::Pending,
        };

        let kind = event.event_type().0;
        let code = event.code();
        let mut value = event.value();

        if kind == EventType::ABSOLUTE.0 {
            // positions are the only device-specific values; everything
            // else (tracking ids, tool bits, pressure) is passed through
            if code == AbsoluteAxisCode::ABS_MT_POSITION_X.0 || code == AbsoluteAxisCode::ABS_X.0 {
                value = self.x.normalize(value);
            } else if code == AbsoluteAxisCode::ABS_MT_POSITION_Y.0
                || code == AbsoluteAxisCode::ABS_Y.0
            {
                value = self.y.normalize(value);
            }
        }

        Poll::Ready(Ok(TouchpadEvent::Frame {
            time: 0,
            kind,
            code,
            value,
        }))
    }
}

/// Capture of the local touchpad.
///
/// A single physical touchpad can expose several indistinguishable event
/// nodes - clones that borrow another vendor's USB id are a common cause,
/// since the kernel then binds more than one interface as a touchpad and only
/// one of them ever reports contacts. Rather than guess, every candidate is
/// grabbed and the first one to actually deliver an event wins; the rest are
/// released immediately.
pub(crate) struct TouchpadCapture {
    candidates: Vec<Candidate>,
    /// index of the node that produced an event, once one has
    committed: Option<usize>,
    /// queued events, used to put the device description ahead of the frame
    /// that triggered the commit
    pending: VecDeque<TouchpadEvent>,
    grabbed: bool,
}

/// A touchpad reports multitouch positions and is not a direct-input device.
///
/// `INPUT_PROP_DIRECT` distinguishes touchscreens and drawing tablets, which
/// report the same axes but must not be grabbed.
fn is_touchpad(device: &Device) -> bool {
    let props = device.properties();
    if props.contains(PropType::DIRECT) {
        return false;
    }
    if !props.contains(PropType::POINTER) && !props.contains(PropType::BUTTONPAD) {
        return false;
    }

    let has_mt = device.supported_absolute_axes().is_some_and(|axes| {
        axes.contains(AbsoluteAxisCode::ABS_MT_POSITION_X)
            && axes.contains(AbsoluteAxisCode::ABS_MT_POSITION_Y)
    });

    let has_finger = device
        .supported_keys()
        .is_some_and(|keys| keys.contains(KeyCode::BTN_TOOL_FINGER));

    has_mt && has_finger
}

fn candidate(path: PathBuf, device: Device) -> io::Result<Candidate> {
    let name = device.name().unwrap_or("unnamed").to_owned();

    let axis = |axis: AbsoluteAxisCode| -> io::Result<AxisScale> {
        let info = device
            .get_absinfo()?
            .find(|(code, _)| *code == axis)
            .map(|(_, info)| info)
            .ok_or_else(|| io::Error::other(format!("touchpad lacks axis {axis:?}")))?;
        Ok(AxisScale::new(
            info.minimum(),
            info.maximum(),
            info.resolution(),
        ))
    };

    let x = axis(AbsoluteAxisCode::ABS_MT_POSITION_X)?;
    let y = axis(AbsoluteAxisCode::ABS_MT_POSITION_Y)?;

    let slots = device
        .get_absinfo()?
        .find(|(code, _)| *code == AbsoluteAxisCode::ABS_MT_SLOT)
        .map(|(_, info)| (info.maximum() - info.minimum() + 1).clamp(1, u8::MAX as i32) as u8)
        .unwrap_or(5);

    let pressure_max = device
        .get_absinfo()?
        .find(|(code, _)| *code == AbsoluteAxisCode::ABS_MT_PRESSURE)
        .map(|(_, info)| info.maximum())
        .unwrap_or(0);

    Ok(Candidate {
        name,
        path,
        stream: device.into_event_stream()?,
        x,
        y,
        slots,
        pressure_max,
        grabbed: false,
    })
}

impl TouchpadCapture {
    /// Open every local touchpad, or `None` when the machine has none.
    pub(crate) fn find() -> Option<Self> {
        let mut candidates = Vec::new();
        for (path, device) in evdev::enumerate() {
            if !is_touchpad(&device) {
                continue;
            }
            match candidate(path.clone(), device) {
                Ok(candidate) => {
                    log::info!("touchpad candidate: {} ({path:?})", candidate.name);
                    candidates.push(candidate);
                }
                Err(e) => log::warn!("cannot use touchpad at {path:?}: {e}"),
            }
        }

        if candidates.is_empty() {
            log::debug!("no local touchpad found - gesture forwarding disabled");
            return None;
        }

        Some(Self {
            candidates,
            committed: None,
            pending: VecDeque::new(),
            grabbed: false,
        })
    }

    /// Take exclusive ownership so the local compositor stops seeing the
    /// touchpad for the duration of the capture.
    pub(crate) fn grab(&mut self) {
        if self.grabbed {
            return;
        }
        for candidate in &mut self.candidates {
            candidate.grab();
        }
        self.grabbed = self.candidates.iter().any(|c| c.grabbed);
        if self.grabbed {
            log::debug!("grabbed local touchpad");
        }
    }

    pub(crate) fn ungrab(&mut self) {
        if !self.grabbed {
            return;
        }
        for candidate in &mut self.candidates {
            candidate.ungrab();
        }
        self.grabbed = false;
        // the live node is re-elected on the next capture: a replug can
        // renumber the nodes underneath us
        self.committed = None;
        self.pending.clear();
        log::debug!("released local touchpad");
    }

    pub(crate) fn is_grabbed(&self) -> bool {
        self.grabbed
    }

    /// Poll the committed node, electing one if that has not happened yet.
    pub(crate) fn poll_event(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<TouchpadEvent>> {
        if let Some(event) = self.pending.pop_front() {
            return Poll::Ready(Ok(event));
        }

        if let Some(index) = self.committed {
            return self.candidates[index].poll_frame(cx);
        }

        // No node has spoken yet - poll all of them so every waker is
        // registered, and let the first event decide.
        for index in 0..self.candidates.len() {
            if !self.candidates[index].grabbed {
                continue;
            }
            match self.candidates[index].poll_frame(cx) {
                Poll::Ready(Ok(frame)) => {
                    self.commit(index);
                    // the receiver needs the geometry before the frame
                    self.pending.push_back(self.candidates[index].description());
                    self.pending.push_back(frame);
                    return Poll::Ready(Ok(self.pending.pop_front().expect("queued")));
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => {}
            }
        }

        Poll::Pending
    }

    /// Keep the node that produced an event and release the others.
    fn commit(&mut self, index: usize) {
        log::info!(
            "forwarding touchpad gestures from {} ({:?})",
            self.candidates[index].name,
            self.candidates[index].path
        );
        for (i, candidate) in self.candidates.iter_mut().enumerate() {
            if i != index {
                candidate.ungrab();
            }
        }
        self.committed = Some(index);
    }
}

impl Drop for TouchpadCapture {
    fn drop(&mut self) {
        self.ungrab();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn axis_maps_onto_full_normalized_range() {
        let axis = AxisScale::new(0, 2560, 18);
        assert_eq!(axis.normalize(0), 0);
        assert_eq!(axis.normalize(2560), TOUCHPAD_AXIS_MAX);
        assert_eq!(axis.size_mm, 142);
    }

    #[test]
    fn axis_handles_nonzero_minimum() {
        let axis = AxisScale::new(-2048, 2048, 0);
        assert_eq!(axis.normalize(-2048), 0);
        assert_eq!(axis.normalize(2048), TOUCHPAD_AXIS_MAX);
        // no resolution reported - fall back rather than divide by zero
        assert_eq!(axis.size_mm, 100);
    }

    #[test]
    fn axis_clamps_out_of_range_values() {
        let axis = AxisScale::new(0, 1000, 10);
        assert_eq!(axis.normalize(-50), 0);
        assert_eq!(axis.normalize(5000), TOUCHPAD_AXIS_MAX);
    }
}
