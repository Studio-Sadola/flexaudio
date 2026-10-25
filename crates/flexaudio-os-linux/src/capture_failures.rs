// Shared capture failure policies and panic boundaries.
use crate::owner::{push_backend_event, BackendEvents};
use crate::readiness::Readiness;
use flexaudio_core::{CaptureDiagnostics, Error, ErrorContext, Event, Operation};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;

#[derive(Clone)]
pub(crate) struct CaptureCallbacks {
    pub(crate) readiness: Rc<Readiness>,
    pub(crate) events: BackendEvents,
    quit: Rc<dyn Fn()>,
}

impl CaptureCallbacks {
    pub(crate) fn new(
        readiness: Rc<Readiness>,
        events: BackendEvents,
        quit: impl Fn() + 'static,
    ) -> Self {
        Self {
            readiness,
            events,
            quit: Rc::new(quit),
        }
    }
    pub(crate) fn quit(&self) {
        (self.quit)();
    }
    pub(crate) fn run(&self, body: impl FnOnce()) {
        run_capture_callback(&self.readiness, &self.events, || self.quit(), body);
    }
    pub(crate) fn fail(&self, error: Error) {
        report_control_failure(&self.readiness, &self.events, error);
        self.quit();
    }
}

pub(crate) fn report_control_failure(readiness: &Readiness, events: &BackendEvents, error: Error) {
    if readiness.is_started() {
        push_backend_event(events, Event::TerminalError { error });
    } else {
        readiness.report_failure(error);
    }
}

pub(crate) fn run_capture_callback(
    readiness: &Readiness,
    events: &BackendEvents,
    quit: impl FnOnce(),
    body: impl FnOnce(),
) {
    if catch_unwind(AssertUnwindSafe(body)).is_err() {
        report_control_failure(
            readiness,
            events,
            Error::Backend(
                "PipeWire capture callback failed; restart the audio session and reopen capture."
                    .into(),
            )
            .with_context(ErrorContext::new(Operation::Link)),
        );
        quit();
    }
}

pub(crate) fn report_bind_failure(
    readiness: &Readiness,
    events: &BackendEvents,
    diagnostics: &CaptureDiagnostics,
    error: Error,
) -> bool {
    if !readiness.is_started() {
        report_control_failure(readiness, events, error);
        true
    } else {
        diagnostics.record_callback_rejected(None);
        diagnostics.mark_discontinuity();
        push_backend_event(events, Event::RecoverableError { error });
        false
    }
}

// SPA offsets are modulo maxsize and sizes are clamped to maxsize. Any positive
// chunk that cannot be mapped is a rejection, never an empty/silent buffer.
pub(crate) fn checked_buffer_region<'a>(
    bytes: Option<&'a [u8]>,
    offset: usize,
    size: usize,
    diagnostics: &CaptureDiagnostics,
) -> Option<&'a [u8]> {
    if size == 0 {
        return None;
    }
    let region = bytes.filter(|bytes| !bytes.is_empty()).and_then(|bytes| {
        let offset = offset % bytes.len();
        let size = size.min(bytes.len());
        offset
            .checked_add(size)
            .filter(|end| *end <= bytes.len())
            .map(|end| &bytes[offset..end])
    });
    if region.is_none() {
        diagnostics.record_malformed_buffer(None);
    }
    region
}

