//! Durable backend failure and the shared policy for repeated starts.

use std::sync::{Mutex, MutexGuard};

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
    pub(crate) fn record(&self, cause: Error) {
        let mut error = self.state();
        if error.is_none() {
            *error = Some(cause);
        }
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
    use flexaudio_core::types::Permission;
    use std::sync::Arc;

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
    fn owner_failure_survives_owner_join_and_keeps_first_typed_cause() {
        let terminal = Arc::new(TerminalFailure::default());
        let owner = terminal.clone();
        let cause = denial();
        let owner_cause = cause.clone();
        std::thread::spawn(move || owner.record(owner_cause))
            .join()
            .unwrap();
        terminal.record(Error::Backend("later shutdown error".into()));
        assert_eq!(terminal.check_start(true), Err(cause.clone()));
        assert_eq!(terminal.check_start(false), Err(cause));
    }

    #[test]
    fn poisoned_durable_state_fails_closed_and_preserves_known_denial() {
        use std::panic::{catch_unwind, AssertUnwindSafe};
        for cause in [None, Some(denial())] {
            let terminal = TerminalFailure::default();
            if let Some(cause) = cause.as_ref() {
                terminal.record(cause.clone());
            }
            let _ = catch_unwind(AssertUnwindSafe(|| {
                let _guard = terminal.error.lock().unwrap();
                panic!("injected terminal state poisoning");
            }));
            let expected =
                cause.unwrap_or_else(|| Error::Backend("backend terminal state poisoned".into()));
            assert_eq!(terminal.check_start(true), Err(expected.clone()));
            terminal.record(Error::Backend("later failure".into()));
            assert_eq!(terminal.check_start(false), Err(expected));
        }
    }
}
