//! Opaque ABI behavior checks using only library converters and mock capture.
use crate::error::{self, code};
use crate::types::{FlexChunk, FlexStream};
use crate::v2::*;
use crate::v2_records::*;
use crate::v2_storage::*;
use flexaudio as fa;
use std::collections::VecDeque;
use std::ffi::CStr;
use std::mem::MaybeUninit;
use std::num::NonZeroU64;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

unsafe fn kind(error: *const FlexErrorInfoV2) -> i32 {
    let mut result = 123;
    assert_eq!(unsafe { flexaudio_error_kind_v2(error, &mut result) }, 1);
    result
}

unsafe fn message(error: *const FlexErrorInfoV2) -> String {
    let pointer = unsafe { flexaudio_error_message_v2(error) };
    assert!(!pointer.is_null());
    unsafe { CStr::from_ptr(pointer) }.to_str().unwrap().into()
}

fn context(error: *const FlexErrorInfoV2, index: usize) -> FlexErrorContextV2 {
    let mut output = MaybeUninit::uninit();
    assert_eq!(
        unsafe { flexaudio_error_context_v2(error, index, output.as_mut_ptr()) },
        1
    );
    unsafe { output.assume_init() }
}

#[test]
fn v2_error_root_codes_survive_context_and_multiple() {
    let roots = [
        (fa::Error::InvalidArg("validation".into()), -1),
        (fa::Error::InvalidState("lifecycle".into()), -4),
        (fa::Error::DeviceNotFound, -5),
        (fa::Error::DeviceLost, -6),
        (
            fa::Error::PermissionDenied {
                permission: fa::Permission::Microphone,
                detail: "private fixture".into(),
            },
            -7,
        ),
        (fa::Error::UnsupportedOsVersion, -8),
        (fa::Error::Unsupported, -9),
        (fa::Error::UnsupportedFormat("validation".into()), -10),
        (
            fa::Error::NativeFormatChanged {
                advertised: (48_000, 2),
                actual: (44_100, 1),
            },
            -11,
        ),
        (fa::Error::AmbiguousDeviceName, -12),
        (fa::Error::Backend("safe backend failure".into()), -2),
    ];
    for (root, expected) in roots {
        let wrapped = fa::Error::Multiple(fa::ErrorGroup::new(
            root.with_context(fa::ErrorContext::new(fa::Operation::Start)),
            fa::Error::DeviceLost,
            Vec::new(),
        ))
        .with_context(fa::ErrorContext::new(fa::Operation::Rollback));
        assert_eq!(crate::fail(wrapped), expected);
        let owner = flexaudio_last_error_info_v2();
        assert!(!owner.is_null());
        unsafe {
            assert_eq!(kind(owner), expected);
            assert_eq!(context(owner, 0).operation, 5);
            assert_eq!(context(owner, 1).operation, 1);
            assert_eq!(kind(flexaudio_error_secondary_v2(owner, 0)), -6);
            flexaudio_error_info_free_v2(owner);
        }
    }
}

