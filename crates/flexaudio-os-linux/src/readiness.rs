// Capture startup policy, kept independent of PipeWire for injected callback tests.
use flexaudio_core::{Error, ErrorContext, Operation, Result};
use std::cell::Cell;
use std::sync::mpsc;
use std::time::Duration;

/// Capture startup's shared bound, used by the timer and its failure guidance.
pub(crate) const NEGOTIATION_DEADLINE: Duration = Duration::from_secs(2);

pub(crate) fn negotiation_timeout_message() -> String {
    format!(
        "PipeWire did not complete capture format negotiation within {} seconds. Check that the audio session and target playback are available, then retry capture; restart the audio session if the problem persists.",
        NEGOTIATION_DEADLINE.as_secs()
    )
}

pub(crate) fn negotiation_timeout_error() -> Error {
    Error::Backend(negotiation_timeout_message()).with_context(ErrorContext::new(Operation::Start))
}

pub(crate) struct Readiness {
    tx: mpsc::Sender<Result<()>>,
    sent: Cell<bool>,
    started: Cell<bool>,
    target_present: Cell<bool>,
    registry_complete: Cell<bool>,
    control_pending: Cell<bool>,
    format_ready: Cell<bool>,
}

impl Readiness {
    pub(crate) fn new(tx: mpsc::Sender<Result<()>>, process_capture: bool) -> Self {
        Self {
            tx,
            sent: Cell::new(false),
            started: Cell::new(false),
            target_present: Cell::new(!process_capture),
            registry_complete: Cell::new(!process_capture),
            control_pending: Cell::new(false),
            format_ready: Cell::new(false),
        }
    }

    pub(crate) fn set_target_present(&self, present: bool) {
        self.target_present.set(present);
        self.try_report_ready();
    }

    pub(crate) fn registry_synchronized(&self) {
        self.registry_complete.set(true);
        self.try_report_ready();
    }

    pub(crate) fn set_control_pending(&self, pending: bool) {
        self.control_pending.set(pending);
        self.try_report_ready();
    }

    pub(crate) fn report_ready(&self) {
        self.format_ready.set(true);
        self.try_report_ready();
    }

