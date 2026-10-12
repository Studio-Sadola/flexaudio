//! Platform selection for microphone consent; capture never interprets OS strings.

use flexaudio_core::types::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Purpose {
    Capture,
    Enumeration,
}

#[cfg(any(target_os = "windows", test))]
fn windows_preflight(purpose: Purpose, capture_check: impl FnOnce() -> Result<()>) -> Result<()> {
    match purpose {
        Purpose::Enumeration => Ok(()),
        Purpose::Capture => capture_check(),
    }
}

/// Native queries execute only after the purpose-specific consent check succeeds.
pub(crate) fn query_after_preflight<T>(
    check: impl FnOnce() -> Result<()>,
    query: impl FnOnce() -> Result<T>,
) -> Result<T> {
    check()?;
    query()
}

/// Keep the consent check before every fallible HAL/native format query.
#[cfg(test)]
pub(crate) fn format_after_preflight(
    check: impl FnOnce() -> Result<()>,
    query: impl FnOnce() -> Option<(u32, u16)>,
) -> Result<(u32, u16)> {
    query_after_preflight(check, || Ok(query().unwrap_or(crate::FALLBACK_FORMAT)))
}

/// Apply the platform's consent policy for the requested native operation.
pub(crate) fn preflight(purpose: Purpose) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        crate::mac_permission::preflight(purpose)
    }
    #[cfg(target_os = "windows")]
    {
        windows_preflight(purpose, crate::windows_permission::preflight)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = purpose;
        Ok(())
    }
}

pub(crate) fn after_failure(error: Error) -> Error {
    #[cfg(target_os = "windows")]
    {
        crate::windows_permission::after_failure(error)
    }
    #[cfg(not(target_os = "windows"))]
    {
        error
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio_core::types::Permission;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn denial() -> Error {
        Error::PermissionDenied {
            permission: Permission::Microphone,
            detail: "synthetic capture consent denied".into(),
        }
    }

    #[test]
    fn windows_enumeration_never_consults_capture_consent() {
        let checks = AtomicUsize::new(0);
        let result = windows_preflight(Purpose::Enumeration, || {
            checks.fetch_add(1, Ordering::SeqCst);
            Err(denial())
        });
        assert_eq!(
            checks.load(Ordering::SeqCst),
            0,
            "Windows enumeration must not query capture capability"
        );
        assert_eq!(
            result,
            Ok(()),
            "enumeration preserves the native discovery path"
        );
    }

    #[test]
    fn windows_capture_still_consults_consent_and_preserves_denial() {
        let checks = AtomicUsize::new(0);
        let error = denial();
        let result = windows_preflight(Purpose::Capture, || {
            checks.fetch_add(1, Ordering::SeqCst);
            Err(error.clone())
        });
        assert_eq!(checks.load(Ordering::SeqCst), 1);
        assert_eq!(result, Err(error));
    }

    #[test]
    fn windows_native_adapter_uses_the_injected_purpose_policy() {
        let lf = include_str!("permission.rs").replace("\r\n", "\n");
        for source in [lf.clone(), lf.replace('\n', "\r\n")] {
            assert_windows_adapter_wiring(&source);
        }
    }

    fn assert_windows_adapter_wiring(source: &str) {
        // Windows checkouts may use CRLF; verify the same code on either checkout.
        let source = source.replace("\r\n", "\n");
        let windows = source
            .split("#[cfg(target_os = \"windows\")]\n    {")
            .nth(1)
            .expect("Windows consent adapter branch")
            .split("#[cfg(not(any(")
            .next()
            .expect("Windows adapter body");
        assert!(
            windows.contains("windows_preflight(purpose, crate::windows_permission::preflight)")
        );
    }

    #[test]
    fn windows_adapter_wiring_rejects_a_capture_policy_bypass() {
        let lf = include_str!("permission.rs").replace("\r\n", "\n");
        let bypassed = lf.replace(
            "windows_preflight(purpose, crate::windows_permission::preflight)",
            "windows_preflight(purpose, || Ok(()))",
        );
        for source in [bypassed.clone(), bypassed.replace('\n', "\r\n")] {
            assert!(
                std::panic::catch_unwind(|| assert_windows_adapter_wiring(&source)).is_err(),
                "the structural check must reject bypassed capture consent on either checkout"
            );
        }
    }
}