#[test]
fn v2_nested_error_contexts_and_related_trees_are_retained() {
    let root = fa::Error::NativeFormatChanged {
        advertised: (48_000, 2),
        actual: (44_100, 1),
    }
    .with_context(
        fa::ErrorContext::new(fa::Operation::Normalize)
            .with_lane(fa::MixLane::Microphone)
            .with_native_status(fa::NativeStatus::HResult {
                call: "FixtureNativeCall",
                bits: 0x8007_0005,
            }),
    );
    let related = fa::Error::Multiple(fa::ErrorGroup::new(
        fa::Error::Backend("safe cleanup failure".into()).with_context(
            fa::ErrorContext::new(fa::Operation::Stop).with_native_status(
                fa::NativeStatus::OsStatus {
                    call: "FixtureStopCall",
                    value: -12_345,
                },
            ),
        ),
        fa::Error::Unsupported,
        vec![fa::Error::DeviceNotFound],
    ))
    .with_context(fa::ErrorContext::new(fa::Operation::Join).with_lane(fa::MixLane::SystemAudio));
    let tree = fa::Error::Multiple(fa::ErrorGroup::new(
        root,
        related,
        vec![fa::Error::DeviceLost],
    ))
    .with_context(fa::ErrorContext::new(fa::Operation::Rollback));
    let owner = Box::into_raw(Box::new(FlexErrorInfoV2::new(tree)));
    unsafe {
        assert_eq!(kind(owner), -11);
        let outer = context(owner, 0);
        assert_eq!(
            (
                outer.operation,
                outer.lane,
                outer.native_code_kind,
                outer.native_code
            ),
            (5, 0, 0, 0)
        );
        assert!(outer.native_call.is_null());
        let native = context(owner, 1);
        assert_eq!(
            (
                native.operation,
                native.lane,
                native.native_code_kind,
                native.native_code
            ),
            (2, 1, 1, 0x8007_0005)
        );
        assert_eq!(
            CStr::from_ptr(native.native_call).to_str().unwrap(),
            "FixtureNativeCall"
        );
        assert!(!message(owner).contains("FixtureNativeCall"));
        let mut size = 99;
        assert_eq!(flexaudio_error_secondary_count_v2(owner, &mut size), 1);
        assert_eq!(size, 2);
        let child = flexaudio_error_secondary_v2(owner, 0);
        assert_eq!(kind(child), -2);
        assert_eq!(
            (context(child, 0).operation, context(child, 0).lane),
            (7, 2)
        );
        let native = context(child, 1);
        assert_eq!(
            (
                native.operation,
                native.native_code_kind,
                native.native_code
            ),
            (6, 2, -12_345)
        );
        assert_eq!(
            CStr::from_ptr(native.native_call).to_str().unwrap(),
            "FixtureStopCall"
        );
        assert_eq!(flexaudio_error_secondary_count_v2(child, &mut size), 1);
        assert_eq!(size, 2);
        assert_eq!(kind(flexaudio_error_secondary_v2(child, 0)), -9);
        assert_eq!(kind(flexaudio_error_secondary_v2(child, 1)), -5);
        assert_eq!(kind(flexaudio_error_secondary_v2(owner, 1)), -6);
        flexaudio_error_info_free_v2(owner);
    }
}

#[test]
fn v2_permission_and_native_format_payloads_are_conditional() {
    for (permission, expected) in [
        (fa::Permission::Microphone, 1),
        (fa::Permission::SystemAudio, 2),
    ] {
        let owner = FlexErrorInfoV2::new(
            fa::Error::PermissionDenied {
                permission,
                detail: "private permission detail".into(),
            }
            .with_context(fa::ErrorContext::new(fa::Operation::Start)),
        );
        let mut result = 99;
        let mut format = FlexNativeFormatChangeV2::default();
        format.actual.sample_rate = 123;
        unsafe {
            assert_eq!(flexaudio_error_permission_v2(&owner, &mut result), 1);
            assert_eq!(result, expected);
            assert_eq!(flexaudio_error_native_format_v2(&owner, &mut format), 0);
            assert_eq!(format.actual.sample_rate, 123);
            assert!(!message(&owner).contains("private permission detail"));
        }
    }
    let owner = FlexErrorInfoV2::new(fa::Error::NativeFormatChanged {
        advertised: (96_000, 2),
        actual: (48_000, 1),
    });
    let mut format = FlexNativeFormatChangeV2::default();
    let mut permission = 99;
    unsafe {
        assert_eq!(flexaudio_error_native_format_v2(&owner, &mut format), 1);
        assert_eq!(
            (format.advertised.sample_rate, format.advertised.channels),
            (96_000, 2)
        );
        assert_eq!(
            (format.actual.sample_rate, format.actual.channels),
            (48_000, 1)
        );
        assert_eq!(flexaudio_error_permission_v2(&owner, &mut permission), 0);
        assert_eq!(permission, 99);
    }
}

