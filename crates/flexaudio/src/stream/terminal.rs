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

    /// Caller holds the delivery lock. Publish the cause before closing delivery.
    /// Returns true only for the first failure (one terminal event per stream).
    pub(super) fn record(&self, cause: Error) -> bool {
        let mut error = self.error.lock().unwrap_or_else(|e| e.into_inner());
        if error.is_some() {
            return false;
        }
        *error = Some(cause);
        self.failed.store(true, Ordering::SeqCst);
        true
    }
}
