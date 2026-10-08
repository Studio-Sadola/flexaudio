//! Platform selection for microphone consent; capture never interprets OS strings.

use flexaudio_core::types::{Error, Result};

/// Whether the capture owner must watch undecided microphone consent.
pub(crate) fn preflight() -> Result<bool> {
    #[cfg(target_os = "macos")]
    {
        crate::mac_permission::preflight()
    }
    #[cfg(target_os = "windows")]
    {
        crate::windows_permission::preflight().map(|_| false)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Ok(false)
    }
}

pub(crate) fn can_query_format() -> bool {
    #[cfg(target_os = "macos")]
    {
        crate::mac_permission::can_query_format()
    }
    #[cfg(target_os = "windows")]
    {
        crate::windows_permission::preflight().is_ok()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        true
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
