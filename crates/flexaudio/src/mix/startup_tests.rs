//! Device-free regression coverage for startup validation in the microphone Mix lane.
use super::*;

struct Samples(Vec<f32>, (u32, u16));
impl CaptureBackend for Samples {
    fn native_format(&self) -> (u32, u16) {
        self.1
    }
    fn start(&mut self, mut sink: RawSink) -> Result<()> {
        sink.push(&self.0, 0);
        Ok(())
    }
    fn stop(&mut self) {}
}

fn lane(samples: Vec<f32>, kind: MixLane) -> Result<ChildLane> {
    native_lane(samples, kind, (48_000, 2))
}

fn native_lane(samples: Vec<f32>, kind: MixLane, format: (u32, u16)) -> Result<ChildLane> {
    let mut backend: Box<dyn CaptureBackend> = Box::new(Samples(samples, format));
    start_child(&mut backend, kind, Arc::new(Notices::default()))
}

#[test]
fn mix_mic_invalid_startup_never_reaches_normalizer_or_fifo() -> Result<()> {
    // Reopening constructs another generation and must validate again.
    for _ in 0..2 {
        let mut mic = lane(vec![3.058; 1_920], MixLane::Microphone)?;
        mic.ingest(&mut [0.0; 1_920])?;
        assert!(
            mic.fifo.is_empty(),
            "invalid microphone startup reached the Mix FIFO"
        );
    }
    Ok(())
}

#[test]
fn mix_valid_zero_mic_and_out_of_range_system_controls_are_unchanged() -> Result<()> {
    for (kind, value) in [(MixLane::Microphone, 0.0), (MixLane::SystemAudio, 3.058)] {
        let mut input = lane(vec![value; 1_920], kind)?;
        input.ingest(&mut [0.0; 1_920])?;
        assert_eq!(input.fifo, vec![value; 1_920]);
        assert!(input.notices.poll().is_none());
    }
    Ok(())
}

#[test]
fn mix_invalid_startup_deadline_is_checked_without_more_input() -> Result<()> {
    let mut mic = lane(vec![3.058; 1_920], MixLane::Microphone)?;
    mic.ingest_at(&mut [0.0; 1_920], Duration::ZERO)?;
    let result = mic.ingest_at(&mut [0.0; 1_920], Duration::from_secs(2));
    let error = result.expect_err("invalid startup must time out even when the ring is empty");
    assert_eq!(error.kind(), ErrorKind::Backend);
    assert!(error.root().to_string().contains("2 seconds"));
    Ok(())
}

#[test]
fn mix_starvation_cannot_publish_system_audio_during_mic_confirmation() -> Result<()> {
    let mut mic = lane(vec![3.058; 1_920], MixLane::Microphone)?;
    mic.ingest(&mut [0.0; 1_920])?;
    mic.last_supply = Instant::now() - STARVATION_FILL_THRESHOLD;
    let mut system = lane(vec![0.2; 1_920], MixLane::SystemAudio)?;
    system.ingest(&mut [0.0; 1_920])?;
    let (producer, mut output) = raw_ring(RAW_RING_SAMPLES);
    let mut sink = RawSink::new(producer, SAMPLE_RATE, CHANNELS);
    assert!(
        !mix_and_push(
            &mut mic,
            &mut system,
            1.0,
            1.0,
            &mut DriftCorrection::new(),
            &mut sink,
            &mut Vec::new()
        ),
        "Mix must wait for confirmed microphone startup before starvation output"
    );
    assert_eq!(output.pop_slice(&mut [0.0; 1_920]), 0);
    Ok(())
}