    fn try_report_ready(&self) {
        let ready =
            self.format_ready.get() || (self.registry_complete.get() && !self.target_present.get());
        if ready && !self.control_pending.get() && !self.sent.replace(true) {
            self.started.set(true);
            #[allow(
                clippy::let_underscore_must_use,
                reason = "A closed readiness receiver means the starter has already abandoned startup."
            )]
            let _ = self.tx.send(Ok(()));
        }
    }

    pub(crate) fn report_failure(&self, error: Error) {
        // This channel announces startup once; the control owner retains later failures in rollback.
        if !self.sent.replace(true) {
            #[allow(
                clippy::let_underscore_must_use,
                reason = "A closed readiness receiver means the starter has already abandoned startup."
            )]
            let _ = self.tx.send(Err(error));
        }
    }

    pub(crate) fn is_reported(&self) -> bool {
        self.sent.get()
    }

    pub(crate) fn is_started(&self) -> bool {
        self.started.get()
    }

    pub(crate) fn requires_negotiation(&self) -> bool {
        self.control_pending.get() || self.target_present.get() || !self.registry_complete.get()
    }

    // Returns true when the capture loop must stop.
    pub(crate) fn deadline_expired(&self) -> bool {
        if self.is_reported() {
            return false;
        }
        if self.requires_negotiation() {
            self.report_failure(negotiation_timeout_error());
            true
        } else {
            self.report_ready();
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation_deadline_has_one_owner_and_matches_error_guidance() {
        let backend_source = include_str!("lib.rs");
        assert!(
            !backend_source.contains("const NEGOTIATE_DEADLINE_MS"),
            "capture readiness must own the deadline used by its timer and error guidance"
        );
        assert!(backend_source.contains("Some(NEGOTIATION_DEADLINE)"));
        assert_eq!(NEGOTIATION_DEADLINE, Duration::from_secs(2));
        assert!(negotiation_timeout_message().contains(&format!(
            "within {} seconds",
            NEGOTIATION_DEADLINE.as_secs()
        )));
    }

    #[test]
    // Structural source check only: no live PipeWire callback is exercised here.
    fn process_setup_wires_the_shared_negotiation_listener_and_timer() {
        let source = include_str!("lib.rs");
        let process_loop = source
            .split("fn run_pw_process_loop(")
            .nth(1)
            .expect("process capture owner")
            .split("type BoundNode")
            .next()
            .expect("process capture body");
        assert!(process_loop.contains("Readiness::new(ready_tx.clone(), true)"));
        assert!(process_loop
            .contains("setup_pw_process(select, sink, events.clone(), readiness.clone())"));
        assert!(process_loop.contains("arm_negotiation_deadline(&main_loop, readiness, events)"));
        assert!(!process_loop.contains("ready_tx.send(Ok(()))"));
        let setup = source
            .split("fn setup_pw_process(")
            .nth(1)
            .expect("process capture setup")
            .split("struct UserData")
            .next()
            .expect("process setup body");
        assert!(setup.contains("readiness: readiness.clone()"));
        assert!(setup.contains("add_capture_listener(&stream, user_data, &main_loop)"));
        assert!(setup.contains("readiness.set_target_present("));
        assert!(setup.contains("readiness_for_sync.registry_synchronized()"));
        assert!(source.contains("user_data.readiness.report_ready()"));
        assert!(!source.contains("readiness: None"));
    }

    #[test]
    fn readiness_preserves_typed_source_context_and_private_diagnostic() {
        let (tx, rx) = mpsc::channel();
        let readiness = Readiness::new(tx, true);
        let error = Error::UnsupportedFormat("synthetic layout; retry".into())
            .with_context(ErrorContext::new(Operation::Link))
            .with_diagnostic_detail("synthetic daemon detail".into());
        let events = crate::owner::BackendEvents::default();
        crate::capture_failures::report_control_failure(&readiness, &events, error.clone());
        assert_eq!(rx.recv().unwrap(), Err(error.clone()));
        crate::capture_failures::report_control_failure(&readiness, &events, Error::DeviceLost);
        assert_eq!(rx.try_recv(), Err(mpsc::TryRecvError::Empty));
        assert_eq!(
            crate::owner::poll_backend_event(&events),
            Some(flexaudio_core::Event::ShutdownError {
                error: Error::DeviceLost
            })
        );
        assert_eq!(error.kind(), flexaudio_core::ErrorKind::UnsupportedFormat);
        assert_eq!(error.diagnostic_details(), ["synthetic daemon detail"]);
    }

    #[test]
    fn eligible_process_target_waits_for_negotiation_and_times_out() {
        let (tx, rx) = mpsc::channel();
        let readiness = Readiness::new(tx, true);
        readiness.set_target_present(true);
        readiness.registry_synchronized();
        assert_eq!(rx.try_recv(), Err(mpsc::TryRecvError::Empty));
        assert!(readiness.deadline_expired());
        let message = rx
            .recv()
            .expect("deadline must report failure")
            .expect_err("eligible target did not negotiate");
        assert!(message.to_string().contains("2 seconds"));
        assert!(message.to_string().contains("retry"));
    }

    #[test]
    fn absent_process_target_remains_successful() {
        let (tx, rx) = mpsc::channel();
        let readiness = Readiness::new(tx, true);
        readiness.registry_synchronized();
        assert!(!readiness.deadline_expired());
        assert_eq!(rx.recv(), Ok(Ok(())));
    }

    #[test]
    fn incomplete_registry_cannot_report_absent_target_success() {
        let (tx, rx) = mpsc::channel();
        let readiness = Readiness::new(tx, true);
        assert!(readiness.deadline_expired());
        assert!(rx.recv().expect("deadline must report").is_err());
    }

    #[test]
    fn monitor_and_process_negotiation_share_deadline_failure() {
        for process_capture in [false, true] {
            let (tx, rx) = mpsc::channel();
            let readiness = Readiness::new(tx, process_capture);
            readiness.set_target_present(true);
            assert!(readiness.deadline_expired());
            let result = rx.recv().expect("deadline must report");
            assert!(result.is_err());
            assert_eq!(result, Err(negotiation_timeout_error()));
            assert!(negotiation_timeout_message().contains("2 seconds"));
        }
    }

    #[test]
    fn negotiated_format_reports_success_once_and_ignores_deadline() {
        for process_capture in [false, true] {
            let (tx, rx) = mpsc::channel();
            let readiness = Readiness::new(tx, process_capture);
            readiness.set_target_present(true);
            readiness.report_ready();
            readiness.report_ready();
            assert!(!readiness.deadline_expired());
            assert_eq!(rx.recv(), Ok(Ok(())));
            assert_eq!(rx.try_recv(), Err(mpsc::TryRecvError::Empty));
        }
    }
    #[test]
    fn vanished_last_source_can_complete_startup_after_control_verification() {
        let (tx, rx) = mpsc::channel();
        let readiness = Readiness::new(tx, true);
        readiness.set_target_present(true);
        readiness.registry_synchronized();
        readiness.set_control_pending(true);
        readiness.set_target_present(false);
        assert_eq!(rx.try_recv(), Err(mpsc::TryRecvError::Empty));
        readiness.set_control_pending(false);
        assert_eq!(rx.try_recv(), Ok(Ok(())));
    }

    #[test]
    fn negotiated_format_cannot_bypass_a_pending_link_error_check() {
        let (tx, rx) = mpsc::channel();
        let readiness = Readiness::new(tx, true);
        readiness.set_control_pending(true);
        readiness.report_ready();
        assert_eq!(rx.try_recv(), Err(mpsc::TryRecvError::Empty));
        readiness.report_failure(Error::DeviceLost);
        readiness.set_control_pending(false);
        assert_eq!(rx.try_recv(), Ok(Err(Error::DeviceLost)));
        assert_eq!(rx.try_recv(), Err(mpsc::TryRecvError::Empty));
    }
    #[test]
    fn replacement_source_still_requires_negotiation_after_a_pending_error() {
        let (tx, rx) = mpsc::channel();
        let readiness = Readiness::new(tx, true);
        readiness.set_control_pending(true);
        readiness.registry_synchronized();
        readiness.set_target_present(true);
        readiness.set_control_pending(false);
        assert_eq!(rx.try_recv(), Err(mpsc::TryRecvError::Empty));
        readiness.report_ready();
        assert_eq!(rx.try_recv(), Ok(Ok(())));
    }
}
