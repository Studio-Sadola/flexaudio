//! Checked Promise completion and typed rescue when JavaScript cannot accept it.

use super::failure_delivery::report_stream_failure as record_failure;
use super::*;

/// Created and consumed on the JS thread; only its delivery crosses worker threads.
#[derive(Clone, Copy)]
pub(super) struct SendDeferred(sys::napi_deferred);
unsafe impl Send for SendDeferred {}
unsafe impl Sync for SendDeferred {}

pub(super) fn create_js_promise(env: &Env) -> napi::Result<(SendDeferred, JsObject)> {
    let mut deferred = ptr::null_mut();
    let mut promise = ptr::null_mut();
    // No deferred is parked yet; creation failure propagates directly to the JS caller.
    check_status!(unsafe { sys::napi_create_promise(env.raw(), &mut deferred, &mut promise) })?;
    Ok((SendDeferred(deferred), unsafe {
        JsObject::from_raw_unchecked(env.raw(), promise)
    }))
}

#[derive(Clone, Copy)]
struct NativeFailure {
    status: Status,
    raw: sys::napi_status,
}

impl From<sys::napi_status> for NativeFailure {
    fn from(raw: sys::napi_status) -> Self {
        Self {
            status: Status::from(raw),
            raw,
        }
    }
}

impl From<Status> for NativeFailure {
    fn from(status: Status) -> Self {
        Self {
            status,
            raw: status.into(),
        }
    }
}

fn native_error(role: &str, failure: impl Into<NativeFailure>) -> flexaudio::Error {
    let failure = failure.into();
    let error = failure_delivery::queue_error(role, failure.status);
    if failure.status == Status::Unknown {
        error.with_diagnostic_detail(format!("native N-API status code: {}", failure.raw))
    } else {
        error
    }
}

#[must_use = "Every native settlement or preparation failure must remain typed."]
fn checked(status: sys::napi_status, role: &str) -> Result<(), flexaudio::Error> {
    match Status::from(status) {
        Status::Ok => Ok(()),
        _ => Err(native_error(role, NativeFailure::from(status))),
    }
}

fn deliver(
    report: &FinalReport,
    result: Result<(), flexaudio::Error>,
    original: Option<flexaudio::Error>,
) {
    if let Err(error) = result {
        if let Some(original) = original {
            record_cleanup_failure(report, original);
        }
        record_failure(report, error);
    }
}

/// A pending exception refuses the call before consuming its deferred. Other failures
/// may consume it, so only pending-exception refusal can safely retry, once.
#[must_use = "A refused Promise completion must reach its retained report and failure receiver."]
fn settle_with<F: Into<NativeFailure>>(
    role: &str,
    mut settle: impl FnMut() -> Result<(), F>,
    mut clear_pending: impl FnMut() -> Result<(), F>,
) -> Result<(), flexaudio::Error> {
    match settle().map_err(Into::into) {
        Ok(()) => Ok(()),
        Err(failure) if failure.status == Status::PendingException => {
            let retry = clear_pending()
                .map_err(|status| native_error(&format!("{role} pending exception clear"), status))
                .and_then(|()| match settle().map_err(Into::into) {
                    Ok(()) => Ok(()),
                    Err(status) => {
                        let mut retained = Some(native_error(role, status));
                        if status.status == Status::PendingException {
                            // Exhausted retry still clears the JS exception, without reusing the deferred.
                            if let Err(status) = clear_pending() {
                                flexaudio::core::terminal_causes::retain_terminal_cause(
                                    &mut retained,
                                    native_error(
                                        &format!("{role} pending exception clear"),
                                        status,
                                    ),
                                );
                            }
                        }
                        Err(retained.expect("refusal remains retained"))
                    }
                });
            // A successful retry transparently handles an already pending JS exception.
            retry.map_err(|error| {
                let mut retained = Some(native_error(role, Status::PendingException));
                flexaudio::core::terminal_causes::retain_terminal_cause(&mut retained, error);
                retained.expect("pending refusal remains retained")
            })
        }
        Err(status) => Err(native_error(role, status)),
    }
}

