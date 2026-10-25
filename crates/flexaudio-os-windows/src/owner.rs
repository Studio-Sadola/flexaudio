//! One retained join outcome for both WASAPI backends.
use std::collections::VecDeque;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use flexaudio_core::{Error, ErrorContext, Event, Operation, Result, ShutdownReport};

#[cfg(all(test, target_os = "windows"))]
pub(crate) static DROP_FAILURE_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Cleanup observed by resources in one capture generation, including Drop.
#[derive(Clone, Default)]
pub(crate) struct CleanupLedger(Arc<Mutex<Vec<Error>>>);

impl CleanupLedger {
    pub(crate) fn retain(&self, error: Error) {
        let mut errors = self.0.lock().unwrap_or_else(|error| error.into_inner());
        match error {
            Error::Multiple(group) => {
                errors.push(group.primary().clone());
                errors.extend(group.secondary().cloned());
            }
            error => errors.push(error),
        }
    }

    pub(crate) fn take(&self) -> Vec<Error> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(|error| error.into_inner()))
    }
}

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
        #[allow(
            clippy::let_underscore_must_use,
            reason = "notification receiver may already be gone; the report retains the cause"
        )]
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
    pub(crate) report: Option<ShutdownReport>,
    pub(crate) cleanup: CleanupLedger,
    events: VecDeque<Event>,
    capture_events: Option<mpsc::Receiver<Event>>,
    pending_capture: VecDeque<Event>,
}

impl OwnerShutdown {
    pub(crate) fn reset(&mut self, capture_events: mpsc::Receiver<Event>) {
        self.report = None;
        self.events.clear();
        self.retain_capture_events(capture_events);
        // Successful acquisition retains the new lifecycle guarantee while
        // keeping the existing reset behavior for ordinary notifications.
        self.pending_capture.retain(|event| {
            matches!(
                event,
                Event::ClassicLoopbackKeepaliveStarted
                    | Event::ClassicLoopbackKeepaliveStopped { .. }
            )
        });
    }

    /// Failed acquisition must not lose a renderer's completed lifecycle, or
    /// erase the previous checked shutdown outcome.
    pub(crate) fn retain_capture_events(&mut self, capture_events: mpsc::Receiver<Event>) {
        if let Some(previous) = self.capture_events.take() {
            self.pending_capture.extend(previous.try_iter());
        }
        self.capture_events = Some(capture_events);
    }

    pub(crate) fn finish(&mut self, handle: Option<JoinHandle<ShutdownReport>>) -> Result<()> {
        // Successful startup resets the report before storing a new handle; repeats have no owner.
        if self.report.is_none() {
            let report = handle
                .map(join_owner)
                .unwrap_or_else(|| ShutdownReport::new(None, Vec::new()));
            self.retain(report);
        }
        self.result()
    }

    /// Join before draining: thread-local resources have all been dropped then.
    pub(crate) fn retain_start_failure(
        &mut self,
        error: Error,
        handle: JoinHandle<ShutdownReport>,
    ) -> Result<()> {
        let joined = join_owner(handle);
        let cleanup = joined
            .cleanup()
            .iter()
            .cloned()
            .chain(self.cleanup.take())
            .collect();
        let report = ShutdownReport::new(Some(error), cleanup);
        let result = report.result();
        // Startup's primary reaches start() through this result; only cleanup belongs to shutdown.
        self.retain(ShutdownReport::new(None, report.cleanup().to_vec()));
        result
    }

    fn retain(&mut self, report: ShutdownReport) {
        let cleanup: Vec<Error> = report
            .cleanup()
            .iter()
            .cloned()
            .chain(self.cleanup.take())
            .collect();
        let previous = self.report.take();
        let primary = match previous.as_ref() {
            Some(previous) => previous.primary().cloned(),
            None => report.primary().cloned(),
        };
        self.events.extend(
            cleanup
                .iter()
                .cloned()
                .map(|error| Event::ShutdownError { error }),
        );
        let mut retained_cleanup = previous
            .map(|report| report.cleanup().to_vec())
            .unwrap_or_default();
        retained_cleanup.extend(cleanup);
        self.report = Some(ShutdownReport::new(primary, retained_cleanup));
    }

    pub(crate) fn result(&self) -> Result<()> {
        self.report
            .as_ref()
            .expect("completed owner shutdown")
            .result()
    }

