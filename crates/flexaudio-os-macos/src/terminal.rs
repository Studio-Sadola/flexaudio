//! Durable backend failure and the shared policy for repeated starts.

use std::sync::{Mutex, MutexGuard};

use flexaudio_core::terminal_causes::retain_terminal_cause;
use flexaudio_core::types::{Error, Result};

#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum StartAction {
    Start,
    AlreadyRunning,
}

/// Shared between the backend and its owner; contains no native or audio resources.
#[derive(Default)]
pub(crate) struct TerminalFailure {
    error: Mutex<Option<Error>>,
}

impl TerminalFailure {
    pub(crate) fn error(&self) -> Option<Error> {
        self.state().clone()
    }
    fn state(&self) -> MutexGuard<'_, Option<Error>> {
        match self.error.lock() {
            Ok(error) => error,
            Err(poisoned) => {
                let mut error = poisoned.into_inner();
                // Preserve a known denial. If no cause survived, fail closed with
                // the synchronization failure rather than claim permission denial.
                if error.is_none() {
                    *error = Some(Error::Backend("backend terminal state poisoned".into()));
                }
                error
            }
        }
    }

    /// The first terminal cause owns the backend's remaining lifetime. Stop does not reset it.
    /// Later causes that do not compare equal stay related to the first, in observation order.
    pub(crate) fn record(&self, cause: Error) {
        retain_terminal_cause(&mut self.state(), cause);
    }

    pub(crate) fn check_start(&self, running: bool) -> Result<StartAction> {
        let error = self.state();
        if let Some(error) = error.as_ref() {
            return Err(error.clone());
        }
        Ok(if running {
            StartAction::AlreadyRunning
        } else {
            StartAction::Start
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio_core::types::{ErrorGroup, Permission};
    use std::sync::Arc;

    #[test]
    fn distinct_failure_contract() {
        flexaudio_core::test_support::assert_distinct_failures_retained(
            Error::DeviceLost,
            Error::Backend("later capture failure".into()),
            |first, second| {
                let terminal = TerminalFailure::default();
                terminal.record(first);
                terminal.record(second);
                terminal.error().unwrap()
            },
        );
    }

    fn denial() -> Error {
        Error::PermissionDenied {
            permission: Permission::SystemAudio,
            detail: "controlled diagnostic capture stayed zero".into(),
        }
    }

    #[test]
    fn ordinary_starts_preserve_running_idempotency() {
        let terminal = TerminalFailure::default();
        assert_eq!(terminal.check_start(false), Ok(StartAction::Start));
        assert_eq!(terminal.check_start(true), Ok(StartAction::AlreadyRunning));
        assert_eq!(terminal.check_start(false), Ok(StartAction::Start));
    }

    #[test]
    fn terminal_cause_rejects_running_and_stopped_restarts_without_consumption() {
        for cause in [denial(), Error::Backend("publication gate poisoned".into())] {
            let terminal = TerminalFailure::default();
            terminal.record(cause.clone());
            for running in [true, false, true, false] {
                assert_eq!(terminal.check_start(running), Err(cause.clone()));
            }
        }
    }

    #[test]
    fn owner_failure_survives_owner_join_and_keeps_first_typed_cause_primary() {
        let terminal = Arc::new(TerminalFailure::default());
        let owner = terminal.clone();
        let cause = denial();
        let owner_cause = cause.clone();
        std::thread::spawn(move || owner.record(owner_cause))
            .join()
            .unwrap();
        let later = Error::Backend("later shutdown error".into());
        terminal.record(later.clone());
        let expected = Error::Multiple(ErrorGroup::new(cause, later, Vec::new()));
        assert_eq!(terminal.check_start(true), Err(expected.clone()));
        assert_eq!(terminal.check_start(false), Err(expected));
    }

    #[test]
    fn later_distinct_cause_is_retained_and_equal_cause_collapses() {
        let terminal = TerminalFailure::default();
        let later = Error::Backend("b".into());
        for cause in [
            Error::DeviceLost,
            Error::DeviceLost,
            later.clone(),
            later.clone(),
        ] {
            terminal.record(cause);
        }
        let expected = Error::Multiple(ErrorGroup::new(Error::DeviceLost, later, Vec::new()));
        assert_eq!(terminal.error(), Some(expected));

        let terminal = TerminalFailure::default();
        terminal.record(Error::DeviceLost);
        terminal.record(Error::DeviceLost);
        assert_eq!(terminal.error(), Some(Error::DeviceLost));
    }

    #[test]
    fn poisoned_durable_state_fails_closed_and_preserves_known_denial() {
        use std::panic::{catch_unwind, AssertUnwindSafe};
        for cause in [None, Some(denial())] {
            let terminal = TerminalFailure::default();
            if let Some(cause) = cause.as_ref() {
                terminal.record(cause.clone());
            }
            assert!(catch_unwind(AssertUnwindSafe(|| {
                let _guard = terminal.error.lock().unwrap();
                panic!("injected terminal state poisoning");
            }))
            .is_err());
            let expected =
                cause.unwrap_or_else(|| Error::Backend("backend terminal state poisoned".into()));
            assert_eq!(terminal.check_start(true), Err(expected.clone()));
            let later = Error::Backend("later failure".into());
            terminal.record(later.clone());
            let expected = Error::Multiple(ErrorGroup::new(expected, later, Vec::new()));
            assert_eq!(terminal.check_start(false), Err(expected));
        }
    }
}
