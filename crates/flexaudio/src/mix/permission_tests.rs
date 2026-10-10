//! Both child mailboxes must terminate the entire composite on denial.

use super::*;
use flexaudio_core::types::{Permission, StreamConfig};
use std::collections::VecDeque;
use std::sync::atomic::AtomicUsize;

struct Child {
    events: VecDeque<Event>,
    stops: Arc<AtomicUsize>,
}

impl CaptureBackend for Child {
    fn native_format(&self) -> (u32, u16) {
        (SAMPLE_RATE, CHANNELS)
    }
    fn start(&mut self, mut sink: RawSink) -> Result<()> {
        sink.push(&[0.2; 1920], 0);
        Ok(())
    }
    fn stop(&mut self) {
        self.stops.fetch_add(1, Ordering::SeqCst);
    }
    fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }
}

fn denial(permission: Permission) -> Event {
    Event::PermissionDenied {
        permission,
        detail: "permission revoked".into(),
    }
}

#[test]
fn either_child_denial_stops_both_lanes_and_gates_stream() {
    for deny_mic in [true, false] {
        let permission = if deny_mic {
            Permission::Microphone
        } else {
            Permission::SystemAudio
        };
        let mic_stops = Arc::new(AtomicUsize::new(0));
        let system_stops = Arc::new(AtomicUsize::new(0));
        let mic = Box::new(Child {
            events: if deny_mic {
                VecDeque::from([denial(permission)])
            } else {
                VecDeque::new()
            },
            stops: mic_stops.clone(),
        });
        let system = Box::new(Child {
            events: if deny_mic {
                VecDeque::new()
            } else {
                VecDeque::from([denial(permission)])
            },
            stops: system_stops.clone(),
        });
        let config = StreamConfig {
            secondary_output: Some(OutputFormat::default()),
            ..Default::default()
        };
        let mut stream = crate::Stream::open(
            config,
            Box::new(CompositeBackend::new(mic, system, 1.0, 1.0)),
        )
        .unwrap();
        stream.start().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while stream.terminal_error().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            matches!(stream.terminal_error(), Some(Error::PermissionDenied { permission: p, .. }) if p == permission)
        );
        assert!(stream.poll_chunk().is_none());
        assert!(stream.poll_secondary().is_none());
        assert!(mic_stops.load(Ordering::SeqCst) > 0);
        assert!(system_stops.load(Ordering::SeqCst) > 0);
        assert_eq!(stream.poll_event(), Some(denial(permission)));
        assert!(stream.poll_event().is_none());
        stream.stop();
    }
}

#[test]
fn both_child_advisories_are_forwarded_without_stopping() {
    let stops = Arc::new(AtomicUsize::new(0));
    let mic_event = Event::RecoverableError {
        error: Error::Backend("mic notice".into()),
    };
    let system_event = Event::SilenceWhileSourceActive {
        detail: "system notice".into(),
    };
    let mic = Box::new(Child {
        events: VecDeque::from([mic_event.clone()]),
        stops: stops.clone(),
    });
    let system = Box::new(Child {
        events: VecDeque::from([system_event.clone()]),
        stops: stops.clone(),
    });
    let mut mix = CompositeBackend::new(mic, system, 1.0, 1.0);
    assert_eq!(mix.poll_event(), Some(system_event));
    assert_eq!(mix.poll_event(), Some(mic_event));
    assert_eq!(mix.poll_event(), None);
    assert_eq!(stops.load(Ordering::SeqCst), 0);
}

