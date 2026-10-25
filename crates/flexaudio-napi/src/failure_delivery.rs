//! Preserve typed failures when their final JavaScript delivery is refused.

use flexaudio::core::{report_drop_failures, DropFailureOwner};
use flexaudio::{Error, ErrorContext, Operation, ShutdownReport};
use napi::Status;

pub(super) fn queue_error(role: &str, status: Status) -> Error {
    Error::Backend(format!("{role} delivery failed: {status:?}"))
        .with_context(ErrorContext::new(Operation::Flush))
}

pub(super) fn report_stream_failure(report: &super::FinalReport, error: Error) {
    let (previous, outcome) = {
        let mut report = report.lock().unwrap_or_else(super::lock_poisoned);
        let retained = report.get_or_insert_with(super::shutdown::ShutdownReport::default);
        if !retained.binding_cleanup.contains(&error) {
            retained.binding_cleanup.push(error);
        }
        let outcome = retained.outcome();
        let previous = retained.delivered_report.replace(outcome.clone());
        retained.gc_reported = true;
        (previous, outcome)
    };
    // A receiver may inspect this stream, so invoke it only after releasing ownership.
    report_drop_failures(DropFailureOwner::Stream, previous.as_ref(), &outcome);
}

pub(super) fn report_rejected(
    owner: DropFailureOwner,
    report: &ShutdownReport,
    status: Status,
    role: &str,
) {
    if status != Status::Ok {
        report_failed(owner, report, queue_error(role, status));
    }
}

pub(super) fn report_failed(owner: DropFailureOwner, report: &ShutdownReport, delivery: Error) {
    let mut cleanup = report.cleanup().to_vec();
    cleanup.push(delivery);
    report_drop_failures(
        owner,
        None,
        &ShutdownReport::new(report.primary().cloned(), cleanup),
    );
}

pub(super) fn device_event(
    event: flexaudio::DeviceEvent,
    send: Option<impl FnOnce(super::JsDeviceEvent) -> Result<(), Status>>,
) -> bool {
    let failure = match &event {
        flexaudio::DeviceEvent::TerminalError { error } => Some(error.clone()),
        _ => None,
    };
    let result = send.map(|send| send(super::device_event_to_js(event)));
    match (failure.as_ref(), result) {
        (Some(error), Some(Err(status))) => report_rejected(
            DropFailureOwner::DeviceWatcher,
            &ShutdownReport::new(Some(error.clone()), Vec::new()),
            status,
            "device callback",
        ),
        (Some(error), None) => report_drop_failures(
            DropFailureOwner::DeviceWatcher,
            None,
            &ShutdownReport::new(Some(error.clone()), Vec::new()),
        ),
        // A closed JavaScript host cannot consume an ordinary device delta.
        (None, Some(Err(Status::Closing))) => {}
        (None, Some(Err(status))) => report_rejected(
            DropFailureOwner::DeviceWatcher,
            &ShutdownReport::new(None, Vec::new()),
            status,
            "device callback",
        ),
        _ => {}
    }
    failure.is_some()
}

