//! Pointer and flush regressions for the versioned C attachment.
use super::*;
use flexaudio_vad::{
    AttachedWhisperVadEvent, EpochEndReason, WhisperVadEvent, WhisperVadEventKind,
    WhisperVadTapError, WhisperVadTapFailure,
};

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
        whisper_error_reported: false,
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
    // Intake failure is reported even if flushing successfully reinitializes the tap.
    stream.whisper_error = Some(WhisperVadTapError::Conversion);
    assert_eq!(
        stream.flush_whisper_with(|_| Ok(Vec::new())),
        code::FLEX_FAILURE
    );
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
