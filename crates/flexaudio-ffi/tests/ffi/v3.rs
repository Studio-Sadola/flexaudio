//! Offline v3 envelope, ownership and shared-processing regressions.
use super::*;
use crate::secondary::*;
use crate::types::FlexConfig;
use crate::vad_integration::*;
use std::{
    mem, ptr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

fn base() -> FlexConfig {
    unsafe { mem::zeroed() }
}
fn envelope(base: &FlexConfig) -> FlexStreamConfigV3 {
    FlexStreamConfigV3 {
        size: u32::try_from(mem::size_of::<FlexStreamConfigV3>()).unwrap(),
        version: 3,
        config: base,
        vad: ptr::null(),
        exclude_pids: ptr::null(),
        exclude_pids_len: 0,
        secondary_output: ptr::null(),
        windows_classic_loopback_keepalive: 0,
    }
}
fn secondary(encoding: u32) -> FlexSecondaryOutputV3 {
    FlexSecondaryOutputV3 {
        size: u32::try_from(mem::size_of::<FlexSecondaryOutputV3>()).unwrap(),
        version: 3,
        rate: 48_000,
        channels: 2,
        encoding,
    }
}
fn vad(tap: u32) -> FlexStreamVadOptions {
    FlexStreamVadOptions {
        params: crate::vad::flexaudio_vad_default_options(),
        provisional: 1,
        tap,
    }
}

unsafe fn assert_invalid(config: *const FlexStreamConfigV3) {
    let mut out = ptr::dangling_mut();
    assert_eq!(
        open_with(config, &mut out, |_, _| panic!(
            "device accessed before validation"
        )),
        code::FLEX_INVALID_ARG
    );
    assert!(out.is_null());
    assert_eq!(
        error::last_audio_error().unwrap().kind(),
        flexaudio::ErrorKind::InvalidArg
    );
}

#[test]
fn envelope_headers_and_pointer_shapes_are_checked_without_full_references() {
    unsafe {
        assert_invalid(ptr::null());
        assert_invalid(ptr::dangling::<u8>().cast());
        #[repr(C, align(8))]
        struct Header([u32; 2]);
        for words in [
            [0, 3],
            [8, 3],
            [u32::MAX, 3],
            [56, 0],
            [56, 2],
            [56, u32::MAX],
        ] {
            let header = Header(words);
            assert_invalid(ptr::from_ref(&header).cast());
        }
        let base = base();
        let mut config = envelope(&base);
        config.config = ptr::null();
        assert_invalid(&config);
        config.config = ptr::dangling::<u8>().cast();
        assert_invalid(&config);
        config.config = &base;
        config.vad = ptr::dangling::<u8>().cast();
        assert_invalid(&config);
        config.vad = ptr::null();
        config.secondary_output = ptr::dangling::<u8>().cast();
        assert_invalid(&config);
        let mut out = ptr::dangling_mut();
        assert_eq!(
            flexaudio_open_v3(&config, ptr::null_mut()),
            code::FLEX_INVALID_ARG
        );
        assert_eq!(
            flexaudio_open_v3(&config, ptr::dangling_mut::<u8>().cast()),
            code::FLEX_INVALID_ARG
        );
        assert_eq!(
            flexaudio_open_v3(ptr::null(), &mut out),
            code::FLEX_INVALID_ARG
        );
        assert!(out.is_null());
    }
}

#[test]
fn all_discriminants_and_pid_lists_precede_model_and_device_access() {
    unsafe {
        let mut base = base();
        let mut config = envelope(&base);
        let mut vad = vad(FLEX_VAD_PRIMARY);
        config.vad = &vad;
        for (pointer, len) in [
            (ptr::null(), 1),
            (ptr::dangling::<u8>().cast(), 1),
            (ptr::dangling(), 4097),
            (ptr::dangling(), usize::MAX),
            ([0].as_ptr(), 1),
        ] {
            config.exclude_pids = pointer;
            config.exclude_pids_len = len;
            assert_invalid(&config);
        }
        config.exclude_pids = ptr::null();
        config.exclude_pids_len = 0;
        for keepalive in [3, u32::MAX] {
            config.windows_classic_loopback_keepalive = keepalive;
            assert_invalid(&config);
        }
        config.windows_classic_loopback_keepalive = 0;
        for field in 0..5 {
            base = base_config_with_invalid(field);
            config.config = &base;
            assert_invalid(&config);
        }
        base = self::base();
        config.config = &base;
        vad.tap = 99;
        config.vad = &vad;
        assert_invalid(&config);
        vad.tap = FLEX_VAD_PRIMARY;
        vad.provisional = 2;
        config.vad = &vad;
        assert_invalid(&config);
        vad.provisional = 1;
        config.vad = &vad;
        let mut format = secondary(FLEX_SECONDARY_F32);
        config.secondary_output = &format;
        for encoding in [0, 3, u32::MAX] {
            format.encoding = encoding;
            config.secondary_output = &format;
            assert_invalid(&config);
        }
        format.encoding = FLEX_SECONDARY_F32;
        format.version = 2;
        config.secondary_output = &format;
        assert_invalid(&config);
        format.version = 3;
        format.size = u32::MAX;
        config.secondary_output = &format;
        assert_invalid(&config);
        config.secondary_output = ptr::null();
        vad.tap = FLEX_VAD_SECONDARY;
        config.vad = &vad;
        let mut out = ptr::null_mut();
        assert_eq!(
            open_with(&config, &mut out, |_, _| panic!("device accessed")),
            FLEX_VAD_UNSUPPORTED_TAP
        );
        assert!(out.is_null());
    }
}
fn base_config_with_invalid(field: usize) -> FlexConfig {
    let mut base = base();
    match field {
        0 => base.kind = 99,
        1 => base.mode = 2,
        2 => base.exclude_self = 2,
        3 => base.denoise = 2,
        _ => base.silence_probe = 3,
    }
    base
}

type Sink = Arc<Mutex<Option<flexaudio::core::backend::RawSink>>>;
struct Push(Sink);
impl flexaudio::CaptureBackend for Push {
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
/// One installed generation. A second start is refused so stall recovery cannot
/// drop the pushed samples while this test is still polling.
struct HeldPush {
    sink: Sink,
    fence: Arc<AtomicBool>,
}
impl flexaudio::CaptureBackend for HeldPush {
    fn native_format(&self) -> (u32, u16) {
        (48_000, 2)
    }
    fn start(&mut self, sink: flexaudio::core::backend::RawSink) -> flexaudio::Result<()> {
        let mut current = self.sink.lock().unwrap();
        if self.fence.load(Ordering::SeqCst) && current.is_some() {
            return Err(flexaudio::Error::Backend(
                "This fixture keeps one capture generation.".into(),
            ));
        }
        *current = Some(sink);
        self.fence.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn stop(&mut self) {
        if !self.fence.load(Ordering::SeqCst) {
            self.sink.lock().unwrap().take();
        }
    }
}
struct Handle(*mut FlexStream);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { crate::flexaudio_free(self.0) }
    }
}
unsafe fn open_mock(config: &FlexStreamConfigV3, sink: &Sink) -> Handle {
    let mut out = ptr::null_mut();
    assert_eq!(
        open_with(config, &mut out, |config, _| flexaudio::Stream::open(
            config,
            Box::new(Push(sink.clone()))
        )),
        0
    );
    assert!(!out.is_null());
    Handle(out)
}
unsafe fn next_secondary(stream: *mut FlexStream) -> *mut FlexSecondaryChunkV3 {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut chunk = ptr::null_mut();
        let status = flexaudio_poll_secondary_chunk_v3(stream, &mut chunk);
        assert!(status >= 0, "secondary poll error {status}");
        if status == 1 {
            return chunk;
        }
        assert!(Instant::now() < deadline, "secondary chunk timeout");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn v3_open_combines_vad_exclusions_secondary_and_keepalive() {
    unsafe {
        let mut base = base();
        base.kind = crate::legacy_codes::FLEX_SOURCE_KIND_SYSTEM;
        let mut config = envelope(&base);
        let vad = vad(FLEX_VAD_SECONDARY);
        let format = secondary(FLEX_SECONDARY_F32);
        let pids = [17, 17, u32::MAX];
        config.vad = &vad;
        config.secondary_output = &format;
        config.exclude_pids = pids.as_ptr();
        config.exclude_pids_len = pids.len();
        config.windows_classic_loopback_keepalive = 2;
        let sink = Arc::new(Mutex::new(None));
        let mut out = ptr::null_mut();
        assert_eq!(
            open_with(&config, &mut out, |config, options| {
                assert!(!options.windows_classic_loopback_keepalive);
                assert_eq!(config.exclude_pids, pids);
                flexaudio::Stream::open(
                    config,
                    Box::new(HeldPush {
                        sink: sink.clone(),
                        fence: Arc::new(AtomicBool::new(false)),
                    }),
                )
            }),
            0
        );
        let handle = Handle(out);
        assert!((*handle.0).vad_tap.is_some());
        assert_eq!(crate::flexaudio_start(handle.0), 0);
        sink.lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .push(&[0.25; 1920], 0);
        // Capture can publish the epoch before either PCM tap. A zero-frame
        // secondary carrier is that event, not this push. Wait for the origin
        // and both PCM taps before stop, so the closing chunk is the epoch end.
        let mut saw_origin = false;
        let mut saw_secondary_pcm = false;
        let mut deadline = Instant::now() + Duration::from_secs(5);
        while !saw_origin || !saw_secondary_pcm {
            assert!(
                Instant::now() < deadline,
                "secondary tap must deliver the epoch origin and the pushed PCM"
            );
            let mut chunk = ptr::null_mut();
            let status = flexaudio_poll_secondary_chunk_v3(handle.0, &mut chunk);
            assert!(status >= 0, "secondary poll error {status}");
            if status == 0 {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            let mut len = 0;
            let events = flexaudio_secondary_vad_events_v3(chunk, &mut len);
            if len > 0 && !saw_origin {
                assert_eq!((*events).r#type, FLEX_VAD_EPOCH_START);
                assert_eq!((*events).data.epoch_start.capture_sample, 0);
                saw_origin = true;
            }
            if (*chunk).metadata.frames > 0 {
                saw_secondary_pcm = true;
            }
            flexaudio_secondary_chunk_free_v3(chunk);
        }
        deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_primary_pcm = false;
        while !saw_primary_pcm {
            assert!(
                Instant::now() < deadline,
                "primary tap must deliver the pushed PCM without secondary events"
            );
            let mut primary = mem::zeroed();
            match flexaudio_poll_chunk_v2(handle.0, &mut primary) {
                1 => {
                    assert_eq!(
                        primary.vad_events_len, 0,
                        "secondary events must not be duplicated on primary"
                    );
                    saw_primary_pcm = primary.chunk.frames > 0;
                    flexaudio_chunk_free_v2(&mut primary);
                }
                0 => std::thread::sleep(Duration::from_millis(1)),
                status => panic!("primary poll error {status}"),
            }
        }
        assert_eq!(crate::flexaudio_stop(handle.0), 0);
        let closing = next_secondary(handle.0);
        let mut len = 0;
        let events = flexaudio_secondary_vad_events_v3(closing, &mut len);
        assert!(len > 0, "stop must publish the secondary epoch end");
        assert_eq!(
            (*events.add(len - 1)).r#type,
            crate::vad_types::FLEX_VAD_EPOCH_END
        );
        flexaudio_secondary_chunk_free_v3(closing);
    }
}

#[test]
fn secondary_encodings_own_samples_and_preserve_prequantization_meters() {
    unsafe {
        for encoding in [FLEX_SECONDARY_F32, FLEX_SECONDARY_S16] {
            let base = base();
            let mut config = envelope(&base);
            let format = secondary(encoding);
            config.secondary_output = &format;
            let sink = Arc::new(Mutex::new(None));
            let handle = open_mock(&config, &sink);
            assert_eq!(crate::flexaudio_start(handle.0), 0);
            sink.lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .push(&[0.12345; 1920], 0);
            let chunk = next_secondary(handle.0);
            let mut meta = FlexSecondaryMetadataV3::default();
            assert_eq!(flexaudio_secondary_metadata_v3(chunk, &mut meta), 1);
            let mut arrival = 0;
            assert_eq!(
                crate::discovery_v4::flexaudio_secondary_arrival_ns_v4(chunk, &mut arrival),
                0
            );
            assert_eq!(arrival, meta.pts_ns);
            assert_eq!(
                (
                    meta.rate,
                    meta.channels,
                    meta.encoding,
                    meta.frames,
                    meta.frame_index,
                    meta.seq
                ),
                (48_000, 2, encoding, 960, 0, 0)
            );
            assert!((meta.peak - 0.12345).abs() < 1e-6);
            assert!((meta.rms - 0.12345).abs() < 1e-6);
            let mut len = 99;
            if encoding == FLEX_SECONDARY_F32 {
                assert!(flexaudio_secondary_s16_v3(chunk, &mut len).is_null());
                assert_eq!(len, 0);
                assert_eq!(
                    error::last_audio_error().unwrap().kind(),
                    flexaudio::ErrorKind::InvalidArg
                );
                let samples = flexaudio_secondary_f32_v3(chunk, &mut len);
                assert_eq!(len, 1920);
                assert!(std::slice::from_raw_parts(samples, len)
                    .iter()
                    .all(|s| (*s - 0.12345).abs() < 1e-6));
            } else {
                assert!(flexaudio_secondary_f32_v3(chunk, &mut len).is_null());
                assert_eq!(len, 0);
                assert_eq!(
                    error::last_audio_error().unwrap().kind(),
                    flexaudio::ErrorKind::InvalidArg
                );
                let samples = flexaudio_secondary_s16_v3(chunk, &mut len);
                assert_eq!(len, 1920);
                assert!(std::slice::from_raw_parts(samples, len)
                    .iter()
                    .all(|s| *s == flexaudio::core::quantize_i16(0.12345)));
            }
            // The chunk owns its arrays independently of the stream. No separate frees.
            drop(handle);
            assert_eq!(flexaudio_secondary_metadata_v3(chunk, &mut meta), 1);
            flexaudio_secondary_chunk_free_v3(chunk);
        }
        flexaudio_secondary_chunk_free_v3(ptr::null_mut());
    }
}

#[test]
fn canonical_denoise_matches_facade_and_stop_flushes_both_taps() {
    let input: Vec<f32> = (0..9600)
        .flat_map(|i| {
            let t = i as f32 / 48_000.0;
            [
                0.3 * (std::f32::consts::TAU * 440.0 * t).sin(),
                0.15 * (std::f32::consts::TAU * 750.0 * t).sin(),
            ]
        })
        .collect();
    for channels in [1, 2] {
        for gain in [1.0, 0.5] {
            unsafe {
                let mut base = base();
                base.denoise = 1;
                base.output_channels = channels;
                base.gain = gain;
                let mut config = envelope(&base);
                let mut format = secondary(FLEX_SECONDARY_F32);
                format.channels = channels;
                config.secondary_output = &format;
                let sink = Arc::new(Mutex::new(None));
                let handle = open_mock(&config, &sink);
                assert_eq!(crate::flexaudio_start(handle.0), 0);
                let facade_sink = Arc::new(Mutex::new(None));
                let mut facade = flexaudio::Stream::open(
                    crate::convert::build_config(&base, Vec::new()).unwrap(),
                    Box::new(Push(facade_sink.clone())),
                )
                .unwrap();
                facade.set_denoise(true);
                facade.start().unwrap();
                for sink in [&sink, &facade_sink] {
                    assert_eq!(
                        sink.lock().unwrap().as_mut().unwrap().push(&input, 0),
                        input.len()
                    );
                }
                let mut actual = Vec::new();
                let mut secondary_samples = Vec::new();
                for _ in 0..10 {
                    let secondary = next_secondary(handle.0);
                    let mut len = 0;
                    let samples = flexaudio_secondary_f32_v3(secondary, &mut len);
                    secondary_samples.extend_from_slice(std::slice::from_raw_parts(samples, len));
                    flexaudio_secondary_chunk_free_v3(secondary);
                }
                // Include an incomplete final chunk as well as the canonical denoiser tail.
                for sink in [&sink, &facade_sink] {
                    assert_eq!(
                        sink.lock()
                            .unwrap()
                            .as_mut()
                            .unwrap()
                            .push(&input[..274], 200_000_000),
                        274
                    );
                }
                assert_eq!(crate::flexaudio_stop(handle.0), 0);
                facade.stop_checked().unwrap();
                let mut primary = mem::zeroed();
                while flexaudio_poll_chunk_v2(handle.0, &mut primary) == 1 {
                    actual.extend_from_slice(std::slice::from_raw_parts(
                        primary.chunk.data,
                        primary.chunk.len,
                    ));
                    flexaudio_chunk_free_v2(&mut primary);
                }
                loop {
                    let mut chunk = ptr::null_mut();
                    let status = flexaudio_poll_secondary_chunk_v3(handle.0, &mut chunk);
                    assert!(status >= 0);
                    if status == 0 {
                        break;
                    }
                    let mut len = 0;
                    let samples = flexaudio_secondary_f32_v3(chunk, &mut len);
                    secondary_samples.extend_from_slice(std::slice::from_raw_parts(samples, len));
                    flexaudio_secondary_chunk_free_v3(chunk);
                }
                let mut expected = Vec::new();
                while let Some(chunk) = facade.poll_chunk() {
                    expected.extend(chunk.data);
                }
                assert_eq!(
                    actual.len(),
                    (9600_usize + 137 + 480).div_ceil(960) * 960 * usize::from(channels)
                );
                assert!(actual.iter().any(|s| s.abs() > 1e-6));
                assert_eq!(actual, expected, "channels={channels}, gain={gain}");
                assert_eq!(
                    actual, secondary_samples,
                    "both taps must share canonical denoise"
                );
                assert_eq!(crate::flexaudio_stop(handle.0), 0);
            }
        }
    }
}

#[test]
fn source_switch_reuses_validation_and_preserves_secondary_format() {
    unsafe {
        let base = base();
        let mut config = envelope(&base);
        let format = secondary(FLEX_SECONDARY_S16);
        config.secondary_output = &format;
        let sink = Arc::new(Mutex::new(None));
        let handle = open_mock(&config, &sink);
        let mut switch = FlexSwitchConfigV3 {
            size: u32::try_from(mem::size_of::<FlexSwitchConfigV3>()).unwrap(),
            version: 3,
            config: &base,
            exclude_pids: ptr::null(),
            exclude_pids_len: 0,
            windows_classic_loopback_keepalive: 0,
        };
        assert_eq!(
            flexaudio_switch_source_v3(handle.0, &switch),
            code::FLEX_INVALID_STATE
        );
        assert_eq!(
            (*handle.0).inner.config().secondary_output,
            Some(flexaudio::OutputFormat {
                sample_rate: 48_000,
                channels: 2
            })
        );
        switch.exclude_pids_len = 4097;
        assert_eq!(
            flexaudio_switch_source_v3(handle.0, &switch),
            code::FLEX_INVALID_ARG
        );
        switch.exclude_pids_len = 0;
        switch.windows_classic_loopback_keepalive = 99;
        assert_eq!(
            flexaudio_switch_source_v3(handle.0, &switch),
            code::FLEX_INVALID_ARG
        );
        switch.windows_classic_loopback_keepalive = 0;
        switch.version = 2;
        assert_eq!(
            flexaudio_switch_source_v3(handle.0, &switch),
            code::FLEX_INVALID_ARG
        );
        assert_eq!(
            flexaudio_switch_source_v3(handle.0, ptr::null()),
            code::FLEX_INVALID_ARG
        );
        assert_eq!(
            flexaudio_switch_source_v3(ptr::null_mut(), &switch),
            code::FLEX_INVALID_ARG
        );
    }
}

#[test]
fn secondary_accessors_validate_outputs_before_consuming_audio() {
    unsafe {
        let base = base();
        let mut config = envelope(&base);
        let format = secondary(FLEX_SECONDARY_F32);
        config.secondary_output = &format;
        let sink = Arc::new(Mutex::new(None));
        let handle = open_mock(&config, &sink);
        assert_eq!(crate::flexaudio_start(handle.0), 0);
        sink.lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .push(&[0.25; 1920], 0);
        assert_eq!(
            flexaudio_poll_secondary_chunk_v3(handle.0, ptr::null_mut()),
            code::FLEX_INVALID_ARG
        );
        assert_eq!(
            flexaudio_poll_secondary_chunk_v3(handle.0, ptr::dangling_mut::<u8>().cast()),
            code::FLEX_INVALID_ARG
        );
        let chunk = next_secondary(handle.0);
        let mut len = 99;
        assert!(flexaudio_secondary_f32_v3(chunk, ptr::null_mut()).is_null());
        assert!(flexaudio_secondary_s16_v3(chunk, ptr::dangling_mut::<u8>().cast()).is_null());
        assert!(
            flexaudio_secondary_vad_events_v3(chunk, ptr::dangling_mut::<u8>().cast()).is_null()
        );
        assert!(flexaudio_secondary_f32_v3(ptr::null(), &mut len).is_null());
        assert_eq!(len, 0);
        assert!(flexaudio_secondary_s16_v3(ptr::dangling::<u8>().cast(), &mut len).is_null());
        assert_eq!(len, 0);
        assert!(flexaudio_secondary_vad_events_v3(ptr::null(), &mut len).is_null());
        assert_eq!(len, 0);
        let mut meta = FlexSecondaryMetadataV3::default();
        assert_eq!(
            flexaudio_secondary_metadata_v3(ptr::null(), &mut meta),
            code::FLEX_INVALID_ARG
        );
        assert_eq!(
            flexaudio_secondary_metadata_v3(chunk, ptr::dangling_mut::<u8>().cast()),
            code::FLEX_INVALID_ARG
        );
        assert!(!flexaudio_secondary_f32_v3(chunk, &mut len).is_null());
        assert_eq!(len, 1920);
        assert_eq!(flexaudio_secondary_metadata_v3(chunk, &mut meta), 1);
        assert_eq!(meta.seq, 0);
        flexaudio_secondary_chunk_free_v3(chunk);
        let mut out = ptr::dangling_mut();
        assert_eq!(
            flexaudio_poll_secondary_chunk_v3(ptr::null_mut(), &mut out),
            code::FLEX_INVALID_ARG
        );
        assert!(out.is_null());
        assert_eq!(
            flexaudio_poll_secondary_chunk_v3(ptr::dangling_mut::<u8>().cast(), &mut out),
            code::FLEX_INVALID_ARG
        );
        assert!(out.is_null());
        assert_eq!(crate::flexaudio_stop(handle.0), 0);
    }
}

#[test]
fn flush_drains_different_secondary_format_and_closes_only_its_vad_carrier() {
    unsafe {
        let base = base();
        let mut config = envelope(&base);
        let mut format = secondary(FLEX_SECONDARY_S16);
        format.rate = 16000;
        format.channels = 1;
        let vad = vad(FLEX_VAD_SECONDARY);
        config.vad = &vad;
        config.secondary_output = &format;
        let sink = Arc::new(Mutex::new(None));
        let handle = open_mock(&config, &sink);
        assert_eq!(crate::flexaudio_start(handle.0), 0);
        sink.lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .push(&[0.25; 7680], 0);
        (*handle.0).inner.stop_checked().unwrap();
        assert_eq!(flexaudio_flush_vad(handle.0), 0);
        assert!(
            (*handle.0).inner.poll_secondary().is_none(),
            "flush must drain the secondary ring"
        );
        assert!(
            (*handle.0).inner.poll_chunk().is_none(),
            "flush must drain the primary ring"
        );
        let mut frames = 0;
        let mut ended = false;
        loop {
            let mut chunk = ptr::null_mut();
            if flexaudio_poll_secondary_chunk_v3(handle.0, &mut chunk) != 1 {
                break;
            }
            let mut meta = FlexSecondaryMetadataV3::default();
            flexaudio_secondary_metadata_v3(chunk, &mut meta);
            assert_eq!(
                (meta.rate, meta.channels, meta.encoding),
                (16000, 1, FLEX_SECONDARY_S16)
            );
            frames += meta.frames;
            let mut len = 0;
            let events = flexaudio_secondary_vad_events_v3(chunk, &mut len);
            if len > 0 {
                ended |= std::slice::from_raw_parts(events, len)
                    .iter()
                    .any(|e| e.r#type == crate::vad_types::FLEX_VAD_EPOCH_END);
            }
            flexaudio_secondary_chunk_free_v3(chunk);
        }
        assert!(frames > 0);
        assert!(ended);
        assert_eq!(flexaudio_flush_vad(handle.0), 0);
        let mut chunk = ptr::null_mut();
        assert_eq!(flexaudio_poll_secondary_chunk_v3(handle.0, &mut chunk), 0);
        assert_eq!(crate::flexaudio_stop(handle.0), 0);
    }
}

#[test]
fn keepalive_default_and_switch_retention_are_explicit() {
    let on = flexaudio::OpenOptions::default();
    let off = on.clone().windows_classic_loopback_keepalive(false);
    assert!(
        validation::options(0, &on)
            .unwrap()
            .windows_classic_loopback_keepalive
    );
    assert!(
        !validation::options(0, &off)
            .unwrap()
            .windows_classic_loopback_keepalive
    );
    assert!(
        validation::options(1, &off)
            .unwrap()
            .windows_classic_loopback_keepalive
    );
    assert!(
        !validation::options(2, &on)
            .unwrap()
            .windows_classic_loopback_keepalive
    );
}

#[test]
fn maximum_pid_count_is_copied_and_an_empty_pointer_is_never_read() {
    unsafe {
        let base = base();
        let mut config = envelope(&base);
        let mut pids = vec![u32::MAX; 4096];
        config.exclude_pids = pids.as_ptr();
        config.exclude_pids_len = pids.len();
        let sink = Arc::new(Mutex::new(None));
        let handle = open_mock(&config, &sink);
        pids.fill(0);
        assert_eq!(
            (*handle.0).inner.config().exclude_pids,
            vec![u32::MAX; 4096]
        );
        config.exclude_pids = ptr::dangling::<u8>().cast();
        config.exclude_pids_len = 0;
        let empty = open_mock(&config, &sink);
        assert!((*empty.0).inner.config().exclude_pids.is_empty());
        let mut out = ptr::dangling_mut();
        assert_eq!(flexaudio_poll_secondary_chunk_v3(empty.0, &mut out), 0);
        assert!(out.is_null());
    }
}
