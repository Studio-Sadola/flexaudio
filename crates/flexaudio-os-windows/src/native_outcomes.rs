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
