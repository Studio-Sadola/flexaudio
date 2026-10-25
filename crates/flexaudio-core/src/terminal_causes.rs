//! One retention rule for terminal causes observed after the first.
//!
//! The first terminal cause stays primary. A later cause that does not compare
//! equal to any retained cause joins it as a related cause of
//! [`Error::Multiple`], in observation order. Equal causes are kept once.

use crate::{Error, ErrorGroup};

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
