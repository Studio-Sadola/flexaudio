#![cfg(windows)]

//! Windows-specific cpal WASAPI lifetime regression test.

use std::thread;

/// cpal remains usable from another thread even after a short-lived calling thread exits.
///
/// This integration test binary contains only this test, so before the fix the first worker
/// initializes cpal's process-wide enumerator inside `list_devices()` and exits. The next
/// worker's first cpal call then terminates the process with an access violation. After the
/// fix every call goes through the keeper first, so both workers exit safely. Even on GitHub
/// runners with no audio endpoint, `list_devices()` is contractually required to return an empty
/// Vec, so no real microphone is needed.
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
