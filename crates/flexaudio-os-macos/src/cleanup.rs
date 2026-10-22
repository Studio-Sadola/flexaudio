//! Generation-owned cleanup diagnostics, including native owners dropped during unwind.

use std::sync::Mutex;

use flexaudio_core::Error;

#[derive(Default)]
pub(crate) struct CleanupLedger(Mutex<Vec<Error>>);

impl CleanupLedger {
    pub(crate) fn extend(&self, errors: impl IntoIterator<Item = Error>) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend(errors);
    }

    pub(crate) fn take(&self) -> Vec<Error> {
        std::mem::take(
            &mut *self
                .0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }
}

/// Native teardown sites shared by production cleanup and device-free injection.
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NativeCleanup {
    TapStop,
    TapIoProc,
    TapAggregate,
    TapDestroy,
    ProbeStop,
    ProbeIoProc,
    ProbeAggregate,
    ProbeTap,
}

#[cfg(target_os = "macos")]
pub(crate) fn native_cleanup(operation: NativeCleanup, call: impl FnOnce() -> i32) -> i32 {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(failing) = INJECTION.with(|injection| injection.get()) {
        return if operation == failing { -1 } else { 0 };
    }
    #[cfg(not(any(test, feature = "test-support")))]
    let _ = operation;
    call()
}

#[cfg(all(target_os = "macos", any(test, feature = "test-support")))]
thread_local! {
    static INJECTION: std::cell::Cell<Option<NativeCleanup>> = const { std::cell::Cell::new(None) };
}

#[cfg(all(target_os = "macos", any(test, feature = "test-support")))]
pub(crate) fn inject<T>(operation: NativeCleanup, run: impl FnOnce() -> T) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            INJECTION.with(|injection| injection.set(None));
        }
    }
    INJECTION.with(|injection| {
        assert!(injection.replace(Some(operation)).is_none());
    });
    let _reset = Reset;
    run()
}
