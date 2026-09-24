//! Windows build-number gate. Process loopback requires build 20348 or later.
//!
//! The core of the check (build number → supported or not) is a pure function decoupled
//! from OS calls, so it can be unit-tested on non-Windows targets. The actual version is
//! obtained with `RtlGetVersion` (ntdll), which compatibility mode cannot fake.

#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

/// Minimum Windows build on which process loopback (`ActivateAudioInterfaceAsync` +
/// `AUDIOCLIENT_ACTIVATION_PARAMS`) is available.
/// Windows 11 and Windows Server 2022 meet this number.
pub(crate) const MIN_PROCESS_LOOPBACK_BUILD: u32 = 20_348;

/// Whether `build` meets the minimum requirement (20348) for process loopback.
///
/// A pure function decoupled from OS calls so it can be tested.
pub(crate) fn process_loopback_supported(build: u32) -> bool {
    build >= MIN_PROCESS_LOOPBACK_BUILD
}

#[cfg(target_os = "windows")]
mod query {
    use flexaudio_core::types::{Error, Result};
    use windows::Wdk::System::SystemServices::RtlGetVersion;
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;

    use super::process_loopback_supported;

    /// Gets the running Windows build number with `RtlGetVersion`.
    ///
    /// `GetVersionEx` is not used because a compatibility-mode manifest can fake it.
    /// If it cannot be obtained, returns [`Error::Backend`] (the caller fails closed,
    /// treating process loopback as unavailable).
    fn current_os_build() -> Result<u32> {
        let mut info = OSVERSIONINFOW {
            dwOSVersionInfoSize: core::mem::size_of::<OSVERSIONINFOW>() as u32,
            ..Default::default()
        };
        // SAFETY: `info` is a valid OSVERSIONINFOW, and dwOSVersionInfoSize is the struct size.
        // RtlGetVersion is a stable ntdll API that bypasses the compatibility layer and
        // returns the real build.
        let status = unsafe { RtlGetVersion(&mut info) };
        if status.is_err() {
            return Err(Error::Backend(format!(
                "RtlGetVersion failed (NTSTATUS {})",
                status.0
            )));
        }
        Ok(info.dwBuildNumber)
    }

    /// Checks whether process loopback is available on this OS. Returns
    /// [`Error::UnsupportedOsVersion`] below build 20348, or [`Error::Backend`] when the
    /// build number cannot be obtained. Enumeration
    /// ([`list_processes`](crate::list_processes)) and capture
    /// ([`WasapiProcessBackend`](crate::WasapiProcessBackend)) use the same function.
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
