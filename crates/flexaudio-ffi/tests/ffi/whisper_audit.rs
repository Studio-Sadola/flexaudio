//! Pointer and flush regressions for the versioned C attachment.
use super::*;
use flexaudio_vad::{
    AttachedWhisperVadEvent, EpochEndReason, WhisperVadEvent, WhisperVadEventKind,
    WhisperVadTapError, WhisperVadTapFailure,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct IdleBackend;
impl flexaudio::CaptureBackend for IdleBackend {
    fn native_format(&self) -> (u32, u16) {
        (48_000, 2)
    }
    fn start(&mut self, _: flexaudio::core::backend::RawSink) -> flexaudio::Result<()> {
        Ok(())
    }
    fn stop(&mut self) {}
}
fn fixture() -> FlexStream {
    FlexStream {
        inner: flexaudio::Stream::open(Default::default(), Box::new(IdleBackend)).unwrap(),
        shutdown: None,
        shutdown_event_index: 0,
        last_output: None,
        whisper: Some(
            flexaudio_vad::WhisperVadTap::new(Default::default(), Default::default()).unwrap(),
        ),
        whisper_events: Vec::new(),
        whisper_origin: (0, 0),
        whisper_error: None,
        whisper_cleanup: Vec::new(),
        ready_chunks: Default::default(),
        denoiser: None,
        vad: None,
    }
}
fn terminal() -> AttachedWhisperVadEvent {
    AttachedWhisperVadEvent::Vad(WhisperVadEvent {
        epoch: 0,
        seq: 1,
        kind: WhisperVadEventKind::EpochEnd {
            reason: EpochEndReason::Error,
        },
    })
}

#[test]
fn misaligned_chunk_and_length_pointers_are_rejected_before_access() {
    unsafe {
        let mut chunk: FlexChunkV2 = mem::zeroed();
        let mut length = 99usize;
        let bad_chunk = (&mut chunk as *mut FlexChunkV2)
            .cast::<u8>()
            .add(1)
            .cast::<FlexChunkV2>();
        let bad_length = (&mut length as *mut usize)
            .cast::<u8>()
            .add(1)
            .cast::<usize>();
        assert!(flexaudio_chunk_whisper_vad_events(bad_chunk, &mut length).is_null());
        assert_eq!(length, 0);
        assert_eq!(
            crate::error::last_audio_error().unwrap().kind(),
            flexaudio::ErrorKind::InvalidArg
        );
        length = 99;
        assert!(flexaudio_chunk_whisper_vad_events(&chunk, bad_length).is_null());
        assert_eq!(length, 99);
        assert!(flexaudio_chunk_whisper_vad_events(ptr::null(), bad_length).is_null());
        flexaudio_chunk_free_v2(bad_chunk);
        assert_eq!(
            crate::error::last_audio_error().unwrap().kind(),
            flexaudio::ErrorKind::InvalidArg
        );
        assert_eq!(chunk.whisper_vad_events_len, 0);
        assert!(flexaudio_chunk_whisper_vad_events(ptr::null(), &mut length).is_null());
        assert_eq!(length, 0);
        assert!(flexaudio_chunk_whisper_vad_events(&chunk, ptr::null_mut()).is_null());
        assert!(crate::flexaudio_last_error().is_null());
        flexaudio_chunk_free_v2(&mut chunk);
        flexaudio_chunk_free_v2(ptr::null_mut());
    }
}

#[test]
fn flush_failures_return_root_status_and_preserve_terminal_carriers() {
    let mut stream = fixture();
    for (error, expected) in [
        (WhisperVadTapError::Conversion, code::FLEX_FAILURE),
        (
            WhisperVadTapError::Vad(flexaudio_vad::WhisperVadError::Inference),
            code::FLEX_FAILURE,
        ),
        (
            WhisperVadTapError::UnsupportedConversionClock,
            FLEX_WHISPER_UNSUPPORTED_CONVERSION_CLOCK,
        ),
        (
            WhisperVadTapError::Vad(flexaudio_vad::WhisperVadError::SessionFinished),
            code::FLEX_INVALID_STATE,
        ),
    ] {
        clear_last_error();
        assert_eq!(
            stream.flush_whisper_with(|_| Err(WhisperVadTapFailure {
                error,
                terminal_events: vec![terminal()],
            })),
            expected
        );
        assert!(!crate::flexaudio_last_error().is_null());
        unsafe {
            let mut carrier = mem::zeroed();
            assert_eq!(flexaudio_poll_chunk_v2(&mut stream, &mut carrier), 1);
            assert_eq!(carrier.chunk.frames, 0);
            assert_eq!(carrier.whisper_vad_events_len, 1);
            assert_eq!((*carrier.whisper_vad_events).r#type, FLEX_WHISPER_EPOCH_END);
            flexaudio_chunk_free_v2(&mut carrier);
            assert_eq!(flexaudio_poll_chunk_v2(&mut stream, &mut carrier), 0);
            assert_eq!(stream.flush_whisper_with(|_| Ok(Vec::new())), code::FLEX_OK);
        }
    }
    assert_eq!(stream.stop_binding(), Ok(()));
    unsafe {
        let mut carrier = mem::zeroed();
        while flexaudio_poll_chunk_v2(&mut stream, &mut carrier) == 1 {
            flexaudio_chunk_free_v2(&mut carrier);
        }
    }
}

/// Pushes canonical capture chunks on demand, so a failure and its recovery boundary can be
/// driven without a device.
struct PushBackend(Arc<Mutex<Option<flexaudio::core::backend::RawSink>>>);
impl flexaudio::CaptureBackend for PushBackend {
    fn native_format(&self) -> (u32, u16) {
        (48_000, 2)
    }
    fn start(&mut self, sink: flexaudio::core::backend::RawSink) -> flexaudio::Result<()> {
        *self.0.lock().unwrap() = Some(sink);
        Ok(())
    }
    fn stop(&mut self) {
        self.0.lock().unwrap().take();
    }
}

fn capture_stream(sink: &Arc<Mutex<Option<flexaudio::core::backend::RawSink>>>) -> FlexStream {
    let mut inner =
        flexaudio::Stream::open(Default::default(), Box::new(PushBackend(sink.clone()))).unwrap();
    inner.enable_capture_tap().unwrap();
    inner.start().unwrap();
    FlexStream {
        inner,
        shutdown: None,
        shutdown_event_index: 0,
        last_output: None,
        whisper: Some(
            flexaudio_vad::WhisperVadTap::new(Default::default(), Default::default()).unwrap(),
        ),
        whisper_events: Vec::new(),
        whisper_origin: (0, 0),
        whisper_error: None,
        whisper_cleanup: Vec::new(),
        ready_chunks: Default::default(),
        denoiser: None,
        vad: None,
    }
}

unsafe fn poll_until(stream: &mut FlexStream, mut accept: impl FnMut(&FlexChunkV2) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            Instant::now() < deadline,
            "fake capture must deliver the expected chunk or events"
        );
        let mut chunk = mem::zeroed();
        match flexaudio_poll_chunk_v2(stream, &mut chunk) {
            1 => {
                let accepted = accept(&chunk);
                flexaudio_chunk_free_v2(&mut chunk);
                if accepted {
                    return;
                }
            }
            0 => std::thread::sleep(Duration::from_millis(2)),
            status => panic!("Whisper failure must not end polling: {status}"),
        }
    }
}

