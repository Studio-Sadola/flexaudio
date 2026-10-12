//! Device-free C startup notice ordering and durable shutdown carriers.
use crate::types::FlexStream;
use flexaudio::ChunkFlags;

fn free(mut chunk: crate::types::FlexChunk) {
    // SAFETY: this live carrier is owned by the test and freed once.
    unsafe { crate::flexaudio_chunk_free(&mut chunk) };
}

struct StartupBackend;
impl flexaudio::CaptureBackend for StartupBackend {
    fn native_format(&self) -> (u32, u16) {
        (48_000, 1)
    }
    fn start(&mut self, mut sink: flexaudio::core::backend::RawSink) -> flexaudio::Result<()> {
        let mut samples = vec![3.058; 480];
        samples.extend([0.25; 960]);
        sink.push(&samples, 0);
        Ok(())
    }
    fn stop(&mut self) {}
}

fn startup_stream(attach_whisper: bool) -> FlexStream {
    let mut inner =
        flexaudio::Stream::open(flexaudio::StreamConfig::default(), Box::new(StartupBackend))
            .expect("synthetic startup stream");
    let whisper = if attach_whisper {
        inner.enable_capture_tap().expect("canonical capture tap");
        Some(
            flexaudio_vad::WhisperVadTap::new(Default::default(), Default::default())
                .expect("offline whisper tap"),
        )
    } else {
        None
    };
    inner.start().expect("synthetic startup");
    // Joining the intake makes pending output deterministic without consuming its notice.
    inner.stop_checked().expect("synthetic capture shutdown");
    binding_stream(inner, whisper)
}

fn binding_stream(
    inner: flexaudio::Stream,
    whisper: Option<flexaudio_vad::WhisperVadTap>,
) -> FlexStream {
    FlexStream {
        inner,
        shutdown: None,
        shutdown_event_index: 0,
        last_output: None,
        whisper,
        whisper_events: Vec::new(),
        whisper_origin: (0, 0),
        whisper_error: None,
        whisper_cleanup: Vec::new(),
        ready_chunks: Default::default(),
        denoiser: None,
        vad: None,
    }
}

#[test]
fn c_pcm_does_not_require_startup_event_poll() {
    let mut stream = startup_stream(false);
    let chunk = stream
        .poll_processed()
        .expect("synthetic PCM poll")
        .expect("C PCM must be available without polling stream events");
    assert_eq!(chunk.frames, 960);
    assert_eq!(chunk.pts_ns, 10_000_000);
    assert_ne!(chunk.flags & ChunkFlags::DISCONTINUITY.bits(), 0);
    free(chunk);
    assert!(stream.poll_processed().expect("remaining PCM").is_none());
    assert!(matches!(
        stream.poll_binding_event(),
        Some(flexaudio::Event::StartupSamplesDropped { frames: 480, .. })
    ));
    assert!(stream.poll_binding_event().is_none());
}

#[test]
fn c_startup_notice_is_queued_before_first_pcm_poll() {
    let mut stream = startup_stream(false);
    assert!(matches!(
        stream.poll_binding_event(),
        Some(flexaudio::Event::StartupSamplesDropped { frames: 480, .. })
    ));
    let chunk = stream
        .poll_processed()
        .expect("first PCM poll")
        .expect("notice was queued before the first chunk was read");
    free(chunk);
}

#[test]
fn c_whisper_epoch_does_not_require_startup_event_poll() {
    let mut stream = startup_stream(true);
    stream.drain_whisper_capture();
    assert!(matches!(
        stream.whisper_events.first(),
        Some(flexaudio_vad::AttachedWhisperVadEvent::EpochStart {
            capture_sample: 480,
            pts_ns: 10_000_000,
            ..
        })
    ));
    let count = stream.whisper_events.len();
    stream.drain_whisper_capture();
    assert_eq!(stream.whisper_events.len(), count);
    assert!(matches!(
        stream.poll_binding_event(),
        Some(flexaudio::Event::StartupSamplesDropped { frames: 480, .. })
    ));
    assert!(stream.poll_binding_event().is_none());
}

