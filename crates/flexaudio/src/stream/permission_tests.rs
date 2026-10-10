//! Device-free regression tests for the terminal capture contract.

use super::*;
use crate::MockBackend;
use std::sync::atomic::AtomicUsize;
use std::time::Instant;

struct EventBackend {
    capture: MockBackend,
    events: Arc<Mutex<VecDeque<Event>>>,
    starts: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
    deny_on_start: Option<usize>,
    deny_on_stop: bool,
    feed: bool,
}

impl CaptureBackend for EventBackend {
    fn native_format(&self) -> (u32, u16) {
        self.capture.native_format()
    }

    fn start(&mut self, sink: RawSink) -> Result<()> {
        let start = self.starts.fetch_add(1, Ordering::SeqCst) + 1;
        if self.deny_on_start == Some(start) {
            return Err(denial());
        }
        if self.feed {
            self.capture.start(sink)
        } else {
            Ok(())
        }
    }

    fn stop(&mut self) {
        self.stops.fetch_add(1, Ordering::SeqCst);
        self.capture.stop();
        if self.deny_on_stop {
            self.deny_on_stop = false;
            self.events.lock().unwrap().push_back(denied_event());
        }
    }

    fn poll_event(&mut self) -> Option<Event> {
        self.events.lock().unwrap().pop_front()
    }
}

fn denial() -> Error {
    Error::PermissionDenied {
        permission: Permission::Microphone,
        detail: "authorization was denied by the user".into(),
    }
}

fn denied_event() -> Event {
    Event::PermissionDenied {
        permission: Permission::Microphone,
        detail: "authorization was denied by the user".into(),
    }
}

fn backend() -> EventBackend {
    EventBackend {
        capture: MockBackend::new(48_000, 2, 440.0),
        events: Arc::new(Mutex::new(VecDeque::new())),
        starts: Arc::new(AtomicUsize::new(0)),
        stops: Arc::new(AtomicUsize::new(0)),
        deny_on_start: None,
        deny_on_stop: false,
        feed: true,
    }
}

#[test]
fn terminal_query_failure_gates_both_lanes_and_never_retries() {
    let backend = backend();
    let mailbox = backend.events.clone();
    let starts = backend.starts.clone();
    let stops = backend.stops.clone();
    let config = StreamConfig {
        secondary_output: Some(OutputFormat::default()),
        ..Default::default()
    };
    let mut stream = Stream::open(config, Box::new(backend)).unwrap();
    stream.start().unwrap();
    let error = Error::Backend("injected authorization query failure".into());
    let event = Event::TerminalError {
        error: error.clone(),
    };
    mailbox.lock().unwrap().push_back(event.clone());
    let deadline = Instant::now() + Duration::from_secs(5);
    // Exercise the real watchdog's mailbox path, without native capture.
    while stream.terminal_error().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(stream.terminal_error(), Some(error.clone()));
    assert_eq!(stream.start(), Err(error.clone()));
    assert_eq!(stream.resume(), Err(error.clone()));
    assert_eq!(
        stream.switch_source(StreamConfig::default()),
        Err(error.clone())
    );
    assert!(stream.poll_chunk().is_none());
    assert!(stream.poll_secondary().is_none());
    stream.stop();
    assert_eq!(stream.terminal_error(), Some(error));
    assert_eq!(stream.poll_event(), Some(event));
    assert!(stream.poll_event().is_none());
    assert_eq!(starts.load(Ordering::SeqCst), 1);
    assert!(stops.load(Ordering::SeqCst) >= 1);
}

fn assert_terminal(stream: &mut Stream) {
    assert_eq!(stream.terminal_error(), Some(denial()));
    assert_eq!(stream.start(), Err(denial()));
    assert_eq!(stream.resume(), Err(denial()));
    assert_eq!(stream.switch_source(StreamConfig::default()), Err(denial()));
    assert_eq!(
        stream.switch_backend(Box::new(MockBackend::new(48_000, 2, 220.0))),
        Err(denial())
    );
    assert!(stream.poll_chunk().is_none());
    assert!(stream.poll_secondary().is_none());
}

