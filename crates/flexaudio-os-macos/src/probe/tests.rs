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
            cleanup: Arc::new(ProbeCleanup::default()),
            cleanup_ledger: Arc::new(CleanupLedger::default()),
            events: None,
            stop_reason: Arc::new(Mutex::new(HelperStopReason::StreamStopped)),
            running: AtomicBool::new(false),
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
fn failure_and_timeout_keep_explained_advisory_once() {
    for (outcome, expected) in [
        (
            ProbeOutcome::Failed {
                error: Error::Backend("unsupported output format".into()),
            },
            "unsupported output format",
        ),
        (ProbeOutcome::TimedOut, "two-second deadline"),
    ] {
        let (control, _) = control();
        let mut probe = fake(outcome);
        let mut generation = GenerationProbe::default();
        let Some(ProbeDecision::Advisory { detail, .. }) =
            generation.on_silence(&mut probe, &control)
        else {
            panic!("inconclusive probe must produce an advisory");
        };
        assert!(detail.contains(ADVISORY_DETAIL));
        assert!(detail.contains(expected));
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
            error: Error::Backend("failure".into()),
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
fn cancellation_retains_the_probe_failure_and_distinct_native_cleanup_failure() {
    struct CancelAfterCleanup {
        outcome: Option<ProbeOutcome>,
        cleanup: Error,
    }
    impl Probe for CancelAfterCleanup {
        fn run(&mut self, control: &ProbeControl) -> ProbeOutcome {
            control.cleanup.0.lock().unwrap().push(self.cleanup.clone());
            control.cancellation.store(true, Ordering::Release);
            self.outcome.take().unwrap()
        }
    }
    for outcome_kind in ["failed", "inconclusive", "timed-out"] {
        flexaudio_core::test_support::assert_distinct_failures_retained(
            if outcome_kind == "timed-out" {
                timeout_error()
            } else {
                Error::Backend("probe failed before cancellation".into())
            },
            Error::Backend("native probe cleanup failed during cancellation".into()),
            |first, second| {
                let (control, _) = control();
                let outcome = match outcome_kind {
                    "failed" => ProbeOutcome::Failed { error: first },
                    "inconclusive" => ProbeOutcome::Inconclusive { error: first },
                    "timed-out" => ProbeOutcome::TimedOut,
                    _ => unreachable!("fixture lists every outcome"),
                };
                let mut probe = CancelAfterCleanup {
                    outcome: Some(outcome),
                    cleanup: second,
                };
                assert_eq!(
                    GenerationProbe::default().on_silence(&mut probe, &control),
                    None
                );
                flexaudio_core::ShutdownReport::new(None, control.cleanup_ledger.take())
                    .result()
                    .expect_err("cancelled diagnostic failures must reach shutdown")
            },
        );
    }
}

#[test]
fn native_cancellation_without_failure_does_not_invent_a_timeout() {
    let (control, clock) = control();
    let mut probe = fake(ProbeOutcome::Cancelled);
    probe.cancel = true;
    probe.advance = Some(clock);
    assert_eq!(
        GenerationProbe::default().on_silence(&mut probe, &control),
        None
    );
    assert!(control.cleanup_ledger.take().is_empty());
}

#[test]
fn rejected_publication_retains_distinct_probe_failures_without_a_late_event() {
    for poisoned in [false, true] {
        flexaudio_core::test_support::assert_distinct_failures_retained(
            Error::Backend("probe setup failed before publication".into()),
            Error::Backend("probe cleanup failed before publication".into()),
            |first, second| {
                let (control, _) = control();
                let gate = PublicationGate::default();
                if poisoned {
                    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let _guard = gate.0.lock().unwrap();
                        panic!("injected publication gate poisoning");
                    }))
                    .is_err());
                } else {
                    gate.cancel(&control.cancellation).unwrap();
                }
                let decision = advisory_failures(vec![first, second]);
                let result = gate.publish_probe_decision(&control, decision, |_| {
                    panic!("a rejected probe decision must not publish an event")
                });
                assert_eq!(
                    result,
                    if poisoned {
                        Err(PublicationError::Poisoned)
                    } else {
                        Ok(false)
                    }
                );
                flexaudio_core::ShutdownReport::new(None, control.cleanup_ledger.take())
                    .result()
                    .expect_err("unpublished failures must reach shutdown")
            },
        );
    }
}

