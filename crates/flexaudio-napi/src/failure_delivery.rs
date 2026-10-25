//! Preserve typed failures when their final JavaScript delivery is refused.

use flexaudio::core::{report_drop_failures, DropFailureOwner};
use flexaudio::{Error, ErrorContext, Operation, ShutdownReport};
use napi::Status;

pub(super) fn queue_error(role: &str, status: Status) -> Error {
    Error::Backend(format!("{role} delivery failed: {status:?}"))
        .with_context(ErrorContext::new(Operation::Flush))
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
    send: impl FnOnce(super::JsDeviceEvent) -> Status,
) -> bool {
    let failure = match &event {
        flexaudio::DeviceEvent::TerminalError { error } => Some(error.clone()),
        _ => None,
    };
    let status = send(super::device_event_to_js(event));
    if let Some(error) = failure.as_ref() {
        report_rejected(
            DropFailureOwner::DeviceWatcher,
            &ShutdownReport::new(Some(error.clone()), Vec::new()),
            status,
            "device callback",
        );
    }
    // Ordinary device deltas contain no failure; an unavailable JS host cannot consume them.
    failure.is_some()
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
                        |_| status,
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
            |_| Status::Ok,
        ));
        assert!(!device_event(
            flexaudio::DeviceEvent::Removed {
                id: "device".into()
            },
            |_| Status::Closing,
        ));
        assert!(received.lock().unwrap().is_empty());
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
        flexaudio::set_drop_failure_handler(None);
    }
}
