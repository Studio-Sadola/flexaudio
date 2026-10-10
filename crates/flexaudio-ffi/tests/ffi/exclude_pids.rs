//! C entry-point regressions. Native-open tests query device configuration and
//! recording permission. Typed host microphone denial skips their hardware assertions
//! on macOS/Windows; boundary-validation and mock tests never access audio devices.

use std::ffi::CStr;
use std::ptr;

use crate::error::code;
use crate::types::{FlexConfig, FlexProcessMode, FlexSourceKind, FlexStream, FlexVadConfig};
use crate::{
    flexaudio_free, flexaudio_last_error, flexaudio_open, flexaudio_open_with_exclude_pids,
    flexaudio_switch_source, flexaudio_switch_source_with_exclude_pids,
};

fn config(kind: i32) -> FlexConfig {
    FlexConfig {
        kind,
        device_id: ptr::null(),
        process_id: 0,
        mode: FlexProcessMode::Include as i32,
        exclude_self: 0,
        output_rate: 0,
        output_channels: 0,
        chunk_ms: 0,
        gain: 0.0,
        mix_mic_device_id: ptr::null(),
        mix_system_device_id: ptr::null(),
        mix_mic_gain: 0.0,
        mix_system_gain: 0.0,
        denoise: 0,
        has_vad: 0,
        vad: FlexVadConfig {
            threshold: 0.0,
            neg_threshold: 0.0,
            min_speech_ms: 0,
            min_silence_ms: 0,
            speech_pad_ms: 0,
            max_speech_ms: 0,
            sample_rate: 0,
        },
    }
}

fn last_error() -> String {
    let message = flexaudio_last_error();
    assert!(!message.is_null(), "expected a thread-local error message");
    // SAFETY: The FFI owns this NUL-terminated message until the next call on this thread.
    unsafe { CStr::from_ptr(message) }
        .to_str()
        .expect("error is UTF-8")
        .to_owned()
}

struct Handle(*mut FlexStream);

impl Handle {
    fn new(pointer: *mut FlexStream) -> Self {
        assert!(!pointer.is_null(), "open failed: {}", last_error());
        Self(pointer)
    }

    fn native(pointer: *mut FlexStream, test: &str) -> Option<Self> {
        if pointer.is_null() {
            if let Some(
                error @ flexaudio::Error::PermissionDenied {
                    permission: flexaudio::Permission::Microphone,
                    ..
                },
            ) = crate::error::take_open_failure()
            {
                if cfg!(any(target_os = "windows", target_os = "macos")) {
                    eprintln!("Skipping {test}: host microphone permission is denied: {error}");
                    return None;
                }
            }
        }
        Some(Self::new(pointer))
    }

    fn pids(&self) -> &[u32] {
        // SAFETY: This handle is live, uniquely owned, and freed only in Drop.
        unsafe { &*self.0 }.inner.config().exclude_pids.as_slice()
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: The owned handle is released exactly once.
        unsafe { flexaudio_free(self.0) };
    }
}

// Switch validation can use an unstarted mock stream: a valid list reaches InvalidState,
// while an invalid list must fail at the C boundary first, without trying any devices.
fn mock_handle() -> Handle {
    let inner = flexaudio::Stream::open(
        flexaudio::StreamConfig::default(),
        Box::new(flexaudio::MockBackend::new(48_000, 2, 0.0)),
    )
    .expect("mock stream opens");
    Handle(Box::into_raw(Box::new(FlexStream {
        shutdown: None,
        shutdown_event_index: 0,
        last_output: None,
        whisper: None,
        whisper_events: Vec::new(),
        whisper_origin: (0, 0),
        whisper_error: None,
        whisper_error_reported: false,
        ready_chunks: std::collections::VecDeque::new(),
        inner,
        denoiser: None,
        vad: None,
    })))
}

fn assert_invalid_detail(expected: &str) {
    let error = crate::error::last_audio_error().expect("typed argument error retained");
    let flexaudio::Error::InvalidArg(detail) = &error else {
        panic!("expected typed InvalidArg");
    };
    assert_eq!(detail, expected);
    assert_eq!(last_error(), error.to_string());
    assert_eq!(last_error(), format!("invalid argument: {expected}"));
}

fn assert_invalid_list(pointer: *const u32, len: usize, expected: &str) {
    let cfg = config(FlexSourceKind::Mic as i32);
    // SAFETY: Invalid pointer shapes must be rejected before any dereference; other test
    // inputs refer to valid arrays. Config and mock handles stay live during the calls.
    let opened = unsafe { flexaudio_open_with_exclude_pids(&cfg, pointer, len) };
    assert!(
        opened.is_null(),
        "invalid list unexpectedly opened a stream"
    );
    assert_invalid_detail(expected);

    let handle = mock_handle();
    let result = unsafe { flexaudio_switch_source_with_exclude_pids(handle.0, &cfg, pointer, len) };
    assert_eq!(result, code::FLEX_INVALID_ARG);
    assert_invalid_detail(expected);
    assert!(
        handle.pids().is_empty(),
        "invalid switch changed the config"
    );
}

#[test]
fn null_with_nonzero_length_is_invalid_for_open_and_switch() {
    assert_invalid_list(
        ptr::null(),
        1,
        "exclude_pids: pointer is null with nonzero length",
    );
}

#[test]
fn excessive_length_is_rejected_before_dereferencing() {
    // A one-element allocation cannot be read as 4097 elements. The cap must reject first.
    let pid = 123;
    assert_invalid_list(&pid, 4097, "exclude_pids: too many entries (max 4096)");
    assert_invalid_list(
        ptr::null(),
        usize::MAX,
        "exclude_pids: too many entries (max 4096)",
    );
}

