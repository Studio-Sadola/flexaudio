//! Shared failure, cleanup and configuration regression tests, without native devices.
use super::*;
use std::sync::atomic::AtomicUsize;

struct CheckedBackend {
    stops: Arc<AtomicUsize>,
    cleanup_failure: bool,
    samples: bool,
    diagnostics_only: bool,
    events: VecDeque<Event>,
    stop_error: Option<Error>,
    final_events: VecDeque<Event>,
}
impl CaptureBackend for CheckedBackend {
    fn native_format(&self) -> (u32, u16) {
        (48_000, 2)
    }
    fn start(&mut self, mut sink: RawSink) -> Result<()> {
        if self.samples {
            sink.push(&[0.25; 1920], 0);
        }
        if self.diagnostics_only {
            sink.diagnostics().record_corrupt_buffer(None);
        }
        Ok(())
    }
    fn stop(&mut self) {
        let _ = self.stop_checked();
    }
    fn stop_checked(&mut self) -> Result<()> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        self.events.append(&mut self.final_events);
        if let Some(error) = &self.stop_error {
            Err(error.clone())
        } else if self.cleanup_failure {
            Err(Error::Backend("owner cleanup failed".into()))
        } else {
            Ok(())
        }
    }
    fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }
}
fn backend() -> CheckedBackend {
    CheckedBackend {
        stops: Arc::new(AtomicUsize::new(0)),
        cleanup_failure: false,
        samples: true,
        diagnostics_only: false,
        events: VecDeque::new(),
        stop_error: None,
        final_events: VecDeque::new(),
    }
}

#[test]
fn chunk_ms_defaults_to_twenty_and_rejects_other_values_before_acquisition() {
    assert_eq!(StreamConfig::default().chunk_ms, 20);
    for chunk_ms in [0, 1, 10, 21, u32::MAX] {
        let config = StreamConfig {
            chunk_ms,
            ..Default::default()
        };
        assert!(matches!(
            Stream::open(config.clone(), Box::new(backend())),
            Err(Error::InvalidArg(_))
        ));
        assert!(matches!(crate::open(config), Err(Error::InvalidArg(_))));
    }
    assert!(Stream::open(StreamConfig::default(), Box::new(backend())).is_ok());
}
#[test]
fn repeated_checked_stop_retains_cleanup_and_valid_output_without_duplicate_events() {
    let mut backend = backend();
    backend.cleanup_failure = true;
    let stops = backend.stops.clone();
    let mut stream = Stream::open(StreamConfig::default(), Box::new(backend)).unwrap();
    assert!(stream.shutdown_report().is_none());
    stream.start().unwrap();
    let first = stream.stop_checked();
    assert!(first.is_err());
    assert_eq!(first, stream.stop_checked());
    stream.stop();
    assert_eq!(stops.load(Ordering::SeqCst), 1);
    assert!(stream.terminal_error().is_none());
    assert_eq!(stream.poll_chunk().unwrap().frames, 960);
    let report = stream.shutdown_report().unwrap();
    assert!(report.primary().is_none());
    assert_eq!(report.cleanup().len(), 1);
    assert!(
        matches!(stream.poll_event(), Some(Event::ShutdownError { error: Error::Context { context, .. } }) if context.operation() == Operation::Stop)
    );
    assert!(stream.poll_event().is_none());
}

