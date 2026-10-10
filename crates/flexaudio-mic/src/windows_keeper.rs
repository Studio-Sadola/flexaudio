//! Keep cpal's process-wide WASAPI enumerator alive on Windows.

use std::any::Any;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc;
use std::sync::OnceLock;
use std::thread;

use cpal::traits::HostTrait;

use flexaudio_core::types::{Error, Result};

/// Result from the long-lived keeper that initializes cpal's `OnceLock<Enumerator>`.
///
/// Other callers wait for the `OnceLock` initializer to return. Therefore, `Ok` guarantees that
/// the keeper has created the WASAPI enumerator.
static KEEPER_READY: OnceLock<std::result::Result<(), String>> = OnceLock::new();

/// Initialize cpal's WASAPI enumerator on the keeper thread.
///
/// cpal 0.16 stores `IMMDeviceEnumerator` in a process-wide `OnceLock`, while the STA
/// initialization of the thread that first creates it is held by thread-local RAII. When that
/// thread exits, the enumerator remains tied to a released COM apartment, and later use causes an
/// access violation. Call this before every cpal entry point to keep the first initialization
/// thread alive until process exit.
pub(super) fn ensure() -> Result<()> {
    match KEEPER_READY.get_or_init(start_keeper) {
        Ok(()) => Ok(()),
        Err(message) => Err(Error::Backend(message.clone())),
    }
}

/// Start the keeper and wait for WASAPI enumerator initialization to finish.
fn start_keeper() -> std::result::Result<(), String> {
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let handle = thread::Builder::new()
        .name("flexaudio-cpal-wasapi-keeper".into())
        .spawn(move || {
            // cpal reports COM initialization failures by panicking. Convert that panic to a
            // typed error at this boundary and return it to the caller. Exit on failure; stay
            // alive only on success.
            let initialized = catch_unwind(AssertUnwindSafe(initialize_enumerator))
                .map_err(panic_message)
                .and_then(|result| result);
            let keep_alive = initialized.is_ok();
            let _ = ready_tx.send(initialized);

            if keep_alive {
                // cpal's COM guard is thread-local and calls CoUninitialize only when the thread
                // exits. The keeper does not accept COM calls; it only keeps alive the thread
                // that created cpal's process-wide enumerator. It does not need to process the
                // Windows message queue. `park` keeps the guard alive until process exit without
                // consuming CPU.
                loop {
                    thread::park();
                }
            }
        })
        .map_err(|error| format!("spawn cpal WASAPI keeper thread: {error}"))?;

    // Drop the JoinHandle to detach the keeper. On success it intentionally stays alive for the
    // process lifetime, so callers do not have to join it.
    drop(handle);

    ready_rx
        .recv()
        .map_err(|_| "cpal WASAPI keeper exited before initialization completed".to_owned())?
}

/// Initialize cpal's process-wide `ENUMERATOR` on this long-lived thread.
fn initialize_enumerator() -> std::result::Result<(), String> {
    let host = cpal::default_host();
    // On Windows/WASAPI, `default_input_device()` calls `get_enumerator()` first even when no
    // input endpoint exists. Thus, the keeper initializes `ENUMERATOR` even if the result is None.
    let _ = host.default_input_device();
    Ok(())
}

/// Panic payloads can contain private paths or device names; never retain them.
fn panic_message(_payload: Box<dyn Any + Send>) -> String {
    "cpal WASAPI keeper initialization panicked".to_owned()
}
