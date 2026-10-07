//! C entry-point regressions. Opening a mic stream needs no working device because its
//! constructor may query devices and recording permission; native-open tests are ignored
//! by default. Boundary-validation and mock tests never access audio devices.

use std::ffi::CStr;
use std::ptr;

use crate::error::code;
use crate::types::{FlexConfig, FlexProcessMode, FlexSourceKind, FlexStream, FlexVadConfig};
use crate::{
    flexaudio_free, flexaudio_last_error, flexaudio_open, flexaudio_open_with_exclude_pids,
    flexaudio_switch_source, flexaudio_switch_source_with_exclude_pids,
};

fn config(kind: FlexSourceKind) -> FlexConfig {
    FlexConfig {
        kind,
        device_id: ptr::null(),
        process_id: 0,
        mode: FlexProcessMode::Include,
        exclude_self: false,
        output_rate: 0,
        output_channels: 0,
        chunk_ms: 0,
        gain: 0.0,
        mix_mic_device_id: ptr::null(),
        mix_system_device_id: ptr::null(),
        mix_mic_gain: 0.0,
        mix_system_gain: 0.0,
        denoise: false,
        has_vad: false,
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
        inner,
        denoiser: None,
        vad: None,
    })))
}

fn assert_invalid_list(pointer: *const u32, len: usize, expected: &str) {
    let cfg = config(FlexSourceKind::Mic);
    // SAFETY: Invalid pointer shapes must be rejected before any dereference; other test
    // inputs refer to valid arrays. Config and mock handles stay live during the calls.
    let opened = unsafe { flexaudio_open_with_exclude_pids(&cfg, pointer, len) };
    assert!(
        opened.is_null(),
        "invalid list unexpectedly opened a stream"
    );
    assert_eq!(last_error(), format!("invalid argument: {expected}"));

    let handle = mock_handle();
    let result = unsafe { flexaudio_switch_source_with_exclude_pids(handle.0, &cfg, pointer, len) };
    assert_eq!(result, code::FLEX_INVALID_ARG);
    assert_eq!(last_error(), format!("invalid argument: {expected}"));
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
        FlexSourceKind::Mic,
        FlexSourceKind::System,
        FlexSourceKind::Process,
        FlexSourceKind::Mix,
    ] {
        let cfg = config(kind);
        // SAFETY: All inputs are valid arrays/configs; the list contains a disallowed value.
        assert!(
            unsafe { flexaudio_open_with_exclude_pids(&cfg, pids.as_ptr(), pids.len()) }.is_null()
        );
        assert_eq!(last_error(), format!("invalid argument: {message}"));
        assert_eq!(
            unsafe {
                flexaudio_switch_source_with_exclude_pids(handle.0, &cfg, pids.as_ptr(), pids.len())
            },
            code::FLEX_INVALID_ARG,
        );
        assert_eq!(last_error(), format!("invalid argument: {message}"));
    }
}

#[test]
fn empty_null_list_and_legacy_open_succeed() {
    let cfg = config(FlexSourceKind::Mic);
    // SAFETY: A zero length permits a NULL list; cfg is valid for both entry points.
    let extended = Handle::new(unsafe { flexaudio_open_with_exclude_pids(&cfg, ptr::null(), 0) });
    assert!(extended.pids().is_empty());
    assert!(flexaudio_last_error().is_null());
    let legacy = Handle::new(unsafe { flexaudio_open(&cfg) });
    assert!(legacy.pids().is_empty());
    assert!(flexaudio_last_error().is_null());

    let handle = mock_handle();
    let extended_result =
        unsafe { flexaudio_switch_source_with_exclude_pids(handle.0, &cfg, ptr::null(), 0) };
    assert_eq!(extended_result, code::FLEX_FAILURE);
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
    let cfg = config(FlexSourceKind::Mic);
    let backing = [123_u32; 2];
    let unaligned = backing.as_ptr().cast::<u8>().wrapping_add(1).cast::<u32>();
    // SAFETY: A zero-length list must not be dereferenced, even if its pointer is unaligned.
    let handle = Handle::new(unsafe { flexaudio_open_with_exclude_pids(&cfg, unaligned, 0) });
    assert!(handle.pids().is_empty());
    assert_eq!(
        unsafe { flexaudio_switch_source_with_exclude_pids(handle.0, &cfg, unaligned, 0) },
        code::FLEX_FAILURE,
    );
    assert!(last_error().contains("switch_source is only available on a started stream"));
}

#[test]
fn duplicates_order_and_maximum_pid_are_preserved_in_owned_storage() {
    let cfg = config(FlexSourceKind::Mic);
    let mut pids = vec![123, u32::MAX, 123, 42];
    let original = pids.clone();
    // SAFETY: All entries are initialized and readable throughout the call.
    let handle =
        Handle::new(unsafe { flexaudio_open_with_exclude_pids(&cfg, pids.as_ptr(), pids.len()) });
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
        code::FLEX_FAILURE,
    );
    assert!(last_error().contains("switch_source is only available on a started stream"));
}

#[test]
fn maximum_length_is_accepted() {
    let cfg = config(FlexSourceKind::Mic);
    let pids = vec![123; 4096];
    // SAFETY: The whole PID allocation is initialized, readable, and within the cap.
    let handle =
        Handle::new(unsafe { flexaudio_open_with_exclude_pids(&cfg, pids.as_ptr(), pids.len()) });
    assert_eq!(handle.pids(), pids);
}