#[test]
fn startup_notice_does_not_erase_c_stop_pcm() {
    let mut stream = startup_stream(false);
    stream.stop_binding().expect("durable C shutdown");
    assert!(
        !stream.ready_chunks.is_empty(),
        "C stop must retain PCM even when the startup notice is pending"
    );
    let chunk = stream
        .poll_processed()
        .expect("C poll without an event poll")
        .expect("shutdown retained pending PCM");
    assert_eq!(chunk.pts_ns, 10_000_000);
    free(chunk);
    assert!(matches!(
        stream.poll_binding_event(),
        Some(flexaudio::Event::StartupSamplesDropped { frames: 480, .. })
    ));
    stream.stop_binding().expect("repeated C shutdown");
    assert!(stream.poll_processed().expect("empty C poll").is_none());
}

#[test]
fn startup_notice_does_not_close_c_whisper_before_pending_capture() {
    let mut stream = startup_stream(true);
    stream.stop_binding().expect("durable attached C shutdown");
    assert!(
        !stream.ready_chunks.is_empty(),
        "C stop closed the attached tap before accepting pending capture"
    );
    // SAFETY: both owners are live; each returned chunk is freed exactly once.
    unsafe {
        let mut chunk: crate::whisper_integration::FlexChunkV2 = std::mem::zeroed();
        assert_eq!(
            crate::whisper_integration::flexaudio_poll_chunk_v2(&mut stream, &mut chunk),
            1
        );
        let events = if chunk.whisper_vad_events_len == 0 {
            &[][..]
        } else {
            assert!(!chunk.whisper_vad_events.is_null());
            std::slice::from_raw_parts(chunk.whisper_vad_events, chunk.whisper_vad_events_len)
        };
        assert!(events
            .iter()
            .any(|event| { event.r#type == crate::whisper_integration::FLEX_WHISPER_EPOCH_START }));
        crate::whisper_integration::flexaudio_chunk_free_v2(&mut chunk);
    }
    stream.stop_binding().expect("repeated attached C shutdown");
    assert!(matches!(
        stream.poll_binding_event(),
        Some(flexaudio::Event::StartupSamplesDropped { frames: 480, .. })
    ));
    assert!(stream.poll_binding_event().is_none());
}

#[test]
fn startup_notice_does_not_make_c_flush_skip_pending_capture() {
    let mut stream = startup_stream(true);
    // SAFETY: the stream is a live, exclusively owned synthetic capture handle.
    let result = unsafe { crate::whisper_integration::flexaudio_flush_whisper_vad(&mut stream) };
    assert_eq!(result, crate::error::code::FLEX_OK);
    assert!(
        !stream.ready_chunks.is_empty(),
        "C flush skipped capture while its startup notice was pending"
    );
    // SAFETY: each successful carrier is owned and released by this test.
    unsafe {
        let mut chunk: crate::whisper_integration::FlexChunkV2 = std::mem::zeroed();
        assert_eq!(
            crate::whisper_integration::flexaudio_poll_chunk_v2(&mut stream, &mut chunk),
            1
        );
        assert_eq!(chunk.chunk.pts_ns, 10_000_000);
        assert!(chunk.whisper_vad_events_len > 0);
        assert!(!chunk.whisper_vad_events.is_null());
        let first = &*chunk.whisper_vad_events;
        assert_eq!(
            first.r#type,
            crate::whisper_integration::FLEX_WHISPER_EPOCH_START
        );
        crate::whisper_integration::flexaudio_chunk_free_v2(&mut chunk);
    }
    stream
        .stop_binding()
        .expect("shutdown after explicit flush");
    assert!(matches!(
        stream.poll_binding_event(),
        Some(flexaudio::Event::StartupSamplesDropped { frames: 480, .. })
    ));
    assert!(stream.poll_binding_event().is_none());
}

#[test]
fn c_v1_terminal_carrier_and_error_do_not_require_startup_event_poll() {
    struct TerminalBackend(std::sync::Arc<std::sync::Mutex<Option<flexaudio::Event>>>);
    impl flexaudio::CaptureBackend for TerminalBackend {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 1)
        }
        fn start(&mut self, sink: flexaudio::core::backend::RawSink) -> flexaudio::Result<()> {
            StartupBackend.start(sink)
        }
        fn stop(&mut self) {}
        fn poll_event(&mut self) -> Option<flexaudio::Event> {
            self.0.lock().expect("synthetic event owner").take()
        }
    }
    let event = std::sync::Arc::new(std::sync::Mutex::new(None));
    let mut inner = flexaudio::Stream::open(
        flexaudio::StreamConfig::default(),
        Box::new(TerminalBackend(event.clone())),
    )
    .expect("synthetic terminal stream");
    inner.start().expect("synthetic capture start");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    // Ordinary Rust polling can confirm queued PCM without claiming the startup notice.
    while inner.poll_chunk().is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "synthetic PCM deadline"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let error =
        flexaudio::Error::Backend("synthetic capture ended; reopen the fixture to continue".into());
    *event.lock().expect("synthetic event owner") = Some(flexaudio::Event::TerminalError {
        error: error.clone(),
    });
    inner
        .stop_checked()
        .expect_err("injected capture terminality");
    let mut stream = binding_stream(inner, None);
    // SAFETY: an empty carrier owns no allocations and has valid zero-count fields.
    let empty: crate::types::FlexChunk = unsafe { std::mem::zeroed() };
    let carrier = stream.versioned_chunk(empty);
    stream.ready_chunks.push_back(carrier);
    // SAFETY: output and stream are aligned, writable live owners.
    unsafe {
        let mut out = std::mem::MaybeUninit::uninit();
        assert_eq!(
            crate::flexaudio_poll_chunk(&mut stream, out.as_mut_ptr()),
            1
        );
        assert_eq!(out.assume_init_ref().frames, 0);
        crate::flexaudio_chunk_free(out.as_mut_ptr());
        let result = crate::flexaudio_poll_chunk(&mut stream, out.as_mut_ptr());
        assert_eq!(
            result,
            crate::error::code::FLEX_FAILURE,
            "the retained terminal error must remain available without event polling"
        );
        assert_eq!(crate::error::last_audio_error(), Some(error));
        assert!(stream.ready_chunks.is_empty());
        assert!(matches!(
            stream.poll_binding_event(),
            Some(flexaudio::Event::StartupSamplesDropped { frames: 480, .. })
        ));
    }
}

#[test]
fn c_startup_timeout_is_terminal_without_event_polling() {
    struct InvalidStartup;
    impl flexaudio::CaptureBackend for InvalidStartup {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 1)
        }
        fn start(&mut self, mut sink: flexaudio::core::backend::RawSink) -> flexaudio::Result<()> {
            sink.push(&[2.0; 480], 0);
            Ok(())
        }
        fn stop(&mut self) {}
    }
    for versioned in [false, true] {
        let mut inner = flexaudio::Stream::open(Default::default(), Box::new(InvalidStartup))
            .expect("invalid synthetic startup");
        inner
            .start()
            .expect("synthetic startup begins pending confirmation");
        let mut stream = binding_stream(inner, None);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
        loop {
            // SAFETY: both outputs and the exclusively owned stream are valid stack owners.
            let result = unsafe {
                if versioned {
                    let mut chunk: crate::whisper_integration::FlexChunkV2 = std::mem::zeroed();
                    crate::whisper_integration::flexaudio_poll_chunk_v2(&mut stream, &mut chunk)
                } else {
                    let mut chunk: crate::types::FlexChunk = std::mem::zeroed();
                    crate::flexaudio_poll_chunk(&mut stream, &mut chunk)
                }
            };
            assert_ne!(result, 1, "out-of-range startup PCM must never escape");
            if result < 0 {
                assert_eq!(result, crate::error::code::FLEX_FAILURE);
                let error = crate::error::last_audio_error().expect("typed startup failure");
                assert!(error.to_string().contains("within 2 seconds"));
                assert!(stream.inner.terminal_error().is_some());
                break;
            }
            assert!(std::time::Instant::now() < deadline,
                "startup terminal failure must be observable without consuming its pending event; versioned={versioned}");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let _ = stream.stop_binding();
    }
}
