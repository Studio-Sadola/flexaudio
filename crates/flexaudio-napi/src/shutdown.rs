//! One retained outcome for core teardown and attached addon processing.
use super::*;
#[cfg(test)]
use flexaudio_vad::VadTapError;
pub(super) use flexaudio_vad::{
    VadAttachmentFailure as VadFailure, VadAttachmentStage as VadFailureStage,
};
use napi::bindgen_prelude::Null;

#[derive(Default)]
pub(super) struct ShutdownReport {
    pub core: Option<flexaudio::core::ShutdownReport>,
    pub attachment: Vec<VadFailure>,
    pub binding_cleanup: Vec<flexaudio::Error>,
    pub gc_requested: bool,
    pub gc_reported: bool,
    pub delivered_report: Option<flexaudio::ShutdownReport>,
}

impl ShutdownReport {
    /// Reconcile a newer facade snapshot without discarding previously retained causes.
    pub(super) fn retain_core(&mut self, incoming: flexaudio::ShutdownReport) {
        let mut primary = self
            .core
            .as_ref()
            .and_then(|report| report.primary())
            .cloned();
        let mut cleanup = self
            .core
            .as_ref()
            .map(|report| report.cleanup().to_vec())
            .unwrap_or_default();
        if let Some(error) = incoming.primary() {
            flexaudio::core::terminal_causes::retain_terminal_cause(&mut primary, error.clone());
        }
        for error in incoming.cleanup() {
            if !cleanup.contains(error) {
                cleanup.push(error.clone());
            }
        }
        self.core = Some(flexaudio::ShutdownReport::new(primary, cleanup));
    }

    pub fn from_core(core: Option<flexaudio::core::ShutdownReport>) -> Self {
        Self {
            core,
            attachment: Vec::new(),
            binding_cleanup: Vec::new(),
            gc_requested: false,
            gc_reported: false,
            delivered_report: None,
        }
    }

    /// One shared outcome contract decides both retained diagnostics and checked stop.
    pub(super) fn outcome(&self) -> flexaudio::core::ShutdownReport {
        fn new_cleanup(
            primary: Option<&flexaudio::Error>,
            cleanup: &[flexaudio::Error],
            incoming: flexaudio::Error,
        ) -> Option<flexaudio::Error> {
            use flexaudio::core::terminal_causes::{retain_terminal_cause, retains_terminal_cause};
            if primary.is_some_and(|error| retains_terminal_cause(error, &incoming))
                || cleanup
                    .iter()
                    .any(|error| retains_terminal_cause(error, &incoming))
            {
                return None;
            }
            match incoming {
                flexaudio::Error::Multiple(group) => {
                    let mut changed = false;
                    let mut remaining = Vec::new();
                    for error in std::iter::once(group.primary()).chain(group.secondary()) {
                        let retained = new_cleanup(primary, cleanup, error.clone());
                        changed |= retained.as_ref() != Some(error);
                        if let Some(error) = retained {
                            remaining.push(error);
                        }
                    }
                    if !changed {
                        return Some(flexaudio::Error::Multiple(group));
                    }
                    let mut retained = None;
                    for error in remaining {
                        retain_terminal_cause(&mut retained, error);
                    }
                    retained
                }
                error => Some(error),
            }
        }
        let primary = self
            .core
            .as_ref()
            .and_then(|report| report.primary())
            .cloned();
        let mut cleanup = self
            .core
            .as_ref()
            .map(|report| report.cleanup().to_vec())
            .unwrap_or_default();
        // Failed early notifications may precede the facade snapshot of the same typed cause.
        for error in self
            .attachment
            .iter()
            .map(VadFailure::audio_error)
            .chain(self.binding_cleanup.iter().cloned())
        {
            if let Some(error) = new_cleanup(primary.as_ref(), &cleanup, error) {
                cleanup.push(error);
            }
        }
        flexaudio::core::ShutdownReport::new(primary, cleanup)
    }