#[test]
fn microphone_pending_advisory_keeps_both_mix_lanes_running() {
    let mic_stops = Arc::new(AtomicUsize::new(0));
    let system_stops = Arc::new(AtomicUsize::new(0));
    let pending = Event::PermissionPending {
        permission: Permission::Microphone,
        detail: "microphone permission has not been decided".into(),
    };
    let mic = Box::new(Child {
        events: VecDeque::from([pending.clone()]),
        stops: mic_stops.clone(),
    });
    let system = Box::new(Child {
        events: VecDeque::new(),
        stops: system_stops.clone(),
    });
    let mut stream = crate::Stream::open(
        StreamConfig::default(),
        Box::new(CompositeBackend::new(mic, system, 1.0, 1.0)),
    )
    .unwrap();
    stream.start().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(event) = stream.poll_event() {
            assert_eq!(event, pending);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "pending advisory was not forwarded"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert!(stream.terminal_error().is_none());
    assert_eq!(mic_stops.load(Ordering::SeqCst), 0);
    assert_eq!(system_stops.load(Ordering::SeqCst), 0);
    assert!(stream.poll_event().is_none());
    stream.stop();
    assert!(mic_stops.load(Ordering::SeqCst) > 0);
    assert!(system_stops.load(Ordering::SeqCst) > 0);
}

#[test]
fn busy_child_cannot_starve_other_mailbox() {
    struct Busy {
        event: Event,
    }
    impl CaptureBackend for Busy {
        fn native_format(&self) -> (u32, u16) {
            (SAMPLE_RATE, CHANNELS)
        }
        fn start(&mut self, _: RawSink) -> Result<()> {
            Ok(())
        }
        fn stop(&mut self) {}
        fn poll_event(&mut self) -> Option<Event> {
            Some(self.event.clone())
        }
    }
    // Polling a busy system mailbox still reaches the microphone on the next call.
    let stops = Arc::new(AtomicUsize::new(0));
    let mic = Box::new(Child {
        events: VecDeque::from([denial(Permission::Microphone)]),
        stops,
    });
    let system = Box::new(Busy {
        event: Event::RecoverableError {
            error: Error::Backend("notice".into()),
        },
    });
    let mut mix = CompositeBackend::new(mic, system, 1.0, 1.0);
    assert!(matches!(
        mix.poll_event(),
        Some(Event::RecoverableError { .. })
    ));
    assert_eq!(mix.poll_event(), Some(denial(Permission::Microphone)));
}

#[test]
fn terminal_error_waits_until_both_mix_children_have_stopped() {
    use std::sync::mpsc;

    struct SlowStopChild {
        event: Option<Event>,
        entered: mpsc::Sender<()>,
        release: Option<mpsc::Receiver<()>>,
        stops: Arc<AtomicUsize>,
    }
    impl CaptureBackend for SlowStopChild {
        fn native_format(&self) -> (u32, u16) {
            (SAMPLE_RATE, CHANNELS)
        }
        fn start(&mut self, mut sink: RawSink) -> Result<()> {
            sink.push(&[0.2; 1920], 0);
            Ok(())
        }
        fn poll_event(&mut self) -> Option<Event> {
            self.event.take()
        }
        fn stop(&mut self) {
            if let Some(release) = self.release.take() {
                self.entered.send(()).unwrap();
                // A bounded wait also prevents a failed assertion from hanging Drop.
                let _ = release.recv_timeout(Duration::from_secs(5));
            }
            self.stops.fetch_add(1, Ordering::SeqCst);
        }
    }
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let mic_stops = Arc::new(AtomicUsize::new(0));
    let system_stops = Arc::new(AtomicUsize::new(0));
    let mic = Box::new(SlowStopChild {
        event: Some(denial(Permission::Microphone)),
        entered: entered_tx,
        release: Some(release_rx),
        stops: mic_stops.clone(),
    });
    let system = Box::new(Child {
        events: VecDeque::new(),
        stops: system_stops.clone(),
    });
    let config = StreamConfig {
        secondary_output: Some(OutputFormat::default()),
        ..Default::default()
    };
    let mut stream = crate::Stream::open(
        config,
        Box::new(CompositeBackend::new(mic, system, 1.0, 1.0)),
    )
    .unwrap();
    stream.start().unwrap();
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    // Shutdown is still blocked in the child, but neither buffered output can escape.
    assert!(stream.poll_chunk().is_none());
    assert!(stream.poll_secondary().is_none());
    let (attempted_tx, attempted_rx) = mpsc::channel();
    // Resume must reject the recorded failure without reversing backend -> delivery
    // lock order or waiting for the child that only this test can release.
    assert!(matches!(
        stream.resume(),
        Err(Error::PermissionDenied { .. })
    ));
    let (observed_tx, observed_rx) = mpsc::channel();
    let observer = thread::spawn(move || {
        attempted_tx.send(()).unwrap();
        let error = stream.terminal_error();
        assert!(mic_stops.load(Ordering::SeqCst) > 0);
        assert!(system_stops.load(Ordering::SeqCst) > 0);
        observed_tx.send(error).unwrap();
        stream
    });
    attempted_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    // The child cannot finish before our explicit release. Publishing an error
    // during this window would violate the contract regardless of stop timing.
    assert_eq!(
        observed_rx.recv_timeout(Duration::from_millis(100)),
        Err(mpsc::RecvTimeoutError::Timeout)
    );
    release_tx.send(()).unwrap();
    assert!(matches!(
        observed_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        Some(Error::PermissionDenied { .. })
    ));
    let mut stream = observer.join().unwrap();
    stream.stop();
}
