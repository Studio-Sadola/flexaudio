//! Retain stream failure payloads when an optional JavaScript receiver is unavailable.

use super::{event_to_js, failure_delivery, lock_poisoned, shutdown, FinalReport, JsStreamEvent};
use flexaudio::core::{report_drop_failures, terminal_causes::legacy_capture_error};
use flexaudio::{DropFailureOwner, Error, Event};
use napi::Status;

const EVENT_ROLE: &str = "stream event callback";

fn event_failure(event: &Event) -> Option<Error> {
    match event {
        Event::PermissionDenied { permission, detail } => Some(Error::PermissionDenied {
            permission: *permission,
            detail: detail.clone(),
        }),
        Event::DeviceLost => Some(Error::DeviceLost),
        Event::Error(detail) => Some(legacy_capture_error(detail.clone())),
        Event::TerminalError { error }
        | Event::RecoverableError { error }
        | Event::ShutdownError { error } => Some(error.clone()),
        // Lifecycle reasons and loss/advisory notices contain no original Error payload.
        Event::SilenceProbeStarted
        | Event::SilenceProbeStopped { .. }
        | Event::ClassicLoopbackKeepaliveStarted
        | Event::ClassicLoopbackKeepaliveStopped { .. }
        | Event::StartupSamplesDropped { .. }
        | Event::ChunkDropped { .. }
        | Event::StreamStalled
        | Event::StreamRecovered
        | Event::PermissionPending { .. }
        | Event::SilenceWhileSourceActive { .. }
        | Event::AudioLoss { .. }
        | Event::Clipped => None,
        // Event is non-exhaustive across the crate boundary; unknown variants have no projection.
        _ => None,
    }
}

fn retain_failure(report: &FinalReport, error: Error) {
    let mut report = report.lock().unwrap_or_else(lock_poisoned);
    let retained = report.get_or_insert_with(shutdown::ShutdownReport::default);
    if !retained.binding_cleanup.contains(&error) {
        retained.binding_cleanup.push(error);
    }
}

fn report_without_receiver(report: &FinalReport, error: Error) {
    retain_failure(report, error);
    let (previous, outcome) = {
        let mut report = report.lock().unwrap_or_else(lock_poisoned);
        let retained = report.get_or_insert_with(shutdown::ShutdownReport::default);
        let outcome = retained.outcome();
        let previous = retained.delivered_report.replace(outcome.clone());
        retained.gc_reported = true;
        (previous, outcome)
    };
    // No N-API operation was attempted, so only the original failure is reported.
    report_drop_failures(DropFailureOwner::Stream, previous.as_ref(), &outcome);
}

/// Send a payload once, retaining actual queue failures beside its original typed cause.
pub(super) fn deliver(
    original: Option<Error>,
    report: &FinalReport,
    role: &str,
    send: impl FnOnce() -> Result<(), Status>,
) {
    let Err(status) = send() else {
        return;
    };
    let failure = match original {
        Some(error) => error,
        // A closed host cannot consume ordinary PCM, startup or advisory notices.
        None if status == Status::Closing => return,
        None => failure_delivery::queue_error(role, status),
    };
    retain_failure(report, failure);
    shutdown::report_unreachable_failure(report, status, role);
}

