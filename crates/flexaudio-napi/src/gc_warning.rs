//! Process-warning delivery for failures first observed during GC teardown.
//!
//! Garbage collection has no caller to reject. Python reports the same situation through
//! `sys.unraisablehook` and keeps running; Node gets the same meaning through
//! `process.emitWarning`, so a nondeterministic GC moment never kills the process.
use super::*;

/// `warning.name` for every GC-observed shutdown failure (Stream and FlacEncoder).
pub(super) const WARNING_NAME: &str = "FlexaudioShutdownWarning";
/// `warning.code` for every GC-observed shutdown failure (Stream and FlacEncoder).
pub(super) const WARNING_CODE: &str = "FLEXAUDIO_GC_SHUTDOWN";

/// Emit a typed warning `Error`. `attach` adds the typed fields an explicit stop/finalize
/// would have delivered. If construction or delivery fails, the original typed report and
/// delivery error reach the process receiver. Exception cleanup failures are retained too.
pub(super) fn emit(
    env: &Env,
    message: &str,
    owner: flexaudio::DropFailureOwner,
    report: &flexaudio::ShutdownReport,
    attach: impl FnOnce(&mut JsObject) -> napi::Result<()>,
) {
    let warning = build(env, message).and_then(|mut warning| {
        attach(&mut warning)?;
        Ok(warning)
    });
    if let Err(error) = warning.and_then(|warning| call_emit_warning(env, warning)) {
        let delivery = warning_failure(
            error,
            || {
                let mut pending = false;
                // SAFETY: called only on the live JS thread from a TSFN callback.
                check_status!(unsafe { sys::napi_is_exception_pending(env.raw(), &mut pending) })?;
                Ok(pending)
            },
            || {
                let mut exception = ptr::null_mut();
                // SAFETY: same JS thread; the exception is intentionally not rethrown.
                check_status!(unsafe {
                    sys::napi_get_and_clear_last_exception(env.raw(), &mut exception)
                })
            },
        );
        failure_delivery::report_failed(owner, report, delivery);
    }
}

fn build(env: &Env, message: &str) -> napi::Result<JsObject> {
    let mut warning = env.create_error(NapiError::new(Status::GenericFailure, message))?;
    warning.set_named_property("name", WARNING_NAME)?;
    warning.set_named_property("code", WARNING_CODE)?;
    Ok(warning)
}

fn call_emit_warning(env: &Env, warning: JsObject) -> napi::Result<()> {
    let process: JsObject = env.get_global()?.get_named_property("process")?;
    let emit_warning: napi::JsFunction = process.get_named_property("emitWarning")?;
    emit_warning.call(Some(&process), &[warning])?;
    Ok(())
}

/// Retain warning refusal and exception cleanup failures through the same typed sink.
fn warning_failure(
    error: NapiError,
    inspect: impl FnOnce() -> napi::Result<bool>,
    clear: impl FnOnce() -> napi::Result<()>,
) -> flexaudio::Error {
    let mut retained = Some(
        failure_delivery::queue_error("shutdown warning", error.status)
            .with_diagnostic_detail(error.reason),
    );
    let cleanup = match inspect() {
        Ok(false) => None,
        Ok(true) => clear()
            .err()
            .map(|error| ("shutdown warning exception clearing", error)),
        // Inspection did not establish an exception to clear, so clearing is not attempted.
        Err(error) => Some(("shutdown warning exception inspection", error)),
    };
    if let Some((role, error)) = cleanup {
        flexaudio::core::terminal_causes::retain_terminal_cause(
            &mut retained,
            failure_delivery::queue_error(role, error.status).with_diagnostic_detail(error.reason),
        );
    }
    retained.expect("warning delivery failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warning_refusal_keeps_distinct_exception_inspection_and_clear_failures() {
        for inspection_failed in [false, true] {
            let role = if inspection_failed {
                "shutdown warning exception inspection"
            } else {
                "shutdown warning exception clearing"
            };
            flexaudio_core::test_support::assert_distinct_failures_retained(
                failure_delivery::queue_error("shutdown warning", Status::GenericFailure)
                    .with_diagnostic_detail("warning refusal".into()),
                failure_delivery::queue_error(role, Status::PendingException)
                    .with_diagnostic_detail("exception cleanup refusal".into()),
                |_first, _second| {
                    warning_failure(
                        NapiError::new(Status::GenericFailure, "warning refusal"),
                        || {
                            if inspection_failed {
                                Err(NapiError::new(
                                    Status::PendingException,
                                    "exception cleanup refusal",
                                ))
                            } else {
                                Ok(true)
                            }
                        },
                        || {
                            assert!(
                                !inspection_failed,
                                "failed inspection must not attempt clearing"
                            );
                            Err(NapiError::new(
                                Status::PendingException,
                                "exception cleanup refusal",
                            ))
                        },
                    )
                },
            );
        }
    }

    #[test]
    fn successful_exception_cleanup_adds_no_failure() {
        for pending in [false, true] {
            let retained = warning_failure(
                NapiError::new(Status::GenericFailure, "warning refusal"),
                || Ok(pending),
                || {
                    assert!(pending, "absent exceptions must not be cleared");
                    Ok(())
                },
            );
            assert_eq!(
                retained,
                failure_delivery::queue_error("shutdown warning", Status::GenericFailure)
                    .with_diagnostic_detail("warning refusal".into()),
            );
        }
    }
}
