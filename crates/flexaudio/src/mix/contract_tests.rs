//! Checked Mix lifecycle and reporting regression tests, without native devices.
use super::*;
use std::collections::VecDeque;
use std::sync::atomic::AtomicUsize;

struct Child {
    events: VecDeque<Event>,
    stops: Arc<AtomicUsize>,
    sink: Option<RawSink>,
    start_samples: usize,
    final_samples: usize,
    stop_error: bool,
    start_error: bool,
    rate: u32,
    final_event: Option<Event>,
}
impl Child {
    fn new(events: Vec<Event>) -> Self {
        Self {
            events: events.into(),
            stops: Arc::new(AtomicUsize::new(0)),
            sink: None,
            start_samples: 0,
            final_samples: 0,
            stop_error: false,
            start_error: false,
            rate: SAMPLE_RATE,
            final_event: None,
        }
    }
}
impl CaptureBackend for Child {
    fn native_format(&self) -> (u32, u16) {
        (self.rate, CHANNELS)
    }
    fn start(&mut self, mut sink: RawSink) -> Result<()> {
        sink.push(&vec![0.25; self.start_samples], 0);
        self.sink = Some(sink);
        if self.start_error {
            Err(Error::DeviceNotFound)
        } else {
            Ok(())
        }
    }
    fn stop(&mut self) {
        panic!("Mix must use checked stop");
    }
    fn stop_checked(&mut self) -> Result<()> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        if let Some(mut sink) = self.sink.take() {
            sink.push(&vec![0.25; self.final_samples], 0);
            // Final loss is observable even after a terminal failure suppresses PCM.
            if self.final_samples > 0 {
                sink.diagnostics().record_corrupt_buffer(None);
            }
        }
        if let Some(event) = self.final_event.take() {
            self.events.push_back(event);
        }
        if self.stop_error {
            Err(Error::Backend("injected checked stop failure".into()))
        } else {
            Ok(())
        }
    }
    fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }
}
fn terminal() -> Event {
    Event::TerminalError {
        error: Error::DeviceLost,
    }
}
fn context_lane(error: &Error) -> Option<MixLane> {
    match error {
        Error::Context { context, source } => context.lane().or_else(|| context_lane(source)),
        Error::Multiple(group) => context_lane(group.primary()),
        _ => None,
    }
}
fn fatal_lane(lane: MixLane) {
    let mic = Child::new(if lane == MixLane::Microphone {
        vec![terminal()]
    } else {
        vec![]
    });
    let system = Child::new(if lane == MixLane::SystemAudio {
        vec![terminal()]
    } else {
        vec![]
    });
    let mic_stops = mic.stops.clone();
    let system_stops = system.stops.clone();
    let mut stream = crate::Stream::open(
        Default::default(),
        Box::new(CompositeBackend::new(
            Box::new(mic),
            Box::new(system),
            1.0,
            1.0,
        )),
    )
    .unwrap();
    stream.start().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let error = loop {
        if let Some(error) = stream.terminal_error() {
            break error;
        }
        assert!(Instant::now() < deadline, "fatal lane was not reconciled");
        thread::sleep(IDLE_SLEEP);
    };
    assert_eq!(error.kind(), ErrorKind::DeviceLost);
    assert_eq!(context_lane(&error), Some(lane));
    assert_eq!(mic_stops.load(Ordering::SeqCst), 1);
    assert_eq!(system_stops.load(Ordering::SeqCst), 1);
    assert!(stream.poll_chunk().is_none());
    assert_eq!(
        stream.stop_checked().unwrap_err().kind(),
        ErrorKind::DeviceLost
    );
    assert!(stream.shutdown_report().unwrap().cleanup().is_empty());
    // terminal_error() reads the retained cause without consuming its one event.
    assert_eq!(stream.poll_event(), Some(Event::TerminalError { error }));
    assert!(stream.poll_event().is_none());
}
#[test]
fn mic_fatal_stops_both_lanes_with_context() {
    fatal_lane(MixLane::Microphone);
}
#[test]
fn system_fatal_stops_both_lanes_with_context() {
    fatal_lane(MixLane::SystemAudio);
}

