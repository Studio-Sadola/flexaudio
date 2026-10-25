//! Device-free end-to-end helper forwarding across generation boundaries.
use super::*;
use flexaudio_core::HelperStopReason;

struct HelperBackend {
    capture: crate::MockBackend,
    events: VecDeque<Event>,
    owned: bool,
    fail_start: bool,
    reason: HelperStopReason,
}

impl HelperBackend {
    fn boxed(fail_start: bool) -> Box<dyn CaptureBackend> {
        Box::new(Self {
            capture: crate::MockBackend::new(48_000, 2, 440.0),
            events: VecDeque::new(),
            owned: false,
            fail_start,
            reason: HelperStopReason::StreamStopped,
        })
    }
}

impl CaptureBackend for HelperBackend {
    fn native_format(&self) -> (u32, u16) {
        (48_000, 2)
    }
    fn start(&mut self, sink: RawSink) -> Result<()> {
        self.reason = HelperStopReason::StreamStopped;
        self.events
            .push_back(Event::ClassicLoopbackKeepaliveStarted);
        self.owned = true;
        if self.fail_start {
            self.fail_start = false;
            self.reason = HelperStopReason::StartupFailed;
            self.stop();
            return Err(Error::DeviceNotFound);
        }
        self.capture.start(sink)
    }
    fn stop(&mut self) {
        self.capture.stop();
        if std::mem::take(&mut self.owned) {
            self.events
                .push_back(Event::ClassicLoopbackKeepaliveStopped {
                    reason: self.reason,
                });
        }
    }
    fn set_helper_stop_reason(&mut self, reason: HelperStopReason) {
        self.reason = reason;
    }
    fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }
}

fn stopped(reason: HelperStopReason) -> Event {
    Event::ClassicLoopbackKeepaliveStopped { reason }
}

fn collect(stream: &mut Stream) -> Vec<Event> {
    std::iter::from_fn(|| stream.poll_event())
        .filter(is_helper_lifecycle)
        .collect()
}

#[test]
fn lifecycle_survives_switch_and_repeated_stop_in_fifo_order() {
    let mut stream = Stream::open(StreamConfig::default(), HelperBackend::boxed(false)).unwrap();
    stream.start().unwrap();
    stream.switch_backend(HelperBackend::boxed(false)).unwrap();
    stream.stop_checked().unwrap();
    stream.stop_checked().unwrap();
    assert_eq!(
        collect(&mut stream),
        [
            Event::ClassicLoopbackKeepaliveStarted,
            stopped(HelperStopReason::SourceSwitched),
            Event::ClassicLoopbackKeepaliveStarted,
            stopped(HelperStopReason::StreamStopped)
        ]
    );
}

#[test]
fn failed_replacement_pair_precedes_restored_source_start() {
    let mut stream = Stream::open(StreamConfig::default(), HelperBackend::boxed(false)).unwrap();
    stream.start().unwrap();
    assert!(stream.switch_backend(HelperBackend::boxed(true)).is_err());
    stream.stop_checked().unwrap();
    assert_eq!(
        collect(&mut stream),
        [
            Event::ClassicLoopbackKeepaliveStarted,
            stopped(HelperStopReason::SourceSwitched),
            Event::ClassicLoopbackKeepaliveStarted,
            stopped(HelperStopReason::StartupFailed),
            Event::ClassicLoopbackKeepaliveStarted,
            stopped(HelperStopReason::StreamStopped)
        ]
    );
}

