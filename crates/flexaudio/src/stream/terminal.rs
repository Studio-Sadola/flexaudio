//! Durable terminal capture state, independent of capture-thread lifetime.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use flexaudio_core::types::Error;

#[derive(Default)]
pub(super) struct TerminalFailure {
    failed: AtomicBool,
    error: Mutex<Option<Error>>,
}

impl TerminalFailure {
    pub(super) fn is_failed(&self) -> bool {
        self.failed.load(Ordering::SeqCst)
    }

    pub(super) fn error(&self) -> Option<Error> {
        self.error.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Whether `error` is already retained as the primary or a related cause.
    pub(super) fn retains(&self, error: &Error) -> bool {
        self.error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|retained| {
                flexaudio_core::terminal_causes::retains_terminal_cause(retained, error)
            })
    }

    /// Caller holds the delivery lock. Publish the cause before closing delivery.
    /// Returns true only for the first failure (one terminal event per stream).
    /// Later causes that do not compare equal stay related to the first.
    pub(super) fn record(&self, cause: Error) -> bool {
        let mut error = self.error.lock().unwrap_or_else(|e| e.into_inner());
        let first = flexaudio_core::terminal_causes::retain_terminal_cause(&mut error, cause);
        self.failed.store(true, Ordering::SeqCst);
        first
    }
}