fn start(mix: &mut CompositeBackend) -> RawConsumer {
    let (producer, consumer) = raw_ring(RAW_RING_SAMPLES * 2);
    mix.start(RawSink::new(producer, SAMPLE_RATE, CHANNELS))
        .unwrap();
    consumer
}
fn events(mix: &mut CompositeBackend) -> Vec<Event> {
    let mut events = Vec::new();
    for _ in 0..FINAL_EVENT_BUDGET {
        let Some(event) = mix.poll_event() else {
            return events;
        };
        events.push(event);
    }
    panic!("Mix event drain exceeded test budget");
}

#[test]
fn checked_shutdown_retains_both_child_errors_once() {
    let mut mic = Child::new(vec![]);
    mic.stop_error = true;
    mic.final_samples = 1920;
    let mut system = Child::new(vec![]);
    system.stop_error = true;
    system.final_samples = 1920;
    let mic_stops = mic.stops.clone();
    let system_stops = system.stops.clone();
    let mut mix = CompositeBackend::new(Box::new(mic), Box::new(system), 1.0, 1.0);
    let mut consumer = start(&mut mix);
    let result = mix.stop_checked();
    assert!(result.is_err());
    let report = mix.shutdown.as_ref().unwrap();
    assert!(report.primary().is_none());
    assert_eq!(report.cleanup().len(), 2);
    assert_eq!(
        context_lane(&report.cleanup()[0]),
        Some(MixLane::Microphone)
    );
    assert_eq!(
        context_lane(&report.cleanup()[1]),
        Some(MixLane::SystemAudio)
    );
    assert!(consumer.pop_slice(&mut vec![0.0; RAW_RING_SAMPLES]) > 0);
    assert_eq!(
        events(&mut mix)
            .iter()
            .filter(|event| matches!(event, Event::ShutdownError { .. }))
            .count(),
        2
    );
    assert_eq!(mix.stop_checked(), result);
    mix.stop();
    assert!(mix.poll_event().is_none());
    assert_eq!(mic_stops.load(Ordering::SeqCst), 1);
    assert_eq!(system_stops.load(Ordering::SeqCst), 1);
}

#[test]
fn facade_checked_shutdown_retains_both_child_errors_once() {
    let mut mic = Child::new(vec![]);
    mic.stop_error = true;
    let mut system = Child::new(vec![]);
    system.stop_error = true;
    let mut stream = crate::Stream::open(
        Default::default(),
        Box::new(CompositeBackend::new(
            Box::new(mic),
            Box::new(system),
            1.0,
            1.0,
        )),
    )
    .unwrap();
    stream.start().unwrap();
    let result = stream.stop_checked();
    assert!(matches!(&result, Err(Error::Multiple(group)) if group.secondary().count() == 1));
    assert_eq!(stream.stop_checked(), result);
    let report = stream.shutdown_report().unwrap();
    assert!(report.primary().is_none());
    assert_eq!(report.cleanup().len(), 2);
    let errors: Vec<_> = std::iter::from_fn(|| stream.poll_event())
        .filter_map(|event| match event {
            Event::ShutdownError { error } => Some(error),
            _ => None,
        })
        .collect();
    assert_eq!(errors, report.cleanup());
    assert_eq!(context_lane(&errors[0]), Some(MixLane::Microphone));
    assert_eq!(context_lane(&errors[1]), Some(MixLane::SystemAudio));
}

