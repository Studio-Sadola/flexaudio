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
#[ignore = "repro: C F42"]
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
#[ignore = "repro: C F44"]
fn repro_ffi_error_kinds_collapsed() {
    let missing = crate::fail(fa::Error::DeviceNotFound);
    let lost = crate::fail(fa::Error::DeviceLost);
    assert_ne!(
        missing, lost,
        "C return values erase DeviceNotFound versus DeviceLost"
    );
}
#[test]
fn repro_ffi_error_control() {
    clear_last_error();
    assert_eq!(crate::fail(fa::Error::DeviceNotFound), code::FLEX_FAILURE);
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
        kind: FlexSourceKind::Mic,
        device_id: std::ptr::null(),
        process_id: 0,
        mode: FlexProcessMode::Include,
        exclude_self: false,
        output_rate: 0,
        output_channels: 0,
        chunk_ms: 0,
        gain: 0.0,
        mix_mic_device_id: std::ptr::null(),
        mix_system_device_id: std::ptr::null(),
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
#[test]
fn repro_ffi_invalid_discriminant_child() {
    if std::env::var_os("FLEXAUDIO_REPRO_F52_CHILD").is_none() {
        return;
    }
    let mut config = config();
    // Deliberately emulate an untrusted C integer. The current ABI's enum field makes this
    // invalid Rust; isolate the probe in a subprocess rather than corrupting the test runner.
    unsafe {
        std::ptr::addr_of_mut!(config.kind).cast::<i32>().write(999);
        assert!(
            crate::convert::build_config(&config, Vec::new()).is_err(),
            "invalid C discriminant 999 was accepted instead of InvalidArg"
        );
    }
}
#[test]
#[ignore = "repro: C F52"]
fn repro_ffi_invalid_discriminant() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "repro_p11_tests::repro_ffi_invalid_discriminant_child",
            "--nocapture",
        ])
        .env("FLEXAUDIO_REPRO_F52_CHILD", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "invalid C discriminant was not safely rejected: {}",
        String::from_utf8_lossy(&output.stderr)
    );
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
#[ignore = "repro: C F42"]
fn repro_ffi_standalone_flush_api() {
    let header = include_str!("../../include/flexaudio.h");
    assert!(
        header.contains("flexaudio_vad_flush("),
        "C VAD cannot flush active speech at EOF"
    );
    assert!(
        header.contains("flexaudio_denoise_flush("),
        "C denoise cannot retrieve retained tail"
    );
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
