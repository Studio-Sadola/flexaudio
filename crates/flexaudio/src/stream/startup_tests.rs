//! Hardware-free facade startup clocks, generation changes, and controls.
use super::*;
use std::time::Instant;

struct StartupBackend {
    input: Vec<f32>,
}
impl CaptureBackend for StartupBackend {
    fn native_format(&self) -> (u32, u16) {
        (48_000, 2)
    }
    fn start(&mut self, mut sink: RawSink) -> Result<()> {
        sink.push(&self.input, 0);
        Ok(())
    }
    fn stop(&mut self) {}
}
fn input(prefix: f32) -> StartupBackend {
    let mut samples = vec![prefix; 960];
    samples.extend([0.25; 3840]);
    StartupBackend { input: samples }
}
fn first_chunk(stream: &mut Stream) -> Result<AudioChunk> {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if let Some(chunk) = stream.poll_chunk() {
            return Ok(chunk);
        }
        if let Some(error) = stream.terminal_error() {
            return Err(error);
        }
        if Instant::now() >= deadline {
            return Err(Error::Backend(
                "synthetic startup output was not delivered; inspect the regression fixture".into(),
            ));
        }
        thread::sleep(Duration::from_millis(1));
    }
}
#[test]
fn startup_mic_gap_preserves_all_tap_frame_indices_and_discontinuity() -> Result<()> {
    let mut config = StreamConfig::default();
    config.secondary_output = Some(OutputFormat {
        sample_rate: 16_000,
        channels: 1,
    });
    let mut stream = Stream::open(config, Box::new(input(3.058)))?;
    stream.enable_capture_tap()?;
    stream.start()?;
    let first = first_chunk(&mut stream)?;
    stream.stop_checked()?;
    assert_eq!(first.frame_index, 480);
    assert_eq!(first.pts_ns, 10_000_000);
    assert!(first.flags.contains(ChunkFlags::DISCONTINUITY));
    assert!(first.data.iter().all(|sample| *sample == 0.25));
    let capture = stream.poll_capture().expect("canonical startup capture");
    assert_eq!(capture.frame_index, 480);
    assert_eq!(capture.pts_ns, 10_000_000);
    assert!(capture.flags.contains(ChunkFlags::DISCONTINUITY));
    let secondary = stream.poll_secondary().expect("secondary startup capture");
    assert_eq!(secondary.frame_index, 480);
    assert!(secondary.flags.contains(ChunkFlags::DISCONTINUITY));
    let mut reports = 0;
    while let Some(event) = stream.poll_event() {
        if let Event::StartupSamplesDropped { frames, reason } = event {
            assert_eq!(frames, 480);
            assert_eq!(reason, flexaudio_core::StartupDropReason::OutOfRange);
            reports += 1;
        }
    }
    assert_eq!(reports, 1);
    Ok(())
}
#[test]
fn startup_valid_zeros_and_system_out_of_range_are_controls() -> Result<()> {
    for (kind, prefix) in [
        (flexaudio_core::SourceKind::Mic, 0.0),
        (flexaudio_core::SourceKind::SystemLoopback, 3.058),
    ] {
        let mut stream = Stream::open(StreamConfig::new(kind), Box::new(input(prefix)))?;
        stream.start()?;
        let first = first_chunk(&mut stream)?;
        stream.stop_checked()?;
        assert_eq!(first.data[0], prefix);
        assert_eq!(first.frame_index, 0);
        assert_eq!(first.pts_ns, 0);
        assert!(!first.flags.contains(ChunkFlags::DISCONTINUITY));
        while let Some(event) = stream.poll_event() {
            assert!(!matches!(event, Event::StartupSamplesDropped { .. }));
        }
    }
    Ok(())
}
#[test]
fn startup_guard_is_fresh_after_backend_switch() -> Result<()> {
    let mut stream = Stream::open(StreamConfig::default(), Box::new(input(0.0)))?;
    stream.start()?;
    let _ = first_chunk(&mut stream)?;
    stream.switch_backend(Box::new(input(3.058)))?;
    stream.stop_checked()?;
    let mut reports = 0;
    while let Some(event) = stream.poll_event() {
        if matches!(event, Event::StartupSamplesDropped { frames: 480, .. }) {
            reports += 1;
        }
    }
    assert_eq!(reports, 1);
    while let Some(chunk) = stream.poll_chunk() {
        assert!(chunk.data.iter().all(|sample| sample.abs() <= 1.0));
    }
    Ok(())
}
#[test]
fn pending_composite_drop_survives_replacement_and_final_drain_once() -> Result<()> {
    let stream = Stream::open(StreamConfig::default(), Box::new(input(0.0)))?;
    let diagnostics = CaptureDiagnostics::new(48_000, 2);
    diagnostics.record_startup_drop(441, 44_100);
    *stream
        .shared
        .raw_diagnostics
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(diagnostics);
    let (_, consumer) = raw_ring(64);
    {
        let mut raw = stream
            .shared
            .raw_consumer
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        stream.shared.publish_raw(
            &mut raw,
            consumer,
            CaptureDiagnostics::new(48_000, 2),
            (48_000, 2),
            GenerationChange::Switch,
        );
    }
    stream.shared.drain_capture_losses();
    assert_eq!(
        stream.shared.primary_frame_index.load(Ordering::SeqCst),
        480
    );
    let mut stream = stream;
    assert!(matches!(
        stream.poll_event(),
        Some(Event::StartupSamplesDropped { frames: 441, .. })
    ));
    assert!(stream.poll_event().is_none());
    Ok(())
}