#[test]
fn v2_nested_primary_groups_keep_related_error_observation_order() {
    let primary = fa::Error::Multiple(fa::ErrorGroup::new(
        fa::Error::DeviceNotFound.with_context(fa::ErrorContext::new(fa::Operation::Start)),
        fa::Error::Unsupported,
        vec![fa::Error::UnsupportedFormat("format".into())],
    ));
    let tree = fa::Error::Multiple(fa::ErrorGroup::new(
        primary,
        fa::Error::DeviceLost,
        Vec::new(),
    ));
    let owner = FlexErrorInfoV2::new(tree);
    unsafe {
        assert_eq!(kind(&owner), -5);
        assert_eq!(context(&owner, 0).operation, 1);
        let mut count = 0;
        assert_eq!(flexaudio_error_secondary_count_v2(&owner, &mut count), 1);
        assert_eq!(count, 3);
        for (index, expected) in [-9, -10, -6].into_iter().enumerate() {
            assert_eq!(kind(flexaudio_error_secondary_v2(&owner, index)), expected);
        }
    }
}

#[test]
fn v2_advisory_messages_do_not_project_backend_detail() {
    for event in [
        fa::Event::PermissionPending {
            permission: fa::Permission::Microphone,
            detail: "private advisory fixture".into(),
        },
        fa::Event::SilenceWhileSourceActive {
            detail: "private advisory fixture".into(),
        },
        fa::Event::Error("private legacy fixture".into()),
    ] {
        let owner = FlexEventV2::new(event);
        let pointer = unsafe { flexaudio_event_message_v2(&owner) };
        assert!(!pointer.is_null());
        let message = unsafe { CStr::from_ptr(pointer) }.to_str().unwrap();
        assert!(!message.contains("private"));
    }
}

#[test]
fn v2_new_stream_event_tags_and_typed_error_payloads() {
    let events = [
        (fa::Event::PermissionGranted, 9, None),
        (
            fa::Event::AudioLoss {
                loss: fa::AudioLoss::raw_overflow(None, None, 48_000, 2).unwrap(),
            },
            10,
            None,
        ),
        (
            fa::Event::ShutdownError {
                error: fa::Error::Backend("cleanup".into()),
            },
            11,
            Some(-2),
        ),
        (
            fa::Event::TerminalError {
                error: fa::Error::DeviceNotFound,
            },
            12,
            Some(-5),
        ),
        (
            fa::Event::RecoverableError {
                error: fa::Error::DeviceLost,
            },
            13,
            Some(-6),
        ),
        (fa::Event::Clipped, 14, None),
    ];
    for (event, expected, error_kind) in events {
        let owner = Box::into_raw(Box::new(FlexEventV2::new(event.clone())));
        let v1 = crate::convert::event_to_c(event);
        assert_eq!(v1.kind, if error_kind.is_some() { 5 } else { 6 });
        unsafe {
            let mut result = -99;
            assert_eq!(flexaudio_event_kind_v2(owner, &mut result), 1);
            assert_eq!(result, expected);
            let borrowed = flexaudio_event_error_v2(owner);
            if let Some(expected) = error_kind {
                assert_eq!(kind(borrowed), expected);
            } else {
                assert!(borrowed.is_null());
            }
            let mut permission = 99;
            assert_eq!(
                flexaudio_event_permission_v2(owner, &mut permission),
                i32::from(expected == 9)
            );
            assert_eq!(permission, if expected == 9 { 1 } else { 99 });
            flexaudio_event_free_v2(owner);
        }
    }
}