#[test]
fn denied_start_is_terminal_and_does_not_retry() {
    let mut backend = backend();
    backend.deny_on_start = Some(1);
    let starts = backend.starts.clone();
    let stops = backend.stops.clone();
    let mut stream = Stream::open(StreamConfig::default(), Box::new(backend)).unwrap();
    assert_eq!(stream.start(), Err(denial()));
    assert_terminal(&mut stream);
    assert_eq!(stream.poll_event(), Some(denied_event()));
    assert_eq!(stream.poll_event(), None);
    assert_eq!(starts.load(Ordering::SeqCst), 1);
    assert_eq!(stops.load(Ordering::SeqCst), 1);
    stream.stop();
    assert_terminal(&mut stream);
}

#[test]
fn runtime_denial_suppresses_buffered_primary_secondary_and_tail() {
    let backend = backend();
    let mailbox = backend.events.clone();
    let starts = backend.starts.clone();
    let stops = backend.stops.clone();
    let config = StreamConfig {
        secondary_output: Some(OutputFormat::default()),
        ..Default::default()
    };
    let mut stream = Stream::open(config, Box::new(backend)).unwrap();
    stream.start().unwrap();
    // Wait for evidence that both delivery paths are active, without native devices.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut primary = false;
    let mut secondary = false;
    while Instant::now() < deadline && !(primary && secondary) {
        primary |= stream.poll_chunk().is_some();
        secondary |= stream.poll_secondary().is_some();
        thread::sleep(Duration::from_millis(5));
    }
    assert!(primary && secondary);
    stream.pause();
    mailbox
        .lock()
        .unwrap()
        .extend([denied_event(), denied_event()]);
    // Drive the control tick directly: denial must outrank pause and stall handling.
    drain_backend_events(&stream.shared);
    assert_terminal(&mut stream);
    assert_eq!(stream.poll_event(), Some(denied_event()));
    drain_backend_events(&stream.shared);
    assert_eq!(stream.poll_event(), None);
    assert_eq!(starts.load(Ordering::SeqCst), 1);
    assert!(stops.load(Ordering::SeqCst) >= 1);
    stream.stop();
    assert_terminal(&mut stream);
}

#[test]
fn watchdog_drains_denial_while_samples_are_flowing() {
    let backend = backend();
    let mailbox = backend.events.clone();
    let starts = backend.starts.clone();
    let mut stream = Stream::open(StreamConfig::default(), Box::new(backend)).unwrap();
    stream.start().unwrap();
    mailbox.lock().unwrap().push_back(denied_event());
    let deadline = Instant::now() + Duration::from_secs(5);
    while stream.terminal_error().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_terminal(&mut stream);
    assert_eq!(stream.poll_event(), Some(denied_event()));
    assert_eq!(stream.poll_event(), None);
    assert_eq!(starts.load(Ordering::SeqCst), 1);
    stream.stop();
}

#[test]
fn permission_denied_on_reopen_closes_stream() {
    let mut backend = backend();
    backend.deny_on_start = Some(2);
    let starts = backend.starts.clone();
    let mut stream = Stream::open(StreamConfig::default(), Box::new(backend)).unwrap();
    stream.start().unwrap();
    // Inject the recovery operation directly, without waiting for a wall-clock stall.
    {
        let mut be = stream.shared.backend.lock().unwrap();
        be.stop();
    }
    assert_eq!(
        Stream::open_backend_once(&stream.shared, GenerationChange::Recovery),
        Err(denial())
    );
    assert_terminal(&mut stream);
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    assert_eq!(stream.poll_event(), Some(denied_event()));
    stream.stop();
}

#[test]
fn watchdog_preserves_denial_produced_while_stopping_before_reopen() {
    let mut backend = backend();
    backend.feed = false;
    backend.deny_on_stop = true;
    let starts = backend.starts.clone();
    let mut stream = Stream::open(StreamConfig::default(), Box::new(backend)).unwrap();
    stream.start().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while stream.terminal_error().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_terminal(&mut stream);
    assert_eq!(starts.load(Ordering::SeqCst), 1);
    let events: Vec<_> = std::iter::from_fn(|| stream.poll_event()).collect();
    assert!(events.contains(&Event::StreamStalled));
    assert!(events.contains(&denied_event()));
    assert!(!events.contains(&Event::StreamRecovered));
    stream.stop();
}

