//! Windows build-number gate. Process loopback requires build 20348 or later.
//!
//! The decision logic (build number → supported or not) is a pure function separate from OS
//! calls, so it can be unit-tested on non-Windows systems. Read the actual version with
//! `RtlGetVersion` (ntdll), which is not affected by compatibility mode.

#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

/// Minimum Windows build that supports process loopback (`ActivateAudioInterfaceAsync` and
/// `AUDIOCLIENT_ACTIVATION_PARAMS`). Windows 11 and Windows Server 2022 meet this requirement.
pub(crate) const MIN_PROCESS_LOOPBACK_BUILD: u32 = 20_348;

/// Whether `build` meets the minimum process-loopback requirement (20348).
///
/// Pure function separated from OS calls so it can be tested.
pub(crate) fn process_loopback_supported(build: u32) -> bool {
    build >= MIN_PROCESS_LOOPBACK_BUILD
}

#[cfg(target_os = "windows")]
mod query {
    use flexaudio_core::types::{Error, Result};
    use windows::Wdk::System::SystemServices::RtlGetVersion;
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;

    use super::process_loopback_supported;

    /// Get the running Windows build number with `RtlGetVersion`.
    ///
    /// Do not use `GetVersionEx`, which can be affected by a compatibility-mode manifest.
    /// If the version cannot be read, return [`Error::Backend`] (the caller fails closed and
    /// treats process loopback as unavailable).
    fn current_os_build() -> Result<u32> {
        let mut info = OSVERSIONINFOW {
            dwOSVersionInfoSize: core::mem::size_of::<OSVERSIONINFOW>() as u32,
            ..Default::default()
        };
        // SAFETY: `info` is a valid OSVERSIONINFOW. dwOSVersionInfoSize is the struct size.
        // RtlGetVersion is a stable ntdll API that bypasses compatibility layers and returns
        // the actual build number.
        let status = unsafe { RtlGetVersion(&mut info) };
        if status.is_err() {
            return Err(Error::Backend(format!(
                "RtlGetVersion failed (NTSTATUS {})",
                status.0
            )));
        }
        Ok(info.dwBuildNumber)
    }

    /// Check whether process loopback is available on this OS. Return
    /// [`Error::UnsupportedOsVersion`] for builds below 20348, or [`Error::Backend`] if the
    /// build number cannot be read. Enumeration ([`list_processes`](crate::list_processes))
    /// and capture ([`WasapiProcessBackend`](crate::WasapiProcessBackend)) use this function.
    pub(crate) fn ensure_process_loopback_supported() -> Result<()> {
        let build = current_os_build()?;
        if process_loopback_supported(build) {
            Ok(())
        } else {
            Err(Error::UnsupportedOsVersion)
        }
    }
}

#[cfg(target_os = "windows")]
pub(crate) use query::ensure_process_loopback_supported;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_below_20348_is_unsupported() {
        assert!(!process_loopback_supported(0));
        assert!(!process_loopback_supported(19_045));
        assert!(!process_loopback_supported(20_347));
    }

    #[test]
    fn build_20348_is_the_boundary() {
        assert!(process_loopback_supported(20_348));
    }

    #[test]
    fn newer_builds_are_supported() {
        assert!(process_loopback_supported(22_000));
        assert!(process_loopback_supported(26_000));
        assert!(process_loopback_supported(u32::MAX));
    }
}