#[test]
fn checked_backend_primary_is_never_recorded_as_cleanup() {
    for final_notice in [false, true] {
        let mut backend = backend();
        let primary = Error::DeviceLost.with_context(
            ErrorContext::new(Operation::Normalize).with_lane(flexaudio_core::MixLane::SystemAudio),
        );
        backend.stop_error = Some(primary.clone());
        let events = if final_notice {
            &mut backend.final_events
        } else {
            &mut backend.events
        };
        events.push_back(Event::TerminalError {
            error: primary.clone(),
        });
        let stops = backend.stops.clone();
        let mut stream = Stream::open(StreamConfig::default(), Box::new(backend)).unwrap();
        stream.start().unwrap();
        assert_eq!(stream.stop_checked(), Err(primary.clone()));
        assert_eq!(stream.stop_checked(), Err(primary.clone()));
        assert_eq!(stream.terminal_error(), Some(primary.clone()));
        let report = stream.shutdown_report().unwrap();
        assert_eq!(report.primary(), Some(&primary));
        assert!(report.cleanup().is_empty());
        assert_eq!(
            stream.poll_event(),
            Some(Event::TerminalError { error: primary })
        );
        assert!(stream.poll_event().is_none());
        assert!(stream.poll_chunk().is_none());
        assert_eq!(stops.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn backend_cleanup_return_and_notice_share_one_report_and_event() {
    for final_notice in [false, true] {
        let mut backend = backend();
        let cleanup = Error::Backend("owner cleanup failed".into()).with_context(
            ErrorContext::new(Operation::Join).with_lane(flexaudio_core::MixLane::Microphone),
        );
        backend.stop_error = Some(cleanup.clone());
        let events = if final_notice {
            &mut backend.final_events
        } else {
            &mut backend.events
        };
        events.push_back(Event::ShutdownError {
            error: cleanup.clone(),
        });
        let mut stream = Stream::open(StreamConfig::default(), Box::new(backend)).unwrap();
        stream.start().unwrap();
        let cleanup = cleanup.with_context(ErrorContext::new(Operation::Stop));
        assert_eq!(stream.stop_checked(), Err(cleanup.clone()));
        assert_eq!(stream.stop_checked(), Err(cleanup.clone()));
        let report = stream.shutdown_report().unwrap();
        assert!(report.primary().is_none());
        assert_eq!(report.cleanup(), std::slice::from_ref(&cleanup));
        assert!(stream.terminal_error().is_none());
        assert_eq!(
            stream.poll_event(),
            Some(Event::ShutdownError { error: cleanup })
        );
        assert!(stream.poll_event().is_none());
        assert!(stream.poll_chunk().is_some());
    }
}

#[test]
fn grouped_backend_stop_preserves_primary_and_each_distinct_cleanup_once() {
    let mut backend = backend();
    let primary = Error::DeviceLost.with_context(
        ErrorContext::new(Operation::Normalize).with_lane(flexaudio_core::MixLane::SystemAudio),
    );
    let cleanup: Vec<_> = [
        flexaudio_core::MixLane::Microphone,
        flexaudio_core::MixLane::SystemAudio,
    ]
    .into_iter()
    .map(|lane| {
        Error::Backend("owner cleanup failed".into())
            .with_context(ErrorContext::new(Operation::Join).with_lane(lane))
    })
    .collect();
    backend.stop_error = Some(Error::Multiple(ErrorGroup::new(
        primary.clone(),
        cleanup[0].clone(),
        vec![cleanup[1].clone()],
    )));
    backend.final_events.push_back(Event::TerminalError {
        error: primary.clone(),
    });
    for error in &cleanup {
        backend.final_events.push_back(Event::ShutdownError {
            error: error.clone(),
        });
    }
    let mut stream = Stream::open(StreamConfig::default(), Box::new(backend)).unwrap();
    stream.start().unwrap();
    let cleanup: Vec<_> = cleanup
        .into_iter()
        .map(|error| error.with_context(ErrorContext::new(Operation::Stop)))
        .collect();
    let expected = Error::Multiple(ErrorGroup::new(
        primary.clone(),
        cleanup[0].clone(),
        vec![cleanup[1].clone()],
    ));
    assert_eq!(stream.stop_checked(), Err(expected.clone()));
    assert_eq!(stream.stop_checked(), Err(expected));
    let report = stream.shutdown_report().unwrap();
    assert_eq!(report.primary(), Some(&primary));
    assert_eq!(report.cleanup(), cleanup);
    assert_eq!(
        stream.poll_event(),
        Some(Event::TerminalError { error: primary })
    );
    for error in cleanup {
        assert_eq!(stream.poll_event(), Some(Event::ShutdownError { error }));
    }
    assert!(stream.poll_event().is_none());
}

#[test]
fn backend_mix_loss_marks_next_delivered_chunk_on_each_tap_once() {
    struct LossBackend {
        loss: Option<AudioLoss>,
    }
    impl CaptureBackend for LossBackend {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 2)
        }
        fn start(&mut self, mut sink: RawSink) -> Result<()> {
            sink.push(&[0.25; 3840], 0);
            Ok(())
        }
        fn stop(&mut self) {}
        fn poll_event(&mut self) -> Option<Event> {
            self.loss.take().map(|loss| Event::AudioLoss { loss })
        }
    }
    for lane in [
        flexaudio_core::MixLane::Microphone,
        flexaudio_core::MixLane::SystemAudio,
    ] {
        for samples in [None, NonZeroU64::new(7)] {
            for loss in [
                AudioLoss::mix_fifo_overflow(lane, samples),
                AudioLoss::raw_overflow(Some(lane), samples, 48_000, 2).unwrap(),
            ] {
                let config = StreamConfig {
                    secondary_output: Some(OutputFormat::default()),
                    ..Default::default()
                };
                let mut stream =
                    Stream::open(config, Box::new(LossBackend { loss: Some(loss) })).unwrap();
                stream.enable_capture_tap().unwrap();
                // Schedule mailbox reconciliation before intake deterministically, including
                // an idle observation. No independent event/PCM ordering is assumed.
                Stream::open_backend_once(&stream.shared, GenerationChange::Initial).unwrap();
                assert_eq!(drain_backend_events(&stream.shared), MailboxDrain::Empty);
                assert_eq!(stream.poll_event(), Some(Event::AudioLoss { loss }));
                let primary = stream.shared.chunk_producer.lock().unwrap().take().unwrap();
                let secondary = stream.shared.secondary_producer.lock().unwrap().take();
                stream.shared.stopping.store(true, Ordering::SeqCst);
                run_intake(
                    stream.shared.clone(),
                    primary,
                    secondary,
                    (48_000, 2),
                    OutputFormat::default(),
                    Some(OutputFormat::default()),
                );
                assert!(stream
                    .poll_chunk()
                    .unwrap()
                    .flags
                    .contains(ChunkFlags::DISCONTINUITY));
                assert!(!stream
                    .poll_chunk()
                    .unwrap()
                    .flags
                    .contains(ChunkFlags::DISCONTINUITY));
                assert!(stream
                    .poll_secondary()
                    .unwrap()
                    .flags
                    .contains(ChunkFlags::DISCONTINUITY));
                assert!(!stream
                    .poll_secondary()
                    .unwrap()
                    .flags
                    .contains(ChunkFlags::DISCONTINUITY));
                assert!(stream
                    .poll_capture()
                    .unwrap()
                    .flags
                    .contains(ChunkFlags::DISCONTINUITY));
                assert!(!stream
                    .poll_capture()
                    .unwrap()
                    .flags
                    .contains(ChunkFlags::DISCONTINUITY));
                stream.stop_checked().unwrap();
                assert!(stream.poll_event().is_none());
            }
        }
    }
}
#[test]
fn legacy_failure_is_terminal_and_cleanup_does_not_replace_the_primary() {
    let mut backend = backend();
    backend.cleanup_failure = true;
    backend
        .events
        .push_back(Event::Error("legacy capture failure".into()));
    let stops = backend.stops.clone();
    let mut stream = Stream::open(StreamConfig::default(), Box::new(backend)).unwrap();
    stream.start().unwrap();
    let result = stream.stop_checked().unwrap_err();
    assert_eq!(
        stream.terminal_error(),
        Some(Error::Backend(
            "backend reported a failure through a legacy error event".into()
        ))
    );
    assert!(matches!(result, Error::Multiple(group) if group.secondary().count() == 1));
    assert!(stream.poll_chunk().is_none());
    assert_eq!(stops.load(Ordering::SeqCst), 1);
    assert_eq!(stream.shutdown_report().unwrap().cleanup().len(), 1);
}

#[test]
fn legacy_backend_payload_is_sanitized_across_terminal_and_shutdown_paths() {
    for (private, final_notice) in [
        (
            "private-device token-like-secret private-native-call",
            false,
        ),
        ("private-device token-like-secret private-native-call", true),
        ("reopen failed: No such device (os error 19)", false),
    ] {
        let mut backend = backend();
        let events = if final_notice {
            &mut backend.final_events
        } else {
            &mut backend.events
        };
        events.push_back(Event::Error(private.into()));
        let mut stream = Stream::open(StreamConfig::default(), Box::new(backend)).unwrap();
        stream.enable_capture_tap().unwrap();
        stream.start().unwrap();
        let result = stream.stop_checked().unwrap_err();
        assert_eq!(result.kind(), ErrorKind::Backend);
        let expected =
            Error::Backend("backend reported a failure through a legacy error event".into());
        assert_eq!(result, expected);
        assert_eq!(
            result.to_string(),
            "backend error: backend reported a failure through a legacy error event"
        );
        assert!(!result.to_string().contains(private));
        let terminal = stream.terminal_error().unwrap();
        assert_eq!(terminal.root(), &expected);
        assert_eq!(terminal.to_string(), expected.to_string());
        assert_eq!(stream.shutdown_report().unwrap().primary(), Some(&terminal));
        assert_eq!(
            stream.poll_event(),
            Some(Event::TerminalError { error: terminal })
        );
        assert!(stream.poll_event().is_none());
        assert!(stream.poll_chunk().is_none());
        assert!(stream.poll_capture().is_none());
        assert_eq!(stream.stop_checked().unwrap_err(), result);
    }
}

#[test]
fn typed_library_backend_errors_keep_explanations_and_os_error_text() {
    let explanation = "reopen failed: No such device (os error 19)";
    for terminal in [false, true] {
        for final_notice in [false, true] {
            let mut backend = backend();
            let error = Error::Backend(explanation.into())
                .with_context(ErrorContext::new(Operation::Reopen));
            let notice = if terminal {
                Event::TerminalError {
                    error: error.clone(),
                }
            } else {
                Event::RecoverableError {
                    error: error.clone(),
                }
            };
            let events = if final_notice {
                &mut backend.final_events
            } else {
                &mut backend.events
            };
            events.push_back(notice.clone());
            let mut stream = Stream::open(StreamConfig::default(), Box::new(backend)).unwrap();
            stream.start().unwrap();
            let result = stream.stop_checked();
            if terminal {
                assert_eq!(result, Err(error.clone()));
                assert_eq!(stream.terminal_error(), Some(error.clone()));
                assert!(stream.poll_chunk().is_none());
            } else {
                assert_eq!(result, Ok(()));
                assert!(stream.terminal_error().is_none());
                assert!(stream.poll_chunk().is_some());
            }
            assert_eq!(stream.poll_event(), Some(notice));
            assert!(stream.poll_event().is_none());
            assert_eq!(
                error.to_string(),
                "backend error: reopen failed: No such device (os error 19) during reopen"
            );
        }
    }
}

#[test]
fn delayed_capture_poll_reports_exact_loss_and_next_delivery_discontinuity() {
    struct ControlledBackend(Arc<Mutex<Option<RawSink>>>);
    impl CaptureBackend for ControlledBackend {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 2)
        }
        fn start(&mut self, sink: RawSink) -> Result<()> {
            *self.0.lock().unwrap() = Some(sink);
            Ok(())
        }
        fn stop(&mut self) {
            self.0.lock().unwrap().take();
        }
    }
    let sink = Arc::new(Mutex::new(None));
    let mut stream = Stream::open(
        StreamConfig {
            ring_capacity_chunks: 2,
            ..Default::default()
        },
        Box::new(ControlledBackend(sink.clone())),
    )
    .unwrap();
    stream.enable_capture_tap().unwrap();
    stream.start().unwrap();
    for seq in 0..4 {
        assert_eq!(
            sink.lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .push(&[0.25; 1920], 0),
            1920
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let primary = loop {
            if let Some(chunk) = stream.poll_chunk() {
                break chunk;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "intake did not publish primary PCM"
            );
            thread::sleep(Duration::from_millis(1));
        };
        assert_eq!(primary.seq, seq);
        assert_eq!(primary.dropped_before, 0);
    }
    stream.stop_checked().unwrap();
    assert_eq!(
        stream.dropped_chunks(),
        0,
        "canonical loss is independent of primary drops"
    );
    let mut lost_samples = 0;
    let mut notices = 0;
    while let Some(event) = stream.poll_event() {
        let Event::AudioLoss { loss } = event else {
            panic!("unexpected event: {event:?}");
        };
        assert_eq!(
            loss.path(),
            flexaudio_core::AudioPath::Capture { lane: None }
        );
        assert_eq!(loss.reason(), flexaudio_core::LossReason::RawOverflow);
        assert_eq!((loss.sample_rate(), loss.channels()), (48_000, 2));
        lost_samples += loss.samples().unwrap().get();
        notices += 1;
    }
    assert_eq!((notices, lost_samples), (2, 3840));
    let first = stream.poll_capture().unwrap();
    assert_eq!(first.frame_index, 1920);
    assert!(first.flags.contains(ChunkFlags::DISCONTINUITY));
    let second = stream.poll_capture().unwrap();
    assert_eq!(second.frame_index, 2880);
    assert!(!second.flags.contains(ChunkFlags::DISCONTINUITY));
    assert!(stream.poll_capture().is_none());
    stream.stop_checked().unwrap();
    assert!(stream.poll_event().is_none());
}
#[test]
fn final_capture_loss_without_following_pcm_is_reported_once() {
    let mut backend = backend();
    backend.samples = false;
    backend.diagnostics_only = true;
    let mut stream = Stream::open(StreamConfig::default(), Box::new(backend)).unwrap();
    stream.start().unwrap();
    stream.stop_checked().unwrap();
    assert!(stream.poll_chunk().is_none());
    let loss = match stream.poll_event().unwrap() {
        Event::AudioLoss { loss } => loss,
        event => panic!("expected loss, got {event:?}"),
    };
    assert_eq!(loss.reason(), flexaudio_core::LossReason::CorruptBuffer);
    assert_eq!(
        loss.path(),
        flexaudio_core::AudioPath::Capture { lane: None }
    );
    assert_eq!(
        (loss.sample_rate(), loss.channels(), loss.samples()),
        (48_000, 2, None)
    );
    assert!(stream.poll_event().is_none());
    stream.stop_checked().unwrap();
    assert!(stream.poll_event().is_none());
}
#[test]
fn wrapped_permission_denial_remains_terminal() {
    for advisory in [false, true] {
        let mut backend = backend();
        let error = Error::PermissionDenied {
            permission: Permission::Microphone,
            detail: "restricted".into(),
        }
        .with_context(ErrorContext::new(Operation::Start));
        backend.events.push_back(if advisory {
            Event::RecoverableError {
                error: error.clone(),
            }
        } else {
            Event::TerminalError {
                error: error.clone(),
            }
        });
        let mut stream = Stream::open(StreamConfig::default(), Box::new(backend)).unwrap();
        stream.start().unwrap();
        assert_eq!(stream.stop_checked(), Err(error.clone()));
        assert_eq!(stream.terminal_error(), Some(error));
        assert!(stream.poll_chunk().is_none());
    }
}