unsafe fn has_start(chunk: &FlexChunkV2) -> bool {
    !chunk.whisper_vad_events.is_null()
        && std::slice::from_raw_parts(chunk.whisper_vad_events, chunk.whisper_vad_events_len)
            .iter()
            .any(|event| event.r#type == FLEX_WHISPER_EPOCH_START)
}

unsafe fn fail_attachment(
    stream: &mut FlexStream,
    sink: &Arc<Mutex<Option<flexaudio::core::backend::RawSink>>>,
) {
    sink.lock().unwrap().as_mut().unwrap().push(&[0.2; 1920], 0);
    poll_until(stream, |chunk| has_start(chunk));
    stream.whisper.as_mut().unwrap().inject_failure();
    sink.lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .push(&[0.2; 1920], 20_000_000);
    poll_until(stream, |chunk| {
        !chunk.whisper_vad_events.is_null()
            && std::slice::from_raw_parts(chunk.whisper_vad_events, chunk.whisper_vad_events_len)
                .iter()
                .any(|event| {
                    event.r#type == FLEX_WHISPER_EPOCH_END
                        && event.data.vad.epoch_end.reason
                            == crate::whisper_types::FLEX_WHISPER_ERROR
                })
    });
    assert_eq!(
        stream
            .whisper_error
            .as_ref()
            .unwrap()
            .audio_error()
            .whisper_code(),
        Some(flexaudio::WhisperCode::Inference)
    );
    assert_eq!(
        stream.whisper_cleanup.len(),
        1,
        "one real processor failure must retain exactly one cleanup cause"
    );
    let mut chunk = mem::zeroed();
    assert_eq!(
        flexaudio_poll_chunk_v2(stream, &mut chunk),
        0,
        "an empty poll must not report the retained processor failure"
    );
    sink.lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .push(&[0.2; 1920], 40_000_000);
    poll_until(stream, |chunk| chunk.chunk.frames == 960);
    assert!(stream.whisper_error.is_some());
}

unsafe fn assert_whisper_shutdown(stream: &mut FlexStream) {
    assert_eq!(crate::flexaudio_stop(stream), code::FLEX_OK);
    assert_eq!(crate::flexaudio_stop(stream), code::FLEX_OK);
    let report = crate::v2::flexaudio_shutdown_report_v2(stream);
    assert!(!report.is_null());
    assert!(crate::v2::flexaudio_shutdown_primary_v2(report).is_null());
    let mut count = 0;
    assert_eq!(
        crate::v2::flexaudio_shutdown_cleanup_count_v2(report, &mut count),
        1
    );
    assert_eq!(
        count, 1,
        "a processor failure must survive recovery exactly once"
    );
    let error = crate::v2::flexaudio_shutdown_cleanup_v2(report, 0);
    let code = crate::v2::flexaudio_error_whisper_code_v2(error);
    assert!(!code.is_null());
    assert_eq!(
        std::ffi::CStr::from_ptr(code).to_str().unwrap(),
        "Inference"
    );
    crate::v2::flexaudio_shutdown_report_free_v2(report);
    let mut chunk = mem::zeroed();
    loop {
        match flexaudio_poll_chunk_v2(stream, &mut chunk) {
            1 => flexaudio_chunk_free_v2(&mut chunk),
            0 => break,
            status => {
                panic!("stopped capture must preserve polling without a processor error: {status}")
            }
        }
    }
}

#[test]
fn whisper_failure_preserves_c_polling_and_stop_with_typed_cleanup() {
    let sink = Arc::new(Mutex::new(None));
    let mut stream = capture_stream(&sink);
    unsafe {
        fail_attachment(&mut stream, &sink);
        assert_whisper_shutdown(&mut stream);
    }
}

#[test]
fn successful_flush_reinitializes_shared_owner_after_process_failure() {
    let sink = Arc::new(Mutex::new(None));
    let mut stream = capture_stream(&sink);
    unsafe {
        fail_attachment(&mut stream, &sink);
        assert_eq!(
            flexaudio_flush_whisper_vad(&mut stream),
            code::FLEX_OK,
            "a successful shared flush must resolve even after a process failure"
        );
        assert!(
            stream.whisper_error.is_none(),
            "successful shared flush must clear the active failure"
        );
        sink.lock()
            .expect("fake capture sink mutex must remain usable")
            .as_mut()
            .expect("fake capture backend must retain its sink until stop")
            .push(&[0.2; 1920], 60_000_000);
        poll_until(&mut stream, |chunk| has_start(chunk));
        assert_whisper_shutdown(&mut stream);
    }
}

#[test]
fn a_failed_attachment_reinitializes_at_the_next_discontinuity() {
    for switch in [false, true] {
        let sink = Arc::new(Mutex::new(None));
        let mut stream = capture_stream(&sink);
        unsafe {
            fail_attachment(&mut stream, &sink);
            if switch {
                stream
                    .inner
                    .switch_backend(Box::new(PushBackend(sink.clone())))
                    .unwrap();
            } else {
                stream.inner.pause();
                assert_eq!(crate::flexaudio_resume(&mut stream), code::FLEX_OK);
            }
            sink.lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .push(&[0.2; 1920], 60_000_000);
            poll_until(&mut stream, |chunk| has_start(chunk));
            assert!(
                stream.whisper_error.is_none(),
                "the shared owner must reinitialize at the discontinuity"
            );
            assert_whisper_shutdown(&mut stream);
        }
    }
}

#[test]
fn scalar_and_record_guards_catch_panics_without_exposing_payloads() {
    let value = crate::guard_value(0u64, || panic!("private panic detail"));
    assert_eq!(value, 0);
    let error = crate::error::last_audio_error().unwrap();
    assert_eq!(error.kind(), flexaudio::ErrorKind::Backend);
    assert!(!error.to_string().contains("private panic detail"));
    let params = crate::guard_value(
        crate::whisper_vad::flexaudio_whisper_vad_default_params(),
        || panic!("private panic detail"),
    );
    assert_eq!(params.threshold, 0.5);
    assert_eq!(params.min_speech_duration_ms, 250);
}

#[test]
fn whisper_code_borrow_preserves_context_and_rejects_invalid_owners() {
    let error = crate::v2_storage::FlexErrorInfoV2::new(
        flexaudio::Error::WhisperVad {
            code: flexaudio::WhisperCode::Inference,
        }
        .with_context(flexaudio::ErrorContext::new(flexaudio::Operation::Flush)),
    );
    let capture = crate::v2_storage::FlexErrorInfoV2::new(flexaudio::Error::DeviceLost);
    unsafe {
        let code = crate::v2::flexaudio_error_whisper_code_v2(&error);
        assert!(!code.is_null());
        assert_eq!(
            std::ffi::CStr::from_ptr(code).to_str().unwrap(),
            "Inference"
        );
        assert!(crate::v2::flexaudio_error_whisper_code_v2(&capture).is_null());
        assert!(crate::flexaudio_last_error().is_null());
        assert!(crate::v2::flexaudio_error_whisper_code_v2(ptr::null()).is_null());
        assert_eq!(
            crate::error::last_audio_error().unwrap().kind(),
            flexaudio::ErrorKind::InvalidArg
        );
        let misaligned = (&error as *const crate::v2_storage::FlexErrorInfoV2)
            .cast::<u8>()
            .add(1)
            .cast();
        assert!(crate::v2::flexaudio_error_whisper_code_v2(misaligned).is_null());
        assert_eq!(
            crate::error::last_audio_error().unwrap().kind(),
            flexaudio::ErrorKind::InvalidArg
        );
    }
}