#[test]
fn v2_loss_records_preserve_exact_u64_unknown_and_path_fields() {
    for count in [
        None,
        NonZeroU64::new((1 << 53) + 1),
        NonZeroU64::new((1 << 63) + 1),
        NonZeroU64::new(u64::MAX),
    ] {
        let losses = [
            (
                fa::AudioLoss::raw_overflow(None, count, 44_100, 1).unwrap(),
                (0, 0, 0, 0, 44_100, 1),
            ),
            (
                fa::AudioLoss::raw_overflow(Some(fa::MixLane::SystemAudio), count, 96_000, 2)
                    .unwrap(),
                (0, 2, 0, 0, 96_000, 2),
            ),
            (
                fa::AudioLoss::mix_fifo_overflow(fa::MixLane::Microphone, count),
                (1, 1, 0, 1, 48_000, 2),
            ),
            (
                fa::AudioLoss::output_overflow(fa::OutputTap::Primary, count, 48_000, 2).unwrap(),
                (2, 0, 0, 5, 48_000, 2),
            ),
            (
                fa::AudioLoss::output_overflow(fa::OutputTap::Secondary, count, 16_000, 1).unwrap(),
                (2, 0, 1, 5, 16_000, 1),
            ),
        ];
        for (loss, expected) in losses {
            let owner = FlexEventV2::new(fa::Event::AudioLoss { loss });
            let mut output = MaybeUninit::uninit();
            assert_eq!(
                unsafe { flexaudio_event_loss_v2(&owner, output.as_mut_ptr()) },
                1
            );
            let output = unsafe { output.assume_init() };
            assert_eq!(
                (
                    output.path,
                    output.lane,
                    output.tap,
                    output.reason,
                    output.sample_rate,
                    output.channels
                ),
                expected
            );
            assert_eq!(output.count_known, u32::from(count.is_some()));
            assert_eq!(output.samples, count.map_or(0, NonZeroU64::get));
        }
    }
    for count in [(1 << 53) + 1, (1 << 63) + 1, u64::MAX] {
        let owner = FlexEventV2::new(fa::Event::ChunkDropped { count });
        let mut result = 0;
        assert_eq!(unsafe { flexaudio_event_count_v2(&owner, &mut result) }, 1);
        assert_eq!(result, count);
    }
    for count in [None, NonZeroU64::new((1 << 63) + 1)] {
        let diagnostics = fa::core::CaptureDiagnostics::new(48_000, 2);
        diagnostics.record_corrupt_buffer(count);
        diagnostics.record_malformed_buffer(count);
        diagnostics.record_callback_rejected(count);
        for (loss, expected_reason) in diagnostics.drain().unwrap().into_iter().zip(2..=4) {
            let owner = FlexEventV2::new(fa::Event::AudioLoss { loss });
            let mut output = MaybeUninit::uninit();
            assert_eq!(
                unsafe { flexaudio_event_loss_v2(&owner, output.as_mut_ptr()) },
                1
            );
            let output = unsafe { output.assume_init() };
            assert_eq!(
                (output.path, output.lane, output.tap, output.reason),
                (0, 0, 0, expected_reason)
            );
            assert_eq!(output.count_known, u32::from(count.is_some()));
            assert_eq!(output.samples, count.map_or(0, NonZeroU64::get));
        }
        assert!(diagnostics.drain().unwrap().is_empty());
    }
}

#[test]
fn v2_device_default_clear_and_rescan_have_no_fabricated_payloads() {
    for (kind, expected) in [
        (fa::DefaultDeviceKind::Microphone, 0),
        (fa::DefaultDeviceKind::SystemAudio, 1),
    ] {
        let owner = FlexDeviceEventV2::new(fa::DeviceEvent::DefaultCleared { kind });
        let mut result = -99;
        let mut count = 123;
        unsafe {
            assert_eq!(flexaudio_device_event_kind_v2(&owner, &mut result), 1);
            assert_eq!(result, 4);
            assert_eq!(
                flexaudio_device_event_default_kind_v2(&owner, &mut result),
                1
            );
            assert_eq!(result, expected);
            assert_eq!(
                flexaudio_device_event_dropped_events_v2(&owner, &mut count),
                0
            );
            assert_eq!(count, 123);
            assert!(flexaudio_device_event_id_v2(&owner).is_null());
            assert!(flexaudio_device_event_device_v2(&owner).is_null());
        }
    }
    for count in [0, (1 << 53) + 1, (1 << 63) + 1, u64::MAX] {
        let owner = FlexDeviceEventV2::new(fa::DeviceEvent::RescanRequired {
            dropped_events: count,
        });
        let mut result = -99;
        let mut delivered = 123;
        unsafe {
            assert_eq!(flexaudio_device_event_kind_v2(&owner, &mut result), 1);
            assert_eq!(result, 5);
            assert_eq!(
                flexaudio_device_event_dropped_events_v2(&owner, &mut delivered),
                1
            );
            assert_eq!(delivered, count);
            assert_eq!(
                flexaudio_device_event_default_kind_v2(&owner, &mut result),
                0
            );
            assert_eq!(result, 5);
            assert!(flexaudio_device_event_id_v2(&owner).is_null());
        }
    }
}