#[derive(Clone, Copy)]
enum Completion {
    Resolve,
    Reject,
}

#[must_use = "Native Promise refusal must be handled before releasing its deferred."]
#[allow(
    clippy::disallowed_methods,
    reason = "Only this checked boundary may call native Promise resolve/reject."
)]
fn complete_status(
    env: sys::napi_env,
    deferred: SendDeferred,
    value: sys::napi_value,
    completion: Completion,
) -> Result<(), NativeFailure> {
    let status = unsafe {
        match completion {
            Completion::Resolve => sys::napi_resolve_deferred(env, deferred.0, value),
            Completion::Reject => sys::napi_reject_deferred(env, deferred.0, value),
        }
    };
    match Status::from(status) {
        Status::Ok => Ok(()),
        _ => Err(NativeFailure::from(status)),
    }
}

#[must_use = "A refused Promise completion must reach its retained report and failure receiver."]
fn complete(
    env: sys::napi_env,
    deferred: SendDeferred,
    value: sys::napi_value,
    completion: Completion,
    role: &str,
) -> Result<(), flexaudio::Error> {
    settle_with(
        role,
        || complete_status(env, deferred, value, completion),
        || {
            let mut pending = ptr::null_mut();
            let status = unsafe { sys::napi_get_and_clear_last_exception(env, &mut pending) };
            match Status::from(status) {
                Status::Ok => Ok(()),
                _ => Err(NativeFailure::from(status)),
            }
        },
    )
}

#[must_use = "A missing completion value must not abandon its Promise silently."]
fn undefined(env: sys::napi_env, role: &str) -> Result<sys::napi_value, flexaudio::Error> {
    let mut value = ptr::null_mut();
    checked(unsafe { sys::napi_get_undefined(env, &mut value) }, role)?;
    Ok(value)
}

#[must_use = "Exception inspection and clearing failures must be retained."]
fn clear_pending(env: sys::napi_env) -> Result<(), flexaudio::Error> {
    let mut pending = false;
    checked(
        unsafe { sys::napi_is_exception_pending(env, &mut pending) },
        "promise exception inspection",
    )?;
    if pending {
        let mut exception = ptr::null_mut();
        checked(
            unsafe { sys::napi_get_and_clear_last_exception(env, &mut exception) },
            "promise exception clear",
        )?;
    }
    Ok(())
}

/// Keep the allocation-fallback policy injectable without requiring a live Node host.
#[must_use = "Fallback rejection must complete or report its failure."]
fn reject_with_fallback<T, E>(
    rich: Result<T, E>,
    fallback: impl FnOnce(E) -> Result<T, flexaudio::Error>,
    reject: impl FnOnce(T) -> Result<(), flexaudio::Error>,
) -> Result<(), flexaudio::Error> {
    reject(match rich {
        Ok(value) => value,
        Err(error) => fallback(error)?,
    })
}

fn fallback_value(
    env: &Env,
    report: &FinalReport,
    role: &str,
    error: NapiError,
) -> Result<sys::napi_value, flexaudio::Error> {
    record_failure(
        report,
        native_error(&format!("{role} error construction"), error.status),
    );
    clear_pending(env.raw())?;
    match env.create_error(error) {
        Ok(error) => return Ok(unsafe { error.raw() }),
        Err(error) => record_failure(
            report,
            native_error("promise fallback error construction", error.status),
        ),
    }
    clear_pending(env.raw())?;
    let message = b"flexaudio Promise failed while creating its error";
    let mut text = ptr::null_mut();
    if let Err(error) = checked(
        unsafe {
            sys::napi_create_string_utf8(
                env.raw(),
                message.as_ptr().cast(),
                message.len(),
                &mut text,
            )
        },
        "promise fallback message construction",
    ) {
        record_failure(report, error);
        clear_pending(env.raw())?;
        return undefined(env.raw(), "promise fallback undefined value");
    }
    let mut plain = ptr::null_mut();
    match checked(
        unsafe { sys::napi_create_error(env.raw(), ptr::null_mut(), text, &mut plain) },
        "promise plain error construction",
    ) {
        Ok(()) => Ok(plain),
        Err(error) => {
            record_failure(report, error);
            clear_pending(env.raw())?;
            // The string was successfully created and remains a valid rejection value.
            Ok(text)
        }
    }
}