#[test]
fn switched_generation_native_format_change_is_terminal_without_rollback() {
    struct Changed {
        stops: Arc<AtomicUsize>,
        error: Error,
    }
    impl CaptureBackend for Changed {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 2)
        }
        fn start(&mut self, _sink: RawSink) -> Result<()> {
            Err(self.error.clone())
        }
        fn stop(&mut self) {
            self.stops.fetch_add(1, Ordering::SeqCst);
        }
    }
    let old = backend();
    let old_stops = old.stops.clone();
    let new_stops = Arc::new(AtomicUsize::new(0));
    let error = Error::NativeFormatChanged {
        advertised: (48_000, 2),
        actual: (44_100, 2),
    }
    .with_context(ErrorContext::new(Operation::Start));
    let mut stream = Stream::open(StreamConfig::default(), Box::new(old)).unwrap();
    stream.start().unwrap();
    assert_eq!(
        stream.switch_backend(Box::new(Changed {
            stops: new_stops.clone(),
            error: error.clone(),
        })),
        Err(error.clone())
    );
    assert_eq!(stream.terminal_error(), Some(error.clone()));
    assert_eq!(stream.stop_checked(), Err(error));
    assert_eq!(old_stops.load(Ordering::SeqCst), 1);
    assert_eq!(new_stops.load(Ordering::SeqCst), 1);
    assert!(stream.poll_chunk().is_none());
    assert!(stream.poll_capture().is_none());
}