    pub fn to_js(&self) -> boundary::JsShutdownReport {
        let report = self.outcome();
        boundary::JsShutdownReport {
            primary: report
                .primary()
                .map(boundary::audio_error)
                .map(Either::A)
                .unwrap_or(Either::B(Null)),
            cleanup_errors: report.cleanup().iter().map(boundary::audio_error).collect(),
        }
    }

    pub fn exception(&self, env: &Env) -> napi::Result<Option<JsObject>> {
        match self.outcome().result() {
            Ok(()) => Ok(None),
            Err(error) => boundary::js_error(env, &error).map(Some),
        }
    }
}

/// GC has no Promise caller. A failure first observed by GC teardown is delivered once as a
/// `FlexaudioShutdownWarning` through `process.emitWarning` (never as an uncaught exception),
/// matching Python's `sys.unraisablehook`. The warning carries `cause` (the error `stop()` would
/// reject with), its `audioError`, and the full `shutdownReport`.
pub(super) fn report_gc_failure(env: &Env, report: &FinalReport) {
    if report_handed_off_gc_failure(report) {
        return;
    }
    let Some((error, retained, outcome)) = take_gc_failure(report) else {
        return;
    };
    let message =
        format!("flexaudio Stream was garbage-collected without stop(); shutdown failed: {error}");
    gc_warning::emit(
        env,
        &message,
        flexaudio::DropFailureOwner::Stream,
        &outcome,
        |warning| {
            warning.set_named_property("cause", boundary::js_error(env, &error)?)?;
            warning.set_named_property("audioError", boundary::audio_error(&error))?;
            warning.set_named_property("shutdownReport", retained)
        },
    );
}

fn report_handed_off_gc_failure(report: &FinalReport) -> bool {
    let snapshot = {
        let mut report = report.lock().unwrap_or_else(lock_poisoned);
        report
            .as_mut()
            .filter(|report| report.gc_requested && report.gc_reported)
            .map(|report| {
                let outcome = report.outcome();
                (report.delivered_report.replace(outcome.clone()), outcome)
            })
    };
    let Some((previous, outcome)) = snapshot else {
        return false;
    };
    // An earlier queue refusal handed ownership to the sink; later GC causes follow it.
    flexaudio::core::report_drop_failures(
        flexaudio::DropFailureOwner::Stream,
        previous.as_ref(),
        &outcome,
    );
    true
}

/// Claim the one GC notification, releasing the lock before any JS runs.
fn take_gc_failure(
    report: &FinalReport,
) -> Option<(
    flexaudio::Error,
    boundary::JsShutdownReport,
    flexaudio::ShutdownReport,
)> {
    let mut report = report.lock().unwrap_or_else(lock_poisoned);
    let report = report
        .as_mut()
        .filter(|report| report.gc_requested && !report.gc_reported)?;
    let outcome = report.outcome();
    let error = outcome.result().err().or_else(|| {
        outcome.cleanup().split_first().and_then(|(first, rest)| {
            flexaudio::ShutdownReport::new(Some(first.clone()), rest.to_vec())
                .result()
                .err()
        })
    })?;
    report.gc_reported = true;
    report.delivered_report = Some(outcome.clone());
    Some((error, report.to_js(), outcome))
}

