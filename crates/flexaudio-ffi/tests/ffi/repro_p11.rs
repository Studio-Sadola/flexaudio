//! Offline C ABI audit reproductions. No native audio devices are opened.
use crate::error::{clear_last_error, code};
use crate::types::{
    FlexChunk, FlexConfig, FlexProcessMode, FlexSourceKind, FlexStream, FlexVadConfig,
};
use flexaudio as fa;
use flexaudio_denoise::Denoiser;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct PushBackend(Arc<Mutex<Option<fa::core::backend::RawSink>>>);
impl fa::CaptureBackend for PushBackend {
    fn native_format(&self) -> (u32, u16) {
        (48_000, 1)
    }
    fn start(&mut self, sink: fa::core::backend::RawSink) -> fa::Result<()> {
        *self.0.lock().unwrap() = Some(sink);
        Ok(())
    }
    fn stop(&mut self) {
        self.0.lock().unwrap().take();
    }
}
fn stream(denoise: bool) -> (FlexStream, Arc<Mutex<Option<fa::core::backend::RawSink>>>) {
    let sink = Arc::new(Mutex::new(None));
    let mut config = fa::StreamConfig::default();
    config.output.channels = 1;
    let mut inner = fa::Stream::open(config, Box::new(PushBackend(sink.clone()))).unwrap();
    inner.start().unwrap();
    (
        FlexStream {
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
            denoiser: denoise.then(|| Denoiser::new(1).unwrap()),
            vad: None,
        },
        sink,
    )
}
fn push(sink: &Arc<Mutex<Option<fa::core::backend::RawSink>>>, data: &[f32]) {
    assert_eq!(
        sink.lock().unwrap().as_mut().unwrap().push(data, 0),
        data.len()
    );
}
fn chunk(stream: &mut FlexStream) -> FlexChunk {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(chunk) = stream.poll_processed().unwrap() {
            return chunk;
        }
        assert!(Instant::now() < deadline, "mock chunk timeout");
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn samples(chunk: &FlexChunk) -> &[f32] {
    // SAFETY: poll_processed returned an owned live allocation with exactly len samples.
    unsafe { std::slice::from_raw_parts(chunk.data, chunk.len) }
}
fn free(mut chunk: FlexChunk) {
    // SAFETY: this chunk is live and released exactly once.
    unsafe { crate::flexaudio_chunk_free(&mut chunk) }
}
#[test]
fn repro_ffi_stop_denoise_tail() {
    let (mut stream, sink) = stream(true);
    push(&sink, &[0.5; 960]);
    let first = chunk(&mut stream);
    let mut delivered = first.len;
    free(first);
    // SAFETY: stream is a valid local handle.
    assert_eq!(unsafe { crate::flexaudio_stop(&mut stream) }, code::FLEX_OK);
    while let Some(tail) = stream.poll_processed().unwrap() {
        delivered += tail.len;
        free(tail);
    }
    assert_eq!(
        delivered,
        960 + 480,
        "stop must return the denoiser's delayed 480-sample tail"
    );
}
#[test]
fn repro_ffi_stop_no_addons_control() {
    let (mut stream, sink) = stream(false);
    push(&sink, &[0.5; 960]);
    let first = chunk(&mut stream);
    assert_eq!(first.len, 960);
    free(first);
    assert_eq!(unsafe { crate::flexaudio_stop(&mut stream) }, code::FLEX_OK);
    assert!(stream.poll_processed().unwrap().is_none());
}
#[test]
fn repro_ffi_resume_resets_denoise() {
    let (mut stream, sink) = stream(true);
    let prior: Vec<f32> = (0..960).map(|i| (i as f32 * 0.1).sin() * 0.5).collect();
    push(&sink, &prior);
    free(chunk(&mut stream));
    stream.inner.pause();
    stream.inner.resume().unwrap();
    push(&sink, &[0.0; 960]);
    let next = chunk(&mut stream);
    assert_ne!(next.flags & fa::ChunkFlags::DISCONTINUITY.bits(), 0);
    let peak = samples(&next)[..480]
        .iter()
        .copied()
        .map(f32::abs)
        .fold(0.0, f32::max);
    free(next);
    stream.inner.stop();
    assert_eq!(
        peak, 0.0,
        "discontinuous silence retained audio from before resume"
    );
}
#[test]
fn repro_ffi_resume_fresh_denoise_control() {
    let (mut stream, sink) = stream(true);
    stream.inner.pause();
    stream.inner.resume().unwrap();
    push(&sink, &[0.0; 960]);
    let next = chunk(&mut stream);
    assert!(samples(&next).iter().all(|sample| *sample == 0.0));
    free(next);
    stream.inner.stop();
}
#[test]
fn repro_ffi_error_kinds_collapsed() {
    let roots = [
        (
            fa::Error::InvalidArg("invalid test input".into()),
            code::FLEX_INVALID_ARG,
        ),
        (
            fa::Error::InvalidState("invalid test state".into()),
            code::FLEX_INVALID_STATE,
        ),
        (fa::Error::DeviceNotFound, code::FLEX_DEVICE_NOT_FOUND),
        (fa::Error::DeviceLost, code::FLEX_DEVICE_LOST),
        (
            fa::Error::PermissionDenied {
                permission: fa::Permission::Microphone,
                detail: "private diagnostic".into(),
            },
            code::FLEX_PERMISSION_DENIED,
        ),
        (
            fa::Error::UnsupportedOsVersion,
            code::FLEX_UNSUPPORTED_OS_VERSION,
        ),
        (
            fa::Error::Backend("backend test failure".into()),
            code::FLEX_FAILURE,
        ),
        (
            fa::Error::UnsupportedFormat("unsupported test format".into()),
            code::FLEX_UNSUPPORTED_FORMAT,
        ),
        (
            fa::Error::NativeFormatChanged {
                advertised: (48000, 2),
                actual: (16000, 1),
            },
            code::FLEX_NATIVE_FORMAT_CHANGED,
        ),
        (fa::Error::Unsupported, code::FLEX_UNSUPPORTED),
        (
            fa::Error::AmbiguousDeviceName,
            code::FLEX_AMBIGUOUS_DEVICE_NAME,
        ),
    ];
    let mut seen = std::collections::HashSet::new();
    for (error, expected) in roots {
        let result = crate::fail(error.clone());
        assert_eq!(result, expected);
        assert!(seen.insert(result), "distinct root kind collapsed");
        let context = error.with_context(fa::ErrorContext::new(fa::Operation::Stop));
        let grouped = fa::Error::Multiple(fa::ErrorGroup::new(
            context,
            fa::Error::Backend("related cleanup failure".into()),
            Vec::new(),
        ));
        assert_eq!(
            crate::fail(grouped),
            expected,
            "wrapper changed the primary root code"
        );
    }
}

#[test]
fn repro_ffi_error_control() {
    clear_last_error();
    assert_eq!(
        crate::fail(fa::Error::DeviceNotFound),
        code::FLEX_DEVICE_NOT_FOUND
    );
    assert!(!crate::flexaudio_last_error().is_null());
    assert_eq!(
        unsafe { crate::flexaudio_stop(std::ptr::null_mut()) },
        code::FLEX_INVALID_ARG
    );
}
#[test]
fn repro_ffi_metrics_after_denoise() {
    let (mut stream, sink) = stream(true);
    push(&sink, &[0.5; 960]);
    let next = chunk(&mut stream);
    let actual = samples(&next)
        .iter()
        .copied()
        .map(f32::abs)
        .fold(0.0, f32::max);
    let advertised = next.peak;
    free(next);
    stream.inner.stop();
    assert_eq!(
        advertised, actual,
        "C peak docs describe delivered samples but peak is pre-denoise"
    );
}
#[test]
fn repro_ffi_metrics_no_addons_control() {
    let (mut stream, sink) = stream(false);
    push(&sink, &[0.5; 960]);
    let next = chunk(&mut stream);
    assert_eq!(
        next.peak,
        samples(&next)
            .iter()
            .copied()
            .map(f32::abs)
            .fold(0.0, f32::max)
    );
    free(next);
    stream.inner.stop();
}
fn config() -> FlexConfig {
    FlexConfig {
        kind: FlexSourceKind::Mic as i32,
        device_id: std::ptr::null(),
        process_id: 0,
        mode: FlexProcessMode::Include as i32,
        exclude_self: 0,
        output_rate: 0,
        output_channels: 0,
        chunk_ms: 0,
        gain: 0.0,
        mix_mic_device_id: std::ptr::null(),
        mix_system_device_id: std::ptr::null(),
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
#[test]
fn repro_ffi_invalid_discriminant() {
    for invalid in [-1, 4, 999, i32::MIN, i32::MAX] {
        let mut raw = config();
        raw.kind = invalid;
        // Incoming integers remain valid Rust; rejected before any backend acquisition.
        assert!(unsafe { crate::convert::build_config(&raw, Vec::new()) }.is_err());
        assert!(unsafe { crate::flexaudio_open(&raw) }.is_null());
        assert_eq!(
            crate::error::root_code(&crate::error::last_audio_error().unwrap()),
            code::FLEX_INVALID_ARG
        );
    }
    for invalid in [-1, 2, 999, i32::MIN, i32::MAX] {
        let mut raw = config();
        raw.mode = invalid; // ignored by microphone, still strictly validated
        assert!(unsafe { crate::convert::build_config(&raw, Vec::new()) }.is_err());
        assert!(unsafe { crate::flexaudio_open(&raw) }.is_null());
    }
    for index in 0..3 {
        let mut raw = config();
        match index {
            0 => raw.exclude_self = 2,
            1 => raw.denoise = 2,
            _ => raw.has_vad = 255,
        }
        assert!(unsafe { crate::convert::build_config(&raw, Vec::new()) }.is_err());
        assert!(unsafe { crate::flexaudio_open(&raw) }.is_null());
    }
    for duration in [1, 10, 21, u32::MAX] {
        let mut raw = config();
        raw.chunk_ms = duration;
        assert!(unsafe { crate::convert::build_config(&raw, Vec::new()) }.is_err());
        assert!(unsafe { crate::flexaudio_open(&raw) }.is_null());
    }
}
#[test]
fn repro_ffi_valid_discriminant_control() {
    assert!(unsafe { crate::convert::build_config(&config(), Vec::new()) }.is_ok());
}

#[test]
fn repro_ffi_getters_valid_handle_control() {
    let (mut stream, _) = stream(false);
    unsafe {
        assert_eq!(crate::flexaudio_gain(&stream), 1.0);
        assert_eq!(crate::flexaudio_dropped_chunks(&stream), 0);
        assert_eq!(crate::flexaudio_stop(&mut stream), 0);
    }
}
#[test]
fn repro_ffi_nul_metadata_injected_control() {
    clear_last_error();
    let ptr = crate::convert::string_to_c("app\0name".to_owned());
    // SAFETY: the converter returns a live CString allocation; reclaim it exactly once.
    let converted = unsafe { std::ffi::CString::from_raw(ptr) };
    assert_eq!(converted.as_bytes(), b"");
    assert!(crate::flexaudio_last_error().is_null());
    let benign = crate::convert::string_to_c("app-name".to_owned());
    assert_eq!(
        unsafe { std::ffi::CString::from_raw(benign) }.as_bytes(),
        b"app-name"
    );
}
#[test]
fn repro_ffi_flac_drop_finalizes_control() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(format!(".repro-ffi-drop-{}.flac", std::process::id()));
    let cpath = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
    unsafe {
        let handle = crate::flac::flexaudio_flac_create(cpath.as_ptr(), 48_000, 1, 0);
        assert!(!handle.is_null());
        assert_eq!(
            crate::flac::flexaudio_flac_write(handle, [0.0; 960].as_ptr(), 960),
            0
        );
        crate::flac::flexaudio_flac_free(handle);
    }
    let bytes = std::fs::read(&path).unwrap();
    std::fs::remove_file(path).unwrap();
    let field = u64::from_be_bytes(bytes[18..26].try_into().unwrap());
    assert_eq!(
        field & ((1 << 36) - 1),
        960,
        "Drop must update STREAMINFO total samples"
    );
}

#[test]
fn repro_ffi_standalone_flush_api() {
    use crate::denoise::*;
    use crate::vad::*;
    unsafe {
        for channels in [1, 2] {
            let handle = flexaudio_denoise_new(channels);
            assert!(!handle.is_null());
            let mut pcm: Vec<f32> = (0..1001 * usize::from(channels))
                .map(|index| (index as f32 * 0.1).sin() * 0.5)
                .collect();
            let mut reference = Denoiser::new(channels).unwrap();
            let mut reference_pcm = pcm.clone();
            reference.process(&mut reference_pcm).unwrap();
            let expected_tail = reference.flush();
            assert_eq!(
                flexaudio_denoise_process(handle, pcm.as_mut_ptr(), pcm.len()),
                code::FLEX_OK
            );
            assert_eq!(pcm, reference_pcm);
            let mut out = std::ptr::null_mut();
            let mut len = 999;
            assert_eq!(
                flexaudio_denoise_flush(handle, &mut out, &mut len),
                code::FLEX_OK
            );
            assert_eq!(len, 480 * usize::from(channels));
            assert_eq!(std::slice::from_raw_parts(out, len), expected_tail);
            flexaudio_denoise_samples_free(out, len);
            assert_eq!(
                flexaudio_denoise_flush(handle, &mut out, &mut len),
                code::FLEX_OK
            );
            assert!(out.is_null());
            assert_eq!(len, 0);
            flexaudio_denoise_samples_free(out, len);
            // A new processing interval is independently flushable.
            assert_eq!(
                flexaudio_denoise_process(handle, pcm.as_mut_ptr(), pcm.len()),
                code::FLEX_OK
            );
            assert_eq!(
                flexaudio_denoise_flush(handle, &mut out, &mut len),
                code::FLEX_OK
            );
            assert_eq!(len, 480 * usize::from(channels));
            flexaudio_denoise_samples_free(out, len);
            flexaudio_denoise_free(handle);
        }
        // Use the same injected active-speech config as the original Rust control.
        let mut vad = FlexVad {
            inner: flexaudio_vad::Vad::new(flexaudio_vad::VadConfig {
                threshold: 0.0,
                neg_threshold: Some(0.0),
                min_speech_ms: 0,
                min_silence_ms: 0,
                speech_pad_ms: 0,
                max_speech_ms: 0,
                sample_rate: 16000,
            })
            .unwrap(),
        };
        let mut events = std::ptr::null_mut();
        let mut len = 999;
        assert_eq!(
            flexaudio_vad_process(
                &mut vad,
                [0.25; 16000].as_ptr(),
                16000,
                16000,
                1,
                &mut events,
                &mut len
            ),
            code::FLEX_OK
        );
        assert!(events.is_null());
        assert_eq!(len, 0);
        assert_eq!(
            flexaudio_vad_flush(&mut vad, std::ptr::null_mut(), &mut len),
            code::FLEX_INVALID_ARG
        );
        assert_eq!(
            flexaudio_vad_flush(&mut vad, &mut events, &mut len),
            code::FLEX_OK
        );
        assert_eq!(len, 2);
        let view = std::slice::from_raw_parts(events, len);
        assert_eq!(view[0].kind, 0);
        assert_eq!(view[1].kind, 1);
        assert!(view[1].at_sample > view[0].at_sample);
        flexaudio_vad_events_free(events, len);
        assert_eq!(
            flexaudio_vad_flush(&mut vad, &mut events, &mut len),
            code::FLEX_OK
        );
        assert!(events.is_null());
        assert_eq!(len, 0);
        flexaudio_vad_events_free(events, len);
    }
}
#[test]
fn repro_ffi_standalone_flush_control() {
    let mut denoise = Denoiser::new(1).unwrap();
    denoise.process(&mut [0.25; 960]).unwrap();
    assert_eq!(denoise.flush().len(), 480);
    let mut vad = flexaudio_vad::Vad::new(flexaudio_vad::VadConfig {
        threshold: 0.0,
        neg_threshold: Some(0.0),
        min_speech_ms: 0,
        min_silence_ms: 0,
        speech_pad_ms: 0,
        max_speech_ms: 0,
        sample_rate: 16000,
    })
    .unwrap();
    assert!(vad
        .process_pcm(&[0.25; 16000], 16000, 1)
        .unwrap()
        .is_empty());
    assert_eq!(vad.flush().unwrap().len(), 2);
}

#[test]
fn standalone_denoise_sample_free_validates_owned_length() {
    use crate::denoise::*;
    unsafe {
        let denoiser = flexaudio_denoise_new(1);
        assert!(!denoiser.is_null());
        let mut input = [0.25; 137];
        assert_eq!(
            flexaudio_denoise_process(denoiser, input.as_mut_ptr(), input.len()),
            0
        );
        let mut samples = std::ptr::null_mut();
        let mut len = 0;
        assert_eq!(flexaudio_denoise_flush(denoiser, &mut samples, &mut len), 0);
        assert_eq!(len, 480);
        flexaudio_denoise_samples_free(samples, len - 1);
        assert!(!crate::flexaudio_last_error().is_null());
        assert!(crate::chunk_storage::frame_index(samples, len).is_some());
        flexaudio_denoise_samples_free(samples, len);
        assert!(crate::chunk_storage::frame_index(samples, len).is_none());
        flexaudio_denoise_free(denoiser);
    }
}
