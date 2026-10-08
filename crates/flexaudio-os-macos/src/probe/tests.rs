use super::*;
use std::sync::Mutex;

struct FakeClock(Mutex<Instant>);
impl Clock for FakeClock {
    fn now(&self) -> Instant {
        *self.0.lock().unwrap()
    }
}

struct FakeProbe {
    outcome: Option<ProbeOutcome>,
    calls: usize,
    cancel: bool,
    advance: Option<Arc<FakeClock>>,
}
impl Probe for FakeProbe {
    fn run(&mut self, control: &ProbeControl) -> ProbeOutcome {
        self.calls += 1;
        if self.cancel {
            control.cancellation.store(true, Ordering::Release);
        }
        if let Some(clock) = self.advance.as_ref() {
            *clock.0.lock().unwrap() = control.deadline();
        }
        self.outcome.take().unwrap()
    }
}

fn fake(outcome: ProbeOutcome) -> FakeProbe {
    FakeProbe {
        outcome: Some(outcome),
        calls: 0,
        cancel: false,
        advance: None,
    }
}

fn control() -> (ProbeControl, Arc<FakeClock>) {
    let now = Instant::now();
    let clock = Arc::new(FakeClock(Mutex::new(now)));
    (
        ProbeControl {
            deadline: now + PROBE_TIMEOUT,
            cancellation: Arc::new(AtomicBool::new(false)),
            clock: clock.clone(),
        },
        clock,
    )
}

#[test]
fn detected_signal_suppresses_advisory_and_finishes_generation() {
    let (control, _) = control();
    let mut probe = fake(ProbeOutcome::Present);
    let mut generation = GenerationProbe::default();
    for _ in 0..10 {
        assert_eq!(generation.on_silence(&mut probe, &control), None);
    }
    assert_eq!(probe.calls, 1);
}

#[test]
fn denial_preserves_typed_cause_once() {
    let (control, _) = control();
    let mut probe = fake(ProbeOutcome::Denied {
        detail: "Known nonzero render was captured as exact zeros".into(),
    });
    let mut generation = GenerationProbe::default();
    assert_eq!(
        generation.on_silence(&mut probe, &control),
        Some(ProbeDecision::Denied {
            detail: "Known nonzero render was captured as exact zeros".into()
        })
    );
    assert_eq!(generation.on_silence(&mut probe, &control), None);
    assert_eq!(probe.calls, 1);
}

#[test]
fn failure_and_timeout_keep_old_advisory_once() {
    for outcome in [
        ProbeOutcome::Failed {
            detail: "unsupported output format".into(),
        },
        ProbeOutcome::TimedOut,
    ] {
        let (control, _) = control();
        let mut probe = fake(outcome);
        let mut generation = GenerationProbe::default();
        assert_eq!(
            generation.on_silence(&mut probe, &control),
            Some(advisory())
        );
        assert_eq!(generation.on_silence(&mut probe, &control), None);
        assert_eq!(probe.calls, 1);
    }
}

#[test]
fn cancelled_outcome_is_silent() {
    let (control, _) = control();
    let mut probe = fake(ProbeOutcome::Cancelled);
    assert_eq!(
        GenerationProbe::default().on_silence(&mut probe, &control),
        None
    );
    assert_eq!(probe.calls, 1);
}

#[test]
fn stop_before_probe_prevents_native_call_and_event() {
    let (control, _) = control();
    control.cancellation.store(true, Ordering::Release);
    let mut probe = fake(ProbeOutcome::Denied {
        detail: "must not run".into(),
    });
    assert_eq!(
        GenerationProbe::default().on_silence(&mut probe, &control),
        None
    );
    assert_eq!(probe.calls, 0);
}

#[test]
fn stop_after_probe_evidence_wins_over_every_outcome() {
    for outcome in [
        ProbeOutcome::Present,
        ProbeOutcome::Denied {
            detail: "denied".into(),
        },
        ProbeOutcome::Failed {
            detail: "failure".into(),
        },
        ProbeOutcome::TimedOut,
    ] {
        let (control, _) = control();
        let mut probe = fake(outcome);
        probe.cancel = true;
        assert_eq!(
            GenerationProbe::default().on_silence(&mut probe, &control),
            None
        );
        assert_eq!(probe.calls, 1);
    }
}

#[test]
fn expired_deadline_never_starts_probe_or_accepts_late_evidence() {
    let (control, clock) = control();
    *clock.0.lock().unwrap() = control.deadline();
    let mut probe = fake(ProbeOutcome::Present);
    assert_eq!(
        GenerationProbe::default().on_silence(&mut probe, &control),
        Some(advisory())
    );
    assert_eq!(probe.calls, 0);
    for outcome in [
        ProbeOutcome::Present,
        ProbeOutcome::Denied {
            detail: "late denial".into(),
        },
    ] {
        let (control, clock) = super::tests::control();
        let mut probe = fake(outcome);
        probe.advance = Some(clock);
        assert_eq!(
            GenerationProbe::default().on_silence(&mut probe, &control),
            Some(advisory())
        );
        assert_eq!(probe.calls, 1);
    }
}