/// Once every JS queue refuses teardown, the process receiver owns the retained failures.
pub(super) fn report_unreachable_failure(report: &FinalReport, status: Status, role: &str) {
    if status == Status::Ok {
        return;
    }
    let (previous, outcome) = {
        let mut report = report.lock().unwrap_or_else(lock_poisoned);
        let report = report.get_or_insert_with(ShutdownReport::default);
        let outcome = report.outcome();
        if status == Status::Closing && outcome.primary().is_none() && outcome.cleanup().is_empty()
        {
            // A clean teardown has no failure to rescue when its host has already closed.
            return;
        }
        // Different roles/statuses are distinct observed failures; identical retries collapse.
        let delivery = failure_delivery::queue_error(role, status);
        if !report.binding_cleanup.contains(&delivery) {
            report.binding_cleanup.push(delivery);
        }
        let outcome = report.outcome();
        let previous = report.delivered_report.replace(outcome.clone());
        // Claim synchronously dispatched failures under the ownership lock, then release it
        // before invoking a receiver that may inspect this stream. Later cleanup remains new.
        report.gc_reported = true;
        (previous, outcome)
    };
    flexaudio::core::report_drop_failures(
        flexaudio::DropFailureOwner::Stream,
        previous.as_ref(),
        &outcome,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_report_fields_match_core_at_runtime() {
        let core = flexaudio::ShutdownReport::new(
            Some(flexaudio::Error::DeviceLost),
            vec![flexaudio::Error::Unsupported],
        );
        let expected: Vec<_> = flexaudio_core::test_support::shutdown_fields(&core)
            .into_iter()
            .map(|(field, count)| {
                (
                    match field {
                        "primary" => "primary",
                        "cleanup" => "cleanupErrors",
                        _ => panic!("Node ShutdownReport field mapping missing {field}"),
                    },
                    count,
                )
            })
            .collect();
        let boundary::JsShutdownReport {
            primary,
            cleanup_errors,
        } = ShutdownReport::from_core(Some(core)).to_js();
        assert!(
            matches!(primary, Either::A(ref error) if error.kind == "deviceLost"),
            "Node ShutdownReport primary must retain the core field"
        );
        assert_eq!(
            cleanup_errors
                .iter()
                .map(|e| e.kind.as_str())
                .collect::<Vec<_>>(),
            ["unsupported"],
            "Node ShutdownReport cleanup must retain the core field"
        );
        assert_eq!(
            expected,
            [("primary", 1), ("cleanupErrors", cleanup_errors.len())]
        );
    }

    fn attachment_failure() -> VadFailure {
        VadFailure {
            error: VadTapError::Vad(flexaudio_vad::VadError::Inference),
            stage: VadFailureStage::Process,
        }
    }

    #[test]
    fn attached_vad_failure_is_cleanup_only_and_keeps_its_typed_code() {
        let report = ShutdownReport {
            core: None,
            attachment: vec![attachment_failure()],
            binding_cleanup: Vec::new(),
            ..ShutdownReport::default()
        };
        let js = report.to_js();
        assert!(
            matches!(js.primary, Either::B(Null)),
            "an attached VAD-only failure must not be the capture primary"
        );
        assert_eq!(js.cleanup_errors.len(), 1);
        assert_eq!(js.cleanup_errors[0].kind, "backend");
        assert_eq!(js.cleanup_errors[0].vad_code.as_deref(), Some("Inference"));
        assert_eq!(js.cleanup_errors[0].contexts[0].operation, "normalize");
    }

    #[test]
    fn capture_primary_keeps_precedence_over_an_attached_vad_failure() {
        let report = ShutdownReport {
            core: Some(flexaudio::core::ShutdownReport::new(
                Some(flexaudio::Error::DeviceLost),
                Vec::new(),
            )),
            attachment: vec![attachment_failure()],
            binding_cleanup: Vec::new(),
            ..ShutdownReport::default()
        };
        let js = report.to_js();
        let Either::A(primary) = js.primary else {
            panic!("a real capture failure remains the primary")
        };
        assert_eq!(primary.kind, "deviceLost");
        assert_eq!(js.cleanup_errors.len(), 1);
        assert_eq!(js.cleanup_errors[0].vad_code.as_deref(), Some("Inference"));
    }

    #[test]
    fn a_stop_flush_vad_failure_is_cleanup_and_keeps_the_flush_context() {
        let report = ShutdownReport {
            core: None,
            attachment: vec![VadFailure {
                error: VadTapError::Conversion,
                stage: VadFailureStage::StopFlush,
            }],
            binding_cleanup: Vec::new(),
            ..ShutdownReport::default()
        };
        let js = report.to_js();
        assert!(matches!(js.primary, Either::B(Null)));
        assert_eq!(js.cleanup_errors.len(), 1);
        assert_eq!(js.cleanup_errors[0].vad_code.as_deref(), Some("Conversion"));
        assert_eq!(js.cleanup_errors[0].contexts[0].operation, "flush");
    }
    #[test]
    fn attachment_only_stop_succeeds_and_binding_cleanup_keeps_the_rejecting_root() {
        let mut report = ShutdownReport {
            core: None,
            attachment: vec![attachment_failure()],
            binding_cleanup: Vec::new(),
            ..ShutdownReport::default()
        };
        assert!(report.outcome().result().is_ok());
        report.binding_cleanup.push(flexaudio::Error::Backend(
            "the bridge could not join because its worker failed".into(),
        ));
        let error = report
            .outcome()
            .result()
            .expect_err("a bridge failure must still reject stop");
        let mapped = boundary::audio_error(&error);
        assert!(mapped.vad_code.is_none());
        assert_eq!(mapped.secondary.len(), 1);
        assert_eq!(mapped.secondary[0].vad_code.as_deref(), Some("Inference"));
    }

    #[test]
    fn entirely_new_cleanup_groups_preserve_their_original_shape() {
        let nested = flexaudio::Error::Multiple(flexaudio::ErrorGroup::new(
            flexaudio::Error::DeviceLost,
            flexaudio::Error::Unsupported,
            vec![],
        ));
        let incoming = flexaudio::Error::Multiple(flexaudio::ErrorGroup::new(
            flexaudio::Error::DeviceNotFound,
            nested,
            vec![flexaudio::Error::Backend("new cleanup failure".into())],
        ));
        let report = ShutdownReport {
            core: Some(flexaudio::ShutdownReport::new(
                Some(flexaudio::Error::Backend("capture failure".into())),
                vec![],
            )),
            binding_cleanup: vec![incoming.clone()],
            ..ShutdownReport::default()
        };
        assert_eq!(report.outcome().cleanup(), &[incoming]);
    }

    #[test]
    fn accepted_gc_notification_preserves_new_failures_after_an_earlier_sink_handoff() {
        const CHILD: &str = "FLEXAUDIO_NAPI_GC_HANDOFF_TEST";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "shutdown::tests::accepted_gc_notification_preserves_new_failures_after_an_earlier_sink_handoff",
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
        flexaudio_core::test_support::assert_distinct_failures_retained(
            flexaudio::Error::DeviceLost,
            flexaudio::Error::Backend("late GC cleanup failure".into()),
            |first, second| {
                let report = Arc::new(Mutex::new(Some(ShutdownReport::from_core(Some(
                    flexaudio::ShutdownReport::new(Some(first.clone()), vec![]),
                )))));
                report_unreachable_failure(&report, Status::QueueFull, "stream event callback");
                assert_eq!(received.lock().unwrap().len(), 1);
                record_cleanup_failure(&report, second.clone());
                report.lock().unwrap().as_mut().unwrap().gc_requested = true;
                report_unreachable_failure(&report, Status::Ok, "GC shutdown notice");
                // The accepted GC callback follows the same existing-sink branch as production.
                assert!(report_handed_off_gc_failure(&report));
                assert!(report_handed_off_gc_failure(&report));
                let mut received = received.lock().unwrap();
                assert_eq!(received.len(), 2, "equal snapshots must remain quiet");
                let later = received.pop().unwrap();
                assert!(later.report.primary().is_none());
                assert_eq!(later.report.cleanup(), std::slice::from_ref(&second));
                flexaudio::ShutdownReport::new(Some(first), later.report.cleanup().to_vec())
                    .result()
                    .unwrap_err()
            },
        );
        flexaudio::set_drop_failure_handler(None);
    }
}
