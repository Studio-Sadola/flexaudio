//! Shared failure, cleanup and configuration regression tests, without native devices.
use super::*;
use std::sync::atomic::AtomicUsize;

struct CheckedBackend {
    stops: Arc<AtomicUsize>,
    cleanup_failure: bool,
    samples: bool,
    diagnostics_only: bool,
    events: VecDeque<Event>,
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
        if self.cleanup_failure {
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
        Some(Error::Backend("legacy capture failure".into()))
    );
    assert!(matches!(result, Error::Multiple(group) if group.secondary().count() == 1));
    assert!(stream.poll_chunk().is_none());
    assert_eq!(stops.load(Ordering::SeqCst), 1);
    assert_eq!(stream.shutdown_report().unwrap().cleanup().len(), 1);
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