#[test]
fn expired_deadline_never_starts_probe_or_accepts_late_evidence() {
    let (control, clock) = control();
    *clock.0.lock().unwrap() = control.deadline();
    let mut probe = fake(ProbeOutcome::Present);
    assert_eq!(
        GenerationProbe::default().on_silence(&mut probe, &control),
        Some(advisory_failures(vec![timeout_error()]))
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
            Some(advisory_failures(vec![timeout_error()]))
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
        assert!(catch_unwind(AssertUnwindSafe(|| {
            let _guard = gate.0.lock().unwrap();
            panic!("injected publication panic");
        }))
        .is_err());
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
    assert!(catch_unwind(AssertUnwindSafe(|| {
        let _guard = gate.0.lock().unwrap();
        panic!("injected publication panic");
    }))
    .is_err());
    let mut terminal_published = false;
    assert!(gate.recover_when_active(&control, || terminal_published = true));
    assert!(terminal_published);
    assert_eq!(
        gate.cancel(&control.cancellation),
        Err(PublicationError::Poisoned)
    );
    assert!(control.cancelled());
}

#[test]
fn failed_probe_preserves_its_safe_reason_in_the_advisory() {
    let (control, _) = control();
    let mut probe = fake(ProbeOutcome::Failed {
        error: Error::Backend("self-probe IOProc stop failed (OSStatus -50)".into()),
    });
    let decision = GenerationProbe::default()
        .on_silence(&mut probe, &control)
        .unwrap();
    let ProbeDecision::Advisory { detail, .. } = decision else {
        panic!("failed self-probe must remain an advisory");
    };
    assert!(detail.contains("self-probe IOProc stop failed (OSStatus -50)"));
}

#[test]
fn native_probe_teardown_retains_all_failed_statuses_after_drop() {
    // Native stop/destroy failures need Core Audio. The real native owners share
    // this cleanup mailbox, including their Drop path; exercise it without audio.
    let (control, _) = control();
    let cleanup = control.cleanup_diagnostics();
    cleanup.record_native("IOProc stop", -50);
    cleanup.record_native("IOProc destruction", -51);
    cleanup.record_native("aggregate destruction", 0);
    let mut probe = fake(ProbeOutcome::Present);
    let Some(ProbeDecision::Advisory { detail, .. }) =
        GenerationProbe::default().on_silence(&mut probe, &control)
    else {
        panic!("failed teardown must invalidate successful evidence");
    };
    assert!(detail.contains("IOProc stop failed"));
    assert!(detail.contains("OSStatus -50"));
    assert!(detail.contains("IOProc destruction failed"));
    assert!(detail.contains("OSStatus -51"));
    assert!(!detail.contains("aggregate destruction"));
    assert!(cleanup.take().is_empty());
}

#[test]
fn advisory_keeps_native_context_and_cleanup_as_typed_related_causes() {
    let (control, _) = control();
    let cause = Error::Backend("self-probe setup failed; retry capture".into()).with_context(
        ErrorContext::new(Operation::Start).with_native_status(NativeStatus::OsStatus {
            call: "fixture_start",
            value: -50,
        }),
    );
    control.cleanup.record_native("IOProc destruction", -51);
    let mut probe = fake(ProbeOutcome::Failed {
        error: cause.clone(),
    });
    let Some(ProbeDecision::Advisory {
        error: Some(Error::Multiple(group)),
        ..
    }) = GenerationProbe::default().on_silence(&mut probe, &control)
    else {
        panic!("probe cause and cleanup must be retained together");
    };
    assert_eq!(group.primary(), &cause);
    let Error::Context { context, .. } = group.secondary().next().unwrap() else {
        panic!("missing cleanup context");
    };
    assert_eq!(context.operation(), Operation::Stop);
    assert_eq!(
        context.native_status(),
        Some(NativeStatus::OsStatus {
            call: "IOProc destruction",
            value: -51
        })
    );
}

#[test]
fn late_native_failure_survives_the_timeout_advisory() {
    // Native synchronous calls cannot be forced to fail at a deadline on Linux.
    // Check the adapter preserves its result, then inject both into shared policy.
    assert!(!include_str!("../native_probe.rs").contains("Err(_) if control.timed_out()"));
    let (control, clock) = control();
    let cause = Error::Backend("native probe setup failed; retry capture".into()).with_context(
        ErrorContext::new(Operation::Start).with_native_status(NativeStatus::OsStatus {
            call: "fixture_setup",
            value: -50,
        }),
    );
    let mut probe = fake(ProbeOutcome::Failed {
        error: cause.clone(),
    });
    probe.advance = Some(clock);
    let Some(ProbeDecision::Advisory {
        error: Some(Error::Multiple(group)),
        ..
    }) = GenerationProbe::default().on_silence(&mut probe, &control)
    else {
        panic!("late failed setup must retain both native and timeout causes");
    };
    assert_eq!(group.primary(), &cause);
    assert_eq!(group.secondary().collect::<Vec<_>>(), [&timeout_error()]);
}

fn lifecycle_control() -> (ProbeControl, std::sync::mpsc::Receiver<Event>) {
    let (mut control, _) = control();
    let (tx, rx) = std::sync::mpsc::channel();
    control.events = Some(tx);
    (control, rx)
}

fn assert_pair(events: std::sync::mpsc::Receiver<Event>, reason: HelperStopReason) {
    assert_eq!(
        events.try_iter().collect::<Vec<_>>(),
        [
            Event::SilenceProbeStarted,
            Event::SilenceProbeStopped { reason },
        ]
    );
}

#[test]
fn lifecycle_pairs_every_diagnostic_outcome_once() {
    for (outcome, reason) in [
        (ProbeOutcome::Present, HelperStopReason::SignalObserved),
        (
            ProbeOutcome::Denied {
                detail: "verified zero capture".into(),
            },
            HelperStopReason::PermissionDenied,
        ),
        (
            ProbeOutcome::Inconclusive {
                error: Error::Backend("ambiguous evidence".into()),
            },
            HelperStopReason::Inconclusive,
        ),
        (
            ProbeOutcome::Failed {
                error: Error::Backend("setup failed".into()),
            },
            HelperStopReason::StartupFailed,
        ),
        (ProbeOutcome::TimedOut, HelperStopReason::TimedOut),
        (ProbeOutcome::Cancelled, HelperStopReason::Cancelled),
    ] {
        let (control, events) = lifecycle_control();
        let mut generation = GenerationProbe::default();
        let mut probe = fake(outcome);
        generation.on_silence(&mut probe, &control);
        generation.on_silence(&mut probe, &control);
        assert_eq!(probe.calls, 1);
        assert_pair(events, reason);
    }
}

#[test]
fn lifecycle_cancellation_retains_shutdown_reason_without_diagnostic_events() {
    for reason in [
        HelperStopReason::StreamStopped,
        HelperStopReason::SourceSwitched,
        HelperStopReason::Recovery,
        HelperStopReason::Cancelled,
    ] {
        let (control, events) = lifecycle_control();
        *control.stop_reason.lock().unwrap() = reason;
        let mut probe = fake(ProbeOutcome::Denied {
            detail: "suppressed evidence".into(),
        });
        probe.cancel = true;
        assert_eq!(
            GenerationProbe::default().on_silence(&mut probe, &control),
            None
        );
        assert_pair(events, reason);
    }
}

#[test]
fn lifecycle_pre_cancelled_attempt_has_no_events() {
    let (control, events) = lifecycle_control();
    control.cancellation.store(true, Ordering::Release);
    let mut probe = fake(ProbeOutcome::Present);
    assert_eq!(
        GenerationProbe::default().on_silence(&mut probe, &control),
        None
    );
    assert_eq!(probe.calls, 0);
    assert!(events.try_recv().is_err());
}

#[test]
fn lifecycle_timeout_covers_entry_and_late_evidence() {
    for late in [false, true] {
        let (mut control, clock) = control();
        let (tx, rx) = std::sync::mpsc::channel();
        control.events = Some(tx);
        let mut probe = fake(ProbeOutcome::Present);
        if late {
            probe.advance = Some(clock);
        } else {
            *clock.0.lock().unwrap() = control.deadline();
        }
        GenerationProbe::default().on_silence(&mut probe, &control);
        assert_eq!(probe.calls, usize::from(late));
        assert_pair(rx, HelperStopReason::TimedOut);
    }
}

#[test]
fn lifecycle_runtime_and_teardown_failures_keep_typed_advisories() {
    for cleanup in [false, true] {
        let (control, events) = lifecycle_control();
        control.mark_running();
        let mut probe = if cleanup {
            control.cleanup.record_native("IOProc stop", -50);
            fake(ProbeOutcome::Present)
        } else {
            fake(ProbeOutcome::Failed {
                error: Error::Backend("runtime failed".into()),
            })
        };
        assert!(matches!(
            GenerationProbe::default().on_silence(&mut probe, &control),
            Some(ProbeDecision::Advisory { error: Some(_), .. })
        ));
        assert_pair(events, HelperStopReason::RuntimeFailed);
    }
}

#[test]
fn lifecycle_panic_reports_after_native_owner_unwinds() {
    struct NativeOwner(Arc<AtomicBool>);
    impl Drop for NativeOwner {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    struct PanickingProbe(Arc<AtomicBool>, bool);
    impl Probe for PanickingProbe {
        fn run(&mut self, control: &ProbeControl) -> ProbeOutcome {
            let _owner = NativeOwner(self.0.clone());
            if self.1 {
                control.mark_running();
            }
            panic!("injected native adapter panic");
        }
    }
    for running in [false, true] {
        let (control, events) = lifecycle_control();
        let released = Arc::new(AtomicBool::new(false));
        let mut probe = PanickingProbe(released.clone(), running);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            GenerationProbe::default().on_silence(&mut probe, &control);
        }))
        .is_err());
        assert!(released.load(Ordering::Acquire));
        assert_pair(
            events,
            if running {
                HelperStopReason::RuntimeFailed
            } else {
                HelperStopReason::StartupFailed
            },
        );
    }
}