#[test]
fn terminal_suppresses_tails_and_retains_final_losses_and_cleanup() {
    let mut mic = Child::new(vec![terminal()]);
    mic.final_samples = RAW_RING_SAMPLES + 7;
    mic.stop_error = true;
    let mut system = Child::new(vec![]);
    system.final_samples = RAW_RING_SAMPLES + 3;
    system.stop_error = true;
    let mut mix = CompositeBackend::new(Box::new(mic), Box::new(system), 1.0, 1.0);
    let mut consumer = start(&mut mix);
    assert!(matches!(
        mix.poll_event(),
        Some(Event::TerminalError { .. })
    ));
    assert!(mix.stop_checked().is_err());
    assert_eq!(consumer.pop_slice(&mut [0.0; 1920]), 0);
    let report = mix.shutdown.as_ref().unwrap();
    assert_eq!(report.primary().unwrap().kind(), ErrorKind::DeviceLost);
    assert_eq!(report.cleanup().len(), 2);
    let losses: Vec<_> = events(&mut mix)
        .into_iter()
        .filter_map(|event| match event {
            Event::AudioLoss { loss } => Some(loss),
            _ => None,
        })
        .collect();
    for (lane, count) in [(MixLane::Microphone, 7), (MixLane::SystemAudio, 3)] {
        let raw = losses
            .iter()
            .find(|loss| {
                loss.path() == AudioPath::Capture { lane: Some(lane) }
                    && loss.reason() == LossReason::RawOverflow
            })
            .unwrap();
        assert_eq!(raw.samples().unwrap().get(), count);
        assert_eq!((raw.sample_rate(), raw.channels()), (SAMPLE_RATE, CHANNELS));
        assert!(losses.iter().any(
            |loss| loss.path() == AudioPath::Capture { lane: Some(lane) }
                && loss.reason() == LossReason::CorruptBuffer
                && loss.samples().is_none()
        ));
    }
    assert!(mix.poll_event().is_none());
}

#[test]
fn mix_forwards_mic_grant_once_and_rejects_system_grants() {
    let pending = Event::PermissionPending {
        permission: flexaudio_core::Permission::Microphone,
        detail: "microphone consent is pending".into(),
    };
    let mic = Child::new(vec![
        pending.clone(),
        Event::PermissionGranted,
        Event::PermissionGranted,
    ]);
    let system = Child::new(vec![Event::PermissionGranted, Event::PermissionGranted]);
    let mut mix = CompositeBackend::new(Box::new(mic), Box::new(system), 1.0, 1.0);
    start(&mut mix);
    let forwarded = events(&mut mix);
    assert_eq!(forwarded, vec![pending, Event::PermissionGranted]);
    assert!(mix.shutdown.is_none());
    assert!(mix.stop_checked().is_ok());
}

#[test]
fn startup_rollback_retains_primary_and_checked_cleanup() {
    let mut mic = Child::new(vec![]);
    mic.stop_error = true;
    let mut system = Child::new(vec![]);
    system.start_error = true;
    system.stop_error = true;
    let mic_stops = mic.stops.clone();
    let system_stops = system.stops.clone();
    let mut mix = CompositeBackend::new(Box::new(mic), Box::new(system), 1.0, 1.0);
    let (producer, _) = raw_ring(RAW_RING_SAMPLES);
    let error = mix
        .start(RawSink::new(producer, SAMPLE_RATE, CHANNELS))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::DeviceNotFound);
    assert_eq!(context_lane(&error), Some(MixLane::SystemAudio));
    assert_eq!(mix.shutdown.as_ref().unwrap().cleanup().len(), 2);
    assert_eq!(mic_stops.load(Ordering::SeqCst), 1);
    assert_eq!(system_stops.load(Ordering::SeqCst), 1);
    assert_eq!(mix.stop_checked().unwrap_err(), error);
}

#[test]
fn graceful_stop_flushes_short_resampled_tails() {
    let mut mic = Child::new(vec![]);
    mic.rate = 44_100;
    mic.final_samples = 200;
    let mut system = Child::new(vec![]);
    system.rate = 44_100;
    system.final_samples = 200;
    let mut mix = CompositeBackend::new(Box::new(mic), Box::new(system), 1.0, 1.0);
    let mut consumer = start(&mut mix);
    mix.stop_checked().unwrap();
    let mut pcm = vec![0.0; RAW_RING_SAMPLES];
    let got = consumer.pop_slice(&mut pcm);
    assert!(
        got >= 1920,
        "short child tails must produce a padded canonical chunk"
    );
    assert!(pcm[..got].iter().any(|sample| *sample != 0.0));
    assert!(mix.stop_checked().is_ok());
    assert_eq!(consumer.pop_slice(&mut pcm), 0);
}