#[test]
fn v2_borrowed_device_and_error_views_survive_tls_changes() {
    let event = Box::into_raw(Box::new(FlexEventV2::new(fa::Event::TerminalError {
        error: fa::Error::Multiple(fa::ErrorGroup::new(
            fa::Error::DeviceLost,
            fa::Error::Unsupported,
            Vec::new(),
        )),
    })));
    let device = Box::into_raw(Box::new(FlexDeviceEventV2::new(fa::DeviceEvent::Added(
        fa::DeviceInfo {
            id: "fixture-device".into(),
            name: "Fixture device".into(),
            source_kind: fa::SourceKind::Mic,
            sample_rate: 48_000,
            channels: 1,
            is_loopback: false,
            is_default: true,
        },
    ))));
    error::set_audio_error(fa::Error::DeviceNotFound);
    let snapshot = flexaudio_last_error_info_v2();
    unsafe {
        let root = flexaudio_event_error_v2(event);
        let child = flexaudio_error_secondary_v2(root, 0);
        let text = flexaudio_error_message_v2(child);
        let view = flexaudio_device_event_device_v2(device);
        let id = flexaudio_device_event_id_v2(device);
        assert!(!view.is_null());
        for _ in 0..3 {
            error::set_audio_error(fa::Error::Backend("replacement TLS error".into()));
            error::clear_last_error();
            assert_eq!(kind(snapshot), -5);
            assert_eq!(kind(root), -6);
            assert_eq!(kind(child), -9);
            assert_eq!(CStr::from_ptr(text).to_str().unwrap(), "unsupported");
            assert_eq!(CStr::from_ptr(id).to_str().unwrap(), "fixture-device");
            assert_eq!(
                CStr::from_ptr((*view).name).to_str().unwrap(),
                "Fixture device"
            );
            assert_eq!(((*view).sample_rate, (*view).channels), (48_000, 1));
        }
        flexaudio_error_info_free_v2(snapshot);
        flexaudio_event_free_v2(event);
        flexaudio_device_event_free_v2(device);
    }
}

#[test]
fn v2_getters_reject_null_alignment_and_indices_before_dereference() {
    let owner = FlexErrorInfoV2::new(
        fa::Error::DeviceLost.with_context(fa::ErrorContext::new(fa::Operation::Stop)),
    );
    let event = FlexEventV2::new(fa::Event::Clipped);
    let device = FlexDeviceEventV2::new(fa::DeviceEvent::RescanRequired { dropped_events: 1 });
    let report =
        FlexShutdownReportV2::new(fa::ShutdownReport::new(None, vec![fa::Error::Unsupported]));
    let mut result = 123;
    let mut bytes = [0_u64; 8];
    let bad_out = unsafe { bytes.as_mut_ptr().cast::<u8>().add(1).cast::<i32>() };
    let bad_owner = unsafe {
        ptr::from_ref(&owner)
            .cast::<u8>()
            .add(1)
            .cast::<FlexErrorInfoV2>()
    };
    let mut record = MaybeUninit::<FlexErrorContextV2>::uninit();
    unsafe {
        assert_eq!(
            flexaudio_error_kind_v2(ptr::null(), &mut result),
            code::FLEX_INVALID_ARG
        );
        assert_eq!(result, 123);
        assert_eq!(
            flexaudio_error_kind_v2(bad_owner, &mut result),
            code::FLEX_INVALID_ARG
        );
        assert_eq!(
            flexaudio_error_kind_v2(&owner, ptr::null_mut()),
            code::FLEX_INVALID_ARG
        );
        assert_eq!(
            flexaudio_error_kind_v2(&owner, bad_out),
            code::FLEX_INVALID_ARG
        );
        assert_eq!(
            flexaudio_error_context_v2(&owner, 1, record.as_mut_ptr()),
            code::FLEX_INVALID_ARG
        );
        assert_eq!(
            flexaudio_error_context_v2(&owner, usize::MAX, record.as_mut_ptr()),
            code::FLEX_INVALID_ARG
        );
        assert!(flexaudio_error_secondary_v2(&owner, 0).is_null());
        assert!(!crate::flexaudio_last_error().is_null());
        assert!(flexaudio_error_message_v2(bad_owner).is_null());
        assert!(!crate::flexaudio_last_error().is_null());
        assert_eq!(flexaudio_event_kind_v2(&event, bad_out), -1);
        assert_eq!(flexaudio_device_event_kind_v2(&device, bad_out), -1);
        assert!(flexaudio_shutdown_cleanup_v2(&report, 1).is_null());
        assert!(!crate::flexaudio_last_error().is_null());
        assert!(flexaudio_shutdown_primary_v2(ptr::null()).is_null());
        flexaudio_event_free_v2(ptr::null_mut());
        flexaudio_device_event_free_v2(ptr::null_mut());
        flexaudio_error_info_free_v2(ptr::null_mut());
        flexaudio_shutdown_report_free_v2(ptr::null_mut());
    }
    assert_eq!(bytes, [0; 8]);
}

