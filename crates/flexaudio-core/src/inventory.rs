//! Shared discovery policy: collection failures are terminal; entry metadata is best effort.
use crate::{Error, ErrorContext, ErrorGroup, NativeStatus, Operation, Result};

/// Collect an inventory; a collection/count failure never becomes an empty success.
/// Entries without a usable identity may be omitted by the adapter.
pub fn collect<I, T>(
    ids: Result<impl IntoIterator<Item = I>>,
    mut read: impl FnMut(I) -> Option<T>,
) -> Result<Vec<T>> {
    Ok(ids?.into_iter().filter_map(&mut read).collect())
}

/// Discovery format and its availability. Zero/zero is the explicit unavailable sentinel.
pub fn device_format(format: Result<(u32, u16)>) -> (u32, u16, bool) {
    match format {
        Ok((rate, channels)) if rate > 0 && channels > 0 => (rate, channels, true),
        _ => (0, 0, false),
    }
}

/// Checked opening retains the original typed cause and explains how to retry.
pub fn require_device_format(format: Result<(u32, u16)>) -> Result<(u32, u16)> {
    let format = format.and_then(|format| {
        if format.0 == 0 || format.1 == 0 {
            Err(Error::UnsupportedFormat(
                "native device format contains a zero rate or channel count".into(),
            ))
        } else {
            Ok(format)
        }
    });
    format.map_err(|cause| Error::Multiple(ErrorGroup::new(
        Error::Backend("device capture format could not be read; reconnect the device and retry discovery/open".into())
            .with_context(ErrorContext::new(Operation::Start)),
        cause,
        Vec::new(),
    )))
}

/// Optional property failures become explicit unknown values.
pub fn optional<T>(value: Result<Option<T>>) -> Option<T> {
    value.ok().flatten()
}

/// Native process disappearance, distinguished from unreadable optional metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessFailure {
    /// The process/object disappeared; its entry may be omitted.
    Vanished,
    /// The process remains a candidate but its name/path is unknown.
    UnknownMetadata,
}

/// Windows: ERROR_INVALID_PARAMETER (87, HRESULT 0x80070057) means vanished.
/// ERROR_ACCESS_DENIED (5, 0x80070005), and all other metadata errors, mean unknown.
pub fn windows_process_failure(bits: u32) -> ProcessFailure {
    if bits == 0x8007_0057 {
        ProcessFailure::Vanished
    } else {
        ProcessFailure::UnknownMetadata
    }
}

/// macOS: kAudioHardwareBadObjectError ('!obj', 0x216f626a), ESRCH (3),
/// and ENOENT (2) mean vanished. Other property/path errors mean unknown metadata.
pub fn macos_process_failure(code: i32) -> ProcessFailure {
    if matches!(code, 0x216f_626a | 3 | 2) {
        ProcessFailure::Vanished
    } else {
        ProcessFailure::UnknownMetadata
    }
}

/// Classify a Windows process failure from its retained native HRESULT.
pub fn windows_error_failure(error: &Error) -> ProcessFailure {
    match error {
        Error::Context { source, context } => match context.native_status() {
            Some(NativeStatus::HResult { bits, .. }) => windows_process_failure(bits),
            _ => windows_error_failure(source),
        },
        Error::Diagnostic { source, .. } => windows_error_failure(source),
        Error::Multiple(group) => windows_error_failure(group.primary()),
        _ => ProcessFailure::UnknownMetadata,
    }
}

/// Interpret an optional process query: outer None means vanished; inner None means unknown.
/// Used by adapters before the inventory collector, so unknown metadata keeps its PID.
pub fn process_metadata<T>(
    value: Result<Option<T>>,
    classify: impl FnOnce(&Error) -> ProcessFailure,
) -> Option<Option<T>> {
    match value {
        Ok(value) => Some(value),
        Err(error) if classify(&error) == ProcessFailure::Vanished => None,
        Err(_) => Some(None),
    }
}

/// Classify a Core Audio property failure while retaining structured native status.
pub fn macos_error_failure(error: &Error) -> ProcessFailure {
    match error {
        Error::Context { source, context } => match context.native_status() {
            Some(NativeStatus::OsStatus { value, .. }) => macos_process_failure(value),
            _ => macos_error_failure(source),
        },
        Error::Diagnostic { source, .. } => macos_error_failure(source),
        Error::Multiple(group) => macos_error_failure(group.primary()),
        _ => ProcessFailure::UnknownMetadata,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fault(code: i32) -> Error {
        Error::Backend("fixture failed; retry".into()).with_context(
            ErrorContext::new(Operation::Enumerate).with_native_status(NativeStatus::OsStatus {
                call: "fixture",
                value: code,
            }),
        )
    }
    #[test]
    fn per_entry_format_failure_keeps_neighbors_and_uses_zero_sentinel() {
        let entries = collect(Ok([0, 1, 2]), |id| {
            Some(device_format(if id == 1 {
                Err(fault(-50))
            } else {
                Ok((44_100, 2))
            }))
        })
        .unwrap();
        assert_eq!(
            entries,
            [(44_100, 2, true), (0, 0, false), (44_100, 2, true)]
        );
        assert_eq!(device_format(Ok((0, 2))), (0, 0, false));
        let error = require_device_format(Err(fault(-50))).unwrap_err();
        assert_eq!(error.kind(), crate::ErrorKind::Backend);
        assert!(error.to_string().contains("could not be read"));
        assert!(error.to_string().contains("retry"));
        let Error::Multiple(group) = error else {
            panic!("missing typed cause")
        };
        assert_eq!(group.secondary().next(), Some(&fault(-50)));
    }
    #[test]
    fn optional_names_do_not_fail_the_inventory() {
        let entries = collect(Ok([0, 1, 2]), |id| {
            Some(optional(if id == 1 {
                Err(fault(-50))
            } else {
                Ok(Some("name"))
            }))
        })
        .unwrap();
        assert_eq!(entries, [Some("name"), None, Some("name")]);
    }
    #[test]
    fn collection_and_count_faults_fail_the_whole_call_with_cause() {
        for code in [-50, -1] {
            assert_eq!(
                collect::<u32, u32>(Err::<Vec<u32>, _>(fault(code)), Some),
                Err(fault(code))
            );
        }
    }
    #[test]
    fn native_process_fault_classes_keep_unknown_names_and_omit_vanished() {
        let windows = collect(Ok([0x8007_0057, 0x8007_0005, 0x8007_001f]), |bits| {
            let error = Error::Backend("fixture failed; retry".into()).with_context(
                ErrorContext::new(Operation::Enumerate).with_native_status(NativeStatus::HResult {
                    call: "OpenProcess",
                    bits,
                }),
            );
            process_metadata(Err::<Option<String>, _>(error), windows_error_failure)
        })
        .unwrap();
        assert_eq!(windows, [None, None]);
        let mac = collect(Ok([0x216f_626a, 3, 2, 13, 1, -50]), |code| {
            process_metadata(Err::<Option<String>, _>(fault(code)), macos_error_failure)
        })
        .unwrap();
        assert_eq!(mac, [None, None, None]);
    }
}