struct DrivenStartupBackend(Arc<Mutex<Option<RawSink>>>);
impl CaptureBackend for DrivenStartupBackend {
    fn native_format(&self) -> (u32, u16) {
        (48_000, 2)
    }
    fn start(&mut self, sink: RawSink) -> Result<()> {
        *self.0.lock().unwrap_or_else(|error| error.into_inner()) = Some(sink);
        Ok(())
    }
    fn stop(&mut self) {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
    }
}
#[test]
fn confirmation_window_clock_survives_the_next_partial_push() -> Result<()> {
    let sink = Arc::new(Mutex::new(None));
    let mut stream = Stream::open(
        StreamConfig::default(),
        Box::new(DrivenStartupBackend(sink.clone())),
    )?;
    stream.enable_capture_tap()?;
    stream.start()?;
    let mut input = vec![3.058; 960];
    input.extend([0.25; 960]);
    let first_publication_ns = monotonic_now_ns();
    sink.lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_mut()
        .expect("synthetic sink")
        .push(&input, 0);
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if let Some(Event::StartupSamplesDropped { frames: 480, .. }) = stream.poll_event() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "synthetic confirmation did not arrive"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert!(
        stream.poll_chunk().is_none(),
        "ten milliseconds is shorter than a primary chunk"
    );
    let epoch = stream.shared.recording_epoch_ns.load(Ordering::SeqCst);
    // Established input follows arrival time even when confirmation PCM is still buffered.
    thread::sleep(Duration::from_millis(60));
    let second_publication_ns = monotonic_now_ns();
    sink.lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_mut()
        .expect("synthetic sink")
        .push(&[0.25; 960], 0);
    let first = first_chunk(&mut stream)?;
    let delivered_ns = monotonic_now_ns();
    stream.stop_checked()?;
    assert_eq!(first.frame_index, 480);
    // Primary extrapolation retains its legacy integer nanoseconds per frame. Its new
    // arrival anchor sits 480 frames after the start of the buffered confirmation PCM.
    let buffered_primary_ns = 480 * (1_000_000_000 / 48_000);
    let earliest_pts = second_publication_ns - epoch - buffered_primary_ns;
    // ClockNormalizer's first arrival is between publication and its normalized epoch;
    // use that measured interval to bound its fixed offset without a scheduling guess.
    let latest_pts = delivered_ns - first_publication_ns - buffered_primary_ns;
    assert!(
        (earliest_pts..=latest_pts).contains(&first.pts_ns),
        "primary confirmation PTS {} must follow delayed arrival within {earliest_pts}..={latest_pts}",
        first.pts_ns,
    );
    assert_eq!(
        stream
            .poll_capture()
            .expect("canonical confirmation")
            .pts_ns,
        10_000_000
    );
    while let Some(event) = stream.poll_event() {
        assert!(!matches!(event, Event::StartupSamplesDropped { .. }));
    }
    Ok(())
}

