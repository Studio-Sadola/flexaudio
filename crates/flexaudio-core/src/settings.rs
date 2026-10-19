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

impl SettingKey {
    const ALL: &'static [Self] = &[
        Self::Kind,
        Self::DeviceId,
        Self::TargetPid,
        Self::Mode,
        Self::ChunkMs,
        Self::RingCapacityChunks,
        Self::ExcludeSelf,
        Self::ExcludePids,
        Self::Output,
        Self::SecondaryOutput,
        Self::SecondaryEncoding,
        Self::Gain,
        Self::MixMicDeviceId,
        Self::MixSystemDeviceId,
        Self::MixMicGain,
        Self::MixSystemGain,
        Self::SilenceProbe,
        Self::WindowsClassicLoopbackKeepalive,
        Self::Denoise,
        Self::Vad,
    ];
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn entries() -> Vec<(SettingKey, SettingValue, SettingValue, SettingOutcome)> {
        SettingKey::ALL
            .iter()
            .map(|&key| {
                (
                    key,
                    SettingValue::Absent,
                    SettingValue::Absent,
                    SettingOutcome::NotRequested,
                )
            })
            .collect()
    }

    #[test]
    fn constructor_requires_every_key_exactly_once() {
        assert_eq!(
            SettingsReport::new(0, Vec::new()).unwrap_err().kind(),
            crate::ErrorKind::InvalidArg
        );
        for index in 0..SettingKey::ALL.len() {
            let mut missing = entries();
            missing.remove(index);
            assert_eq!(
                SettingsReport::new(0, missing).unwrap_err().kind(),
                crate::ErrorKind::InvalidArg
            );
            let mut duplicate = entries();
            duplicate.push(duplicate[index].clone());
            assert_eq!(
                SettingsReport::new(0, duplicate).unwrap_err().kind(),
                crate::ErrorKind::InvalidArg
            );
            let mut substituted = entries();
            substituted[index] = substituted[(index + 1) % substituted.len()].clone();
            assert!(SettingsReport::new(0, substituted).is_err());
        }
    }

    #[test]
    fn constructor_sorts_complete_entries_and_queries_outcomes() {
        let mut input = entries();
        input[0].3 = SettingOutcome::Ignored(IgnoreReason::WrongSourceKind);
        input.reverse();
        let report = SettingsReport::new(7, input).unwrap();
        assert_eq!(report.revision, 7);
        assert!(report
            .entries
            .iter()
            .map(|entry| entry.setting)
            .eq(SettingKey::ALL.iter().copied()));
        assert!(report.has_ignored());
        assert_eq!(
            report.get(SettingKey::Kind).unwrap().outcome,
            SettingOutcome::Ignored(IgnoreReason::WrongSourceKind)
        );
        assert!(!SettingsReport::new(0, entries()).unwrap().has_ignored());
    }
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
    /// Construct a complete snapshot from `(key, requested, effective, outcome)` entries.
    ///
    /// Entries are stored in stable key order. Returns [`crate::Error::InvalidArg`]
    /// if any recognized key is missing or repeated. Tuple input permits construction
    /// across crates while keeping [`SettingResolution`] non-exhaustive.
    pub fn new(
        revision: u64,
        entries: Vec<(SettingKey, SettingValue, SettingValue, SettingOutcome)>,
    ) -> crate::Result<Self> {
        let mut entries: Vec<_> = entries
            .into_iter()
            .map(
                |(setting, requested, effective, outcome)| SettingResolution {
                    setting,
                    requested,
                    effective,
                    outcome,
                },
            )
            .collect();
        entries.sort_by_key(|entry| entry.setting);
        if !entries
            .iter()
            .map(|entry| entry.setting)
            .eq(SettingKey::ALL.iter().copied())
        {
            return Err(crate::Error::InvalidArg(
                "settings report must contain every setting exactly once".into(),
            ));
        }
        Ok(Self { revision, entries })
    }

    /// Whether any requested setting was ignored, requiring strict validation to reject it.
    pub fn has_ignored(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| matches!(entry.outcome, SettingOutcome::Ignored(_)))
    }

    /// Look up a setting's decoded request, effective value, and outcome.
    ///
    /// A freshly constructed report contains every key; `None` is possible if a
    /// caller has subsequently removed an entry from the public `entries` field.
    pub fn get(&self, setting: SettingKey) -> Option<&SettingResolution> {
        self.entries.iter().find(|entry| entry.setting == setting)
    }
}