#[test]
fn legacy_failure_is_terminal_with_lane_and_safe_message() {
    let system = Child::new(vec![Event::Error(
        "private device /private/user/name".into(),
    )]);
    let mut mix = CompositeBackend::new(Box::new(Child::new(vec![])), Box::new(system), 1.0, 1.0);
    start(&mut mix);
    let Some(Event::TerminalError { error }) = mix.poll_event() else {
        panic!("legacy failure must be terminal");
    };
    assert_eq!(error.kind(), ErrorKind::Backend);
    assert_eq!(context_lane(&error), Some(MixLane::SystemAudio));
    assert_eq!(
        error.to_string(),
        "backend error: legacy mix child failure during normalize (system audio)"
    );
    assert!(mix.stop_checked().is_err());
}

#[test]
fn failed_restart_preserves_previous_shutdown_report_and_events() {
    struct FailsSecondStart {
        attempts: usize,
        stops: Arc<AtomicUsize>,
    }
    impl CaptureBackend for FailsSecondStart {
        fn native_format(&self) -> (u32, u16) {
            (SAMPLE_RATE, CHANNELS)
        }
        fn start(&mut self, _: RawSink) -> Result<()> {
            self.attempts += 1;
            if self.attempts == 2 {
                Err(Error::DeviceNotFound)
            } else {
                Ok(())
            }
        }
        fn stop(&mut self) {
            self.stops.fetch_add(1, Ordering::SeqCst);
        }
    }
    let stops = Arc::new(AtomicUsize::new(0));
    let mut mix = CompositeBackend::new(
        Box::new(FailsSecondStart {
            attempts: 0,
            stops: stops.clone(),
        }),
        Box::new(Child::new(vec![])),
        1.0,
        1.0,
    );
    start(&mut mix);
    mix.stop_checked().unwrap();
    let report = mix.shutdown.clone();
    mix.notices.queue_final(Event::PermissionPending {
        permission: flexaudio_core::Permission::Microphone,
        detail: "previous pending notice".into(),
    });
    let (producer, _) = raw_ring(RAW_RING_SAMPLES);
    assert_eq!(
        mix.start(RawSink::new(producer, SAMPLE_RATE, CHANNELS))
            .unwrap_err()
            .kind(),
        ErrorKind::DeviceNotFound
    );
    assert_eq!(mix.shutdown, report);
    assert_eq!(stops.load(Ordering::SeqCst), 2);
    assert!(matches!(
        mix.poll_event(),
        Some(Event::PermissionPending { .. })
    ));
    assert!(mix.stop_checked().is_ok());
}

#[test]
fn loss_notices_coalesce_intervals_and_keep_unknown_counts() {
    let notices = Notices::default();
    for count in [2, 3] {
        notices.loss(
            AudioLoss::raw_overflow(
                Some(MixLane::SystemAudio),
                NonZeroU64::new(count),
                44_100,
                1,
            )
            .unwrap(),
        );
    }
    let Some(Event::AudioLoss { loss }) = notices.poll() else {
        panic!("expected raw loss");
    };
    assert_eq!(loss.samples().unwrap().get(), 5);
    assert_eq!((loss.sample_rate(), loss.channels()), (44_100, 1));
    assert!(notices.poll().is_none());
    for count in [Some(7), None, Some(11)] {
        notices.loss(AudioLoss::mix_fifo_overflow(
            MixLane::Microphone,
            count.and_then(NonZeroU64::new),
        ));
    }
    let Some(Event::AudioLoss { loss }) = notices.poll() else {
        panic!("expected FIFO loss");
    };
    assert!(loss.samples().is_none());
    assert!(notices.poll().is_none());
    notices.clipped();
    notices.clipped();
    assert_eq!(notices.poll(), Some(Event::Clipped));
    assert!(notices.poll().is_none());
}

