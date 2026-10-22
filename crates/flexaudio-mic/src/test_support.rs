//! Internal shutdown injection fixtures, with no stability guarantee.
//!
//! Enabled only for tests or by the non-default `test-support` feature.

use crate::*;

/// Real owner operations selected for failure injection.
#[derive(Clone, Copy)]
pub enum Phase {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn microphone_shutdown_retains_primary_and_join_cleanup() {
        check(Phase::Join);
        check(Phase::TerminalAndJoin);
        check(Phase::ConsumedTerminalAndJoin);
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
        assert_eq!(report.primary(), Some(&Error::DeviceLost));
        assert_eq!(report.cleanup(), &[first_cleanup.clone(), first_cleanup]);
        assert_eq!(result, report.result());
        owner.stop();
        assert_eq!(owner.shutdown_report(), Some(&report));
        assert_eq!(owner.stop_checked(), report.result());
    }
}