pub(super) fn retain_queue_result(
    report: &super::FinalReport,
    result: Result<(), Status>,
    role: &str,
) {
    match result {
        Ok(()) => {}
        // Nonfailure payloads cannot be consumed after the JavaScript host has closed.
        Err(Status::Closing) => {}
        Err(status) => super::record_cleanup_failure(report, queue_error(role, status)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn refused_failure_notifications_reach_the_typed_sink_and_success_stays_quiet() {
        const CHILD: &str = "FLEXAUDIO_NAPI_FAILURE_DELIVERY_TEST";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "failure_delivery::tests::refused_failure_notifications_reach_the_typed_sink_and_success_stays_quiet",
                    "--exact",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let received = Arc::new(Mutex::new(Vec::new()));
        let output = received.clone();
        flexaudio::set_drop_failure_handler(Some(Box::new(move |failure| {
            output.lock().unwrap().push(failure.clone());
        })));
        for status in [Status::Closing, Status::QueueFull] {
            flexaudio_core::test_support::assert_distinct_failures_retained(
                Error::DeviceLost,
                queue_error("device callback", status),
                |first, _second| {
                    assert!(device_event(
                        flexaudio::DeviceEvent::TerminalError { error: first },
                        Some(|_| Err(status)),
                    ));
                    let failure = received.lock().unwrap().pop().unwrap();
                    assert_eq!(failure.owner, DropFailureOwner::DeviceWatcher);
                    failure.report.result().unwrap_err()
                },
            );
        }
        assert!(device_event(
            flexaudio::DeviceEvent::TerminalError {
                error: Error::DeviceLost
            },
            Some(|_| Ok(())),
        ));
        assert!(!device_event(
            flexaudio::DeviceEvent::Removed {
                id: "device".into()
            },
            Some(|_| Err(Status::Closing)),
        ));
        assert!(received.lock().unwrap().is_empty());
        assert!(!device_event(
            flexaudio::DeviceEvent::Removed {
                id: "device".into()
            },
            Some(|_| Err(Status::QueueFull)),
        ));
        let rejected_delta = received.lock().unwrap().pop().unwrap();
        assert_eq!(
            rejected_delta.report.cleanup(),
            &[queue_error("device callback", Status::QueueFull)]
        );
        assert!(device_event(
            flexaudio::DeviceEvent::TerminalError {
                error: Error::DeviceLost
            },
            None::<fn(super::super::JsDeviceEvent) -> Result<(), Status>>,
        ));
        let absent_receiver = received.lock().unwrap().pop().unwrap();
        assert_eq!(absent_receiver.report.primary(), Some(&Error::DeviceLost));
        assert!(absent_receiver.report.cleanup().is_empty());
        for status in [Status::Closing, Status::QueueFull] {
            flexaudio_core::test_support::assert_distinct_failures_retained(
                Error::DeviceLost,
                Error::Backend("retained cleanup failure".into()),
                |first, second| {
                    let original = ShutdownReport::new(Some(first), vec![second]);
                    let retained = Arc::new(Mutex::new(Some(
                        super::super::shutdown::ShutdownReport::from_core(Some(original.clone())),
                    )));
                    super::super::shutdown::report_unreachable_failure(
                        &retained,
                        Status::Ok,
                        "stop settlement",
                    );
                    assert!(!retained.lock().unwrap().as_ref().unwrap().gc_reported);
                    assert!(received.lock().unwrap().is_empty());
                    super::super::shutdown::report_unreachable_failure(
                        &retained,
                        status,
                        "stop settlement",
                    );
                    super::super::shutdown::report_unreachable_failure(
                        &retained,
                        status,
                        "stop settlement",
                    );
                    let failure = {
                        let mut received = received.lock().unwrap();
                        assert_eq!(
                            received.len(),
                            1,
                            "the final report must reach the sink only once"
                        );
                        received.pop().unwrap()
                    };
                    assert_eq!(failure.owner, DropFailureOwner::Stream);
                    assert_eq!(failure.report.cleanup().len(), 2);
                    assert_eq!(
                        failure.report.cleanup()[1],
                        queue_error("stop settlement", status)
                    );
                    assert_eq!(
                        retained.lock().unwrap().as_ref().unwrap().core,
                        Some(original)
                    );
                    let later_status = if status == Status::Closing {
                        Status::QueueFull
                    } else {
                        Status::Closing
                    };
                    super::super::shutdown::report_unreachable_failure(
                        &retained,
                        later_status,
                        "GC shutdown notice",
                    );
                    let delivery_failure = received.lock().unwrap().pop().unwrap();
                    assert!(delivery_failure.report.primary().is_none());
                    assert_eq!(
                        delivery_failure.report.cleanup(),
                        &[queue_error("GC shutdown notice", later_status)]
                    );
                    super::super::shutdown::report_unreachable_failure(
                        &retained,
                        later_status,
                        "GC shutdown notice",
                    );
                    assert!(received.lock().unwrap().is_empty());
                    let later = Error::Backend("later reaper cleanup failure".into());
                    retained
                        .lock()
                        .unwrap()
                        .as_mut()
                        .unwrap()
                        .binding_cleanup
                        .push(later.clone());
                    super::super::shutdown::report_unreachable_failure(
                        &retained,
                        later_status,
                        "GC shutdown notice",
                    );
                    let later_failure = received.lock().unwrap().pop().unwrap();
                    assert!(later_failure.report.primary().is_none());
                    assert_eq!(later_failure.report.cleanup(), std::slice::from_ref(&later));
                    super::super::shutdown::report_unreachable_failure(
                        &retained,
                        later_status,
                        "GC shutdown notice",
                    );
                    assert!(received.lock().unwrap().is_empty());
                    failure.report.result().unwrap_err()
                },
            );
        }
        let encoder = ShutdownReport::new(
            Some(Error::Backend("encoder finalize failure".into())),
            Vec::new(),
        );
        report_rejected(
            DropFailureOwner::FlacWriter,
            &encoder,
            Status::Ok,
            "encoder GC notice",
        );
        assert!(received.lock().unwrap().is_empty());
        report_rejected(
            DropFailureOwner::FlacWriter,
            &encoder,
            Status::Closing,
            "encoder GC notice",
        );
        let failure = received.lock().unwrap().pop().unwrap();
        assert_eq!(failure.owner, DropFailureOwner::FlacWriter);
        assert_eq!(failure.report.primary(), encoder.primary());
        assert_eq!(
            failure.report.cleanup(),
            &[queue_error("encoder GC notice", Status::Closing)]
        );
        let clean = Arc::new(Mutex::new(Some(
            super::super::shutdown::ShutdownReport::default(),
        )));
        super::super::shutdown::report_unreachable_failure(
            &clean,
            Status::Closing,
            "GC shutdown notice",
        );
        assert!(!clean.lock().unwrap().as_ref().unwrap().gc_reported);
        assert!(received.lock().unwrap().is_empty());
        for initial in [
            None,
            Some(super::super::shutdown::ShutdownReport::default()),
        ] {
            let clean = Arc::new(Mutex::new(initial));
            for status in [Status::QueueFull, Status::GenericFailure] {
                for _attempt in 0..2 {
                    super::super::shutdown::report_unreachable_failure(
                        &clean,
                        status,
                        "GC shutdown notice",
                    );
                }
                let mut received = received.lock().unwrap();
                assert_eq!(received.len(), 1, "equal delivery retries must stay quiet");
                let failure = received.pop().unwrap();
                assert!(failure.report.primary().is_none());
                assert_eq!(
                    failure.report.cleanup(),
                    &[queue_error("GC shutdown notice", status)]
                );
            }
            assert_eq!(
                clean.lock().unwrap().as_ref().unwrap().binding_cleanup,
                vec![
                    queue_error("GC shutdown notice", Status::QueueFull),
                    queue_error("GC shutdown notice", Status::GenericFailure),
                ]
            );
        }
        flexaudio::set_drop_failure_handler(None);
    }

    #[test]
    fn ordinary_queue_refusals_retain_nonclosing_errors() {
        let report = Arc::new(Mutex::new(None));
        retain_queue_result(&report, Ok(()), "audio chunk");
        retain_queue_result(&report, Err(Status::Closing), "audio chunk");
        assert!(report.lock().unwrap().is_none());
        retain_queue_result(&report, Err(Status::QueueFull), "audio chunk");
        retain_queue_result(&report, Err(Status::GenericFailure), "audio chunk");
        let report = report.lock().unwrap();
        assert_eq!(
            report.as_ref().unwrap().binding_cleanup,
            vec![
                queue_error("audio chunk", Status::QueueFull),
                queue_error("audio chunk", Status::GenericFailure),
            ]
        );
    }
}
