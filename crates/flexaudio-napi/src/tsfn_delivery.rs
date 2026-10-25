//! One checked boundary for native-to-JavaScript queue delivery.

use napi::threadsafe_function::{ErrorStrategy, ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi::Status;

#[must_use = "Queue refusal must be retained, retried, or routed to the failure sink."]
#[allow(
    clippy::disallowed_methods,
    reason = "Only this adapter converts the native TSFN status into a must-use Result."
)]
pub(super) fn call<T: 'static>(
    function: &ThreadsafeFunction<T, ErrorStrategy::Fatal>,
    value: T,
    mode: ThreadsafeFunctionCallMode,
) -> Result<(), Status> {
    match function.call(value, mode) {
        Status::Ok => Ok(()),
        status => Err(status),
    }
}
