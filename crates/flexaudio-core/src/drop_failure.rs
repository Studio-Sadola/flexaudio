//! Process-wide destination for failures whose owner can no longer return them.

use crate::{terminal_causes::retains_terminal_cause, Error, ShutdownReport};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex, RwLock};

/// Kind of owner that discovered a failure during implicit shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DropFailureOwner {
    /// Public capture stream.
    Stream,
    /// Device-change watcher.
    DeviceWatcher,
    /// Microphone capture backend.
    Microphone,
    /// Combined capture backend.
    Mix,
    /// Device-free capture backend used for verification.
    MockCapture,
    /// Native system capture backend.
    SystemCapture,
    /// Native process capture backend.
    ProcessCapture,
    /// Streaming FLAC writer.
    FlacWriter,
    /// Process query whose caller has already timed out.
    ProcessQuery,
    /// Capability probe whose caller has already timed out.
    CapabilityProbe,
}

/// Typed failures that an owner can no longer hand to its caller.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DropFailure {
    /// Owner that performed implicit cleanup (or an abandoned bounded query).
    pub owner: DropFailureOwner,
    /// Failures not previously handed to a caller or this receiver.
    pub report: ShutdownReport,
}

/// Process-wide implicit-shutdown receiver.
pub type DropFailureHandler = dyn Fn(&DropFailure) + Send + Sync + 'static;

static HANDLER: RwLock<Option<Arc<DropFailureHandler>>> = RwLock::new(None);

/// Tracks failures actually handed to a caller or the process receiver.
/// Internal cross-crate lifecycle support; no stability guarantee.
#[doc(hidden)]
#[derive(Default)]
pub struct ShutdownReceipt {
    handed_off: Mutex<Option<Error>>,
}

impl ShutdownReceipt {
    /// Acknowledge only the typed failure returned to a caller.
    pub fn acknowledge_error(&self, error: &Error) {
        let mut handed_off = self.handed_off.lock().unwrap_or_else(|e| e.into_inner());
        crate::terminal_causes::retain_terminal_cause(&mut handed_off, error.clone());
    }

    /// Acknowledge a complete report handed to a caller or retained by its parent owner.
    pub fn acknowledge(&self, report: &ShutdownReport) {
        let mut handed_off = self.handed_off.lock().unwrap_or_else(|e| e.into_inner());
        for error in report.primary().into_iter().chain(report.cleanup()) {
            crate::terminal_causes::retain_terminal_cause(&mut handed_off, error.clone());
        }
    }

    /// Route only failures not previously handed off, claiming them before invoking the receiver.
    pub fn report_unobserved(&self, owner: DropFailureOwner, report: &ShutdownReport) {
        let previous = {
            let mut handed_off = self.handed_off.lock().unwrap_or_else(|e| e.into_inner());
            let previous = handed_off
                .clone()
                .map(|error| ShutdownReport::new(Some(error), Vec::new()));
            for error in report.primary().into_iter().chain(report.cleanup()) {
                crate::terminal_causes::retain_terminal_cause(&mut handed_off, error.clone());
            }
            previous
        };
        // Receiver code may inspect its owner; never invoke it while holding the receipt lock.
        report_drop_failures(owner, previous.as_ref(), report);
    }
}

/// Replace the process-wide receiver for failures left unread during Drop.
///
/// `None` restores the default: one line per failure on stderr. The receiver
/// also receives late failures from bounded queries whose caller timed out.
/// It runs synchronously on the owner/worker thread, without holding the global
/// lock. A receiver panic is caught and the failures fall back to stderr.
/// Checked stop/finalize errors, returned failure events, and public shutdown
/// report reads hand their failures to the caller and suppress repeated Drop
/// reporting. A plain `stop()` with no returned error leaves unread failures
/// eligible for the Drop receiver.
pub fn set_drop_failure_handler(handler: Option<Box<DropFailureHandler>>) {
    let previous = {
        let mut slot = HANDLER.write().unwrap_or_else(|poison| poison.into_inner());
        std::mem::replace(&mut *slot, handler.map(Arc::from))
    };
    // Receiver destructors may themselves replace the receiver.
    drop(previous);
}

fn contains(report: &ShutdownReport, error: &Error) -> bool {
    report
        .primary()
        .into_iter()
        .chain(report.cleanup())
        .any(|retained| retains_terminal_cause(retained, error))
}

fn new_cause(error: &Error, previous: Option<&ShutdownReport>) -> Option<Error> {
    if previous.is_some_and(|report| contains(report, error)) {
        return None;
    }
    if let Error::Multiple(group) = error {
        let mut retained = None;
        for cause in std::iter::once(group.primary()).chain(group.secondary()) {
            if let Some(cause) = new_cause(cause, previous) {
                crate::terminal_causes::retain_terminal_cause(&mut retained, cause);
            }
        }
        return retained;
    }
    Some(error.clone())
}

/// Route newly observed owner failures through the shared receiver.
/// Internal cross-crate lifecycle support; no stability guarantee.
#[doc(hidden)]
pub fn report_drop_failures(
    owner: DropFailureOwner,
    previous: Option<&ShutdownReport>,
    report: &ShutdownReport,
) {
    let primary = report
        .primary()
        .and_then(|error| new_cause(error, previous));
    let cleanup = report
        .cleanup()
        .iter()
        .filter_map(|error| new_cause(error, previous))
        .collect::<Vec<_>>();
    if primary.is_none() && cleanup.is_empty() {
        return;
    }
    dispatch(&DropFailure {
        owner,
        report: ShutdownReport::new(primary, cleanup),
    });
}