#[test]
fn advisory_event_continues_capture() {
    let backend = backend();
    let mailbox = backend.events.clone();
    let advisory = Event::SilenceWhileSourceActive {
        detail: "permission may be missing; digital silence is also possible".into(),
    };
    let mut stream = Stream::open(StreamConfig::default(), Box::new(backend)).unwrap();
    stream.start().unwrap();
    mailbox.lock().unwrap().push_back(advisory.clone());
    drain_backend_events(&stream.shared);
    assert_eq!(stream.poll_event(), Some(advisory));
    assert_eq!(stream.terminal_error(), None);
    let deadline = Instant::now() + Duration::from_secs(5);
    while stream.poll_chunk().is_none() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    stream.stop();
}

#[test]
fn permission_denied_on_source_switch_does_not_restore_old_source() {
    let original = backend();
    let starts = original.starts.clone();
    let mut replacement = backend();
    replacement.deny_on_start = Some(1);
    let mut stream = Stream::open(StreamConfig::default(), Box::new(original)).unwrap();
    stream.start().unwrap();
    assert_eq!(stream.switch_backend(Box::new(replacement)), Err(denial()));
    assert_terminal(&mut stream);
    assert_eq!(starts.load(Ordering::SeqCst), 1);
    stream.stop();
}

#[test]
fn stop_preserves_queued_and_final_owner_denial_without_tail_delivery() {
    for during_stop in [false, true] {
        let mut backend = backend();
        backend.deny_on_stop = during_stop;
        let mailbox = backend.events.clone();
        let config = StreamConfig {
            secondary_output: Some(OutputFormat::default()),
            ..Default::default()
        };
        let mut stream = Stream::open(config, Box::new(backend)).unwrap();
        stream.start().unwrap();
        if !during_stop {
            mailbox.lock().unwrap().push_back(denied_event());
        }
        stream.stop();
        assert_terminal(&mut stream);
        assert_eq!(stream.poll_event(), Some(denied_event()));
        assert_eq!(stream.poll_event(), None);
    }
}

#[test]
fn source_switch_preserves_queued_and_final_owner_denial() {
    for during_stop in [false, true] {
        let mut backend = backend();
        backend.deny_on_stop = during_stop;
        let mailbox = backend.events.clone();
        let mut stream = Stream::open(StreamConfig::default(), Box::new(backend)).unwrap();
        stream.start().unwrap();
        if !during_stop {
            mailbox.lock().unwrap().push_back(denied_event());
        }
        assert_eq!(
            stream.switch_backend(Box::new(MockBackend::new(48_000, 2, 220.0))),
            Err(denial())
        );
        assert_terminal(&mut stream);
        assert_eq!(stream.poll_event(), Some(denied_event()));
        stream.stop();
    }
}

#[test]
fn mailbox_work_is_bounded_for_noisy_custom_backends() {
    struct Noisy {
        polls: Arc<AtomicUsize>,
    }
    impl CaptureBackend for Noisy {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 2)
        }
        fn start(&mut self, _: RawSink) -> Result<()> {
            Ok(())
        }
        fn stop(&mut self) {}
        fn poll_event(&mut self) -> Option<Event> {
            self.polls.fetch_add(1, Ordering::SeqCst);
            Some(Event::RecoverableError {
                error: Error::Backend("notice".into()),
            })
        }
    }
    let polls = Arc::new(AtomicUsize::new(0));
    let mut stream = Stream::open(
        StreamConfig::default(),
        Box::new(Noisy {
            polls: polls.clone(),
        }),
    )
    .unwrap();
    assert_eq!(
        drain_backend_events(&stream.shared),
        MailboxDrain::BudgetExhausted
    );
    assert_eq!(polls.load(Ordering::SeqCst), MAX_BACKEND_EVENTS_PER_TICK);
    stream.stop();
    assert!(matches!(stream.terminal_error(), Some(Error::Backend(_))));
    assert!(stream.poll_chunk().is_none());
    assert!(matches!(stream.start(), Err(Error::Backend(_))));
}