/// Forward supported events, rescuing refused failures and unsubscribed recoverable errors.
pub(super) fn event(
    event: Event,
    report: &FinalReport,
    send: Option<impl FnOnce(JsStreamEvent) -> Result<(), Status>>,
) {
    match send {
        Some(send) => deliver(event_failure(&event), report, EVENT_ROLE, || {
            send(event_to_js(event))
        }),
        None => {
            if let Event::RecoverableError { error } = event {
                report_without_receiver(report, error);
            }
            // Durable failures already reach stop/GC through the facade report; no call failed.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio::core::terminal_causes::{retain_terminal_cause, retains_terminal_cause};
    use flexaudio::{HelperStopReason, Permission, ShutdownReport, VadErrorCode};
    use std::sync::{Arc, Mutex};

    type Sink = Arc<Mutex<Vec<flexaudio::DropFailure>>>;
    type Sender = fn(JsStreamEvent) -> Result<(), Status>;

    fn isolated(name: &str, test: impl FnOnce(Sink)) {
        const CHILD: &str = "FLEXAUDIO_NAPI_STREAM_EVENT_DELIVERY_TEST";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([name, "--exact"])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                flexaudio::set_drop_failure_handler(None);
            }
        }
        let received = Arc::new(Mutex::new(Vec::new()));
        let output = received.clone();
        flexaudio::set_drop_failure_handler(Some(Box::new(move |failure| {
            output.lock().unwrap().push(failure.clone());
        })));
        let _reset = Reset;
        test(received);
    }

    fn report() -> FinalReport {
        Arc::new(Mutex::new(None))
    }

    fn failures() -> Vec<(Event, Error, &'static str)> {
        let typed = Error::Backend("observed stream failure".into());
        let denied = Error::PermissionDenied {
            permission: Permission::SystemAudio,
            detail: "observed access refusal".into(),
        };
        vec![
            (
                Event::PermissionDenied {
                    permission: Permission::SystemAudio,
                    detail: "observed access refusal".into(),
                },
                denied,
                "permissionDenied",
            ),
            (Event::DeviceLost, Error::DeviceLost, "deviceLost"),
            (
                Event::Error("legacy native diagnostic".into()),
                legacy_capture_error("legacy native diagnostic".into()),
                "error",
            ),
            (
                Event::TerminalError {
                    error: typed.clone(),
                },
                typed.clone(),
                "terminalError",
            ),
            (
                Event::RecoverableError {
                    error: typed.clone(),
                },
                typed.clone(),
                "recoverableError",
            ),
            (
                Event::ShutdownError {
                    error: typed.clone(),
                },
                typed,
                "shutdownError",
            ),
        ]
    }

    fn notices() -> Vec<Event> {
        vec![
            Event::SilenceProbeStarted,
            Event::SilenceProbeStopped {
                reason: HelperStopReason::StreamStopped,
            },
            Event::ClassicLoopbackKeepaliveStarted,
            Event::ClassicLoopbackKeepaliveStopped {
                reason: HelperStopReason::StreamStopped,
            },
            Event::StartupSamplesDropped {
                frames: 12,
                reason: flexaudio::core::startup::StartupDropReason::OutOfRange,
            },
            Event::ChunkDropped { count: 1 },
            Event::StreamStalled,
            Event::StreamRecovered,
            Event::PermissionPending {
                permission: Permission::Microphone,
                detail: "awaiting consent".into(),
            },
            Event::SilenceWhileSourceActive {
                detail: "silence diagnostic".into(),
            },
            Event::AudioLoss {
                loss: flexaudio::AudioLoss::raw_overflow(None, None, 48_000, 2).unwrap(),
            },
            Event::Clipped,
        ]
    }

    #[test]
    fn every_failure_event_retains_its_original_cause_and_actual_queue_failure() {
        isolated("stream_event_delivery::tests::every_failure_event_retains_its_original_cause_and_actual_queue_failure", |received| {
            assert_eq!(failures().len(), 6);
            for status in [Status::Closing, Status::QueueFull] {
                for (event, original, kind) in failures() {
                    flexaudio_core::test_support::assert_distinct_failures_retained(
                        original,
                        failure_delivery::queue_error(EVENT_ROLE, status),
                        |first, _second| {
                            assert_eq!(event_failure(&event), Some(first));
                            let retained = report();
                            super::event(event, &retained, Some(|event: JsStreamEvent| {
                                assert_eq!(event.kind, kind);
                                Err(status)
                            }));
                            let failure = received.lock().unwrap().pop().unwrap();
                            assert_eq!(failure.owner, DropFailureOwner::Stream);
                            failure.report.result().unwrap_err()
                        },
                    );
                }
            }
            assert!(received.lock().unwrap().is_empty());
        });
    }

    #[test]
    fn two_distinct_recoverable_failures_survive_refusal_and_equal_retries_are_quiet() {
        isolated("stream_event_delivery::tests::two_distinct_recoverable_failures_survive_refusal_and_equal_retries_are_quiet", |received| {
            flexaudio_core::test_support::assert_distinct_failures_retained(
                Error::Backend("first recoverable failure".into()),
                Error::Backend("second recoverable failure".into()),
                |first, second| {
                    let retained = report();
                    for error in [first.clone(), first.clone(), second.clone(), second.clone()] {
                        event(Event::RecoverableError { error }, &retained, Some(|_| Err(Status::Closing)));
                    }
                    let failures = std::mem::take(&mut *received.lock().unwrap());
                    assert_eq!(failures.len(), 2);
                    assert!(retains_terminal_cause(&failures[0].report.result().unwrap_err(), &failure_delivery::queue_error(EVENT_ROLE, Status::Closing)));
                    assert_eq!(failures[1].report.cleanup(), std::slice::from_ref(&second));
                    let mut observed = None;
                    // Project the original payloads received by the sink; queue failure is checked above.
                    for failure in failures {
                        for error in failure.report.cleanup() {
                            if error == &first || error == &second {
                                retain_terminal_cause(&mut observed, error.clone());
                            }
                        }
                    }
                    let retained = retained.lock().unwrap();
                    assert_eq!(retained.as_ref().unwrap().binding_cleanup.len(), 3);
                    observed.expect("both recoverable errors must reach the sink")
                },
            );
        });
    }

    #[test]
    fn absent_callback_rescues_only_recoverable_failures_without_claiming_durable_gc_reports() {
        isolated("stream_event_delivery::tests::absent_callback_rescues_only_recoverable_failures_without_claiming_durable_gc_reports", |received| {
            for (event, original, _) in failures() {
                let recoverable = matches!(event, Event::RecoverableError { .. });
                let retained = report();
                super::event(event, &retained, None::<Sender>);
                if recoverable {
                    let failure = received.lock().unwrap().pop().unwrap();
                    assert_eq!(failure.report, ShutdownReport::new(None, vec![original]));
                } else {
                    assert!(retained.lock().unwrap().is_none());
                    assert!(received.lock().unwrap().is_empty());
                }
            }
            for (event, original, _) in failures() {
                if matches!(event, Event::RecoverableError { .. }) {
                    continue;
                }
                let core = if matches!(event, Event::ShutdownError { .. }) {
                    ShutdownReport::new(None, vec![original])
                } else {
                    ShutdownReport::new(Some(original), Vec::new())
                };
                let retained = report();
                *retained.lock().unwrap() = Some(shutdown::ShutdownReport::from_core(Some(core.clone())));
                retained.lock().unwrap().as_mut().unwrap().gc_requested = true;
                super::event(event, &retained, None::<Sender>);
                let retained = retained.lock().unwrap();
                let retained = retained.as_ref().unwrap();
                assert_eq!(retained.outcome(), core);
                assert!(!retained.gc_reported);
                assert!(retained.delivered_report.is_none());
                assert!(retained.binding_cleanup.is_empty());
                assert!(received.lock().unwrap().is_empty());
            }
            let retained = report();
            for _ in 0..2 {
                event(Event::RecoverableError { error: Error::DeviceLost }, &retained, None::<Sender>);
            }
            assert_eq!(received.lock().unwrap().len(), 1);
            received.lock().unwrap().clear();
            event(Event::RecoverableError { error: Error::DeviceLost }, &retained, Some(|_| Err(Status::Closing)));
            let failure = received.lock().unwrap().pop().unwrap();
            assert_eq!(failure.report.cleanup(), &[failure_delivery::queue_error(EVENT_ROLE, Status::Closing)]);
        });
    }

    #[test]
    fn a_late_core_snapshot_keeps_distinct_causes_without_repeating_refused_durable_events() {
        fn count(error: &Error, expected: &Error) -> usize {
            if error == expected {
                return 1;
            }
            match error {
                Error::Multiple(group) => std::iter::once(group.primary())
                    .chain(group.secondary())
                    .map(|error| count(error, expected))
                    .sum(),
                _ => 0,
            }
        }
        isolated("stream_event_delivery::tests::a_late_core_snapshot_keeps_distinct_causes_without_repeating_refused_durable_events", |received| {
            for shape in ["terminal", "cleanup", "grouped"] {
                flexaudio_core::test_support::assert_distinct_failures_retained(
                    Error::DeviceLost,
                    Error::Backend("later native cleanup failure".into()),
                    |first, second| {
                        let retained = report();
                        let event = if shape == "cleanup" {
                            Event::ShutdownError { error: first.clone() }
                        } else {
                            Event::TerminalError { error: first.clone() }
                        };
                        super::event(event, &retained, Some(|_| Err(Status::Closing)));
                        let refused = received.lock().unwrap().pop().unwrap();
                        assert_eq!(refused.report.cleanup()[0], first);
                        let incoming = match shape {
                            "terminal" => ShutdownReport::new(Some(first.clone()), vec![second.clone()]),
                            "cleanup" => ShutdownReport::new(None, vec![first.clone(), second.clone()]),
                            "grouped" => ShutdownReport::new(Some(Error::Multiple(flexaudio::ErrorGroup::new(first.clone(), second.clone(), Vec::new()))), Vec::new()),
                            _ => unreachable!("fixture lists all core snapshot shapes"),
                        };
                        let outcome = {
                            let mut retained = retained.lock().unwrap();
                            let retained = retained.as_mut().unwrap();
                            retained.retain_core(incoming);
                            retained.outcome()
                        };
                        let occurrences = outcome.primary().into_iter().chain(outcome.cleanup()).map(|error| count(error, &first)).sum::<usize>();
                        assert_eq!(occurrences, 1, "a refused event must not duplicate its durable core cause");
                        let result = outcome.result().unwrap_err();
                        assert!(retains_terminal_cause(&result, &first));
                        assert!(retains_terminal_cause(&result, &second));
                        assert!(retains_terminal_cause(&result, &failure_delivery::queue_error(EVENT_ROLE, Status::Closing)));
                        shutdown::report_unreachable_failure(&retained, Status::Closing, EVENT_ROLE);
                        let later = received.lock().unwrap().pop().unwrap();
                        assert_eq!(later.report.result(), Err(second));
                        assert!(received.lock().unwrap().is_empty());
                        let mut observed = None;
                        retain_terminal_cause(&mut observed, refused.report.cleanup()[0].clone());
                        retain_terminal_cause(&mut observed, later.report.result().unwrap_err());
                        observed.unwrap()
                    },
                );
            }
        });
    }

    #[test]
    fn successful_sends_and_nonfailure_closing_or_absent_receivers_stay_quiet() {
        isolated("stream_event_delivery::tests::successful_sends_and_nonfailure_closing_or_absent_receivers_stay_quiet", |received| {
            for (event, _, _) in failures() {
                let retained = report();
                super::event(event, &retained, Some(|_| Ok(())));
                assert!(retained.lock().unwrap().is_none());
            }
            assert_eq!(notices().len(), 12);
            for event in notices() {
                assert_eq!(event_failure(&event), None);
                let retained = report();
                super::event(event, &retained, Some(|_| Err(Status::Closing)));
                assert!(retained.lock().unwrap().is_none());
            }
            for event in notices() {
                let retained = report();
                super::event(event, &retained, None::<Sender>);
                assert!(retained.lock().unwrap().is_none());
            }
            assert!(received.lock().unwrap().is_empty());
        });
    }

    #[test]
    fn distinct_queue_statuses_are_retained_without_repeating_the_original_failure() {
        isolated("stream_event_delivery::tests::distinct_queue_statuses_are_retained_without_repeating_the_original_failure", |received| {
            let retained = report();
            for status in [Status::Closing, Status::QueueFull, Status::QueueFull] {
                event(Event::RecoverableError { error: Error::DeviceLost }, &retained, Some(|_| Err(status)));
            }
            let received = received.lock().unwrap();
            assert_eq!(received.len(), 2);
            assert_eq!(received[1].report.cleanup(), &[failure_delivery::queue_error(EVENT_ROLE, Status::QueueFull)]);
        });
    }

    #[test]
    fn nonfailure_queue_errors_and_generic_vad_failure_delivery_are_retained() {
        isolated("stream_event_delivery::tests::nonfailure_queue_errors_and_generic_vad_failure_delivery_are_retained", |received| {
            for status in [Status::QueueFull, Status::GenericFailure] {
                let retained = report();
                deliver(None, &retained, "startup notice", || Err(status));
                let failure = received.lock().unwrap().pop().unwrap();
                assert_eq!(failure.report.cleanup(), &[failure_delivery::queue_error("startup notice", status)]);
            }
            let original = Error::Vad { code: VadErrorCode::Inference };
            let retained = report();
            deliver(Some(original.clone()), &retained, "VAD failure notice", || Err(Status::Closing));
            let failure = received.lock().unwrap().pop().unwrap();
            assert_eq!(failure.report.cleanup(), &[original, failure_delivery::queue_error("VAD failure notice", Status::Closing)]);
        });
    }
}