#[test]
fn valid_short_startup_survives_switch_and_recovery() -> Result<()> {
    for recovery in [false, true] {
        let sink = Arc::new(Mutex::new(None));
        let mut stream = Stream::open(
            StreamConfig::default(),
            Box::new(DrivenStartupBackend(sink.clone())),
        )?;
        stream.enable_capture_tap()?;
        stream.start()?;
        let before = stream.shared.last_sample_ns.load(Ordering::SeqCst);
        sink.lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_mut()
            .expect("started fixture")
            .push(&[0.25; 26], 0);
        let deadline = Instant::now() + Duration::from_secs(1);
        while stream.shared.last_sample_ns.load(Ordering::SeqCst) == before {
            assert!(Instant::now() < deadline, "fixture was not ingested");
            thread::sleep(Duration::from_millis(1));
        }
        if recovery {
            Stream::open_backend_once(&stream.shared, GenerationChange::Recovery)?;
        } else {
            stream.switch_backend(Box::new(input(0.5)))?;
        }
        stream.stop_checked()?;
        let prefix = stream
            .poll_capture()
            .expect("valid prefix must survive generation closure");
        assert_eq!(
            prefix.frames, 13,
            "generation closure silently discarded valid startup audio"
        );
        assert_eq!(prefix.data, [0.25; 26]);
        assert_eq!(prefix.frame_index, 0);
        while let Some(event) = stream.poll_event() {
            assert!(!matches!(event, Event::StartupSamplesDropped { .. }));
        }
    }
    Ok(())
}

#[test]
fn valid_pending_startup_is_accounted_when_terminal_closes_capture() -> Result<()> {
    let sink = Arc::new(Mutex::new(None));
    let mut stream = Stream::open(
        StreamConfig::default(),
        Box::new(DrivenStartupBackend(sink.clone())),
    )?;
    stream.enable_capture_tap()?;
    stream.start()?;
    let before = stream.shared.last_sample_ns.load(Ordering::SeqCst);
    sink.lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_mut()
        .expect("started fixture")
        .push(&[0.25; 26], 0);
    let deadline = Instant::now() + Duration::from_secs(1);
    while stream.shared.last_sample_ns.load(Ordering::SeqCst) == before {
        assert!(Instant::now() < deadline, "fixture was not ingested");
        thread::sleep(Duration::from_millis(1));
    }
    stream.shared.fail_terminal(Error::DeviceLost);
    assert_eq!(stream.stop_checked(), Err(Error::DeviceLost));
    assert!(
        stream.poll_capture().is_none(),
        "terminal capture must not deliver held PCM"
    );
    let mut losses = Vec::new();
    while let Some(event) = stream.poll_event() {
        match event {
            Event::AudioLoss { loss } => losses.push(loss),
            Event::StartupSamplesDropped { .. } => panic!("valid input is not out of range"),
            _ => {}
        }
    }
    assert_eq!(
        losses.len(),
        1,
        "terminal closure silently discarded valid startup audio"
    );
    assert_eq!(losses[0].samples().map(NonZeroU64::get), Some(26));
    assert_eq!(
        losses[0].path(),
        flexaudio_core::AudioPath::Capture { lane: None }
    );
    assert_eq!(
        losses[0].reason(),
        flexaudio_core::LossReason::GenerationEnded
    );
    assert_eq!((losses[0].sample_rate(), losses[0].channels()), (48_000, 2));
    assert_eq!(stream.shared.capture_frame_index.load(Ordering::SeqCst), 13);
    assert_eq!(stream.shared.primary_frame_index.load(Ordering::SeqCst), 13);
    assert_eq!(
        stream.shared.secondary_frame_index.load(Ordering::SeqCst),
        13
    );
    assert!(stream.shared.discontinuity_pending.load(Ordering::SeqCst));
    assert_eq!(stream.stop_checked(), Err(Error::DeviceLost));
    assert_eq!(stream.shared.capture_frame_index.load(Ordering::SeqCst), 13);
    assert_eq!(stream.shared.primary_frame_index.load(Ordering::SeqCst), 13);
    assert_eq!(
        stream.shared.secondary_frame_index.load(Ordering::SeqCst),
        13
    );
    Ok(())
}

#[test]
fn pending_startup_report_precedes_ordered_primary_secondary_and_capture() -> Result<()> {
    let mut config = StreamConfig::default();
    config.secondary_output = Some(OutputFormat {
        sample_rate: 16_000,
        channels: 1,
    });
    let mut stream = Stream::open(config, Box::new(input(3.058)))?;
    stream.enable_capture_tap()?;
    stream.enable_ordered_delivery()?;
    stream.start()?;
    stream.stop_checked()?;
    // The N-API push adapter must claim the report before emitting PCM.
    assert!(
        stream.poll_chunk_ordered().is_none(),
        "primary escaped before its startup event"
    );
    assert!(
        stream.poll_secondary_ordered().is_none(),
        "secondary escaped before its startup event"
    );
    assert!(
        stream.poll_capture_ordered().is_none(),
        "Capture tap escaped before its startup event"
    );
    assert!(matches!(
        stream.poll_event(),
        Some(Event::StartupSamplesDropped { .. })
    ));
    assert!(stream.poll_chunk_ordered().is_some());
    assert!(stream.poll_secondary_ordered().is_some());
    assert!(stream.poll_capture_ordered().is_some());
    Ok(())
}

