#![cfg(windows)]

//! Windows-specific regression test for cpal WASAPI lifetime.

use std::thread;

/// cpal remains usable from another thread after a short-lived caller thread exits.
///
/// This is the only test in this integration test binary. Before the fix, the first worker
/// initialized cpal's process-wide enumerator inside `list_devices()` and then exited. The next
/// worker's first cpal call terminated the process with an access violation. With the fix, each
/// call goes through the keeper first, so both workers exit safely. The test needs no real
/// microphone: `list_devices()` is expected to return an empty Vec on GitHub runners without audio endpoints.
#[test]
fn cpal_survives_after_calling_thread_exits() {
    let first = thread::spawn(flexaudio_mic::list_devices)
        .join()
        .expect("first cpal caller must not panic");
    first.expect("list_devices must return Ok even when no input endpoint exists");

    let second = thread::spawn(flexaudio_mic::list_devices)
        .join()
        .expect("second cpal caller must not panic after the first thread exits");
    second.expect("list_devices must return Ok even when no input endpoint exists");
}
