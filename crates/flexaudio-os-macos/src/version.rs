//! macOS version gate. Process Taps require macOS 14.4 or later.
//!
//! `start` of `MacSystemBackend` / `MacProcessBackend` passes this check before proceeding to
//! tap creation and returns [`Error::UnsupportedOsVersion`] below 14.4. Without it, on older
//! OSes `AudioHardwareCreateProcessTap` returns a raw `OSStatus` that turns into
//! `Error::Backend` via [`map_os_status`](crate::common::map_os_status), which is asymmetric
//! with the `UnsupportedOsVersion` path of the other OSes. Gating by type keeps the error
//! kinds consistent.
//!
//! # How the version is obtained
//! To avoid enabling the `NSProcessInfo` feature of `objc2-foundation`, Foundation's
//! `[[NSProcessInfo processInfo] operatingSystemVersion]` is called directly with `objc2`'s
//! `class!` / `msg_send!`. The returned `NSOperatingSystemVersion` (three `NSInteger`s:
//! major/minor/patch) is received into a layout-identical local mirror
//! [`NSOperatingSystemVersion`] (which implements `Encode`/`RefEncode` to support a struct
//! return value).

use objc2::encode::{Encode, Encoding, RefEncode};
use objc2::ffi::NSInteger;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};

use flexaudio_core::types::{Error, Result};

/// Major of the minimum macOS version required for Process Taps.
const MIN_MAJOR: i64 = 14;
/// Minor of the minimum macOS version required for Process Taps (14.4).
const MIN_MINOR: i64 = 4;

/// Local mirror whose layout matches Foundation's `NSOperatingSystemVersion`.
///
/// A `#[repr(C)]` struct of three `NSInteger`s (`i64` on 64-bit targets). Implements
/// `Encode`/`RefEncode` so that `msg_send!` can receive the struct return value of
/// `operatingSystemVersion` (encoded as an anonymous struct, like Foundation's
/// `NSOperatingSystemVersion`).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NSOperatingSystemVersion {
    major: NSInteger,
    minor: NSInteger,
    patch: NSInteger,
}

// SAFETY: A `#[repr(C)]` struct of three NSIntegers with the same layout as Foundation's
// `NSOperatingSystemVersion`. The encoding is likewise an anonymous struct ("?").
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

// SAFETY: Reference encoding for a type with the `Encode` above.
unsafe impl RefEncode for NSOperatingSystemVersion {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Self::ENCODING);
}

/// Whether `(major, minor)` meets the minimum requirement for Process Taps (14.4).
///
/// patch is not checked (Apple announced the feature as introduced in 14.4, so every 14.4.x
/// is OK). It is a pure function decoupled from OS calls so it can be tested.
fn meets_min_version(major: i64, minor: i64) -> bool {
    major > MIN_MAJOR || (major == MIN_MAJOR && minor >= MIN_MINOR)
}

/// Gets the running macOS version as `(major, minor)`.
///
/// Reads it by sending `[[NSProcessInfo processInfo] operatingSystemVersion]` directly.
/// Foundation is always linked, and the `NSProcessInfo` class always exists at runtime.
fn current_os_version() -> (i64, i64) {
    // SAFETY: The `NSProcessInfo` class is always present in Foundation. `processInfo` returns
    // an autoreleased singleton, but here we only send operatingSystemVersion to it right
    // away, so there is no need to retain it. `operatingSystemVersion` is a zero-argument
    // selector returning NSOperatingSystemVersion, received into the layout-matched local
    // mirror.
    unsafe {
        let cls = class!(NSProcessInfo);
        let process_info: *mut AnyObject = msg_send![cls, processInfo];
        let version: NSOperatingSystemVersion = msg_send![process_info, operatingSystemVersion];
        (version.major as i64, version.minor as i64)
    }
}

/// Checks whether Process Taps are available on this OS. Returns
/// [`Error::UnsupportedOsVersion`] below 14.4, and `Ok(())` otherwise.
///
/// Each backend's `start` calls this before proceeding to tap creation (CoreAudio calls).
/// This makes a failure on an older OS the typed `UnsupportedOsVersion` rather than a raw
/// `OSStatus` → `Error::Backend`.
pub(crate) fn ensure_process_tap_supported() -> Result<()> {
    let (major, minor) = current_os_version();
    if meets_min_version(major, minor) {
        Ok(())
    } else {
        Err(Error::UnsupportedOsVersion)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 14.3 is below the requirement (i.e. Unsupported).
    #[test]
    fn version_14_3_is_unsupported() {
        assert!(!meets_min_version(14, 3));
        assert!(!meets_min_version(14, 0));
        assert!(!meets_min_version(13, 9));
    }

    /// Exactly 14.4 meets the requirement (boundary).
    #[test]
    fn version_14_4_is_supported() {
        assert!(meets_min_version(14, 4));
    }

    /// Later versions such as 14.5 / 15.x / 26.x meet it.
    #[test]
    fn newer_versions_are_supported() {
        assert!(meets_min_version(14, 5));
        assert!(meets_min_version(15, 0));
        assert!(meets_min_version(26, 6));
    }

    /// A higher major meets it even with a smaller minor (15.0 > 14.4).
    #[test]
    fn higher_major_with_low_minor_is_supported() {
        assert!(meets_min_version(15, 0));
        assert!(meets_min_version(99, 0));
    }

    /// Getting the running OS version does not panic and returns a sane value
    /// (major >= 10). On macOS, both CI and real hardware are macOS 10 or later, so major has
    /// two digits.
    #[test]
    fn current_os_version_is_sane() {
        let (major, minor) = current_os_version();
        assert!(major >= 10, "unexpected macOS major version: {major}");
        assert!(minor >= 0, "unexpected macOS minor version: {minor}");
    }
}