#[test]
fn startup_notice_is_in_fifo_before_plain_pull_exposes_pcm() -> Result<()> {
    let mut stream = Stream::open(StreamConfig::default(), Box::new(input(3.058)))?;
    stream.start()?;
    stream.stop_checked()?;
    let first = stream
        .poll_chunk()
        .expect("plain pull never waits for event polling");
    assert_eq!(first.frame_index, 480);
    assert!(
        stream
            .events
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .any(|event| matches!(event, Event::StartupSamplesDropped { frames: 480, .. })),
        "the notice must be in the chronological event FIFO before PCM becomes pollable"
    );
    let frames = first.frames
        + std::iter::from_fn(|| stream.poll_chunk())
            .map(|chunk| chunk.frames)
            .sum::<usize>();
    assert_eq!(frames, 1920, "event-free pull retains every valid frame");
    Ok(())
}

#[test]
fn startup_notice_does_not_overtake_older_events() -> Result<()> {
    let mut stream = Stream::open(StreamConfig::default(), Box::new(input(3.058)))?;
    stream.shared.push_event(Event::Clipped);
    stream.enable_ordered_delivery()?;
    stream.start()?;
    stream.stop_checked()?;
    assert_eq!(
        stream.poll_event(),
        Some(Event::Clipped),
        "startup must retain chronological event order"
    );
    assert!(matches!(
        stream.poll_event(),
        Some(Event::StartupSamplesDropped { frames: 480, .. })
    ));
    assert!(stream.poll_event().is_none());
    assert!(
        stream.poll_chunk_ordered().is_some(),
        "push barrier opens when its chronological notice is claimed"
    );
    Ok(())
}

#[test]
fn successful_retry_discards_only_the_failed_startup_boundary() -> Result<()> {
    struct Retry {
        failed: bool,
        pending_notice: bool,
    }
    impl CaptureBackend for Retry {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 2)
        }
        fn start(&mut self, mut sink: RawSink) -> Result<()> {
            if !self.failed {
                self.failed = true;
                self.pending_notice = true;
                return Err(Error::Backend(
                    "synthetic startup failed; retry the fixture".into(),
                ));
            }
            sink.push(&input(3.058).input, 0);
            Ok(())
        }
        fn stop(&mut self) {}
        fn poll_event(&mut self) -> Option<Event> {
            std::mem::take(&mut self.pending_notice).then_some(Event::StartupSamplesDropped {
                frames: 480,
                reason: flexaudio_core::StartupDropReason::OutOfRange,
            })
        }
    }
    let mut stream = Stream::open(
        StreamConfig::default(),
        Box::new(Retry {
            failed: false,
            pending_notice: false,
        }),
    )?;
    stream.enable_ordered_delivery()?;
    assert!(stream.start().is_err());
    // A PCM-only caller leaves the failed attempt's notice unclaimed.
    stream.start()?;
    stream.stop_checked()?;
    assert!(stream.poll_chunk_ordered().is_none());
    assert!(matches!(
        stream.poll_event(),
        Some(Event::StartupSamplesDropped { frames: 480, .. })
    ));
    assert!(stream.poll_event().is_none());
    assert!(
        stream.poll_chunk_ordered().is_some(),
        "discarded prior events must not orphan a push boundary after retry"
    );
    Ok(())
}

#[test]
fn pull_only_generations_never_register_push_barriers() -> Result<()> {
    let mut stream = Stream::open(StreamConfig::default(), Box::new(input(3.058)))?;
    stream.start()?;
    stream.stop_checked()?;
    let mut frames = 0;
    while let Some(chunk) = stream.poll_chunk() {
        frames += chunk.frames;
    }
    assert_eq!(frames, 1920);
    for _ in 0..256 {
        stream.shared.raw_generation.fetch_add(1, Ordering::SeqCst);
        stream.shared.push_event(Event::StartupSamplesDropped {
            frames: 480,
            reason: flexaudio_core::StartupDropReason::OutOfRange,
        });
    }
    assert_eq!(
        stream
            .events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len(),
        257
    );
    assert_eq!(
        stream
            .shared
            .startup_order
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending_count(),
        0,
        "pull-only generations must not accumulate unclaimed push barriers"
    );
    Ok(())
}