fn reject_exception(
    env: &Env,
    deferred: SendDeferred,
    report: &FinalReport,
    role: &str,
    original: flexaudio::Error,
    rich: napi::Result<JsObject>,
) {
    let result = reject_with_fallback(
        rich.map(|error| unsafe { error.raw() }),
        |error| {
            record_cleanup_failure(report, original.clone());
            fallback_value(env, report, role, error)
        },
        |value| complete(env.raw(), deferred, value, Completion::Reject, role),
    );
    deliver(report, result, Some(original));
}

pub(super) fn settle_stop(env: sys::napi_env, deferred: SendDeferred, report: &FinalReport) {
    let failure = {
        let retained = report.lock().unwrap_or_else(lock_poisoned);
        match retained.as_ref() {
            Some(report) => report.outcome().result().err(),
            None => Some(flexaudio::Error::InvalidState(
                "shutdown report missing".into(),
            )),
        }
    };
    // Snapshot ownership is released before JS object construction or receiver calls.
    let env = unsafe { Env::from_raw(env) };
    match failure {
        Some(error) => {
            let rich = boundary::js_error(&env, &error);
            reject_exception(
                &env,
                deferred,
                report,
                "stop promise rejection",
                error,
                rich,
            );
        }
        None => deliver(
            report,
            undefined(env.raw(), "stop promise undefined value").and_then(|value| {
                complete(
                    env.raw(),
                    deferred,
                    value,
                    Completion::Resolve,
                    "stop promise resolution",
                )
            }),
            None,
        ),
    }
}

