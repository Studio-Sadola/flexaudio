//! One retention rule for terminal causes observed after the first.
//!
//! The first terminal cause stays primary. A later cause that does not compare
//! equal to any retained cause joins it as a related cause of
//! [`Error::Multiple`], in observation order. Equal causes are kept once.

use crate::{Error, ErrorGroup, ShutdownReport};

/// Merge retained shutdown observations without replacing either report's primary failure.
/// Previous causes keep their order; unequal incoming primary and cleanup causes remain typed.
/// Internal cross-crate lifecycle support; no stability guarantee.
#[doc(hidden)]
pub fn merge_shutdown_reports(
    previous: Option<&ShutdownReport>,
    incoming: &ShutdownReport,
) -> ShutdownReport {
    let mut primary = previous.and_then(ShutdownReport::primary).cloned();
    if let Some(error) = incoming.primary() {
        retain_terminal_cause(&mut primary, error.clone());
    }
    let mut cleanup = previous
        .map(|report| report.cleanup().to_vec())
        .unwrap_or_default();
    for error in incoming.cleanup() {
        if !cleanup.contains(error) {
            cleanup.push(error.clone());
        }
    }
    ShutdownReport::new(primary, cleanup)
}

/// Preserve an untrusted legacy payload without exposing it in safe Display.
/// Internal adapter support; no stability guarantee.
#[doc(hidden)]
pub fn legacy_capture_error(detail: String) -> Error {
    Error::Backend("backend reported a failure through a legacy error event".into())
        .with_diagnostic_detail(detail)
}

/// Retain `error` beside the terminal failure already held in `retained`.
///
/// Returns `true` only when `error` became the first retained cause, so the
/// caller can publish exactly one terminal notification per capture.
pub fn retain_terminal_cause(retained: &mut Option<Error>, error: Error) -> bool {
    if let Error::Multiple(group) = error {
        let first = retain_terminal_cause(retained, group.primary().clone());
        for cause in group.secondary() {
            retain_terminal_cause(retained, cause.clone());
        }
        return first;
    }
    let Some(previous) = retained.take() else {
        *retained = Some(error);
        return true;
    };
    if previous == error {
        *retained = Some(previous);
        return false;
    }
    let (primary, mut related) = match previous {
        Error::Multiple(group) => (
            group.primary().clone(),
            group.secondary().cloned().collect::<Vec<_>>(),
        ),
        primary => (primary, Vec::new()),
    };
    if error != primary && !related.contains(&error) {
        related.push(error);
    }
    *retained = Some(if related.is_empty() {
        primary
    } else {
        let first = related.remove(0);
        Error::Multiple(ErrorGroup::new(primary, first, related))
    });
    false
}

/// Whether `error` equals the retained failure or one of its grouped causes.
pub fn retains_terminal_cause(retained: &Error, error: &Error) -> bool {
    if retained == error {
        return true;
    }
    match retained {
        Error::Multiple(group) => {
            retains_terminal_cause(group.primary(), error)
                || group
                    .secondary()
                    .any(|cause| retains_terminal_cause(cause, error))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_merge_preserves_distinct_primaries_cleanup_and_repeat_identity() {
        let previous = ShutdownReport::new(Some(Error::DeviceLost), vec![Error::Unsupported]);
        let incoming = ShutdownReport::new(
            Some(Error::DeviceNotFound),
            vec![
                Error::Unsupported,
                Error::Vad {
                    code: crate::VadErrorCode::Inference,
                },
            ],
        );
        let merged = merge_shutdown_reports(Some(&previous), &incoming);
        crate::test_support::assert_distinct_failures_retained(
            Error::DeviceLost,
            Error::DeviceNotFound,
            |_, _| merged.primary().unwrap().clone(),
        );
        assert_eq!(merged.cleanup(), incoming.cleanup());
        assert_eq!(merge_shutdown_reports(Some(&merged), &incoming), merged);
        assert_eq!(merge_shutdown_reports(None, &incoming), incoming);
        let startup = ShutdownReport::new(
            Some(Error::InvalidState("startup refused".into())),
            Vec::new(),
        );
        let returned = merge_shutdown_reports(Some(&startup), &incoming)
            .result()
            .unwrap_err();
        assert!(retains_terminal_cause(&returned, &Error::DeviceNotFound));
        assert!(retains_terminal_cause(
            &returned,
            &Error::Vad {
                code: crate::VadErrorCode::Inference
            }
        ));
    }

    #[test]
    fn newer_aggregate_snapshots_merge_without_hiding_distinct_causes() {
        crate::test_support::assert_distinct_failures_retained(
            Error::DeviceLost,
            Error::Unsupported,
            |first, second| {
                let mut retained = Some(first.clone());
                assert!(!retain_terminal_cause(
                    &mut retained,
                    Error::Multiple(ErrorGroup::new(first, second, vec![]))
                ));
                retained.unwrap()
            },
        );
    }

    #[test]
    fn later_distinct_causes_join_the_first_and_equal_causes_collapse() {
        let mut retained = None;
        assert!(retain_terminal_cause(&mut retained, Error::DeviceLost));
        assert!(!retain_terminal_cause(&mut retained, Error::DeviceLost));
        assert_eq!(retained, Some(Error::DeviceLost));
        let second = Error::Backend("second".into());
        let third = Error::Unsupported;
        assert!(!retain_terminal_cause(&mut retained, second.clone()));
        assert!(!retain_terminal_cause(&mut retained, Error::DeviceLost));
        assert!(!retain_terminal_cause(&mut retained, second.clone()));
        assert!(!retain_terminal_cause(&mut retained, third.clone()));
        let expected = Error::Multiple(ErrorGroup::new(
            Error::DeviceLost,
            second.clone(),
            vec![third.clone()],
        ));
        assert_eq!(retained, Some(expected.clone()));
        for cause in [Error::DeviceLost, second, third, expected.clone()] {
            assert!(retains_terminal_cause(&expected, &cause));
        }
        assert!(!retains_terminal_cause(&expected, &Error::DeviceNotFound));
    }
}