#[test]
fn mix_startup_alignment_consumes_prefix_from_future_system_buffers() -> Result<()> {
    let mut input = vec![3.058; 1_920];
    input.extend_from_slice(&[0.25; 1_920]);
    let mut mic = lane(input, MixLane::Microphone)?;
    mic.ingest(&mut [0.0; 3_840])?;
    let mut system = lane(Vec::new(), MixLane::SystemAudio)?;
    let (producer, mut output) = raw_ring(RAW_RING_SAMPLES);
    let mut sink = RawSink::new(producer, SAMPLE_RATE, CHANNELS);
    let diagnostics = sink.diagnostics();
    let mut drift = DriftCorrection::new();
    let mut mixed = Vec::new();
    assert!(!mix_and_push(
        &mut mic,
        &mut system,
        1.0,
        1.0,
        &mut drift,
        &mut sink,
        &mut mixed
    ));
    assert!(diagnostics.take_startup_drop().is_none());
    system.fifo.extend_from_slice(&[0.1; 1_920]);
    assert!(
        !mix_and_push(
            &mut mic,
            &mut system,
            1.0,
            1.0,
            &mut drift,
            &mut sink,
            &mut mixed
        ),
        "the entire delayed system prefix belongs to the dropped microphone interval"
    );
    system.fifo.extend_from_slice(&[0.2; 1_920]);
    assert!(mix_and_push(
        &mut mic,
        &mut system,
        1.0,
        1.0,
        &mut drift,
        &mut sink,
        &mut mixed
    ));
    let mut samples = [0.0; 1_920];
    let got = output.pop_slice(&mut samples);
    assert!(got > 0);
    assert!(samples[..got]
        .iter()
        .all(|sample| (*sample - 0.45).abs() < 1e-6));
    assert_eq!(diagnostics.take_startup_drop(), Some((960, 48_000)));
    assert!(diagnostics.take_startup_drop().is_none());
    assert!(
        mic.notices.poll().is_none(),
        "successful startup must not duplicate the diagnostics event"
    );
    Ok(())
}

#[test]
fn mix_startup_alignment_counts_system_prefix_already_lost_to_fifo_cap() -> Result<()> {
    let mut input = vec![3.058; 1_920];
    input.extend_from_slice(&[0.25; 1_920]);
    let mut mic = lane(input, MixLane::Microphone)?;
    mic.ingest(&mut [0.0; 3_840])?;
    let mut system = lane(Vec::new(), MixLane::SystemAudio)?;
    let mut system_samples = vec![0.1; 1_920];
    system_samples.extend_from_slice(&vec![0.2; FIFO_MAX_SAMPLES]);
    system.normalizer.push(&system_samples, 0)?;
    system.append_output();
    assert_eq!(system.fifo.len(), FIFO_MAX_SAMPLES);
    let (producer, mut output) = raw_ring(RAW_RING_SAMPLES);
    let mut sink = RawSink::new(producer, SAMPLE_RATE, CHANNELS);
    assert!(mix_and_push(
        &mut mic,
        &mut system,
        1.0,
        1.0,
        &mut DriftCorrection::new(),
        &mut sink,
        &mut Vec::new()
    ));
    let mut samples = [0.0; 1_920];
    let got = output.pop_slice(&mut samples);
    assert!(got > 0);
    assert!(samples[..got]
        .iter()
        .all(|sample| (*sample - 0.45).abs() < 1e-6));
    assert_eq!(
        system.fifo.len(),
        FIFO_MAX_SAMPLES - 1_920,
        "the FIFO cap already removed the matching prefix; startup must not discard it twice"
    );
    Ok(())
}

#[test]
fn mix_stop_before_confirmation_reports_dropped_frames_once() -> Result<()> {
    let mut mic = lane(vec![3.058; 1_920], MixLane::Microphone)?;
    mic.ingest(&mut [0.0; 1_920])?;
    mic.finish(&mut [0.0; 1_920])?;
    assert!(
        matches!(
            mic.notices.poll(),
            Some(Event::StartupSamplesDropped { frames: 960, .. })
        ),
        "early stop must retain the startup report even without accepted microphone output"
    );
    mic.finish(&mut [0.0; 1_920])?;
    assert!(mic.notices.poll().is_none());
    assert!(mic.fifo.is_empty());
    Ok(())
}