#[derive(Default)]
struct BackendState {
    sink: Option<fa::core::backend::RawSink>,
    events: VecDeque<fa::Event>,
}

struct Backend {
    state: Arc<Mutex<BackendState>>,
    stops: Arc<AtomicUsize>,
    cleanup: Option<fa::Error>,
}
impl fa::CaptureBackend for Backend {
    fn native_format(&self) -> (u32, u16) {
        (48_000, 1)
    }
    fn start(&mut self, sink: fa::core::backend::RawSink) -> fa::Result<()> {
        self.state.lock().unwrap().sink = Some(sink);
        Ok(())
    }
    fn stop(&mut self) {
        self.stops.fetch_add(1, Ordering::SeqCst);
        self.state.lock().unwrap().sink.take();
    }
    fn stop_checked(&mut self) -> fa::Result<()> {
        self.stop();
        self.cleanup.clone().map_or(Ok(()), Err)
    }
    fn poll_event(&mut self) -> Option<fa::Event> {
        self.state.lock().unwrap().events.pop_front()
    }
}

fn stream(
    cleanup: Option<fa::Error>,
    denoise: bool,
) -> (FlexStream, Arc<Mutex<BackendState>>, Arc<AtomicUsize>) {
    let state = Arc::new(Mutex::new(BackendState::default()));
    let stops = Arc::new(AtomicUsize::new(0));
    let config = fa::StreamConfig {
        output: fa::OutputFormat {
            sample_rate: 48_000,
            channels: 1,
        },
        ..Default::default()
    };
    let inner = fa::Stream::open(
        config,
        Box::new(Backend {
            state: state.clone(),
            stops: stops.clone(),
            cleanup,
        }),
    )
    .unwrap();
    let mut stream = FlexStream {
        inner,
        shutdown: None,
        shutdown_event_index: 0,
        last_output: None,
        whisper: None,
        whisper_events: Vec::new(),
        whisper_origin: (0, 0),
        whisper_error: None,
        whisper_error_reported: false,
        ready_chunks: VecDeque::new(),
        denoiser: denoise.then(|| flexaudio_denoise::Denoiser::new(1).unwrap()),
        vad: None,
    };
    assert_eq!(unsafe { crate::flexaudio_start(&mut stream) }, 0);
    (stream, state, stops)
}

fn push_and_poll(stream: &mut FlexStream, state: &Arc<Mutex<BackendState>>) -> FlexChunk {
    assert_eq!(
        state
            .lock()
            .unwrap()
            .sink
            .as_mut()
            .unwrap()
            .push(&[0.5; 960], 0),
        960
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let mut chunk = MaybeUninit::uninit();
        match unsafe { crate::flexaudio_poll_chunk(stream, chunk.as_mut_ptr()) } {
            1 => return unsafe { chunk.assume_init() },
            0 => {
                assert!(Instant::now() < deadline, "mock chunk timed out");
                std::thread::sleep(Duration::from_millis(1));
            }
            result => panic!("unexpected mock poll result {result}"),
        }
    }
}

fn release(mut chunk: FlexChunk) {
    unsafe { crate::flexaudio_chunk_free(&mut chunk) }
}