#[test]
fn flush_errors_from_both_lanes_are_retained_with_context() {
    let mut mic = Child::new(vec![]);
    mic.final_samples = 1;
    let mut system = Child::new(vec![]);
    system.final_samples = 1;
    let mut mix = CompositeBackend::new(Box::new(mic), Box::new(system), 1.0, 1.0);
    let mut consumer = start(&mut mix);
    assert!(mix.stop_checked().is_err());
    let report = mix.shutdown.as_ref().unwrap();
    assert!(report.primary().is_none());
    assert_eq!(report.cleanup().len(), 2);
    for (error, lane) in report
        .cleanup()
        .iter()
        .zip([MixLane::Microphone, MixLane::SystemAudio])
    {
        assert!(matches!(error, Error::Context { context, .. }
            if context.operation() == Operation::Flush && context.lane() == Some(lane)));
    }
    assert_eq!(consumer.pop_slice(&mut [0.0; 1920]), 0);
}

#[test]
fn final_fatal_events_gate_stream_tails_from_either_lane() {
    for lane in [MixLane::Microphone, MixLane::SystemAudio] {
        let mut mic = Child::new(vec![]);
        mic.final_samples = 1920;
        let mut system = Child::new(vec![]);
        system.final_samples = 1920;
        if lane == MixLane::Microphone {
            mic.final_event = Some(terminal());
        } else {
            system.final_event = Some(terminal());
        }
        let mic_stops = mic.stops.clone();
        let system_stops = system.stops.clone();
        let mut stream = crate::Stream::open(
            Default::default(),
            Box::new(CompositeBackend::new(
                Box::new(mic),
                Box::new(system),
                1.0,
                1.0,
            )),
        )
        .unwrap();
        stream.start().unwrap();
        assert_eq!(
            stream.stop_checked().unwrap_err().kind(),
            ErrorKind::DeviceLost
        );
        assert_eq!(context_lane(&stream.terminal_error().unwrap()), Some(lane));
        assert!(stream.poll_chunk().is_none());
        assert_eq!(mic_stops.load(Ordering::SeqCst), 1);
        assert_eq!(system_stops.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn normalizer_failure_is_terminal_and_stops_both_lanes() {
    struct Panics;
    impl flexaudio_core::normalizer::InnerProcessor for Panics {
        fn process(&mut self, _: &mut [f32]) {
            panic!("injected processor failure");
        }
        fn flush(&mut self) -> Vec<f32> {
            Vec::new()
        }
    }
    for failing in [MixLane::Microphone, MixLane::SystemAudio] {
        let mut mic = Child::new(vec![]);
        mic.start_samples = 1920;
        let mut system = Child::new(vec![]);
        system.start_samples = 1920;
        let mic_stops = mic.stops.clone();
        let system_stops = system.stops.clone();
        let mut mix = CompositeBackend::new(Box::new(mic), Box::new(system), 1.0, 1.0);
        let mut mic = start_child(&mut mix.mic, MixLane::Microphone, mix.notices.clone()).unwrap();
        mix.mic_owned = true;
        let mut system =
            start_child(&mut mix.system, MixLane::SystemAudio, mix.notices.clone()).unwrap();
        mix.system_owned = true;
        if failing == MixLane::Microphone {
            mic.normalizer = mic.normalizer.with_inner_processor(Box::new(Panics));
        } else {
            system.normalizer = system.normalizer.with_inner_processor(Box::new(Panics));
        }
        let stopping = mix.stopping.clone();
        let notices = mix.notices.clone();
        let (producer, mut consumer) = raw_ring(RAW_RING_SAMPLES);
        let sink = RawSink::new(producer, SAMPLE_RATE, CHANNELS);
        mix.mixer = Some(thread::spawn(move || {
            run_mixer(mic, system, 1.0, 1.0, sink, stopping, notices)
        }));
        let deadline = Instant::now() + Duration::from_secs(3);
        let error = loop {
            if let Some(Event::TerminalError { error }) = mix.poll_event() {
                break error;
            }
            assert!(
                Instant::now() < deadline,
                "normalizer failure was not observable"
            );
            thread::sleep(IDLE_SLEEP);
        };
        assert_eq!(context_lane(&error), Some(failing));
        assert_eq!(error.kind(), ErrorKind::Backend);
        assert!(mix.stop_checked().is_err());
        assert_eq!(mic_stops.load(Ordering::SeqCst), 1);
        assert_eq!(system_stops.load(Ordering::SeqCst), 1);
        assert_eq!(consumer.pop_slice(&mut [0.0; 1920]), 0);
    }
}
