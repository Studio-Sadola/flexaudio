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
/// delivery error reach the process receiver. Pending JS exceptions are cleared before returning.
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
        clear_pending_exception(env);
        failure_delivery::report_failed(
            owner,
            report,
            failure_delivery::queue_error("shutdown warning", error.status)
                .with_diagnostic_detail(error.reason),
        );
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

/// A pending exception would make the TSFN's Fatal strategy escalate to `uncaughtException`.
fn clear_pending_exception(env: &Env) {
    let mut pending = false;
    // SAFETY: called only on the live JS thread from a TSFN callback.
    let status = unsafe { sys::napi_is_exception_pending(env.raw(), &mut pending) };
    if status == sys::Status::napi_ok && pending {
        let mut exception = ptr::null_mut();
        // SAFETY: same JS thread; the cleared exception is intentionally not rethrown.
        unsafe { sys::napi_get_and_clear_last_exception(env.raw(), &mut exception) };
    }
}
