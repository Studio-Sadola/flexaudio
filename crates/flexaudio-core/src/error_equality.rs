//! Preserve error equivalence even when rejected requests contain NaN gains.

use crate::{Error, SettingValue, SettingsReport};

impl PartialEq for Error {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::IgnoredSettings { report: a }, Self::IgnoredSettings { report: b }) => {
                reports_equal(a, b)
            }
            (Self::InvalidArg(a), Self::InvalidArg(b))
            | (Self::InvalidState(a), Self::InvalidState(b))
            | (Self::Backend(a), Self::Backend(b))
            | (Self::UnsupportedFormat(a), Self::UnsupportedFormat(b)) => a == b,
            (Self::DeviceNotFound, Self::DeviceNotFound)
            | (Self::UnsupportedOsVersion, Self::UnsupportedOsVersion)
            | (Self::DeviceLost, Self::DeviceLost)
            | (Self::Unsupported, Self::Unsupported)
            | (Self::AmbiguousDeviceName, Self::AmbiguousDeviceName) => true,
            (
                Self::PermissionDenied {
                    permission: a,
                    detail: ad,
                },
                Self::PermissionDenied {
                    permission: b,
                    detail: bd,
                },
            ) => a == b && ad == bd,
            (
                Self::Addon {
                    kind: a,
                    message: am,
                },
                Self::Addon {
                    kind: b,
                    message: bm,
                },
            ) => a == b && am == bm,
            (Self::Vad { code: a }, Self::Vad { code: b }) => a == b,
            (
                Self::NativeFormatChanged {
                    advertised: a,
                    actual: aa,
                },
                Self::NativeFormatChanged {
                    advertised: b,
                    actual: ba,
                },
            ) => a == b && aa == ba,
            (
                Self::Diagnostic {
                    source: a,
                    detail: ad,
                },
                Self::Diagnostic {
                    source: b,
                    detail: bd,
                },
            ) => a == b && ad == bd,
            (
                Self::Context {
                    source: a,
                    context: ac,
                },
                Self::Context {
                    source: b,
                    context: bc,
                },
            ) => a == b && ac == bc,
            (Self::Multiple(a), Self::Multiple(b)) => a == b,
            _ => false,
        }
    }
}
impl Eq for Error {}

fn reports_equal(a: &SettingsReport, b: &SettingsReport) -> bool {
    a.revision == b.revision
        && a.entries.len() == b.entries.len()
        && a.entries.iter().zip(&b.entries).all(|(a, b)| {
            a.setting == b.setting
                && a.outcome == b.outcome
                && values_equal(&a.requested, &b.requested)
                && values_equal(&a.effective, &b.effective)
        })
}
fn values_equal(a: &SettingValue, b: &SettingValue) -> bool {
    match (a, b) {
        (SettingValue::Float(a), SettingValue::Float(b)) => a == b || (a.is_nan() && b.is_nan()),
        _ => a == b,
    }
}