#[test]
fn mix_stop_releases_valid_zero_tail_shorter_than_confirmation_window() -> Result<()> {
    let samples = vec![0.0; 26];
    let mut mic = lane(samples.clone(), MixLane::Microphone)?;
    mic.ingest(&mut [0.0; 26])?;
    assert!(
        mic.fifo.is_empty(),
        "short valid input should await confirmation until stop"
    );
    mic.finish(&mut [0.0; 26])?;
    assert!(
        mic.fifo.len() >= samples.len(),
        "stop discarded the wholly valid 13-frame microphone tail"
    );
    assert_eq!(
        &mic.fifo[..samples.len()],
        &samples,
        "stop must release the valid prefix even before ten milliseconds"
    );
    assert!(mic.notices.poll().is_none());
    let mut system = lane(vec![0.2; 26], MixLane::SystemAudio)?;
    system.ingest(&mut [0.0; 26])?;
    system.finish(&mut [0.0; 26])?;
    let (producer, mut output) = raw_ring(RAW_RING_SAMPLES);
    let mut sink = RawSink::new(producer, SAMPLE_RATE, CHANNELS);
    assert!(
        mix_and_push(
            &mut mic,
            &mut system,
            1.0,
            0.0,
            &mut DriftCorrection::new(),
            &mut sink,
            &mut Vec::new()
        ),
        "successful short EOF confirmation must open the Mix publication gate"
    );
    let mut delivered = [1.0; 1_920];
    let got = output.pop_slice(&mut delivered);
    assert!(got >= samples.len());
    assert!(delivered[..got].iter().all(|sample| *sample == 0.0));
    assert!(sink.diagnostics().take_startup_drop().is_none());
    Ok(())
}

#[test]
fn mix_stop_discards_unconfirmed_candidate_and_withholds_system_tail() -> Result<()> {
    let mut input = vec![f32::NAN; 1_920];
    input.extend_from_slice(&[0.0; 26]);
    let mut mic = lane(input, MixLane::Microphone)?;
    mic.ingest(&mut [0.0; 1_946])?;
    mic.finish(&mut [0.0; 1_946])?;
    assert!(
        matches!(
            mic.notices.poll(),
            Some(Event::StartupSamplesDropped { frames: 973, .. })
        ),
        "stop must count the discarded, unconfirmed 13-frame candidate as well"
    );
    let mut system = lane(vec![0.2; 1_920], MixLane::SystemAudio)?;
    system.ingest(&mut [0.0; 1_920])?;
    mic.last_supply = Instant::now() - STARVATION_FILL_THRESHOLD;
    let (producer, mut output) = raw_ring(RAW_RING_SAMPLES);
    let mut sink = RawSink::new(producer, SAMPLE_RATE, CHANNELS);
    assert!(
        !mix_and_push(
            &mut mic,
            &mut system,
            1.0,
            1.0,
            &mut DriftCorrection::new(),
            &mut sink,
            &mut Vec::new()
        ),
        "finishing an unconfirmed microphone must not allow system-only output"
    );
    assert_eq!(output.pop_slice(&mut [0.0; 1_920]), 0);
    mic.finish(&mut [0.0; 1_946])?;
    assert!(mic.notices.poll().is_none());
    assert!(mic.fifo.is_empty());
    Ok(())
}

#[test]
fn mix_mic_native_frames_split_between_buffers_remain_valid() -> Result<()> {
    struct Split(Arc<Mutex<Option<RawSink>>>);
    impl CaptureBackend for Split {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 2)
        }
        fn start(&mut self, mut sink: RawSink) -> Result<()> {
            sink.push(&[0.0], 0);
            *self.0.lock().expect("test sink lock") = Some(sink);
            Ok(())
        }
        fn stop(&mut self) {}
    }
    let input = Arc::new(Mutex::new(None));
    let mut backend: Box<dyn CaptureBackend> = Box::new(Split(input.clone()));
    let mut mic = start_child(
        &mut backend,
        MixLane::Microphone,
        Arc::new(Notices::default()),
    )?;
    assert!(
        mic.ingest(&mut [0.0; 1_920]).is_ok(),
        "a scalar split across callback buffers is incomplete only at EOF"
    );
    input
        .lock()
        .expect("test sink lock")
        .as_mut()
        .expect("started test sink")
        .push(&[0.0; 959], 0);
    mic.ingest(&mut [0.0; 1_920])?;
    mic.finish(&mut [0.0; 1_920])?;
    assert_eq!(mic.fifo, vec![0.0; 1_920]);
    assert!(mic.notices.poll().is_none());
    Ok(())
}