#[test]
fn raw_overflow_without_intake_is_exact_and_final_drain_is_idempotent() {
    let mut backend = backend();
    backend.samples = false;
    let mut stream = Stream::open(StreamConfig::default(), Box::new(backend)).unwrap();
    let (producer, consumer) = raw_ring(2);
    let mut sink = RawSink::new(producer, 48_000, 2);
    assert_eq!(sink.push(&[0.25; 5], 0), 2);
    *stream.shared.raw_diagnostics.lock().unwrap() = Some(sink.diagnostics());
    *stream.shared.raw_consumer.lock().unwrap() = Some(consumer);
    drop(sink);
    stream.stop_checked().unwrap();
    let loss = match stream.poll_event().unwrap() {
        Event::AudioLoss { loss } => loss,
        event => panic!("expected loss, got {event:?}"),
    };
    assert_eq!(loss.reason(), flexaudio_core::LossReason::RawOverflow);
    assert_eq!(
        loss.samples().unwrap().get(),
        3,
        "legacy partial writes count scalar samples, including odd counts"
    );
    assert!(stream.poll_chunk().is_none());
    stream.stop_checked().unwrap();
    assert!(stream.poll_event().is_none());
}

#[test]
fn unsuccessful_start_preserves_previous_outcome_and_successful_retry_clears_it() {
    struct Retry {
        starts: usize,
        stops: usize,
    }
    impl CaptureBackend for Retry {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 2)
        }
        fn start(&mut self, _: RawSink) -> Result<()> {
            self.starts += 1;
            if self.starts < 3 {
                Err(Error::Backend("startup failure".into()))
            } else {
                Ok(())
            }
        }
        fn stop(&mut self) {
            let _ = self.stop_checked();
        }
        fn stop_checked(&mut self) -> Result<()> {
            self.stops += 1;
            if self.stops == 1 {
                Err(Error::Backend("first owner cleanup failure".into()))
            } else {
                Ok(())
            }
        }
    }
    let mut stream = Stream::open(
        StreamConfig::default(),
        Box::new(Retry {
            starts: 0,
            stops: 0,
        }),
    )
    .unwrap();
    assert!(matches!(stream.start(), Err(Error::Multiple(_))));
    let previous = stream.shutdown_report().unwrap();
    assert_eq!(previous.cleanup().len(), 1);
    assert!(
        matches!(stream.start(), Err(Error::Backend(_))),
        "old cleanup must not be reattached to an unrelated retry failure"
    );
    assert_eq!(stream.shutdown_report(), Some(previous));
    stream.start().unwrap();
    assert!(stream.shutdown_report().is_none());
    stream.stop_checked().unwrap();
    assert!(stream.shutdown_report().unwrap().cleanup().is_empty());
    assert!(stream.poll_event().is_none());
}

