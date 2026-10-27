//! Internal shutdown injection fixtures, with no stability guarantee.
//!
//! Enabled only for tests or by the non-default `test-support` feature.

use crate::*;

macro_rules! phases {
    ($( $(#[$doc:meta])* $variant:ident, )*) => {
        /// Real owner operations selected for failure injection.
        #[derive(Clone, Copy)]
        pub enum Phase {
            $( $(#[$doc])* $variant, )*
        }

        impl Phase {
            /// Every supported injection phase, generated from its definition.
            pub const ALL: &'static [Self] = &[$(Self::$variant),*];
        }
    };
}

phases! {
    /// Microphone capture owner join failure.
    Join,
    /// Callback terminal failure followed by an owner join failure.
    TerminalAndJoin,
    /// Callback terminal event consumed before its owner join fails.
    ConsumedTerminalAndJoin,
}

/// Exercise void stop, checked stop, and repeated stop through a real worker join.
/// This internal test entry has no stability guarantee and runs on every CI OS.
pub fn check(phase: Phase) {
    check_observed(phase, |_| {});
}

fn check_observed(phase: Phase, observe: impl Fn(&CpalMicBackend)) {
    for checked_first in [false, true] {
        let mut owner = CpalMicBackend::with_format(None, FALLBACK_FORMAT);
        let terminal = match phase {
            Phase::Join => false,
            Phase::TerminalAndJoin | Phase::ConsumedTerminalAndJoin => true,
        };
        if terminal {
            owner
                .callback_errors
                .record(cpal::StreamError::DeviceNotAvailable);
        }
        if matches!(phase, Phase::ConsumedTerminalAndJoin) {
            assert_eq!(
                owner.poll_event(),
                Some(Event::TerminalError {
                    error: Error::DeviceLost
                })
            );
        }
        owner.handle = Some(thread::spawn(|| panic!("injected microphone join failure")));
        let cleanup = Error::Backend("microphone capture owner panicked".into())
            .with_context(ErrorContext::new(Operation::Join));
        let expected = ShutdownReport::new(terminal.then_some(Error::DeviceLost), vec![cleanup]);
        if checked_first {
            assert_eq!(owner.stop_checked(), expected.result());
        } else {
            owner.stop();
        }
        assert_eq!(owner.shutdown_report(), Some(&expected));
        assert_eq!(owner.stop_checked(), expected.result());
        assert_eq!(owner.shutdown_report(), Some(&expected));
        owner.stop();
        assert_eq!(owner.shutdown_report(), Some(&expected));
        assert_eq!(owner.stop_checked(), expected.result());
        observe(&owner);
    }
}

/// Exercise every microphone phase and observe each tested concrete owner.
/// This internal test entry has no stability guarantee and runs on every CI OS.
pub fn check_all(observe: fn(&CpalMicBackend)) {
    let mut owner = CpalMicBackend::with_format(None, FALLBACK_FORMAT);
    owner.handle = Some(thread::spawn(|| {}));
    assert_eq!(
        owner.complete_start(Err(Error::DeviceNotFound), None),
        Err(Error::DeviceNotFound)
    );
    assert!(owner.stop_checked().is_ok());
    assert!(owner.shutdown_report().unwrap().primary().is_none());
    assert!(owner.poll_event().is_none());
    observe(&owner);
    for join_failure in [false, true] {
        let mut owner = CpalMicBackend::with_format(None, FALLBACK_FORMAT);
        owner
            .callback_errors
            .record(cpal::StreamError::DeviceNotAvailable);
        owner.handle = Some(thread::spawn(move || {
            assert!(!join_failure, "injected failed-start owner panic");
        }));
        let result = owner
            .complete_start(Err(Error::DeviceNotFound), None)
            .unwrap_err();
        assert_eq!(result.root(), &Error::DeviceNotFound);
        let report = owner.shutdown_report().unwrap();
        assert!(report.primary().is_none());
        assert_eq!(report.cleanup().len(), usize::from(join_failure));
        let startup = Error::Multiple(flexaudio_core::ErrorGroup::new(
            Error::DeviceNotFound,
            Error::DeviceLost,
            Vec::new(),
        ));
        assert_eq!(
            Err(result),
            ShutdownReport::new(Some(startup), report.cleanup().to_vec()).result()
        );
        assert_eq!(owner.stop_checked().is_err(), join_failure);
        while let Some(event) = owner.poll_event() {
            assert!(!matches!(event, Event::TerminalError { .. }));
        }
        assert!(owner.terminal_error.is_none());
        // A completed fixture owner takes the existing duplicate-start path,
        // proving the rejected attempt cannot immediately reject the next start.
        owner.handle = Some(thread::spawn(|| {}));
        let (producer, _consumer) = flexaudio_core::raw_ring(16);
        assert_eq!(owner.start(RawSink::new(producer, 48_000, 1)), Ok(()));
        // The prior retained report intentionally bypasses a second owner join.
        assert!(owner.stop_owner(OwnerStopPhase::Runtime).is_none());
        observe(&owner);
    }
    for &phase in Phase::ALL {
        check_observed(phase, observe);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_failure_during_rejected_start_is_returned_without_runtime_terminality() {
        check_all(|_| {});
    }

    #[test]
    fn microphone_shutdown_retains_primary_and_join_cleanup() {
        check_all(|_| {});
    }

    #[test]
    fn microphone_rollback_retains_current_and_previous_join_cleanup() {
        let mut owner = CpalMicBackend::with_format(None, FALLBACK_FORMAT);
        owner.handle = Some(thread::spawn(|| panic!("previous generation join failure")));
        owner.stop();
        let previous = owner.shutdown.take();
        let first_cleanup = previous.as_ref().unwrap().cleanup()[0].clone();
        owner.handle = Some(thread::spawn(|| panic!("rollback join failure")));
        let result = owner.complete_start(Err(Error::DeviceLost), previous);
        let report = owner
            .shutdown_report()
            .expect("rollback retains its report")
            .clone();
        assert!(report.primary().is_none());
        assert_eq!(report.cleanup(), &[first_cleanup]);
        assert_eq!(
            result,
            ShutdownReport::new(Some(Error::DeviceLost), report.cleanup().to_vec()).result()
        );
        owner.stop();
        assert_eq!(owner.shutdown_report(), Some(&report));
        assert_eq!(owner.stop_checked(), report.result());
    }

    #[test]
    fn failed_restart_keeps_previous_primary_and_new_start_error_distinct() {
        flexaudio_core::test_support::assert_distinct_failures_retained(
            Error::Unsupported,
            Error::DeviceLost,
            |startup, previous| {
                let mut owner = CpalMicBackend::with_format(None, FALLBACK_FORMAT);
                let old_cleanup = Error::Backend("previous capture cleanup failure".into());
                let old = ShutdownReport::new(Some(previous.clone()), vec![old_cleanup.clone()]);
                owner.handle = Some(thread::spawn(|| panic!("failed restart join failure")));
                let result = owner.complete_start(Err(startup), Some(old)).unwrap_err();
                let retained = owner.shutdown_report().unwrap();
                assert_eq!(retained.primary(), Some(&previous));
                assert_eq!(retained.cleanup()[0], old_cleanup);
                assert_eq!(retained.cleanup().len(), 2);
                for error in retained.cleanup() {
                    assert!(flexaudio_core::terminal_causes::retains_terminal_cause(
                        &result, error
                    ));
                }
                result
            },
        );
    }
}
