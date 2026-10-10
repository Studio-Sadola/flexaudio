//! One retained join outcome for both WASAPI backends.
use std::collections::VecDeque;
use std::sync::mpsc;
use std::thread::JoinHandle;

use flexaudio_core::{Error, ErrorContext, Event, Operation, Result, ShutdownReport};

pub(crate) fn join_owner(handle: JoinHandle<ShutdownReport>) -> ShutdownReport {
    match handle.join() {
        Ok(report) => report,
        Err(_) => ShutdownReport::new(
            None,
            vec![Error::Backend("capture owner thread panicked".into())
                .with_context(ErrorContext::new(Operation::Join))],
        ),
    }
}

/// Publish capture terminality before cleanup, retaining each cleanup separately.
pub(crate) fn finish_capture(
    capture: Result<()>,
    stop: impl FnOnce() -> Result<()>,
    events: &mpsc::Sender<Event>,
) -> ShutdownReport {
    let primary = capture.err();
    if let Some(error) = &primary {
        let _ = events.send(Event::TerminalError {
            error: error.clone(),
        });
    }
    let cleanup = match stop() {
        Ok(()) => Vec::new(),
        Err(Error::Multiple(group)) => std::iter::once(group.primary().clone())
            .chain(group.secondary().cloned())
            .collect(),
        Err(error) => vec![error],
    };
    ShutdownReport::new(primary, cleanup)
}

#[derive(Default)]
pub(crate) struct OwnerShutdown {
    report: Option<ShutdownReport>,
    events: VecDeque<Event>,
    capture_events: Option<mpsc::Receiver<Event>>,
}

impl OwnerShutdown {
    pub(crate) fn reset(&mut self, capture_events: mpsc::Receiver<Event>) {
        self.report = None;
        self.events.clear();
        self.capture_events = Some(capture_events);
    }

    pub(crate) fn finish(&mut self, handle: Option<JoinHandle<ShutdownReport>>) -> Result<()> {
        if self.report.is_none() {
            let report = handle
                .map(join_owner)
                .unwrap_or_else(|| ShutdownReport::new(None, Vec::new()));
            self.events.extend(
                report
                    .cleanup()
                    .iter()
                    .cloned()
                    .map(|error| Event::ShutdownError { error }),
            );
            self.report = Some(report);
        }
        self.report
            .as_ref()
            .expect("completed owner shutdown")
            .result()
    }

    pub(crate) fn poll_event(&mut self) -> Option<Event> {
        self.capture_events
            .as_ref()
            .and_then(|events| events.try_recv().ok())
            .or_else(|| self.events.pop_front())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio_core::{ErrorKind, Permission};
    use std::thread;

    #[test]
    fn capture_failure_publishes_before_cleanup_and_retains_both_outcomes() {
        let primary = Error::PermissionDenied {
            permission: Permission::SystemAudio,
            detail: "injected GetBuffer access denial".into(),
        };
        let cleanup = Error::Backend("injected capture stop failure".into())
            .with_context(ErrorContext::new(Operation::Stop));
        let (events_tx, events_rx) = mpsc::channel();
        let (stopping_tx, stopping_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut shutdown = OwnerShutdown::default();
        shutdown.reset(events_rx);
        let capture_error = primary.clone();
        let stop_error = cleanup.clone();
        let handle = thread::spawn(move || {
            finish_capture(
                Err(capture_error),
                || {
                    stopping_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Err(stop_error)
                },
                &events_tx,
            )
        });

        // The event is readable while the owner is still inside native cleanup.
        stopping_rx.recv().unwrap();
        assert!(!handle.is_finished());
        assert!(
            matches!(shutdown.poll_event(), Some(Event::TerminalError { error }) if error == primary)
        );
        release_tx.send(()).unwrap();
        let result = shutdown.finish(Some(handle));
        let report = shutdown.report.as_ref().unwrap();
        assert_eq!(report.primary(), Some(&primary));
        assert_eq!(report.cleanup(), std::slice::from_ref(&cleanup));
        let Error::Multiple(group) = result.as_ref().unwrap_err() else {
            panic!("capture primary and cleanup must both survive")
        };
        assert_eq!(group.primary(), &primary);
        assert_eq!(group.secondary().collect::<Vec<_>>(), [&cleanup]);
        assert!(
            matches!(shutdown.poll_event(), Some(Event::ShutdownError { error }) if error == cleanup)
        );
        assert_eq!(shutdown.finish(None), result);
        assert!(shutdown.poll_event().is_none());
    }

    #[test]
    fn capture_failure_with_clean_stop_is_primary_without_shutdown_error() {
        let (events_tx, events_rx) = mpsc::channel();
        let mut shutdown = OwnerShutdown::default();
        shutdown.reset(events_rx);
        let handle =
            thread::spawn(move || finish_capture(Err(Error::DeviceLost), || Ok(()), &events_tx));
        assert_eq!(shutdown.finish(Some(handle)), Err(Error::DeviceLost));
        let report = shutdown.report.as_ref().unwrap();
        assert_eq!(report.primary(), Some(&Error::DeviceLost));
        assert!(report.cleanup().is_empty());
        assert!(matches!(
            shutdown.poll_event(),
            Some(Event::TerminalError {
                error: Error::DeviceLost
            })
        ));
        assert!(shutdown.poll_event().is_none());
    }

    #[test]
    fn cleanup_only_preserves_each_error_without_capture_terminality() {
        let cleanup = vec![
            Error::DeviceLost,
            Error::Backend("render stop failure".into()),
        ];
        let expected = ShutdownReport::new(None, cleanup.clone());
        let stopped = expected.result();
        let (events_tx, events_rx) = mpsc::channel();
        let mut shutdown = OwnerShutdown::default();
        shutdown.reset(events_rx);
        let handle = thread::spawn(move || finish_capture(Ok(()), || stopped, &events_tx));
        assert_eq!(shutdown.finish(Some(handle)), expected.result());
        assert_eq!(shutdown.report.as_ref(), Some(&expected));
        for cause in cleanup {
            assert!(
                matches!(shutdown.poll_event(), Some(Event::ShutdownError { error }) if error == cause)
            );
        }
        assert!(shutdown.poll_event().is_none());
    }

    #[test]
    fn owner_panic_is_cleanup_with_safe_join_context() {
        let mut shutdown = OwnerShutdown::default();
        let handle = thread::spawn(|| panic!("private panic payload"));
        let error = shutdown.finish(Some(handle)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Backend);
        assert!(
            matches!(&error, Error::Context { context, .. } if context.operation() == Operation::Join)
        );
        assert!(!error.to_string().contains("private panic payload"));
        assert!(shutdown.report.as_ref().unwrap().primary().is_none());
        assert_eq!(shutdown.finish(None), Err(error));
        assert!(matches!(
            shutdown.poll_event(),
            Some(Event::ShutdownError { .. })
        ));
        assert!(shutdown.poll_event().is_none());
    }
}
