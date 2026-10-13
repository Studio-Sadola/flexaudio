//! Device-free interpretation of COM initialization and activation waits.

use flexaudio_core::{Error, ErrorContext, NativeStatus, Operation, Result};

pub(crate) fn com_initialization(status: i32) -> Result<bool> {
    match status as u32 {
        0 | 1 => Ok(true),
        0x8001_0106 => Ok(false), // RPC_E_CHANGED_MODE: another component owns COM.
        bits => Err(Error::Backend(format!(
            "COM initialization failed (HRESULT 0x{bits:08X}); retry capture or restart the host"
        ))
        .with_context(ErrorContext::new(Operation::Start).with_native_status(
            NativeStatus::HResult {
                call: "CoInitializeEx",
                bits,
            },
        ))),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WaitOutcome {
    Signaled,
    TimedOut,
    Failed { status: u32, os_error: Option<u32> },
}

pub(crate) fn wait_outcome(status: u32, os_error: Option<u32>) -> WaitOutcome {
    match status {
        0 => WaitOutcome::Signaled,
        258 => WaitOutcome::TimedOut,
        status => WaitOutcome::Failed { status, os_error },
    }
}

pub(crate) fn activation_wait(outcome: WaitOutcome) -> Result<()> {
    match outcome {
        WaitOutcome::Signaled => Ok(()),
        WaitOutcome::TimedOut => Err(Error::Backend(
            "process loopback activation timed out; retry capture".into(),
        )
        .with_context(ErrorContext::new(Operation::Start))),
        WaitOutcome::Failed { status, os_error } => {
            let mut context = ErrorContext::new(Operation::Start);
            if let Some(code) = os_error {
                let bits = if code == 0 || code & 0x8000_0000 != 0 {
                    code
                } else {
                    0x8007_0000 | (code & 0xffff)
                };
                context = context.with_native_status(NativeStatus::HResult {
                    call: "WaitForSingleObject",
                    bits,
                });
            }
            Err(Error::Backend(format!("process loopback activation wait failed (wait status 0x{status:08X}, OS code {os_error:?}); retry capture"))
                .with_context(context))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Native COM and invalid-handle waits cannot execute on Linux. These tests
    // inject their exact statuses into the helpers used by the native owner.
    #[test]
    fn failed_com_initialization_retains_hresult() {
        for bits in [0x8000_4005u32, 0x8007_0005] {
            let error = com_initialization(bits as i32).unwrap_err();
            assert_eq!(error.kind(), flexaudio_core::ErrorKind::Backend);
            let Error::Context { context, .. } = error else {
                panic!("missing COM context")
            };
            assert_eq!(
                context.native_status(),
                Some(NativeStatus::HResult {
                    call: "CoInitializeEx",
                    bits
                })
            );
        }
        assert_eq!(com_initialization(0), Ok(true));
        assert_eq!(com_initialization(1), Ok(true));
        assert_eq!(com_initialization(0x8001_0106u32 as i32), Ok(false));
    }

    #[test]
    fn failed_activation_wait_is_not_a_timeout() {
        let outcome = wait_outcome(u32::MAX, Some(6));
        assert_eq!(
            outcome,
            WaitOutcome::Failed {
                status: u32::MAX,
                os_error: Some(6)
            }
        );
        let error = activation_wait(outcome).unwrap_err();
        assert!(error.to_string().contains("wait failed"));
        assert!(!error.to_string().contains("timed out"));
        let Error::Context { context, .. } = error else {
            panic!("missing wait context")
        };
        assert_eq!(
            context.native_status(),
            Some(NativeStatus::HResult {
                call: "WaitForSingleObject",
                bits: 0x8007_0006
            })
        );
        assert_eq!(activation_wait(wait_outcome(0, None)), Ok(()));
        assert!(activation_wait(wait_outcome(258, None))
            .unwrap_err()
            .to_string()
            .contains("timed out"));
        assert!(matches!(
            wait_outcome(128, None),
            WaitOutcome::Failed {
                status: 128,
                os_error: None
            }
        ));
    }
}

// All endpoint acquisition/count stages share this per-endpoint policy.
pub(crate) fn endpoint_query<T>(result: Result<T>) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if endpoint_vanished(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn endpoint_vanished(error: &Error) -> bool {
    match error {
        Error::Context { source, context } => match context.native_status() {
            Some(NativeStatus::HResult { bits, .. }) => matches!(
                bits,
                0x8889_0004 // AUDCLNT_E_DEVICE_INVALIDATED
                | 0x8007_0057 // E_INVALIDARG
                | 0x8007_0490 // HRESULT_FROM_WIN32(ERROR_NOT_FOUND)
                | 0x8007_0002 // HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND)
            ),
            _ => endpoint_vanished(source),
        },
        Error::Diagnostic { source, .. } => endpoint_vanished(source),
        Error::Multiple(group) => endpoint_vanished(group.primary()),
        _ => false,
    }
}

#[cfg(test)]
mod endpoint_tests {
    use super::*;

    fn fault(bits: u32) -> Error {
        let cause = if bits == 0x8007_0005 {
            Error::PermissionDenied {
                permission: flexaudio_core::Permission::SystemAudio,
                detail: "endpoint access denied; retry discovery".into(),
            }
        } else {
            Error::Backend("endpoint query failed; retry discovery".into())
        };
        cause.with_context(ErrorContext::new(Operation::Enumerate).with_native_status(
            NativeStatus::HResult {
                call: "endpoint fixture",
                bits,
            },
        ))
    }

    // WASAPI endpoint invalidation cannot be triggered on Linux. Inject retained
    // HRESULTs into the helper used by Item/Activate/session enumeration/count.
    #[test]
    fn invalidated_endpoint_keeps_healthy_sessions() {
        let sessions = flexaudio_core::inventory::collect(Ok([0, 1]), |id| {
            endpoint_query(if id == 0 {
                Err(fault(0x8889_0004))
            } else {
                Ok(vec![42])
            })
        })
        .unwrap();
        assert_eq!(sessions, [vec![42]]);
    }

    #[test]
    fn one_endpoint_access_denied_retains_typed_cause() {
        let error = fault(0x8007_0005);
        let result = flexaudio_core::inventory::collect(Ok([0, 1]), |id| {
            endpoint_query(if id == 1 {
                Err(error.clone())
            } else {
                Ok(vec![42])
            })
        });
        assert_eq!(result, Err(error));
        assert_eq!(
            result.unwrap_err().kind(),
            flexaudio_core::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn device_item_query_uses_the_process_endpoint_policy() {
        // Item is Windows-only; verify its native wiring and inject failures below it.
        let source = include_str!("system.rs");
        let item = source.split("collection.Item(index)").nth(1).unwrap();
        assert!(item.starts_with(".map_err("));
        let scan = source
            .split("flexaudio_core::inventory::collect(Ok(0..count)")
            .nth(1)
            .unwrap();
        assert!(scan
            .split("endpoint_identity")
            .next()
            .unwrap()
            .contains("crate::native_outcomes::endpoint_query("));
        for bits in [0x8889_0004, 0x8007_0005, 0x8000_4005] {
            let result = flexaudio_core::inventory::collect(Ok([0, 1]), |id| {
                endpoint_query(if id == 0 { Err(fault(bits)) } else { Ok(id) })
            });
            if bits == 0x8889_0004 {
                assert_eq!(result, Ok(vec![1]));
            } else {
                assert_eq!(result, Err(fault(bits)));
            }
        }
    }

    #[test]
    fn only_invalidated_or_vanished_endpoint_errors_are_omitted() {
        for bits in [0x8889_0004, 0x8007_0057, 0x8007_0490, 0x8007_0002] {
            assert_eq!(endpoint_query::<u32>(Err(fault(bits))), Ok(None));
        }
        for bits in [0x8007_0005, 0x8000_4005, 0x8889_000a] {
            assert_eq!(endpoint_query::<u32>(Err(fault(bits))), Err(fault(bits)));
        }
    }
}
