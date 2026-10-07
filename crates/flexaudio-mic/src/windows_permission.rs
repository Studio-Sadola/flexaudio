//! Public Windows AppCapability adapter. No ConsentStore registry heuristics.

use windows::core::HSTRING;
use windows::Security::Authorization::AppCapabilityAccess::{
    AppCapability, AppCapabilityAccessStatus,
};

use crate::windows_policy::{self, Access, Provider};
use flexaudio_core::types::{Error, Result};

struct Native;

impl Provider for Native {
    fn access(&self) -> Result<Access> {
        // Keep each WinRT object local to the calling thread. The lowercase name
        // is the Windows microphone capability identifier.
        let capability = AppCapability::Create(&HSTRING::from("microphone"))
            .map_err(|error| Error::Backend(format!("create microphone capability: {error}")))?;
        let status = capability
            .CheckAccess()
            .map_err(|error| Error::Backend(format!("query microphone capability: {error}")))?;
        Ok(match status {
            AppCapabilityAccessStatus::Allowed => Access::Allowed,
            AppCapabilityAccessStatus::DeniedByUser => Access::DeniedByUser,
            AppCapabilityAccessStatus::DeniedBySystem => Access::DeniedBySystem,
            _ => Access::Unknown,
        })
    }
}

pub(crate) fn preflight() -> Result<()> {
    windows_policy::preflight(&Native).map(|_| ())
}

pub(crate) fn after_failure(error: Error) -> Error {
    windows_policy::after_failure(&Native, error)
}