    pub(crate) fn poll_event(&mut self) -> Option<Event> {
        self.pending_capture
            .pop_front()
            .or_else(|| {
                self.capture_events
                    .as_ref()
                    .and_then(|events| events.try_recv().ok())
            })
            .or_else(|| self.events.pop_front())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio_core::{ErrorKind, Permission};
    use std::thread;

    #[test]
    fn failed_start_has_clean_shutdown_and_no_terminal_notice() {
        let mut shutdown = OwnerShutdown::default();
        assert_eq!(
            shutdown.retain_start_failure(
                Error::DeviceNotFound,
                thread::spawn(|| ShutdownReport::new(None, Vec::new())),
            ),
            Err(Error::DeviceNotFound)
        );
        assert_eq!(shutdown.finish(None), Ok(()));
        assert!(shutdown.report.as_ref().unwrap().primary().is_none());
        assert!(shutdown.poll_event().is_none());
    }

    #[test]
    fn startup_failure_retains_join_cleanup() {
        let mut shutdown = OwnerShutdown::default();
        let worker = thread::spawn(|| panic!("injected startup owner panic"));
        let primary = Error::Backend("injected start failure".into());
        let expected = ShutdownReport::new(
            Some(primary.clone()),
            vec![Error::Backend("capture owner thread panicked".into())
                .with_context(ErrorContext::new(Operation::Join))],
        );
        assert_eq!(
            shutdown.retain_start_failure(primary, worker),
            expected.result()
        );
        let retained = ShutdownReport::new(None, expected.cleanup().to_vec());
        assert_eq!(shutdown.report, Some(retained.clone()));
        assert_eq!(shutdown.finish(None), retained.result());
        assert_eq!(shutdown.report, Some(retained));
        assert_eq!(
            shutdown.poll_event(),
            Some(Event::ShutdownError {
                error: expected.cleanup()[0].clone(),
            })
        );
        assert!(shutdown.poll_event().is_none());
    }

    #[test]
    fn failed_restart_keeps_previous_primary_and_appends_cleanup_until_success() {
        for previous_primary in [None, Some(Error::DeviceLost)] {
            let mut shutdown = OwnerShutdown::default();
            let (events, receiver) = mpsc::channel();
            shutdown.reset(receiver);
            let previous_cleanup = Error::Backend("injected previous stop failure".into())
                .with_context(ErrorContext::new(Operation::Stop));
            let capture = previous_primary.clone().map_or(Ok(()), Err);
            let stop_error = previous_cleanup.clone();
            let previous_worker =
                thread::spawn(move || finish_capture(capture, || Err(stop_error), &events));
            let previous =
                ShutdownReport::new(previous_primary.clone(), vec![previous_cleanup.clone()]);
            assert_eq!(shutdown.finish(Some(previous_worker)), previous.result());
            if let Some(error) = &previous_primary {
                assert_eq!(
                    shutdown.poll_event(),
                    Some(Event::TerminalError {
                        error: error.clone(),
                    })
                );
            }
            assert_eq!(
                shutdown.poll_event(),
                Some(Event::ShutdownError {
                    error: previous_cleanup.clone(),
                })
            );
            assert!(shutdown.poll_event().is_none());

            let restart_error = Error::Backend("injected restart failure".into())
                .with_context(ErrorContext::new(Operation::Start));
            let restart_cleanup = Error::Backend("capture owner thread panicked".into())
                .with_context(ErrorContext::new(Operation::Join));
            let attempt =
                ShutdownReport::new(Some(restart_error.clone()), vec![restart_cleanup.clone()]);
            let worker = thread::spawn(|| panic!("injected failed restart owner panic"));
            assert_eq!(
                shutdown.retain_start_failure(restart_error, worker),
                attempt.result()
            );
            let retained = ShutdownReport::new(
                previous_primary,
                vec![previous_cleanup, restart_cleanup.clone()],
            );
            assert_eq!(shutdown.report.as_ref(), Some(&retained));
            assert_eq!(shutdown.finish(None), retained.result());
            assert_eq!(shutdown.report.as_ref(), Some(&retained));
            assert_eq!(
                shutdown.poll_event(),
                Some(Event::ShutdownError {
                    error: restart_cleanup,
                })
            );
            assert!(shutdown.poll_event().is_none());

            let (_events, receiver) = mpsc::channel();
            shutdown.reset(receiver);
            assert!(shutdown.finish(None).is_ok());
            assert_eq!(shutdown.report, Some(ShutdownReport::new(None, Vec::new())));
        }
    }

    #[test]
    fn replacing_failed_start_mailbox_preserves_the_unread_stopped_before_new_start() {
        let mut shutdown = OwnerShutdown::default();
        let (first_tx, first_rx) = mpsc::channel();
        shutdown.retain_capture_events(first_rx);
        first_tx
            .send(Event::ClassicLoopbackKeepaliveStarted)
            .unwrap();
        let stopped = Event::ClassicLoopbackKeepaliveStopped {
            reason: flexaudio_core::HelperStopReason::StartupFailed,
        };
        first_tx.send(stopped.clone()).unwrap();
        assert_eq!(
            shutdown.poll_event(),
            Some(Event::ClassicLoopbackKeepaliveStarted)
        );
        let (next_tx, next_rx) = mpsc::channel();
        next_tx
            .send(Event::ClassicLoopbackKeepaliveStarted)
            .unwrap();
        shutdown.reset(next_rx);
        assert_eq!(shutdown.poll_event(), Some(stopped));
        assert_eq!(
            shutdown.poll_event(),
            Some(Event::ClassicLoopbackKeepaliveStarted)
        );
        assert!(shutdown.poll_event().is_none());
    }

    #[test]
    fn failed_acquisition_retains_previous_ordinary_notifications_and_report() {
        let mut shutdown = OwnerShutdown::default();
        let (first_tx, first_rx) = mpsc::channel();
        shutdown.reset(first_rx);
        let terminal = Event::TerminalError {
            error: Error::DeviceLost,
        };
        first_tx.send(terminal.clone()).unwrap();
        assert!(shutdown.finish(None).is_ok());
        let previous = shutdown.report.clone();
        let (_next_tx, next_rx) = mpsc::channel();
        shutdown.retain_capture_events(next_rx);
        assert_eq!(shutdown.report, previous);
        assert_eq!(shutdown.poll_event(), Some(terminal));
    }

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