// A rejected renegotiation invalidates the old decoder state. Record the loss
// now; subsequent positive chunks are rejected until a new format is accepted.
pub(crate) fn replace_format(
    current: &mut pipewire::spa::param::audio::AudioInfoRaw,
    next: pipewire::spa::param::audio::AudioInfoRaw,
    accepted: bool,
    diagnostics: &CaptureDiagnostics,
) -> bool {
    if accepted {
        *current = next;
    } else {
        *current = pipewire::spa::param::audio::AudioInfoRaw::new();
        diagnostics.record_malformed_buffer(None);
        diagnostics.mark_discontinuity();
    }
    accepted
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::{mpsc, Arc, Mutex};

    #[test]
    fn positive_unmapped_empty_or_out_of_bounds_buffers_report_malformed_once() {
        let bytes = [0; 16];
        for (data, offset, size) in [
            (None, 0, 8),
            (Some(&[][..]), 0, 8),
            (Some(&bytes[..]), 12, 8),
        ] {
            let diagnostics = CaptureDiagnostics::new(48_000, 2);
            assert!(checked_buffer_region(data, offset, size, &diagnostics).is_none());
            let losses = diagnostics.drain().unwrap();
            assert_eq!(losses.len(), 1);
            assert_eq!(
                losses[0].reason(),
                flexaudio_core::LossReason::MalformedBuffer
            );
            assert!(diagnostics.drain().unwrap().is_empty());
        }
        let diagnostics = CaptureDiagnostics::new(48_000, 2);
        assert!(checked_buffer_region(None, 0, 0, &diagnostics).is_none());
        assert!(diagnostics.drain().unwrap().is_empty());
        assert_eq!(
            checked_buffer_region(Some(&bytes), 16, 8, &diagnostics),
            Some(&bytes[..8])
        );
    }

    #[test]
    fn rejected_renegotiation_clears_stale_format_and_records_loss() {
        use pipewire::spa::param::audio::AudioInfoRaw;
        let diagnostics = CaptureDiagnostics::new(48_000, 2);
        let mut current = AudioInfoRaw::new();
        current.set_channels(2);
        current.set_rate(48_000);
        assert!(!replace_format(
            &mut current,
            AudioInfoRaw::new(),
            false,
            &diagnostics
        ));
        assert_eq!(current.channels(), 0);
        assert_eq!(
            diagnostics.drain().unwrap()[0].reason(),
            flexaudio_core::LossReason::MalformedBuffer
        );
        assert!(diagnostics.take_discontinuity());
        let mut next = AudioInfoRaw::new();
        next.set_channels(2);
        next.set_rate(48_000);
        assert!(replace_format(&mut current, next, true, &diagnostics));
        assert_eq!(current.channels(), 2);
        assert!(diagnostics.drain().unwrap().is_empty());
    }

    #[test]
    fn bind_failure_is_returned_before_readiness_and_observable_after() {
        for ready in [false, true] {
            let (tx, rx) = mpsc::channel();
            let readiness = Readiness::new(tx, true);
            if ready {
                readiness.report_ready();
                assert_eq!(rx.recv().unwrap(), Ok(()));
            }
            let diagnostics = CaptureDiagnostics::new(48_000, 2);
            let events = Arc::new(Mutex::new(Default::default()));
            let error = Error::Backend("synthetic bind failure; retry".into())
                .with_context(ErrorContext::new(Operation::Link));
            assert_eq!(
                report_bind_failure(&readiness, &events, &diagnostics, error.clone()),
                !ready
            );
            if ready {
                assert_eq!(
                    crate::owner::poll_backend_event(&events),
                    Some(Event::RecoverableError { error })
                );
                assert_eq!(
                    diagnostics.drain().unwrap()[0].reason(),
                    flexaudio_core::LossReason::CallbackRejected
                );
                assert!(diagnostics.take_discontinuity());
            } else {
                assert!(crate::owner::poll_backend_event(&events).is_none());
                assert_eq!(rx.recv().unwrap(), Err(error.clone()));
            }
        }
    }

    #[test]
    fn callback_panic_reports_terminal_and_quits_without_exposing_panic_text() {
        for started in [false, true] {
            let (tx, rx) = mpsc::channel();
            let readiness = Readiness::new(tx, true);
            if started {
                readiness.report_ready();
                assert_eq!(rx.recv(), Ok(Ok(())));
            }
            let events = Arc::new(Mutex::new(Default::default()));
            let quit = Cell::new(false);
            run_capture_callback(
                &readiness,
                &events,
                || quit.set(true),
                || panic!("synthetic failure"),
            );
            assert!(quit.get());
            if started {
                assert!(
                    matches!(crate::owner::poll_backend_event(&events), Some(Event::TerminalError { error }) if error.kind() == flexaudio_core::ErrorKind::Backend && !error.to_string().contains("synthetic failure"))
                );
            } else {
                let error = rx.recv().unwrap().unwrap_err();
                assert_eq!(error.kind(), flexaudio_core::ErrorKind::Backend);
                assert!(!error.to_string().contains("synthetic failure"));
                assert!(crate::owner::poll_backend_event(&events).is_none());
            }
        }
    }

    #[test]
    fn rejected_readiness_cannot_publish_a_runtime_terminal_failure() {
        let (tx, rx) = mpsc::channel();
        let readiness = Readiness::new(tx, true);
        let events = BackendEvents::default();
        report_control_failure(&readiness, &events, Error::DeviceNotFound);
        assert_eq!(rx.recv(), Ok(Err(Error::DeviceNotFound)));
        report_control_failure(&readiness, &events, Error::DeviceLost);
        assert!(crate::owner::poll_backend_event(&events).is_none());
    }
}