pub(super) fn settle_flush(
    env: &Env,
    deferred: SendDeferred,
    error: Option<flexaudio_vad::VadTapError>,
    report: &FinalReport,
) {
    match error {
        Some(error) => {
            let rich = attachment::js_error(env, &error);
            // Successful rejection leaves the attachment's existing report shape unchanged.
            reject_exception(
                env,
                deferred,
                report,
                "VAD flush promise rejection",
                error.audio_error(),
                rich,
            );
        }
        None => deliver(
            report,
            undefined(env.raw(), "VAD flush promise undefined value").and_then(|value| {
                complete(
                    env.raw(),
                    deferred,
                    value,
                    Completion::Resolve,
                    "VAD flush promise resolution",
                )
            }),
            None,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silent_failure_failed_error_creation_still_rejects_once() {
        let mut rejected = Vec::new();
        reject_with_fallback(
            Err::<&str, _>(()),
            |_| Ok("plain fallback"),
            |value| {
                rejected.push(value);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(rejected, ["plain fallback"]);
    }

    #[test]
    fn pending_exception_retry_is_transparent_and_other_refusals_never_retry() {
        let mut attempts = 0;
        settle_with(
            "test promise",
            || {
                attempts += 1;
                if attempts == 1 {
                    Err(Status::PendingException)
                } else {
                    Ok(())
                }
            },
            || Ok(()),
        )
        .unwrap();
        assert_eq!(attempts, 2);
        for status in [Status::GenericFailure, Status::Closing] {
            let mut attempts = 0;
            assert!(settle_with(
                "test promise",
                || {
                    attempts += 1;
                    Err(status)
                },
                || panic!("nonpending refusal must not clear or retry")
            )
            .is_err());
            assert_eq!(attempts, 1);
        }
        flexaudio_core::test_support::assert_distinct_failures_retained(
            native_error("test promise", Status::PendingException),
            native_error(
                "test promise pending exception clear",
                Status::GenericFailure,
            ),
            |_first, _second| {
                settle_with(
                    "test promise",
                    || Err(Status::PendingException),
                    || Err(Status::GenericFailure),
                )
                .unwrap_err()
            },
        );
        flexaudio_core::test_support::assert_distinct_failures_retained(
            native_error("test promise", Status::PendingException),
            native_error("test promise", Status::GenericFailure),
            |_first, _second| {
                let mut attempts = 0;
                settle_with(
                    "test promise",
                    || {
                        attempts += 1;
                        Err(if attempts == 1 {
                            Status::PendingException
                        } else {
                            Status::GenericFailure
                        })
                    },
                    || Ok(()),
                )
                .unwrap_err()
            },
        );
        let mut attempts = 0;
        let mut clears = 0;
        let error = settle_with(
            "test promise",
            || {
                attempts += 1;
                Err(Status::PendingException)
            },
            || {
                clears += 1;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(attempts, 2);
        assert_eq!(clears, 2);
        assert_eq!(
            error,
            native_error("test promise", Status::PendingException)
        );
    }

    #[test]
    fn unknown_native_status_codes_remain_distinct() {
        flexaudio_core::test_support::assert_distinct_failures_retained(
            native_error("test promise", NativeFailure::from(23)),
            native_error("test promise", NativeFailure::from(24)),
            |first, _second| {
                let later = settle_with(
                    "test promise",
                    || Err(NativeFailure::from(24)),
                    || panic!("unknown refusal cannot safely retry"),
                )
                .unwrap_err();
                let mut retained = Some(first);
                flexaudio::core::terminal_causes::retain_terminal_cause(&mut retained, later);
                retained.unwrap()
            },
        );
    }

    #[test]
    fn refused_settlement_preserves_original_and_distinct_delivery_failures() {
        const CHILD: &str = "FLEXAUDIO_NAPI_PROMISE_SETTLEMENT_TEST";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["promise_settlement::tests::refused_settlement_preserves_original_and_distinct_delivery_failures", "--exact"])
                .env(CHILD, "1").output().unwrap();
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
        for status in [Status::GenericFailure, Status::Closing] {
            flexaudio_core::test_support::assert_distinct_failures_retained(
                flexaudio::Error::DeviceLost,
                native_error("stop promise rejection", status),
                |first, _second| {
                    let report = Arc::new(Mutex::new(Some(shutdown::ShutdownReport::from_core(
                        Some(flexaudio::ShutdownReport::new(Some(first.clone()), vec![])),
                    ))));
                    for _attempt in 0..2 {
                        deliver(
                            &report,
                            settle_with(
                                "stop promise rejection",
                                || Err(status),
                                || panic!("unsafe retry"),
                            ),
                            Some(first.clone()),
                        );
                    }
                    let mut notifications = received.lock().unwrap();
                    assert_eq!(notifications.len(), 1, "equal failures must remain quiet");
                    let failure = notifications.pop().unwrap();
                    drop(notifications);
                    deliver(
                        &report,
                        settle_with(
                            "stop promise rejection",
                            || Err(Status::InvalidArg),
                            || panic!("unsafe retry"),
                        ),
                        Some(first),
                    );
                    let distinct = received.lock().unwrap().pop().unwrap();
                    assert_eq!(
                        distinct.report.cleanup(),
                        &[native_error("stop promise rejection", Status::InvalidArg)]
                    );
                    failure.report.result().unwrap_err()
                },
            );
        }
        let clean = Arc::new(Mutex::new(None));
        deliver(
            &clean,
            settle_with(
                "stop promise resolution",
                || Err(Status::Closing),
                || panic!("unsafe retry"),
            ),
            None,
        );
        let clean_failure = received.lock().unwrap().pop().unwrap();
        assert!(clean_failure.report.primary().is_none());
        assert_eq!(
            clean_failure.report.cleanup(),
            &[native_error("stop promise resolution", Status::Closing)]
        );
        flexaudio::set_drop_failure_handler(None);
    }
}