#[test]
fn each_new_capture_generation_can_probe_once() {
    let (control, _) = control();
    let mut probe = fake(ProbeOutcome::Present);
    assert_eq!(
        GenerationProbe::default().on_silence(&mut probe, &control),
        None
    );
    probe.outcome = Some(ProbeOutcome::Present);
    assert_eq!(
        GenerationProbe::default().on_silence(&mut probe, &control),
        None
    );
    assert_eq!(probe.calls, 2);
}

#[test]
fn production_control_has_shared_cancellation_and_two_second_budget() {
    let cancellation = Arc::new(AtomicBool::new(false));
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let control = ProbeControl::new(deadline, cancellation.clone());
    assert_eq!(PROBE_TIMEOUT, Duration::from_secs(2));
    assert_eq!(control.deadline(), deadline);
    assert!(!control.cancelled());
    assert!(!control.timed_out());
    assert!(control.remaining() <= PROBE_TIMEOUT);
    cancellation.store(true, Ordering::Release);
    assert!(control.cancelled());
    assert!(control.cancellation_flag().load(Ordering::Acquire));
}

#[test]
fn stop_between_native_result_and_publication_suppresses_event() {
    let (control, _) = control();
    let mut probe = fake(ProbeOutcome::Denied {
        detail: "injected denial".into(),
    });
    let decision = GenerationProbe::default().on_silence(&mut probe, &control);
    assert!(matches!(decision, Some(ProbeDecision::Denied { .. })));
    let gate = PublicationGate::default();
    assert_eq!(gate.cancel(&control.cancellation), Ok(()));
    let mut published = false;
    assert_eq!(gate.when_active(&control, || published = true), Ok(false));
    assert!(!published);
}

#[test]
fn publication_that_wins_the_gate_precedes_stop_linearization() {
    assert_publication_precedes_stop(false);
}

#[test]
fn recovered_poison_publication_that_wins_precedes_stop_linearization() {
    assert_publication_precedes_stop(true);
}

fn assert_publication_precedes_stop(poisoned: bool) {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::mpsc;
    use std::thread;

    let (control, _) = control();
    let gate = Arc::new(PublicationGate::default());
    if poisoned {
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let _guard = gate.0.lock().unwrap();
            panic!("injected publication panic");
        }));
    }
    let control = Arc::new(control);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (trace_tx, trace_rx) = mpsc::channel();
    let owner_gate = gate.clone();
    let owner_control = control.clone();
    let owner_trace = trace_tx.clone();
    let owner = thread::spawn(move || {
        let publish = || {
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            owner_trace.send("published").unwrap();
        };
        if poisoned {
            Ok(owner_gate.recover_when_active(&owner_control, publish))
        } else {
            owner_gate.when_active(&owner_control, publish)
        }
    });
    entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    let stop_gate = gate.clone();
    let stop_control = control.clone();
    let stop = thread::spawn(move || {
        let cancelled = stop_gate.cancel(&stop_control.cancellation);
        assert_eq!(
            cancelled,
            if poisoned {
                Err(PublicationError::Poisoned)
            } else {
                Ok(())
            }
        );
        trace_tx.send("stopped").unwrap();
    });
    assert!(
        !control.cancelled(),
        "stop must wait for publication's critical section"
    );
    release_tx.send(()).unwrap();
    assert_eq!(
        trace_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        "published"
    );
    assert_eq!(
        trace_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        "stopped"
    );
    assert_eq!(owner.join().unwrap(), Ok(true));
    stop.join().unwrap();
    assert!(control.cancelled());
}

#[test]
fn poisoned_publication_gate_reports_typed_failure_and_still_cancels() {
    use std::panic::{catch_unwind, AssertUnwindSafe};

    let (control, _) = control();
    let gate = PublicationGate::default();
    assert!(catch_unwind(AssertUnwindSafe(|| {
        let _guard = gate.0.lock().unwrap();
        panic!("injected publication panic");
    }))
    .is_err());
    assert_eq!(
        gate.when_active(&control, || panic!("poisoned gate cannot publish")),
        Err(PublicationError::Poisoned)
    );
    assert_eq!(
        gate.cancel(&control.cancellation),
        Err(PublicationError::Poisoned)
    );
    assert!(control.cancelled());
    assert!(!gate.recover_when_active(&control, || panic!(
        "cancelled poisoned gate must not emit a terminal event"
    )));
}

#[test]
fn uncancelled_poison_can_publish_terminal_failure_in_order() {
    use std::panic::{catch_unwind, AssertUnwindSafe};

    let (control, _) = control();
    let gate = PublicationGate::default();
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let _guard = gate.0.lock().unwrap();
        panic!("injected publication panic");
    }));
    let mut terminal_published = false;
    assert!(gate.recover_when_active(&control, || terminal_published = true));
    assert!(terminal_published);
    assert_eq!(
        gate.cancel(&control.cancellation),
        Err(PublicationError::Poisoned)
    );
    assert!(control.cancelled());
}
