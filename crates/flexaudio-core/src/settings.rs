//! Capture-setting snapshots and stable, language-independent identifiers.

use crate::OutputFormat;

macro_rules! tags {
    ($name:ident, $description:literal, {$($variant:ident => $tag:literal),+ $(,)?}) => {
        #[doc = $description]
        #[non_exhaustive]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Ord, PartialOrd, Hash)]
        pub enum $name {
            $(#[doc = $tag] $variant),+
        }
        impl $name {
            /// Stable lower-camel identifier shared by language bindings.
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $tag),+ }
            }
        }
    };
}

tags!(SettingKey, "Recognized capture setting.", {
    Kind => "kind", DeviceId => "deviceId", TargetPid => "targetPid", Mode => "mode",
    ChunkMs => "chunkMs", RingCapacityChunks => "ringCapacityChunks",
    ExcludeSelf => "excludeSelf", ExcludePids => "excludePids", Output => "output",
    SecondaryOutput => "secondaryOutput", SecondaryEncoding => "secondaryEncoding",
    Gain => "gain", MixMicDeviceId => "mixMicDeviceId", MixSystemDeviceId => "mixSystemDeviceId",
    MixMicGain => "mixMicGain", MixSystemGain => "mixSystemGain", SilenceProbe => "silenceProbe",
    WindowsClassicLoopbackKeepalive => "windowsClassicLoopbackKeepalive", Denoise => "denoise",
    Vad => "vad",
});
tags!(IgnoreReason, "Why a requested setting cannot affect this capture.", {
    WrongSourceKind => "wrongSourceKind", UnsupportedPlatform => "unsupportedPlatform",
    ExclusionDeviceUnscoped => "exclusionDeviceUnscoped", ProcessLoopbackPath => "processLoopbackPath",
    FixedAtOpen => "fixedAtOpen",
});
tags!(AdjustmentReason, "Existing normalization applied to a requested value.", {
    ExclusionSetNormalized => "exclusionSetNormalized", LegacySentinel => "legacySentinel",
});

/// Decoded request or configured routing value. Inapplicable values are absent.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum SettingValue {
    /// No configured value, including default-device selection.
    Absent,
    /// Boolean setting.
    Bool(bool),
    /// Nonnegative integral setting.
    Unsigned(u64),
    /// Linear gain, retaining the decoded floating-point value.
    Float(f32),
    /// Selector or stable textual option.
    Text(String),
    /// Process IDs.
    Pids(Vec<u32>),
    /// Sample rate and channel count.
    Format(OutputFormat),
}

/// Classification uses differences from documented defaults, never property presence.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum SettingOutcome {
    /// The decoded request equals its documented default.
    NotRequested,
    /// A non-default request is configured.
    Applied,
    /// A non-default request has no effect.
    Ignored(IgnoreReason),
    /// A non-default request is configured after existing normalization.
    Adjusted(AdjustmentReason),
}

/// One setting in a complete capture snapshot.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct SettingResolution {
    /// Stable setting identity.
    pub setting: SettingKey,
    /// Decoded candidate value, including defaults.
    pub requested: SettingValue,
    /// Configured routing, rather than evidence of hardware activity.
    pub effective: SettingValue,
    /// Default, applicability, or normalization result.
    pub outcome: SettingOutcome,
}

/// Immutable-by-convention snapshot committed by a successful open or source switch.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsReport {
    /// Zero at open; advances only when a source switch commits successfully.
    pub revision: u64,
    /// Every recognized capture setting, in stable key order.
    pub entries: Vec<SettingResolution>,
}

impl SettingsReport {
    /// Construct a snapshot from already resolved settings.
    pub fn new(revision: u64, entries: Vec<SettingResolution>) -> Self {
        Self { revision, entries }
    }

    /// Whether strict validation must reject this candidate.
    pub fn has_ignored(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| matches!(entry.outcome, SettingOutcome::Ignored(_)))
    }

    /// Resolution for a recognized setting.
    pub fn get(&self, setting: SettingKey) -> Option<&SettingResolution> {
        self.entries.iter().find(|entry| entry.setting == setting)
    }
}