fn dispatch(failure: &DropFailure) {
    let handler = HANDLER
        .read()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    deliver(failure, handler.as_deref());
}

fn deliver(failure: &DropFailure, handler: Option<&DropFailureHandler>) {
    if let Some(handler) = handler {
        if catch_unwind(AssertUnwindSafe(|| handler(failure))).is_ok() {
            return;
        }
    }
    fn write(owner: DropFailureOwner, error: &Error) {
        fn leaves(error: &Error) -> Vec<Error> {
            match error {
                Error::Multiple(group) => std::iter::once(group.primary())
                    .chain(group.secondary())
                    .flat_map(leaves)
                    .collect(),
                Error::Context { source, context } => leaves(source)
                    .into_iter()
                    .map(|error| error.with_context(*context))
                    .collect(),
                Error::Diagnostic { source, detail } => leaves(source)
                    .into_iter()
                    .map(|error| error.with_diagnostic_detail(detail.clone()))
                    .collect(),
                error => vec![error.clone()],
            }
        }
        for error in leaves(error) {
            // A closed stderr must not turn shutdown into a second panic.
            match catch_unwind(AssertUnwindSafe(|| {
                eprintln!("flexaudio {owner:?} implicit shutdown: {error}");
            })) {
                Ok(()) | Err(_) => {}
            }
        }
    }
    for error in failure
        .report
        .primary()
        .into_iter()
        .chain(failure.report.cleanup())
    {
        write(failure.owner, error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_retains_distinct_handoffs_and_reports_only_unread_causes() {
        const CHILD: &str = "FLEXAUDIO_SHUTDOWN_RECEIPT_TEST";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["drop_failure::tests::receipt_retains_distinct_handoffs_and_reports_only_unread_causes", "--exact"])
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
        let receipt = Arc::new(ShutdownReceipt::default());
        let observed = Arc::new(Mutex::new(Vec::new()));
        let output = observed.clone();
        let inspect = receipt.clone();
        set_drop_failure_handler(Some(Box::new(move |failure| {
            // The receiver may inspect or acknowledge its owner's receipt without deadlocking.
            assert!(inspect.handed_off.try_lock().is_ok());
            output.lock().unwrap().push(failure.report.clone());
        })));
        receipt.acknowledge_error(&Error::DeviceLost);
        let report = ShutdownReport::new(Some(Error::DeviceLost), vec![Error::Unsupported]);
        receipt.report_unobserved(DropFailureOwner::Stream, &report);
        receipt.report_unobserved(DropFailureOwner::Stream, &report);
        let later = ShutdownReport::new(
            Some(Error::DeviceLost),
            vec![Error::Unsupported, Error::DeviceNotFound],
        );
        receipt.report_unobserved(DropFailureOwner::Stream, &later);
        receipt.acknowledge(&later);
        receipt.report_unobserved(DropFailureOwner::Stream, &later);
        set_drop_failure_handler(None);
        assert_eq!(
            *observed.lock().unwrap(),
            [
                ShutdownReport::new(None, vec![Error::Unsupported]),
                ShutdownReport::new(None, vec![Error::DeviceNotFound]),
            ]
        );
        crate::test_support::assert_distinct_failures_retained(
            Error::DeviceLost,
            Error::Unsupported,
            |first, second| {
                let receipt = ShutdownReceipt::default();
                receipt.acknowledge_error(&first);
                receipt.acknowledge_error(&second);
                receipt.acknowledge_error(&first);
                receipt.handed_off.into_inner().unwrap().unwrap()
            },
        );
    }

    #[test]
    fn default_and_panicking_handler_never_unwind() {
        let failure = DropFailure {
            owner: DropFailureOwner::Stream,
            report: ShutdownReport::new(Some(Error::DeviceLost), vec![Error::Unsupported]),
        };
        deliver(&failure, None);
        deliver(&failure, Some(&|_| panic!("injected receiver panic")));
    }

    #[test]
    fn only_new_distinct_failures_are_routed() {
        let previous = ShutdownReport::new(Some(Error::DeviceLost), vec![]);
        let mut retained = Some(Error::DeviceLost);
        crate::terminal_causes::retain_terminal_cause(&mut retained, Error::Unsupported);
        assert_eq!(
            new_cause(&retained.unwrap(), Some(&previous)),
            Some(Error::Unsupported)
        );
        assert_eq!(new_cause(&Error::DeviceLost, Some(&previous)), None);
    }

    #[test]
    fn default_stderr_keeps_wrapped_related_vad_failure() {
        const CHILD: &str = "FLEXAUDIO_WRAPPED_DROP_TEST";
        if std::env::var_os(CHILD).is_some() {
            let error = Error::Multiple(crate::ErrorGroup::new(
                Error::DeviceLost,
                Error::Vad {
                    code: crate::VadErrorCode::Inference,
                },
                vec![],
            ))
            .with_context(crate::ErrorContext::new(crate::Operation::Stop))
            .with_diagnostic_detail("private diagnostic".into());
            deliver(
                &DropFailure {
                    owner: DropFailureOwner::Stream,
                    report: ShutdownReport::new(Some(error), vec![]),
                },
                None,
            );
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "drop_failure::tests::default_stderr_keeps_wrapped_related_vad_failure",
                "--exact",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        assert_eq!(
            stderr
                .lines()
                .filter(|line| line.starts_with("flexaudio Stream implicit shutdown:"))
                .count(),
            2,
            "{stderr}"
        );
        assert!(stderr.contains("device lost during stop"), "{stderr}");
        assert!(
            stderr.contains("model inference or reset could not complete"),
            "{stderr}"
        );
        assert!(!stderr.contains("private diagnostic"), "{stderr}");
    }
}