#[test]
fn unaligned_pointer_is_invalid_for_open_and_switch() {
    let backing = [123_u32; 2];
    let unaligned = backing.as_ptr().cast::<u8>().wrapping_add(1).cast::<u32>();
    assert_invalid_list(
        unaligned,
        1,
        "exclude_pids: pointer is not aligned for uint32_t",
    );
}

#[test]
fn zero_pid_message_names_index_two_for_every_source() {
    let pids = [123, 123, 0];
    let message = "exclude_pids[2] must be a positive integer in 1..=4294967295, got 0";
    assert_invalid_list(pids.as_ptr(), pids.len(), message);
    let handle = mock_handle();
    for kind in [
        FlexSourceKind::Mic as i32,
        FlexSourceKind::System as i32,
        FlexSourceKind::Process as i32,
        FlexSourceKind::Mix as i32,
    ] {
        let cfg = config(kind);
        // SAFETY: All inputs are valid arrays/configs; the list contains a disallowed value.
        assert!(
            unsafe { flexaudio_open_with_exclude_pids(&cfg, pids.as_ptr(), pids.len()) }.is_null()
        );
        assert_invalid_detail(message);
        assert_eq!(
            unsafe {
                flexaudio_switch_source_with_exclude_pids(handle.0, &cfg, pids.as_ptr(), pids.len())
            },
            code::FLEX_INVALID_ARG,
        );
        assert_invalid_detail(message);
    }
}

#[test]
fn empty_null_list_and_legacy_open_succeed() {
    if std::env::var("FLEXAUDIO_RUN_NATIVE_TESTS").as_deref() != Ok("1") {
        return;
    }
    let cfg = config(FlexSourceKind::Mic as i32);
    // SAFETY: A zero length permits a NULL list; cfg is valid for both entry points.
    let Some(extended) = Handle::native(
        unsafe { flexaudio_open_with_exclude_pids(&cfg, ptr::null(), 0) },
        "empty_null_list_and_legacy_open_succeed",
    ) else {
        return;
    };
    assert!(extended.pids().is_empty());
    assert!(flexaudio_last_error().is_null());
    let Some(legacy) = Handle::native(
        unsafe { flexaudio_open(&cfg) },
        "empty_null_list_and_legacy_open_succeed",
    ) else {
        return;
    };
    assert!(legacy.pids().is_empty());
    assert!(flexaudio_last_error().is_null());

    let handle = mock_handle();
    let extended_result =
        unsafe { flexaudio_switch_source_with_exclude_pids(handle.0, &cfg, ptr::null(), 0) };
    assert_eq!(extended_result, code::FLEX_INVALID_STATE);
    assert!(last_error().contains("switch_source is only available on a started stream"));
    let extended_error = last_error();
    assert_eq!(
        unsafe { flexaudio_switch_source(handle.0, &cfg) },
        extended_result,
    );
    assert_eq!(last_error(), extended_error);
}

#[test]
fn empty_list_never_checks_or_dereferences_its_pointer() {
    if std::env::var("FLEXAUDIO_RUN_NATIVE_TESTS").as_deref() != Ok("1") {
        return;
    }
    let cfg = config(FlexSourceKind::Mic as i32);
    let backing = [123_u32; 2];
    let unaligned = backing.as_ptr().cast::<u8>().wrapping_add(1).cast::<u32>();
    // SAFETY: A zero-length list must not be dereferenced, even if its pointer is unaligned.
    let Some(handle) = Handle::native(
        unsafe { flexaudio_open_with_exclude_pids(&cfg, unaligned, 0) },
        "empty_list_never_checks_or_dereferences_its_pointer",
    ) else {
        return;
    };
    assert!(handle.pids().is_empty());
    assert_eq!(
        unsafe { flexaudio_switch_source_with_exclude_pids(handle.0, &cfg, unaligned, 0) },
        code::FLEX_INVALID_STATE,
    );
    assert!(last_error().contains("switch_source is only available on a started stream"));
}

#[test]
fn duplicates_order_and_maximum_pid_are_preserved_in_owned_storage() {
    if std::env::var("FLEXAUDIO_RUN_NATIVE_TESTS").as_deref() != Ok("1") {
        return;
    }
    let cfg = config(FlexSourceKind::Mic as i32);
    let mut pids = vec![123, u32::MAX, 123, 42];
    let original = pids.clone();
    // SAFETY: All entries are initialized and readable throughout the call.
    let Some(handle) = Handle::native(
        unsafe { flexaudio_open_with_exclude_pids(&cfg, pids.as_ptr(), pids.len()) },
        "duplicates_order_and_maximum_pid_are_preserved_in_owned_storage",
    ) else {
        return;
    };
    pids.fill(0);
    drop(pids);
    assert_eq!(handle.pids(), original);
    assert!(flexaudio_last_error().is_null());

    let mock = mock_handle();
    assert_eq!(
        unsafe {
            flexaudio_switch_source_with_exclude_pids(
                mock.0,
                &cfg,
                original.as_ptr(),
                original.len(),
            )
        },
        code::FLEX_INVALID_STATE,
    );
    assert!(last_error().contains("switch_source is only available on a started stream"));
}

#[test]
fn maximum_length_is_accepted() {
    if std::env::var("FLEXAUDIO_RUN_NATIVE_TESTS").as_deref() != Ok("1") {
        return;
    }
    let cfg = config(FlexSourceKind::Mic as i32);
    let pids = vec![123; 4096];
    // SAFETY: The whole PID allocation is initialized, readable, and within the cap.
    let Some(handle) = Handle::native(
        unsafe { flexaudio_open_with_exclude_pids(&cfg, pids.as_ptr(), pids.len()) },
        "maximum_length_is_accepted",
    ) else {
        return;
    };
    assert_eq!(handle.pids(), pids);
}
