//! Device-free capture lifecycle and wait-result validation.

use crate::owner::CleanupLedger;
use flexaudio_core::types::{Error, Result};
use flexaudio_core::{ErrorContext, Event, HelperStopReason, Operation, ShutdownReport};
use std::sync::{mpsc, Arc, Mutex};

/// Add startup context without erasing the underlying error kind.
pub(crate) fn keepalive_error(error: Error) -> Error {
    error.with_context(ErrorContext::new(Operation::Start))
}

/// Construct render resources only when classic-loopback keepalive is enabled.
pub(crate) fn create_keepalive<K>(
    enabled: bool,
    create: impl FnOnce() -> Result<K>,
) -> Result<Option<K>> {
    if enabled {
        create().map(Some).map_err(keepalive_error)
    } else {
        Ok(None)
    }
}

/// Operations used by the owner thread; implementations own their OS resources.
pub(crate) trait StreamClient {
    fn start(&mut self) -> Result<()>;
    fn stop(&mut self) -> Result<()>;
    fn fill_silence(&mut self) -> Result<()>;
}

fn stop_client(client: &mut impl StreamClient, role: &str) -> Result<()> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| client.stop())).unwrap_or_else(|_| {
        Err(
            Error::Backend(format!("{role} audio client panicked during teardown"))
                .with_context(ErrorContext::new(Operation::Stop)),
        )
    })
}

/// Keeps render running throughout capture, including startup and teardown.
pub(crate) struct Session<C: StreamClient, K: StreamClient> {
    capture: C,
    keepalive: Option<K>,
    capture_started: bool,
    render_started: bool,
    lifecycle: Option<KeepaliveLifecycle>,
    cleanup_events: Option<mpsc::Sender<Event>>,
    cleanup: CleanupLedger,
    shutdown_report: Option<ShutdownReport>,
}

/// The session owns teardown; this final field also reports unwinding after
/// native client teardown attempts. Sending never takes a consumer-owned lock.
struct KeepaliveLifecycle {
    events: mpsc::Sender<Event>,
    stop_reason: Arc<Mutex<HelperStopReason>>,
    reason: Option<HelperStopReason>,
    startup_complete: bool,
}

impl KeepaliveLifecycle {
    fn start(events: mpsc::Sender<Event>, stop_reason: Arc<Mutex<HelperStopReason>>) -> Self {
        let guard = Self {
            events,
            stop_reason,
            reason: None,
            startup_complete: false,
        };
        #[allow(
            clippy::let_underscore_must_use,
            reason = "lifecycle notification is optional after the receiver is dropped"
        )]
        let _ = guard.events.send(Event::ClassicLoopbackKeepaliveStarted);
        guard
    }
}

impl Drop for KeepaliveLifecycle {
    fn drop(&mut self) {
        let reason = if std::thread::panicking() && !self.startup_complete {
            HelperStopReason::StartupFailed
        } else if std::thread::panicking() {
            HelperStopReason::RuntimeFailed
        } else {
            self.reason.unwrap_or_else(|| {
                *self
                    .stop_reason
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
            })
        };
        #[allow(
            clippy::let_underscore_must_use,
            reason = "lifecycle notification is optional after the receiver is dropped"
        )]
        let _ = self
            .events
            .send(Event::ClassicLoopbackKeepaliveStopped { reason });
    }
}

impl<C: StreamClient, K: StreamClient> Session<C, K> {
    /// Report readiness only after both clients start. Failed startup rolls back
    /// before returning, and losing the receiver also stops both clients.
    #[cfg(test)]
    pub(crate) fn start_and_report(
        capture: C,
        keepalive: Option<K>,
        report: impl FnOnce(Result<()>) -> bool,
    ) -> Option<Self> {
        Self::start_and_report_with_lifecycle(
            capture,
            keepalive,
            report,
            None,
            Arc::new(Mutex::new(HelperStopReason::StreamStopped)),
        )
    }

