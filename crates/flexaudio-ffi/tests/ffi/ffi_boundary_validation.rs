//! Offline C ABI validation, processing, and ownership regressions.
use crate::error::{clear_last_error, code};
use crate::types::{FlexChunk, FlexConfig, FlexProcessMode, FlexSourceKind, FlexStream};
use flexaudio as fa;
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
    inner.set_denoise(denoise);
    inner.start().unwrap();
    (
        FlexStream {
            events: Default::default(),
            secondary: None,
            shutdown: None,
            shutdown_event_index: 0,
            vad_tap: None,
            vad_events: Vec::new(),
            vad_origin: (0, 0),
            vad_error: None,
            vad_cleanup: Vec::new(),
            ready_chunks: std::collections::VecDeque::new(),
            inner,
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
fn stop_denoise_tail() {
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
        960 + 960,
        "stop must retain the canonical denoise tail padded to a 20 ms chunk"
    );
}
#[test]
fn stop_no_addons_control() {
    let (mut stream, sink) = stream(false);
    push(&sink, &[0.5; 960]);
    let first = chunk(&mut stream);
    assert_eq!(first.len, 960);
    free(first);
    assert_eq!(unsafe { crate::flexaudio_stop(&mut stream) }, code::FLEX_OK);
    assert!(stream.poll_processed().unwrap().is_none());
}
#[test]
fn resume_denoise_matches_facade_history() {
    let (mut binding, binding_sink) = stream(true);
    let (mut reference, reference_sink) = stream(true);
    let prior: Vec<f32> = (0..960).map(|i| (i as f32 * 0.1).sin() * 0.5).collect();
    for sink in [&binding_sink, &reference_sink] {
        push(sink, &prior);
    }
    free(chunk(&mut binding));
    let facade_chunk = |inner: &mut fa::Stream| {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(chunk) = inner.poll_chunk() {
                return chunk;
            }
            assert!(Instant::now() < deadline, "facade chunk timeout");
            std::thread::sleep(Duration::from_millis(1));
        }
    };
    facade_chunk(&mut reference.inner);
    for inner in [&binding.inner, &reference.inner] {
        inner.pause();
        inner.resume().unwrap();
    }
    for sink in [&binding_sink, &reference_sink] {
        push(sink, &[0.0; 960]);
    }
    let actual = chunk(&mut binding);
    let expected = facade_chunk(&mut reference.inner);
    assert_ne!(actual.flags & fa::ChunkFlags::DISCONTINUITY.bits(), 0);
    assert!(expected.flags.contains(fa::ChunkFlags::DISCONTINUITY));
    assert_eq!(samples(&actual), expected.data);
    free(actual);
    binding.inner.stop();
    reference.inner.stop();
}

#[test]
fn resume_fresh_denoise_control() {
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
fn error_kinds_collapsed() {
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
fn error_control() {
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
fn metrics_after_denoise() {
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
fn metrics_no_addons_control() {
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
        silence_probe: 0,
    }
}
#[test]
fn invalid_discriminant() {
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
            _ => raw.silence_probe = 255,
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
fn valid_discriminant_control() {
    assert!(unsafe { crate::convert::build_config(&config(), Vec::new()) }.is_ok());
}

#[test]
fn getters_valid_handle_control() {
    let (mut stream, _) = stream(false);
    unsafe {
        assert_eq!(crate::flexaudio_gain(&stream), 1.0);
        assert_eq!(crate::flexaudio_dropped_chunks(&stream), 0);
        assert_eq!(crate::flexaudio_stop(&mut stream), 0);
    }
}
#[test]
fn nul_metadata_injected_control() {
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
fn flac_drop_finalizes_control() {
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
