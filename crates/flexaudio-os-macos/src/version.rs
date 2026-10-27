//! macOS version gate. Process Tap requires macOS 14.4 or later.
//!
//! `MacSystemBackend` / `MacProcessBackend` run this check before creating a tap and return
//! [`Error::UnsupportedOsVersion`] on versions earlier than 14.4. Without the check, older OS
//! versions return a raw `OSStatus` from `AudioHardwareCreateProcessTap`, which becomes
//! `Error::Backend` through [`map_os_status`](crate::common::map_os_status) and differs from the
//! `UnsupportedOsVersion` path on other OSes. The typed gate keeps error kinds consistent.
//!
//! # Version lookup
//! To avoid enabling the `NSProcessInfo` feature in `objc2-foundation`, call Foundation's
//! `[[NSProcessInfo processInfo] operatingSystemVersion]` directly with `objc2`'s `class!` /
//! `msg_send!`. Receive the returned `NSOperatingSystemVersion` (three `NSInteger` values for
//! major/minor/patch) in the layout-matched local mirror [`NSOperatingSystemVersion`], which
//! implements `Encode`/`RefEncode` for the struct return value.

#[cfg(target_os = "macos")]
use objc2::encode::{Encode, Encoding, RefEncode};
#[cfg(target_os = "macos")]
use objc2::ffi::NSInteger;
#[cfg(target_os = "macos")]
use objc2::runtime::AnyObject;
#[cfg(target_os = "macos")]
use objc2::{class, msg_send};

#[cfg(target_os = "macos")]
use flexaudio_core::types::{Error, Result};

/// Minimum required macOS major version for Process Tap.
pub(crate) const MIN_MAJOR: u32 = 14;
/// Minimum required macOS minor version for Process Tap (14.4).
pub(crate) const MIN_MINOR: u32 = 4;

/// Local mirror with the same layout as Foundation's `NSOperatingSystemVersion`.
///
/// A `#[repr(C)]` struct of three `NSInteger` values (`i64` on 64-bit targets). Implements
/// `Encode`/`RefEncode` to receive the struct return from `operatingSystemVersion` via `msg_send!`
/// (encoded as an anonymous struct, like Foundation's `NSOperatingSystemVersion`).
#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NSOperatingSystemVersion {
    major: NSInteger,
    minor: NSInteger,
    patch: NSInteger,
}

// SAFETY: This `#[repr(C)]` struct has three NSInteger values and the same layout as Foundation's
// `NSOperatingSystemVersion`. It is also encoded as an anonymous struct ("?").
#[cfg(target_os = "macos")]
unsafe impl Encode for NSOperatingSystemVersion {
    const ENCODING: Encoding = Encoding::Struct(
        "?",
        &[
            <NSInteger>::ENCODING,
            <NSInteger>::ENCODING,
            <NSInteger>::ENCODING,
        ],
    );
}

// SAFETY: This is a reference encoding for the type with the `Encode` implementation above.
#[cfg(target_os = "macos")]
unsafe impl RefEncode for NSOperatingSystemVersion {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Self::ENCODING);
}

/// Whether `(major, minor)` meets Process Tap's minimum requirement (14.4).
///
/// Ignores the patch version (Apple announced the feature for 14.4, so all 14.4.x versions qualify).
/// Kept as a pure function separate from OS calls so it can be tested.
pub(crate) fn meets_min_version(major: u32, minor: u32) -> bool {
    major > MIN_MAJOR || (major == MIN_MAJOR && minor >= MIN_MINOR)
}

/// Get the running macOS version without requesting capture permission.
///
/// Reads `[[NSProcessInfo processInfo] operatingSystemVersion]` directly. Foundation is always
/// linked, so the `NSProcessInfo` class is always present at runtime.
#[cfg(target_os = "macos")]
pub(crate) fn current_os_version() -> Result<flexaudio_core::OsVersion> {
    // SAFETY: The `NSProcessInfo` class is always available in Foundation. `processInfo` returns an
    // autoreleased singleton, but we only send `operatingSystemVersion` immediately, so retaining it
    // is unnecessary. `operatingSystemVersion` is a zero-argument selector returning
    // NSOperatingSystemVersion, received into the layout-matched local mirror.
    unsafe {
        let cls = class!(NSProcessInfo);
        let process_info: *mut AnyObject = msg_send![cls, processInfo];
        let version: NSOperatingSystemVersion = msg_send![process_info, operatingSystemVersion];
        let invalid_version = |_| Error::Backend("invalid operating system version".into());
        Ok(flexaudio_core::OsVersion::new(
            u32::try_from(version.major).map_err(invalid_version)?,
            u32::try_from(version.minor).map_err(invalid_version)?,
            Some(u32::try_from(version.patch).map_err(invalid_version)?),
            None,
        ))
    }
}

/// Check whether Process Tap is available on this OS. Returns
/// [`Error::UnsupportedOsVersion`] before 14.4, or `Ok(())` when supported.
///
/// Each backend calls this before creating a tap (a CoreAudio call). This makes failures on older
/// OS versions return the typed `UnsupportedOsVersion` instead of raw `OSStatus` -> `Error::Backend`.
#[cfg(target_os = "macos")]
pub(crate) fn ensure_process_tap_supported() -> Result<()> {
    let version = current_os_version()?;
    if meets_min_version(version.major, version.minor) {
        Ok(())
    } else {
        Err(Error::UnsupportedOsVersion)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 14.3 is below the requirement (equivalent to Unsupported).
    #[test]
    fn version_14_3_is_unsupported() {
        assert!(!meets_min_version(14, 3));
        assert!(!meets_min_version(14, 0));
        assert!(!meets_min_version(13, 9));
    }

    /// Exactly 14.4 meets the requirement (boundary case).
    #[test]
    fn version_14_4_is_supported() {
        assert!(meets_min_version(14, 4));
    }

    /// 14.5 / 15.x / 26.x and later meet the requirement.
    #[test]
    fn newer_versions_are_supported() {
        assert!(meets_min_version(14, 5));
        assert!(meets_min_version(15, 0));
        assert!(meets_min_version(26, 6));
    }

    /// A higher major version qualifies even if its minor version is lower (15.0 > 14.4).
    #[test]
    fn higher_major_with_low_minor_is_supported() {
        assert!(meets_min_version(15, 0));
        assert!(meets_min_version(99, 0));
    }

    /// The running OS version lookup does not panic and returns a plausible value (major >= 10).
    /// CI and real hardware running macOS are version 10 or later, so the major is at least 10.
    #[cfg(target_os = "macos")]
    #[test]
    fn current_os_version_is_sane() {
        let version = current_os_version().expect("valid native OS version");
        assert!(version.major >= 10, "unexpected macOS version: {version:?}");
        assert!(version.patch.is_some());
    }
}