    #[cfg(test)]
    pub(crate) fn start_and_report_with_lifecycle(
        capture: C,
        keepalive: Option<K>,
        report: impl FnOnce(Result<()>) -> bool,
        events: Option<mpsc::Sender<Event>>,
        stop_reason: Arc<Mutex<HelperStopReason>>,
    ) -> Option<Self> {
        Self::start_and_report_with_cleanup(
            capture,
            keepalive,
            report,
            events,
            stop_reason,
            CleanupLedger::default(),
        )
    }

    pub(crate) fn start_and_report_with_cleanup(
        capture: C,
        keepalive: Option<K>,
        report: impl FnOnce(Result<()>) -> bool,
        events: Option<mpsc::Sender<Event>>,
        stop_reason: Arc<Mutex<HelperStopReason>>,
        cleanup: CleanupLedger,
    ) -> Option<Self> {
        let mut session = Self {
            capture,
            keepalive,
            capture_started: false,
            render_started: false,
            lifecycle: None,
            cleanup_events: events.clone(),
            cleanup,
            shutdown_report: None,
        };
        let start = (|| {
            if let Some(render) = session.keepalive.as_mut() {
                render.fill_silence().map_err(keepalive_error)?;
                render.start()?;
                session.render_started = true;
                session.lifecycle =
                    events.map(|events| KeepaliveLifecycle::start(events, stop_reason));
            }
            session.capture.start()?;
            session.capture_started = true;
            Ok(())
        })();
        if let Err(error) = start {
            session.set_stop_reason(HelperStopReason::StartupFailed);
            session.retain_shutdown_outcome();
            report(Err(error));
            return None;
        }
        if !report(Ok(())) {
            session.set_stop_reason(HelperStopReason::Cancelled);
            return None;
        }
        if let Some(lifecycle) = session.lifecycle.as_mut() {
            lifecycle.startup_complete = true;
        }
        Some(session)
    }

    pub(crate) fn service_keepalive(&mut self) -> Result<()> {
        if let Some(render) = self.keepalive.as_mut() {
            if let Err(error) = render.fill_silence() {
                self.set_stop_reason(HelperStopReason::RuntimeFailed);
                return Err(error);
            }
        }
        Ok(())
    }

    pub(crate) fn set_stop_reason(&mut self, reason: HelperStopReason) {
        if let Some(lifecycle) = self.lifecycle.as_mut() {
            lifecycle.reason = Some(reason);
        }
    }

    /// Attempt both stops even when capture stop fails; preserve every failure.
    #[cfg(test)]
    pub(crate) fn stop(&mut self) -> Result<()> {
        self.retain_shutdown_outcome();
        self.shutdown_report
            .as_ref()
            .expect("retained session shutdown")
            .result()
    }

    pub(crate) fn retain_shutdown_outcome(&mut self) {
        if self.shutdown_report.is_some() {
            return;
        }
        let capture = if self.capture_started {
            self.capture_started = false;
            stop_client(&mut self.capture, "capture")
        } else {
            Ok(())
        };
        let render = if self.render_started {
            self.render_started = false;
            stop_client(
                self.keepalive.as_mut().expect("started render exists"),
                "render",
            )
        } else {
            Ok(())
        };
        if capture.is_err() || render.is_err() {
            if let Some(lifecycle) = self.lifecycle.as_mut() {
                lifecycle
                    .reason
                    .get_or_insert(HelperStopReason::RuntimeFailed);
            }
        }
        let report = ShutdownReport::new(
            None,
            capture.err().into_iter().chain(render.err()).collect(),
        );
        for error in report.cleanup() {
            self.cleanup.retain(error.clone());
        }
        self.shutdown_report = Some(report);
        // Retain cleanup before publishing the helper's final lifecycle event.
        drop(self.lifecycle.take());
    }
}

impl<C: StreamClient, K: StreamClient> Drop for Session<C, K> {
    fn drop(&mut self) {
        let needs_stop = self.shutdown_report.is_none();
        self.retain_shutdown_outcome();
        if needs_stop {
            if let Some(events) = &self.cleanup_events {
                for error in self
                    .shutdown_report
                    .as_ref()
                    .expect("retained session shutdown")
                    .cleanup()
                {
                    // The ledger is already durable for this generation before notification.
                    #[allow(
                        clippy::let_underscore_must_use,
                        reason = "the generation ledger retains cleanup when the notification receiver has gone"
                    )]
                    let _ = events.send(Event::ShutdownError {
                        error: error.clone(),
                    });
                }
            }
        }
    }
}