#[test]
fn failed_replacement_closes_each_forwarded_start_when_stops_exceed_drain_limit() {
    struct FloodedBackend(VecDeque<Event>);
    impl CaptureBackend for FloodedBackend {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 2)
        }
        fn start(&mut self, _sink: RawSink) -> Result<()> {
            self.0.extend([
                Event::SilenceProbeStarted,
                Event::SilenceProbeStopped {
                    reason: HelperStopReason::SignalObserved,
                },
                Event::SilenceProbeStarted,
                Event::ClassicLoopbackKeepaliveStarted,
                Event::ClassicLoopbackKeepaliveStarted,
            ]);
            self.0.extend(std::iter::repeat_n(
                Event::StreamStalled,
                MAX_BACKEND_EVENTS_PER_TICK * MAX_FINAL_EVENT_BATCHES,
            ));
            Err(Error::DeviceNotFound)
        }
        fn stop(&mut self) {
            self.0.extend([
                Event::SilenceProbeStopped {
                    reason: HelperStopReason::StartupFailed,
                },
                stopped(HelperStopReason::StartupFailed),
                stopped(HelperStopReason::StartupFailed),
            ]);
        }
        fn poll_event(&mut self) -> Option<Event> {
            self.0.pop_front()
        }
    }
    let mut stream = Stream::open(StreamConfig::default(), HelperBackend::boxed(false)).unwrap();
    stream.start().unwrap();
    assert!(stream
        .switch_backend(Box::new(FloodedBackend(VecDeque::new())))
        .is_err());
    // Stopping also reconciles the restored backend's pending mailbox.
    // Mailbox exhaustion remains observable through the existing cleanup result.
    assert!(stream.stop_checked().is_err());
    assert_eq!(
        collect(&mut stream),
        [
            Event::ClassicLoopbackKeepaliveStarted,
            stopped(HelperStopReason::SourceSwitched),
            Event::SilenceProbeStarted,
            Event::SilenceProbeStopped {
                reason: HelperStopReason::SignalObserved
            },
            Event::SilenceProbeStarted,
            Event::ClassicLoopbackKeepaliveStarted,
            Event::ClassicLoopbackKeepaliveStarted,
            Event::SilenceProbeStopped {
                reason: HelperStopReason::StartupFailed
            },
            stopped(HelperStopReason::StartupFailed),
            stopped(HelperStopReason::StartupFailed),
            Event::ClassicLoopbackKeepaliveStarted,
            stopped(HelperStopReason::StreamStopped),
        ]
    );
    assert!(collect(&mut stream).is_empty());
}

#[test]
fn recovery_joins_old_helper_before_new_start() {
    let mut stream = Stream::open(StreamConfig::default(), HelperBackend::boxed(false)).unwrap();
    stream.start().unwrap();
    {
        let mut backend = stream.shared.backend.lock().unwrap();
        stop_backend_reconciling(&stream.shared, &mut backend, false);
    }
    Stream::open_backend_once(&stream.shared, GenerationChange::Recovery).unwrap();
    stream.stop_checked().unwrap();
    assert_eq!(
        collect(&mut stream),
        [
            Event::ClassicLoopbackKeepaliveStarted,
            stopped(HelperStopReason::Recovery),
            Event::ClassicLoopbackKeepaliveStarted,
            stopped(HelperStopReason::StreamStopped)
        ]
    );
}

#[test]
fn mix_forwards_pairs_through_failed_switch_and_stop() {
    let mix = |fail| {
        Box::new(crate::mix::CompositeBackend::new(
            Box::new(crate::MockBackend::new(48_000, 2, 440.0)),
            HelperBackend::boxed(fail),
            1.0,
            1.0,
        )) as Box<dyn CaptureBackend>
    };
    let mut stream = Stream::open(StreamConfig::default(), mix(false)).unwrap();
    stream.start().unwrap();
    assert!(stream.switch_backend(mix(true)).is_err());
    // Rejected startup is returned to the switch caller; helper teardown succeeded.
    assert!(stream.stop_checked().is_ok());
    assert!(stream.shutdown_report().unwrap().cleanup().is_empty());
    assert!(stream.terminal_error().is_none());
    assert_eq!(
        collect(&mut stream),
        [
            Event::ClassicLoopbackKeepaliveStarted,
            stopped(HelperStopReason::SourceSwitched),
            Event::ClassicLoopbackKeepaliveStarted,
            stopped(HelperStopReason::StartupFailed),
            Event::ClassicLoopbackKeepaliveStarted,
            stopped(HelperStopReason::StreamStopped)
        ]
    );
}