#[test]
fn mix_terminal_accounts_valid_pending_startup_once_without_pcm() -> Result<()> {
    let mut mic = lane(vec![0.25; 26], MixLane::Microphone)?;
    mic.ingest(&mut [0.0; 26])?;
    assert!(
        mic.fifo.is_empty(),
        "fixture must hold the valid startup prefix"
    );
    let notices = mic.notices.clone();
    let cause = Error::DeviceLost;
    notices.fail(cause.clone());
    let system = lane(Vec::new(), MixLane::SystemAudio)?;
    let (producer, mut output) = raw_ring(RAW_RING_SAMPLES);
    run_mixer(
        mic,
        system,
        1.0,
        1.0,
        RawSink::new(producer, SAMPLE_RATE, CHANNELS),
        Arc::new(AtomicBool::new(true)),
        notices.clone(),
    );
    assert_eq!(
        output.pop_slice(&mut [0.0; 1_920]),
        0,
        "terminal capture cannot deliver PCM"
    );
    assert_eq!(notices.report().primary(), Some(&cause));
    let mut losses = Vec::new();
    while let Some(event) = notices.poll() {
        match event {
            Event::AudioLoss { loss } => losses.push(loss),
            Event::StartupSamplesDropped { .. } => panic!("valid input is not out of range"),
            _ => {}
        }
    }
    assert_eq!(
        losses.len(),
        1,
        "terminal Mix silently discarded a valid startup prefix"
    );
    assert_eq!(losses[0].samples().map(NonZeroU64::get), Some(26));
    assert_eq!(
        losses[0].path(),
        AudioPath::Capture {
            lane: Some(MixLane::Microphone)
        }
    );
    assert_eq!(losses[0].reason(), LossReason::GenerationEnded);
    assert_eq!((losses[0].sample_rate(), losses[0].channels()), (48_000, 2));
    assert!(
        notices.poll().is_none(),
        "terminal startup loss is delivered once"
    );
    Ok(())
}

#[test]
fn mix_generation_ended_losses_coalesce_without_losing_native_attribution() -> Result<()> {
    let notices = Notices::default();
    for count in [26, 10] {
        notices.loss(
            AudioLoss::generation_ended(NonZeroU64::new(count), 44_100, 2)?
                .with_capture_lane(MixLane::Microphone)?,
        );
    }
    let Some(Event::AudioLoss { loss }) = notices.poll() else {
        panic!("expected coalesced generation-ended loss");
    };
    assert_eq!(loss.samples().map(NonZeroU64::get), Some(36));
    assert_eq!(
        loss.path(),
        AudioPath::Capture {
            lane: Some(MixLane::Microphone)
        }
    );
    assert_eq!(loss.reason(), LossReason::GenerationEnded);
    assert_eq!((loss.sample_rate(), loss.channels()), (44_100, 2));
    assert!(notices.poll().is_none());
    Ok(())
}

#[test]
fn mix_generation_ended_losses_with_different_lane_or_rate_are_distinct() -> Result<()> {
    let notices = Notices::default();
    for (rate, lane) in [
        (48_000, MixLane::Microphone),
        (44_100, MixLane::Microphone),
        (48_000, MixLane::SystemAudio),
    ] {
        notices.loss(
            AudioLoss::generation_ended(NonZeroU64::new(26), rate, 2)?.with_capture_lane(lane)?,
        );
    }
    assert!((0..3).all(|_| matches!(notices.poll(), Some(Event::AudioLoss { .. }))));
    assert!(notices.poll().is_none());
    Ok(())
}

#[test]
fn mix_repeated_terminal_discard_retains_noncanonical_native_format_once() -> Result<()> {
    let mut mic = native_lane(vec![0.25; 13], MixLane::Microphone, (44_100, 1))?;
    mic.ingest(&mut [0.0; 13])?;
    mic.reject_startup()?;
    mic.reject_startup()?;
    assert!(mic.fifo.is_empty());
    let Some(Event::AudioLoss { loss }) = mic.notices.poll() else {
        panic!("expected native startup discard report");
    };
    assert_eq!(loss.samples().map(NonZeroU64::get), Some(13));
    assert_eq!((loss.sample_rate(), loss.channels()), (44_100, 1));
    assert_eq!(
        loss.path(),
        AudioPath::Capture {
            lane: Some(MixLane::Microphone)
        }
    );
    assert_eq!(loss.reason(), LossReason::GenerationEnded);
    assert!(mic.notices.poll().is_none());
    Ok(())
}