#[test]
fn budget_exhaustion_defers_recovery_until_permission_events_are_read() {
    let backend = backend();
    backend
        .events
        .lock()
        .unwrap()
        .extend(
            (0..MAX_BACKEND_EVENTS_PER_TICK).map(|_| Event::RecoverableError {
                error: Error::Backend("notice".into()),
            }),
        );
    backend.events.lock().unwrap().push_back(denied_event());
    let mut stream = Stream::open(StreamConfig::default(), Box::new(backend)).unwrap();
    assert_eq!(
        drain_backend_events(&stream.shared),
        MailboxDrain::BudgetExhausted
    );
    assert!(stream.terminal_error().is_none());
    assert_eq!(drain_backend_events(&stream.shared), MailboxDrain::Terminal);
    assert_terminal(&mut stream);
}

#[test]
fn final_reconciliation_finds_denial_behind_multiple_event_batches() {
    for switch in [false, true] {
        let backend = backend();
        let mailbox = backend.events.clone();
        let mut stream = Stream::open(StreamConfig::default(), Box::new(backend)).unwrap();
        stream.start().unwrap();
        mailbox
            .lock()
            .unwrap()
            .extend(
                (0..MAX_BACKEND_EVENTS_PER_TICK * 2).map(|_| Event::RecoverableError {
                    error: Error::Backend("notice".into()),
                }),
            );
        mailbox.lock().unwrap().push_back(denied_event());
        if switch {
            assert_eq!(
                stream.switch_backend(Box::new(MockBackend::new(48_000, 2, 220.0))),
                Err(denial())
            );
        } else {
            stream.stop();
        }
        assert_terminal(&mut stream);
        let permission_events = std::iter::from_fn(|| stream.poll_event())
            .filter(|event| matches!(event, Event::PermissionDenied { .. }))
            .count();
        assert_eq!(permission_events, 1);
        stream.stop();
    }
}

fn poll_panic_then_event(denied: bool) {
    struct PanicOnce {
        panicked: bool,
        events: Arc<Mutex<VecDeque<Event>>>,
    }
    impl CaptureBackend for PanicOnce {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 2)
        }
        fn start(&mut self, mut sink: RawSink) -> Result<()> {
            sink.push(&[0.25; 1920], 0);
            Ok(())
        }
        fn stop(&mut self) {}
        fn poll_event(&mut self) -> Option<Event> {
            if !std::mem::replace(&mut self.panicked, true) {
                panic!("injected poll panic");
            }
            self.events.lock().unwrap().pop_front()
        }
    }
    let events = Arc::new(Mutex::new(VecDeque::new()));
    let mut stream = Stream::open(
        StreamConfig::default(),
        Box::new(PanicOnce {
            panicked: false,
            events: events.clone(),
        }),
    )
    .unwrap();
    stream.start().unwrap();
    assert_eq!(
        drain_backend_events(&stream.shared),
        MailboxDrain::BudgetExhausted
    );
    assert!(matches!(
        stream.poll_event(),
        Some(Event::RecoverableError {
            error: Error::Backend(_)
        })
    ));
    assert!(stream.terminal_error().is_none());
    assert!(!stream.shared.stopping.load(Ordering::SeqCst));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while stream.chunk_consumer.is_empty() {
        assert!(std::time::Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
    assert!(
        stream.poll_chunk().is_some(),
        "poll warning must not suppress PCM"
    );
    events.lock().unwrap().push_back(if denied {
        denied_event()
    } else {
        Event::PermissionGranted
    });
    assert_eq!(
        drain_backend_events(&stream.shared),
        if denied {
            MailboxDrain::Terminal
        } else {
            MailboxDrain::Empty
        }
    );
    if denied {
        assert_eq!(stream.terminal_error(), Some(denial()));
        assert!(stream.poll_chunk().is_none());
        assert_eq!(stream.poll_event(), Some(denied_event()));
    } else {
        assert_eq!(stream.poll_event(), Some(Event::PermissionGranted));
        assert!(stream.terminal_error().is_none());
    }
    stream.stop();
}
#[test]
fn poll_event_panic_is_recoverable_then_polling_resumes() {
    poll_panic_then_event(false);
}
#[test]
fn denial_after_poll_panic_still_latches_terminality() {
    poll_panic_then_event(true);
}
