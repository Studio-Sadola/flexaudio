//! Separate ordinary and helper lifecycle queues, shared by every stream poll.
use crate::{
    boundary::{invalid, output},
    convert::valid_pointer,
    error, guard_i32,
    types::FlexStream,
};
use flexaudio::{Event, HelperStopReason};
use std::{collections::VecDeque, ffi::CString, os::raw::c_char};

pub const FLEX_LIFECYCLE_UNKNOWN: u32 = 0;
pub const FLEX_LIFECYCLE_SILENCE_PROBE_STARTED: u32 = 1;
pub const FLEX_LIFECYCLE_SILENCE_PROBE_STOPPED: u32 = 2;
pub const FLEX_LIFECYCLE_CLASSIC_LOOPBACK_KEEPALIVE_STARTED: u32 = 3;
pub const FLEX_LIFECYCLE_CLASSIC_LOOPBACK_KEEPALIVE_STOPPED: u32 = 4;
const LIFECYCLE_QUEUE_CAPACITY: usize = 256;

/// Owned immutable lifecycle notification, independent of stream lifetime.
pub struct FlexLifecycleEventV4 {
    kind: u32,
    reason: Option<u32>,
    message: Option<CString>,
}
fn reason_code(reason: HelperStopReason) -> u32 {
    match reason {
        HelperStopReason::SignalObserved => 1,
        HelperStopReason::PermissionDenied => 2,
        HelperStopReason::Inconclusive => 3,
        HelperStopReason::TimedOut => 4,
        HelperStopReason::StreamStopped => 5,
        HelperStopReason::SourceSwitched => 6,
        HelperStopReason::Recovery => 7,
        HelperStopReason::StartupFailed => 8,
        HelperStopReason::RuntimeFailed => 9,
        HelperStopReason::Cancelled => 10,
        _ => 0,
    }
}
impl FlexLifecycleEventV4 {
    fn from_event(event: &Event) -> Option<Self> {
        let (kind, reason) = match event {
            Event::SilenceProbeStarted => (FLEX_LIFECYCLE_SILENCE_PROBE_STARTED, None),
            Event::SilenceProbeStopped { reason } => (
                FLEX_LIFECYCLE_SILENCE_PROBE_STOPPED,
                Some(reason_code(*reason)),
            ),
            Event::ClassicLoopbackKeepaliveStarted => {
                (FLEX_LIFECYCLE_CLASSIC_LOOPBACK_KEEPALIVE_STARTED, None)
            }
            Event::ClassicLoopbackKeepaliveStopped { reason } => (
                FLEX_LIFECYCLE_CLASSIC_LOOPBACK_KEEPALIVE_STOPPED,
                Some(reason_code(*reason)),
            ),
            _ => return None,
        };
        Some(Self {
            kind,
            reason,
            message: None,
        })
    }
}
#[derive(Default)]
pub(crate) struct EventQueues {
    pub(crate) ordinary: VecDeque<Event>,
    lifecycle: VecDeque<FlexLifecycleEventV4>,
}
impl EventQueues {
    pub(crate) fn route(&mut self, event: Event) {
        if let Some(event) = FlexLifecycleEventV4::from_event(&event) {
            if self.lifecycle.len() == LIFECYCLE_QUEUE_CAPACITY {
                self.lifecycle.pop_front();
            }
            self.lifecycle.push_back(event);
        } else {
            self.ordinary.push_back(event);
        }
    }
}
impl FlexStream {
    /// The caller holds stream.events for the entire drain and subsequent pop.
    /// No reference to the whole stream may overlap another event poll.
    pub(crate) unsafe fn drain_binding_events(stream: *mut Self, queues: &mut EventQueues) {
        while let Some(event) = (*stream).inner.poll_event() {
            queues.route(event);
        }
    }
}
/// Poll the lifecycle FIFO independently of ordinary polls, including after stop.
/// Return 1 with an owned event, 0 empty, or a negative error; out is NULL on absence/error.
/// The FIFO retains at most 256 events, dropping the oldest on overflow. Started/Stopped
/// pairing is guaranteed only while the caller drains it fast enough to avoid overflow.
/// # Safety
/// stream is live and exclusively borrowed, out is writable. Use a stream from one
/// thread at a time or externally serialize its calls, including lifecycle polling.
/// Internal event-queue locking does not make other stream operations thread-safe.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_poll_lifecycle_event_v4(
    stream: *mut FlexStream,
    out: *mut *mut FlexLifecycleEventV4,
) -> i32 {
    guard_i32(|| {
        error::clear_last_error();
        if let Err(status) = output(out) {
            return status;
        }
        if !valid_pointer(stream) {
            return invalid("invalid stream handle");
        }
        let mut queues = (*stream).events.lock().unwrap_or_else(|e| e.into_inner());
        FlexStream::drain_binding_events(stream, &mut queues);
        match queues.lifecycle.pop_front() {
            Some(event) => {
                out.write(Box::into_raw(Box::new(event)));
                1
            }
            None => 0,
        }
    })
}
/// Return the lifecycle kind, zero on success.
/// # Safety
/// event is live, out is writable.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_lifecycle_event_kind_v4(
    event: *const FlexLifecycleEventV4,
    out: *mut u32,
) -> i32 {
    guard_i32(|| {
        error::clear_last_error();
        if !valid_pointer(event) || !valid_pointer(out) {
            return invalid("invalid lifecycle pointer");
        }
        out.write((*event).kind);
        0
    })
}
/// Return 1 and a stop-reason code (1-10, 0 unknown), or 0 without modifying out.
/// # Safety
/// event is live, out is writable.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_lifecycle_event_reason_v4(
    event: *const FlexLifecycleEventV4,
    out: *mut u32,
) -> i32 {
    guard_i32(|| {
        error::clear_last_error();
        if !valid_pointer(event) || !valid_pointer(out) {
            return invalid("invalid lifecycle pointer");
        }
        match (*event).reason {
            Some(reason) => {
                out.write(reason);
                1
            }
            None => 0,
        }
    })
}
/// Return 1 and a borrowed safe message, or 0 without modifying out.
/// # Safety
/// event is live, out is writable. The string borrows from event.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_lifecycle_event_message_v4(
    event: *const FlexLifecycleEventV4,
    out: *mut *const c_char,
) -> i32 {
    guard_i32(|| {
        error::clear_last_error();
        if !valid_pointer(event) || !valid_pointer(out) {
            return invalid("invalid lifecycle pointer");
        }
        match (*event).message.as_ref() {
            Some(message) => {
                out.write(message.as_ptr());
                1
            }
            None => 0,
        }
    })
}
/// Free a lifecycle owner; NULL-safe.
/// # Safety
/// event is NULL or an unfreed owner returned by poll_lifecycle_event_v4.
#[no_mangle]
pub unsafe extern "C" fn flexaudio_lifecycle_event_free_v4(event: *mut FlexLifecycleEventV4) {
    guard_i32(|| {
        if event.is_null() {
            return 0;
        }
        if !valid_pointer(event) {
            return invalid("invalid lifecycle owner");
        }
        drop(Box::from_raw(event));
        0
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lifecycle_fifo_drops_oldest_and_keeps_latest_events_in_order() {
        let mut queues = EventQueues::default();
        queues.route(Event::SilenceProbeStarted);
        for _ in 0..4096 {
            queues.route(Event::ClassicLoopbackKeepaliveStarted);
            assert!(queues.lifecycle.len() <= LIFECYCLE_QUEUE_CAPACITY);
        }
        queues.route(Event::StreamStalled);
        queues.route(Event::SilenceProbeStopped {
            reason: HelperStopReason::Recovery,
        });
        assert_eq!(queues.lifecycle.len(), LIFECYCLE_QUEUE_CAPACITY);
        for _ in 0..LIFECYCLE_QUEUE_CAPACITY - 1 {
            assert_eq!(
                queues.lifecycle.pop_front().unwrap().kind,
                FLEX_LIFECYCLE_CLASSIC_LOOPBACK_KEEPALIVE_STARTED
            );
        }
        let latest = queues.lifecycle.pop_front().unwrap();
        assert_eq!(latest.kind, FLEX_LIFECYCLE_SILENCE_PROBE_STOPPED);
        assert_eq!(latest.reason, Some(7));
        assert!(queues.lifecycle.is_empty());
        assert!(matches!(
            queues.ordinary.pop_front(),
            Some(Event::StreamStalled)
        ));
        assert!(queues.ordinary.is_empty());
    }

    #[test]
    fn all_stop_reason_codes_follow_enum_order() {
        for (index, reason) in [
            HelperStopReason::SignalObserved,
            HelperStopReason::PermissionDenied,
            HelperStopReason::Inconclusive,
            HelperStopReason::TimedOut,
            HelperStopReason::StreamStopped,
            HelperStopReason::SourceSwitched,
            HelperStopReason::Recovery,
            HelperStopReason::StartupFailed,
            HelperStopReason::RuntimeFailed,
            HelperStopReason::Cancelled,
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(reason_code(reason), u32::try_from(index + 1).unwrap());
        }
    }
    #[test]
    fn unknown_owner_retains_safe_message_and_unknown_reason() {
        let event = FlexLifecycleEventV4 {
            kind: FLEX_LIFECYCLE_UNKNOWN,
            reason: Some(0),
            message: Some(crate::v2_storage::cstring("unknown helper lifecycle event")),
        };
        unsafe {
            let mut kind = 99;
            let mut reason = 99;
            let mut message = std::ptr::null();
            assert_eq!(flexaudio_lifecycle_event_kind_v4(&event, &mut kind), 0);
            assert_eq!(kind, 0);
            assert_eq!(flexaudio_lifecycle_event_reason_v4(&event, &mut reason), 1);
            assert_eq!(reason, 0);
            assert_eq!(
                flexaudio_lifecycle_event_message_v4(&event, &mut message),
                1
            );
            assert_eq!(
                std::ffi::CStr::from_ptr(message),
                c"unknown helper lifecycle event"
            );
        }
    }
}