#[test]
fn failed_start_drains_realtime_rejections_without_pcm() {
    struct Failed;
    impl CaptureBackend for Failed {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 2)
        }
        fn start(&mut self, sink: RawSink) -> Result<()> {
            sink.diagnostics().record_callback_rejected(None);
            Err(Error::Backend("startup failed".into()))
        }
        fn stop(&mut self) {}
    }
    let mut stream = Stream::open(StreamConfig::default(), Box::new(Failed)).unwrap();
    assert!(stream.start().is_err());
    assert!(
        matches!(stream.poll_event(), Some(Event::AudioLoss { loss }) if loss.reason() == flexaudio_core::LossReason::CallbackRejected && loss.samples().is_none())
    );
    assert!(stream.poll_event().is_none());
    assert!(stream.poll_chunk().is_none());
}
#[test]
fn clamp_flags_ignore_nan_and_preserve_unity_gain_passthrough() {
    let mut samples = [f32::NAN, f32::INFINITY, 0.5];
    assert!(!apply_gain(&mut samples, 1.0));
    assert!(samples[0].is_nan());
    assert_eq!(samples[1], f32::INFINITY);
    let mut samples = [f32::NAN, 0.25];
    assert!(!apply_gain(&mut samples, 2.0));
    assert_eq!(samples[1], 0.5);
    let mut samples = [-0.8, 0.8];
    assert!(apply_gain(&mut samples, 2.0));
    assert_eq!(samples, [-1.0, 1.0]);
}