#[test]
fn ordinary_microphone_capture_does_not_invent_helper_events() {
    let mut stream = Stream::open(
        StreamConfig::default(),
        Box::new(crate::MockBackend::new(48_000, 2, 440.0)),
    )
    .unwrap();
    stream.start().unwrap();
    stream.stop_checked().unwrap();
    assert!(collect(&mut stream).is_empty());
}

#[test]
fn mix_forwards_completed_probe_pair_unchanged() {
    struct ProbeBackend {
        capture: crate::MockBackend,
        events: VecDeque<Event>,
    }
    impl CaptureBackend for ProbeBackend {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 2)
        }
        fn start(&mut self, sink: RawSink) -> Result<()> {
            self.capture.start(sink)?;
            self.events.push_back(Event::SilenceProbeStarted);
            self.events.push_back(Event::SilenceProbeStopped {
                reason: HelperStopReason::SignalObserved,
            });
            Ok(())
        }
        fn stop(&mut self) {
            self.capture.stop();
        }
        fn poll_event(&mut self) -> Option<Event> {
            self.events.pop_front()
        }
    }
    let mix = crate::mix::CompositeBackend::new(
        Box::new(crate::MockBackend::new(48_000, 2, 440.0)),
        Box::new(ProbeBackend {
            capture: crate::MockBackend::new(48_000, 2, 440.0),
            events: VecDeque::new(),
        }),
        1.0,
        1.0,
    );
    let mut stream = Stream::open(StreamConfig::default(), Box::new(mix)).unwrap();
    stream.start().unwrap();
    stream.stop_checked().unwrap();
    assert_eq!(
        collect(&mut stream),
        [
            Event::SilenceProbeStarted,
            Event::SilenceProbeStopped {
                reason: HelperStopReason::SignalObserved
            }
        ]
    );
}

#[test]
fn mix_restart_preserves_final_lifecycle_before_next_generation() {
    let mut mix = crate::mix::CompositeBackend::new(
        Box::new(crate::MockBackend::new(48_000, 2, 440.0)),
        HelperBackend::boxed(false),
        1.0,
        1.0,
    );
    for _ in 0..2 {
        let (producer, _consumer) = raw_ring(1920);
        mix.start(RawSink::new(producer, 48_000, 2)).unwrap();
        mix.stop_checked().unwrap();
    }
    let events: Vec<_> = std::iter::from_fn(|| mix.poll_event())
        .filter(is_helper_lifecycle)
        .collect();
    assert_eq!(
        events,
        [
            Event::ClassicLoopbackKeepaliveStarted,
            stopped(HelperStopReason::StreamStopped),
            Event::ClassicLoopbackKeepaliveStarted,
            stopped(HelperStopReason::StreamStopped)
        ]
    );
}

#[test]
fn successful_retry_preserves_an_unread_stopped_after_started_was_consumed() {
    let mut stream = Stream::open(StreamConfig::default(), HelperBackend::boxed(true)).unwrap();
    assert!(stream.start().is_err());
    assert_eq!(
        stream.poll_event(),
        Some(Event::ClassicLoopbackKeepaliveStarted)
    );
    stream.start().unwrap();
    stream.stop_checked().unwrap();
    assert_eq!(
        collect(&mut stream),
        [
            stopped(HelperStopReason::StartupFailed),
            Event::ClassicLoopbackKeepaliveStarted,
            stopped(HelperStopReason::StreamStopped)
        ]
    );
}

#[test]
fn post_backend_startup_failure_uses_startup_failed() {
    let stream = Stream::open(StreamConfig::default(), HelperBackend::boxed(false)).unwrap();
    Stream::open_backend_once(&stream.shared, GenerationChange::Initial).unwrap();
    {
        let mut backend = stream.shared.backend.lock().unwrap();
        stop_backend_reconciling_with_reason(
            &stream.shared,
            &mut backend,
            true,
            HelperStopReason::StartupFailed,
        );
    }
    let mut stream = stream;
    assert_eq!(
        collect(&mut stream),
        [
            Event::ClassicLoopbackKeepaliveStarted,
            stopped(HelperStopReason::StartupFailed)
        ]
    );
}
