//! Keeps cpal's process-shared WASAPI enumerator alive on Windows.

use std::any::Any;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc;
use std::sync::OnceLock;
use std::thread;

use cpal::traits::HostTrait;

use flexaudio_core::types::{Error, Result};

/// Result of the long-lived keeper that initialized cpal's `OnceLock<Enumerator>`.
///
/// Other callers wait until the `OnceLock` initialization closure returns. Therefore, if this
/// value is `Ok`, it is guaranteed that the keeper has already created the WASAPI enumerator.
static KEEPER_READY: OnceLock<std::result::Result<(), String>> = OnceLock::new();

/// Ensures cpal's WASAPI enumerator has been initialized on the keeper thread.
///
/// cpal 0.16 holds the `IMMDeviceEnumerator` in a process-shared `OnceLock`, while it keeps the
/// STA initialization of the thread that first created it in a thread-local RAII guard. When
/// that thread exits, the enumerator is tied to a released COM apartment, and any later use is
/// an access violation. Call this function before every cpal entry point so that the first
/// initializing thread lives until the process exits.
pub(super) fn ensure() -> Result<()> {
    match KEEPER_READY.get_or_init(start_keeper) {
        Ok(()) => Ok(()),
        Err(message) => Err(Error::Backend(message.clone())),
    }
}

/// Starts the keeper and waits for WASAPI enumerator initialization to complete.
fn start_keeper() -> std::result::Result<(), String> {
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let handle = thread::Builder::new()
        .name("flexaudio-cpal-wasapi-keeper".into())
        .spawn(move || {
            // cpal reports a COM initialization failure with a panic. At this boundary it is
            // converted into a typed error and returned to the caller. On failure the thread
            // exits; only on success does it stay alive.
            let initialized = catch_unwind(AssertUnwindSafe(initialize_enumerator))
                .map_err(panic_message)
                .and_then(|result| result);
            let keep_alive = initialized.is_ok();
            let _ = ready_tx.send(initialized);

            if keep_alive {
                // cpal's COM guard is thread-local and calls CoUninitialize only when the
                // thread exits. The keeper itself accepts no COM calls and only keeps alive the
                // creator of the enumerator that cpal holds process-wide, so it does not need
                // to process a Windows message queue. park waits until the process exits while
                // keeping the guard alive, without consuming CPU.
                loop {
                    thread::park();
                }
            }
        })
        .map_err(|error| format!("spawn cpal WASAPI keeper thread: {error}"))?;

    // Drop the JoinHandle to detach the keeper. On success the keeper intentionally does not
    // exit for the whole process lifetime, so the caller is not given responsibility to join it.
    drop(handle);

    ready_rx
        .recv()
        .map_err(|_| "cpal WASAPI keeper exited before initialization completed".to_owned())?
}

/// Initializes cpal's process-wide `ENUMERATOR` on this long-lived thread.
fn initialize_enumerator() -> std::result::Result<(), String> {
    let host = cpal::default_host();
    // On Windows/WASAPI, `default_input_device()` goes through `get_enumerator()` first even
    // when there is no input endpoint. So even if it returns None, the goal of having the keeper
    // initialize `ENUMERATOR` is met.
    let _ = host.default_input_device();
    Ok(())
}

/// Turns a `catch_unwind` payload into a diagnosable error string.
fn panic_message(payload: Box<dyn Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        format!("cpal WASAPI keeper initialization panicked: {message}")
    } else if let Some(message) = payload.downcast_ref::<String>() {
        format!("cpal WASAPI keeper initialization panicked: {message}")
    } else {
        "cpal WASAPI keeper initialization panicked with a non-string payload".to_owned()
    }
}