/// Validate Win32 wait values without calling Win32. Timeout is a normal wakeup
/// for stop polling; abandoned, failed, and unknown results fail closed.
pub(crate) fn check_wait_result(value: u32, handles: u32) -> Result<()> {
    const WAIT_TIMEOUT: u32 = 258;
    const WAIT_ABANDONED_0: u32 = 128;
    const WAIT_FAILED: u32 = u32::MAX;
    if value < handles || value == WAIT_TIMEOUT {
        return Ok(());
    }
    let reason = if value == WAIT_FAILED {
        "WAIT_FAILED"
    } else if (WAIT_ABANDONED_0..WAIT_ABANDONED_0 + handles).contains(&value) {
        "abandoned handle"
    } else {
        "unexpected result"
    };
    Err(Error::Backend(format!(
        "WASAPI event wait: {reason} ({value:#x})"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    type Trace = Rc<RefCell<Vec<String>>>;

    struct FakeClient {
        name: &'static str,
        trace: Trace,
        fail: Option<&'static str>,
    }

    impl FakeClient {
        fn operation(&self, op: &str) -> Result<()> {
            let entry = format!("{}.{}", self.name, op);
            self.trace.borrow_mut().push(entry.clone());
            if self.fail == Some("panic-start") && op == "start" {
                panic!("injected client startup panic");
            }
            if self.fail == Some("panic-stop") && op == "stop" {
                panic!("injected client teardown panic");
            }
            if self.fail == Some(op) {
                Err(Error::Backend(entry))
            } else {
                Ok(())
            }
        }
    }

    impl StreamClient for FakeClient {
        fn start(&mut self) -> Result<()> {
            self.operation("start")
        }
        fn stop(&mut self) -> Result<()> {
            self.operation("stop")
        }
        fn fill_silence(&mut self) -> Result<()> {
            self.operation("fill")
        }
    }

    fn client(name: &'static str, trace: &Trace, fail: Option<&'static str>) -> FakeClient {
        FakeClient {
            name,
            trace: trace.clone(),
            fail,
        }
    }

    #[test]
    fn render_surrounds_capture_and_readiness_follows_start() {
        let trace = Trace::default();
        let mut session = Session::start_and_report(
            client("capture", &trace, None),
            Some(client("render", &trace, None)),
            |r| {
                r.unwrap();
                trace.borrow_mut().push("ready".into());
                true
            },
        )
        .unwrap();
        session.service_keepalive().unwrap();
        session.stop().unwrap();
        session.stop().unwrap();
        drop(session);
        assert_eq!(
            *trace.borrow(),
            [
                "render.fill",
                "render.start",
                "capture.start",
                "ready",
                "render.fill",
                "capture.stop",
                "render.stop"
            ]
        );
    }

    #[test]
    fn failed_capture_start_rolls_back_render_before_reporting_error() {
        let trace = Trace::default();
        let session = Session::start_and_report(
            client("capture", &trace, Some("start")),
            Some(client("render", &trace, None)),
            |r| {
                assert!(r.is_err());
                trace.borrow_mut().push("error".into());
                true
            },
        );
        assert!(session.is_none());
        assert_eq!(
            *trace.borrow(),
            [
                "render.fill",
                "render.start",
                "capture.start",
                "render.stop",
                "error"
            ]
        );
    }

    #[test]
    fn render_fill_or_start_failure_never_starts_capture() {
        for fail in ["fill", "start"] {
            let trace = Trace::default();
            let session = Session::start_and_report(
                client("capture", &trace, None),
                Some(client("render", &trace, Some(fail))),
                |r| {
                    assert!(r.is_err());
                    true
                },
            );
            assert!(session.is_none());
            assert!(!trace.borrow().iter().any(|s| s.starts_with("capture.")));
        }
    }

    #[test]
    fn disconnected_readiness_receiver_stops_both_clients() {
        let trace = Trace::default();
        assert!(Session::start_and_report(
            client("capture", &trace, None),
            Some(client("render", &trace, None)),
            |_| false,
        )
        .is_none());
        assert_eq!(
            *trace.borrow(),
            [
                "render.fill",
                "render.start",
                "capture.start",
                "capture.stop",
                "render.stop"
            ]
        );
    }

    #[test]
    fn process_capture_has_no_render_operations() {
        let trace = Trace::default();
        let session = Session::<_, FakeClient>::start_and_report(
            client("capture", &trace, None),
            None,
            |r| {
                r.unwrap();
                true
            },
        )
        .unwrap();
        drop(session);
        assert_eq!(*trace.borrow(), ["capture.start", "capture.stop"]);
    }

    #[test]
    fn disabled_classic_keepalive_never_constructs_or_services_render() {
        let trace = Trace::default();
        let keepalive = create_keepalive::<FakeClient>(false, || {
            panic!("disabled keepalive must not construct a render client")
        })
        .unwrap();
        let mut session =
            Session::start_and_report(client("capture", &trace, None), keepalive, |result| {
                result.unwrap();
                true
            })
            .unwrap();
        session.service_keepalive().unwrap();
        drop(session);
        assert_eq!(*trace.borrow(), ["capture.start", "capture.stop"]);
    }

    #[test]
    fn enabled_classic_keepalive_constructs_once_and_retains_startup_context() {
        let calls = std::cell::Cell::new(0);
        assert_eq!(
            create_keepalive(true, || {
                calls.set(calls.get() + 1);
                Ok(())
            }),
            Ok(Some(()))
        );
        assert_eq!(calls.get(), 1);
        assert_eq!(
            create_keepalive::<()>(true, || Err(Error::DeviceNotFound)),
            Err(keepalive_error(Error::DeviceNotFound))
        );
    }

    #[test]
    fn runtime_render_failure_publishes_terminal_error_and_cleanup_remains_ordered() {
        let trace = Trace::default();
        let mut session = Session::start_and_report(
            client("capture", &trace, None),
            Some(client("render", &trace, None)),
            |_| true,
        )
        .unwrap();
        session.keepalive.as_mut().unwrap().fail = Some("fill");
        let (events_tx, events_rx) = std::sync::mpsc::channel();
        let report = crate::owner::finish_capture(
            session.service_keepalive(),
            || session.stop(),
            &events_tx,
        );
        let error = Error::Backend("render.fill".into());
        assert_eq!(report.primary(), Some(&error));
        assert!(report.cleanup().is_empty());
        assert!(matches!(
            events_rx.try_recv(),
            Ok(flexaudio_core::Event::TerminalError { error: observed }) if observed == error
        ));
        assert!(events_rx.try_recv().is_err());
        drop(session);
        assert_eq!(
            &trace.borrow()[3..],
            ["render.fill", "capture.stop", "render.stop"]
        );
    }

    #[test]
    fn capture_stop_failure_still_stops_render_once() {
        let trace = Trace::default();
        let mut session = Session::start_and_report(
            client("capture", &trace, Some("stop")),
            Some(client("render", &trace, None)),
            |_| true,
        )
        .unwrap();
        assert!(session.stop().is_err());
        drop(session);
        assert_eq!(&trace.borrow()[3..], ["capture.stop", "render.stop"]);
    }

    #[test]
    fn drop_publishes_native_cleanup_failures_without_keepalive() {
        let trace = Trace::default();
        let (events, receiver) = mpsc::channel();
        let cleanup = CleanupLedger::default();
        let session = Session::<FakeClient, FakeClient>::start_and_report_with_cleanup(
            client("capture", &trace, Some("stop")),
            None,
            |_| true,
            Some(events),
            Arc::new(Mutex::new(HelperStopReason::StreamStopped)),
            cleanup.clone(),
        )
        .unwrap();
        drop(session);
        assert_eq!(cleanup.take(), [Error::Backend("capture.stop".into())]);
        assert!(
            matches!(receiver.try_recv(), Ok(Event::ShutdownError { error })
            if error == Error::Backend("capture.stop".into())),
            "dropping a session must publish native cleanup failures"
        );
        assert!(
            receiver.try_recv().is_err(),
            "cleanup must be reported once"
        );
    }

    #[test]
    fn wait_signals_and_timeout_are_normal() {
        for result in [0, 1, 258] {
            check_wait_result(result, 2).unwrap();
        }
    }

    #[test]
    fn wait_failures_abandonment_and_unknown_values_are_errors() {
        for result in [u32::MAX, 128, 129, 2, 300] {
            assert!(matches!(
                check_wait_result(result, 2),
                Err(Error::Backend(_))
            ));
        }
    }

    #[test]
    fn keepalive_context_preserves_typed_errors() {
        let denied = Error::PermissionDenied {
            permission: flexaudio_core::types::Permission::SystemAudio,
            detail: "keepalive access denied".into(),
        };
        assert_eq!(keepalive_error(denied.clone()).root(), &denied);
        assert!(matches!(
            keepalive_error(Error::DeviceNotFound).root(),
            Error::DeviceNotFound
        ));
        assert!(matches!(
            keepalive_error(Error::UnsupportedFormat("format".into())).root(),
            Error::UnsupportedFormat(message) if message == "format"
        ));
    }

    #[test]
    fn keepalive_context_keeps_explanation_and_operation() {
        assert!(matches!(
            keepalive_error(Error::Backend("native initialization failed".into())),
            Error::Context { source, context } if matches!(*source, Error::Backend(ref message) if message == "native initialization failed") && context.operation() == Operation::Start
        ));
    }

    fn assert_pair(events: mpsc::Receiver<Event>, reason: HelperStopReason) {
        assert_eq!(
            events.try_iter().collect::<Vec<_>>(),
            [
                Event::ClassicLoopbackKeepaliveStarted,
                Event::ClassicLoopbackKeepaliveStopped { reason },
            ]
        );
    }

    #[test]
    fn lifecycle_setup_failures_emit_no_pair_and_capture_failure_pairs_once() {
        for (capture_fail, render_fail, reason) in [
            (None, Some("fill"), None),
            (None, Some("start"), None),
            (Some("start"), None, Some(HelperStopReason::StartupFailed)),
        ] {
            let trace = Trace::default();
            let (tx, rx) = mpsc::channel();
            let result = Session::start_and_report_with_lifecycle(
                client("capture", &trace, capture_fail),
                Some(client("render", &trace, render_fail)),
                |status| {
                    assert!(status.is_err());
                    true
                },
                Some(tx),
                Arc::new(Mutex::new(HelperStopReason::StreamStopped)),
            );
            assert!(result.is_none());
            if let Some(reason) = reason {
                assert_pair(rx, reason);
            } else {
                assert!(rx.try_recv().is_err());
            }
        }
    }

    #[test]
    fn lifecycle_shutdown_reasons_and_repeated_stop_are_exactly_once() {
        for reason in [
            HelperStopReason::StreamStopped,
            HelperStopReason::SourceSwitched,
            HelperStopReason::Recovery,
        ] {
            let trace = Trace::default();
            let (tx, rx) = mpsc::channel();
            let stop_reason = Arc::new(Mutex::new(HelperStopReason::StreamStopped));
            let mut session = Session::start_and_report_with_lifecycle(
                client("capture", &trace, None),
                Some(client("render", &trace, None)),
                |_| true,
                Some(tx),
                stop_reason.clone(),
            )
            .unwrap();
            *stop_reason.lock().unwrap() = reason;
            session.stop().unwrap();
            session.stop().unwrap();
            drop(session);
            assert_pair(rx, reason);
            assert_eq!(&trace.borrow()[3..], ["capture.stop", "render.stop"]);
        }
    }

    #[test]
    fn lifecycle_runtime_failure_and_teardown_failure_still_pair_once() {
        for failed_client in ["runtime", "capture", "render"] {
            let trace = Trace::default();
            let (tx, rx) = mpsc::channel();
            let mut session = Session::start_and_report_with_lifecycle(
                client(
                    "capture",
                    &trace,
                    (failed_client == "capture").then_some("stop"),
                ),
                Some(client(
                    "render",
                    &trace,
                    (failed_client == "render").then_some("stop"),
                )),
                |_| true,
                Some(tx),
                Arc::new(Mutex::new(HelperStopReason::StreamStopped)),
            )
            .unwrap();
            if failed_client == "runtime" {
                session.keepalive.as_mut().unwrap().fail = Some("fill");
                assert!(session.service_keepalive().is_err());
            }
            let result = session.stop();
            assert_eq!(result.is_err(), failed_client != "runtime");
            drop(session);
            assert_pair(rx, HelperStopReason::RuntimeFailed);
            assert!(trace.borrow().iter().any(|entry| entry == "render.stop"));
        }
    }

    #[test]
    fn lifecycle_cancelled_readiness_pairs_after_both_stops() {
        let trace = Trace::default();
        let (tx, rx) = mpsc::channel();
        assert!(Session::start_and_report_with_lifecycle(
            client("capture", &trace, None),
            Some(client("render", &trace, None)),
            |_| false,
            Some(tx),
            Arc::new(Mutex::new(HelperStopReason::StreamStopped)),
        )
        .is_none());
        assert_eq!(&trace.borrow()[3..], ["capture.stop", "render.stop"]);
        assert_pair(rx, HelperStopReason::Cancelled);
    }

    #[test]
    fn lifecycle_absent_render_has_no_events() {
        let trace = Trace::default();
        let (tx, rx) = mpsc::channel();
        let session = Session::<_, FakeClient>::start_and_report_with_lifecycle(
            client("capture", &trace, None),
            None,
            |_| true,
            Some(tx),
            Arc::new(Mutex::new(HelperStopReason::StreamStopped)),
        )
        .unwrap();
        drop(session);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn lifecycle_unwind_stops_clients_before_one_runtime_failed_notice() {
        let trace = Trace::default();
        let (tx, rx) = mpsc::channel();
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _session = Session::start_and_report_with_lifecycle(
                client("capture", &trace, None),
                Some(client("render", &trace, None)),
                |_| true,
                Some(tx),
                Arc::new(Mutex::new(HelperStopReason::StreamStopped)),
            )
            .unwrap();
            panic!("injected capture owner panic");
        }))
        .is_err());
        assert_eq!(&trace.borrow()[3..], ["capture.stop", "render.stop"]);
        assert_pair(rx, HelperStopReason::RuntimeFailed);
    }

    #[test]
    fn lifecycle_capture_start_panic_is_startup_failed_after_render_teardown() {
        let trace = Trace::default();
        let (tx, rx) = mpsc::channel();
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Session::start_and_report_with_lifecycle(
                client("capture", &trace, Some("panic-start")),
                Some(client("render", &trace, None)),
                |_| true,
                Some(tx),
                Arc::new(Mutex::new(HelperStopReason::StreamStopped)),
            );
        }))
        .is_err());
        assert_eq!(&trace.borrow()[3..], ["render.stop"]);
        assert_pair(rx, HelperStopReason::StartupFailed);
    }

    #[test]
    fn lifecycle_native_stop_panic_attempts_both_stops_and_pairs_once() {
        for panic_capture in [false, true] {
            let trace = Trace::default();
            let (tx, rx) = mpsc::channel();
            let mut session = Session::start_and_report_with_lifecycle(
                client("capture", &trace, panic_capture.then_some("panic-stop")),
                Some(client(
                    "render",
                    &trace,
                    (!panic_capture).then_some("panic-stop"),
                )),
                |_| true,
                Some(tx),
                Arc::new(Mutex::new(HelperStopReason::StreamStopped)),
            )
            .unwrap();
            assert!(session.stop().is_err());
            drop(session);
            assert_eq!(&trace.borrow()[3..], ["capture.stop", "render.stop"]);
            assert_pair(rx, HelperStopReason::RuntimeFailed);
        }
    }
}