#[test]
fn v2_poll_invalid_output_keeps_event_and_initializes_valid_output() {
    let (mut stream, state, _) = stream(None, false);
    state
        .lock()
        .unwrap()
        .events
        .push_back(fa::Event::PermissionGranted);
    assert_eq!(unsafe { crate::flexaudio_stop(&mut stream) }, 0);
    let mut bytes = [0_u64; 2];
    let bad_out = unsafe {
        bytes
            .as_mut_ptr()
            .cast::<u8>()
            .add(1)
            .cast::<*mut FlexEventV2>()
    };
    unsafe {
        assert_eq!(flexaudio_poll_event_v2(&mut stream, ptr::null_mut()), -1);
        assert_eq!(flexaudio_poll_event_v2(&mut stream, bad_out), -1);
        let mut event = ptr::dangling_mut();
        assert_eq!(flexaudio_poll_event_v2(&mut stream, &mut event), 1);
        let mut tag = -99;
        assert_eq!(flexaudio_event_kind_v2(event, &mut tag), 1);
        assert_eq!(tag, 9);
        flexaudio_event_free_v2(event);
        event = ptr::dangling_mut();
        assert_eq!(flexaudio_poll_event_v2(&mut stream, &mut event), 0);
        assert!(event.is_null());
        event = ptr::dangling_mut();
        assert_eq!(flexaudio_poll_event_v2(ptr::null_mut(), &mut event), -1);
        assert!(event.is_null());
        let mut device = ptr::dangling_mut();
        assert_eq!(flexaudio_watcher_poll_v2(ptr::null_mut(), &mut device), -1);
        assert!(device.is_null());
    }
    assert_eq!(bytes, [0; 2]);
}

#[test]
fn v2_checked_stop_retains_cleanup_without_capture_primary_and_emits_once() {
    let cleanup = fa::Error::Unsupported.with_context(fa::ErrorContext::new(fa::Operation::Stop));
    let (mut stream, _, stops) = stream(Some(cleanup), false);
    // A binding-local failure is added after the independently checked core teardown.
    stream.whisper_error = Some(flexaudio_vad::WhisperVadTapError::Conversion);
    unsafe {
        assert!(flexaudio_shutdown_report_v2(&stream).is_null());
        assert_eq!(crate::flexaudio_stop(&mut stream), -9);
        let calls = stops.load(Ordering::SeqCst);
        assert_eq!(crate::flexaudio_stop(&mut stream), -9);
        assert_eq!(stops.load(Ordering::SeqCst), calls);
        assert!(flexaudio_terminal_error_info_v2(&stream).is_null());
        let report = flexaudio_shutdown_report_v2(&stream);
        assert!(!report.is_null());
        assert!(flexaudio_shutdown_primary_v2(report).is_null());
        let mut count = 99;
        assert_eq!(flexaudio_shutdown_cleanup_count_v2(report, &mut count), 1);
        assert_eq!(count, 2);
        assert_eq!(kind(flexaudio_shutdown_cleanup_v2(report, 0)), -9);
        let binding = flexaudio_shutdown_cleanup_v2(report, 1);
        assert_eq!(kind(binding), -2);
        assert_eq!(context(binding, 0).operation, 3);
        let mut event = ptr::null_mut();
        let mut errors = Vec::new();
        while flexaudio_poll_event_v2(&mut stream, &mut event) == 1 {
            let mut tag = -99;
            assert_eq!(flexaudio_event_kind_v2(event, &mut tag), 1);
            if tag == 11 {
                errors.push(kind(flexaudio_event_error_v2(event)));
            }
            flexaudio_event_free_v2(event);
        }
        assert_eq!(errors, [-9, -2]);
        assert_eq!(crate::flexaudio_stop(&mut stream), -9);
        assert_eq!(flexaudio_poll_event_v2(&mut stream, &mut event), 0);
        drop(stream);
        assert_eq!(kind(flexaudio_shutdown_cleanup_v2(report, 1)), -2);
        flexaudio_shutdown_report_free_v2(report);
    }
}

#[test]
fn v2_capture_primary_suppresses_denoise_tail_and_preserves_cleanup() {
    let (mut stream, state, _) = stream(Some(fa::Error::Unsupported), true);
    release(push_and_poll(&mut stream, &state));
    state
        .lock()
        .unwrap()
        .events
        .push_back(fa::Event::TerminalError {
            error: fa::Error::DeviceNotFound,
        });
    let deadline = Instant::now() + Duration::from_secs(2);
    while stream.inner.terminal_error().is_none() {
        assert!(Instant::now() < deadline, "mock terminal failure timed out");
        std::thread::sleep(Duration::from_millis(1));
    }
    unsafe {
        assert_eq!(crate::flexaudio_stop(&mut stream), -5);
        assert_eq!(crate::flexaudio_stop(&mut stream), -5);
        assert!(stream.ready_chunks.is_empty());
        let mut chunk = MaybeUninit::<FlexChunk>::uninit();
        assert_eq!(
            crate::flexaudio_poll_chunk(&mut stream, chunk.as_mut_ptr()),
            -2
        );
        let capture = flexaudio_terminal_error_info_v2(&stream);
        let report = flexaudio_shutdown_report_v2(&stream);
        assert_eq!(kind(capture), -5);
        assert_eq!(kind(flexaudio_shutdown_primary_v2(report)), -5);
        let mut count = 99;
        assert_eq!(flexaudio_shutdown_cleanup_count_v2(report, &mut count), 1);
        assert_eq!(count, 1);
        assert_eq!(kind(flexaudio_shutdown_cleanup_v2(report, 0)), -9);
        drop(stream);
        assert_eq!(kind(capture), -5);
        assert_eq!(kind(flexaudio_shutdown_primary_v2(report)), -5);
        flexaudio_error_info_free_v2(capture);
        flexaudio_shutdown_report_free_v2(report);
    }
}

#[test]
fn v2_repeated_stop_delivers_one_denoise_tail_and_spent_stream_keeps_addon() {
    let (mut stream, state, _) = stream(None, true);
    release(push_and_poll(&mut stream, &state));
    assert_eq!(unsafe { crate::flexaudio_stop(&mut stream) }, 0);
    assert_eq!(unsafe { crate::flexaudio_stop(&mut stream) }, 0);
    let mut output = MaybeUninit::<FlexChunk>::uninit();
    assert_eq!(
        unsafe { crate::flexaudio_poll_chunk(&mut stream, output.as_mut_ptr()) },
        1
    );
    let tail = unsafe { output.assume_init() };
    assert_eq!(tail.len, 480);
    let data = unsafe { std::slice::from_raw_parts(tail.data, tail.len) };
    let (peak, rms) = crate::integration::peak_rms(data);
    assert_eq!((tail.peak, tail.rms), (peak, rms));
    release(tail);
    assert_eq!(unsafe { crate::flexaudio_stop(&mut stream) }, 0);
    let mut empty = MaybeUninit::<FlexChunk>::uninit();
    assert_eq!(
        unsafe { crate::flexaudio_poll_chunk(&mut stream, empty.as_mut_ptr()) },
        0
    );
    assert_eq!(
        unsafe { crate::flexaudio_start(&mut stream) },
        code::FLEX_INVALID_STATE
    );
    let last_error = crate::flexaudio_last_error();
    assert!(!last_error.is_null());
    let message = unsafe { CStr::from_ptr(last_error) }.to_str().unwrap();
    assert!(
        message.contains("already stopped") || message.contains("spent"),
        "restart must explain the spent stream: {message}"
    );
    assert!(stream.denoiser.is_some());
    assert_eq!(unsafe { crate::flexaudio_stop(&mut stream) }, 0);
    assert_eq!(
        unsafe { crate::flexaudio_poll_chunk(&mut stream, empty.as_mut_ptr()) },
        0
    );
}

#[test]
fn v2_free_releases_ordinary_stream_with_an_unread_denoise_tail() {
    let (mut stream, state, _) = stream(None, true);
    release(push_and_poll(&mut stream, &state));
    assert_eq!(Arc::strong_count(&state), 2);
    let owner = Box::into_raw(Box::new(stream));
    // Free performs the same stop, then owns disposal of unread ordinary output.
    unsafe { crate::flexaudio_free(owner) };
    assert_eq!(Arc::strong_count(&state), 1);
    assert!(state.lock().unwrap().sink.is_none());
}
