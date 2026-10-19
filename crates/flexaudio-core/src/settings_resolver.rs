//! Pure default comparison and static applicability; never queries native capabilities.

use crate::settings::{
    AdjustmentReason, IgnoreReason, SettingKey, SettingOutcome, SettingResolution, SettingValue,
    SettingsReport,
};
use crate::types::ProcessMode;
use crate::{Error, Result, SourceKind, StreamConfig};

/// Platform used for static setting applicability, independent of the build target.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsPlatform {
    /// Linux PipeWire routing.
    Linux,
    /// Windows WASAPI routing.
    Windows,
    /// macOS Core Audio routing.
    MacOs,
    /// Other platforms.
    Other,
}
impl SettingsPlatform {
    /// Build target, without OS queries or permission checks.
    pub fn current() -> Self {
        if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Other
        }
    }
}

/// Decoded candidate settings. Controls such as strictness are deliberately excluded.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsRequest {
    /// Source, formats, queue capacity, and gains.
    pub config: StreamConfig,
    /// Windows silent-render option, default true.
    pub windows_classic_loopback_keepalive: bool,
    /// Effective inherited keepalive for an omitted switch option. Its request
    /// value is still the documented default, true.
    pub retained_windows_classic_loopback_keepalive: Option<bool>,
    /// Canonical capture noise suppression, default false.
    pub denoise: bool,
    /// Effective inherited denoise for an omitted switch option. Its request
    /// value is still the documented default, false.
    pub retained_denoise: Option<bool>,
    /// Attached VAD presence, default false; no model paths are recorded.
    pub vad: bool,
    /// Secondary encoding, default `f32`.
    pub secondary_encoding: String,
    /// Bindings whose processing attachments are fixed at open opt in here.
    /// Rust explicit denoise switches leave this false.
    pub processing_fixed_at_open: bool,
    /// Settings decoded using an existing legacy sentinel.
    pub legacy_sentinels: Vec<SettingKey>,
}
impl SettingsRequest {
    /// A Rust capture request with the documented option defaults.
    pub fn new(config: StreamConfig) -> Self {
        Self {
            config,
            windows_classic_loopback_keepalive: true,
            retained_windows_classic_loopback_keepalive: None,
            denoise: false,
            retained_denoise: None,
            vad: false,
            secondary_encoding: "f32".into(),
            processing_fixed_at_open: false,
            legacy_sentinels: Vec::new(),
        }
    }
}

/// Resolve every setting without native access, allocation of models, or capture mutation.
///
/// `previous` selects switch semantics and supplies retained fixed-at-open values.
/// `self_pid` is supplied by the caller so exclusion normalization remains pure.
/// Switch request values are always compared with documented defaults.
pub fn resolve_settings(
    request: &SettingsRequest,
    platform: SettingsPlatform,
    self_pid: u32,
    previous: Option<&SettingsReport>,
) -> Result<SettingsReport> {
    let revision = match previous {
        Some(report) => report
            .revision
            .checked_add(1)
            .ok_or_else(|| Error::InvalidState("settings revision exhausted".into()))?,
        None => 0,
    };
    let c = &request.config;
    let d = StreamConfig::default();
    let defaults = SettingsRequest::new(d.clone());
    let mut entries = Vec::new();
    for key in [
        SettingKey::Kind,
        SettingKey::DeviceId,
        SettingKey::TargetPid,
        SettingKey::Mode,
        SettingKey::ChunkMs,
        SettingKey::RingCapacityChunks,
        SettingKey::ExcludeSelf,
        SettingKey::ExcludePids,
        SettingKey::Output,
        SettingKey::SecondaryOutput,
        SettingKey::SecondaryEncoding,
        SettingKey::Gain,
        SettingKey::MixMicDeviceId,
        SettingKey::MixSystemDeviceId,
        SettingKey::MixMicGain,
        SettingKey::MixSystemGain,
        SettingKey::SilenceProbe,
        SettingKey::WindowsClassicLoopbackKeepalive,
        SettingKey::Denoise,
        SettingKey::Vad,
    ] {
        let requested = value(key, c, request);
        let default = value(key, &d, &defaults);
        let reason = ignored_reason(
            key,
            c,
            platform,
            previous.is_some(),
            request.processing_fixed_at_open,
        );
        let mut effective = requested.clone();
        if reason.is_some() {
            effective = if reason == Some(IgnoreReason::FixedAtOpen) {
                previous
                    .and_then(|report| report.get(key))
                    .map(|entry| entry.effective.clone())
                    .unwrap_or(default.clone())
            } else {
                SettingValue::Absent
            };
        } else if previous.is_some() {
            let retained = match key {
                SettingKey::WindowsClassicLoopbackKeepalive => {
                    request.retained_windows_classic_loopback_keepalive
                }
                SettingKey::Denoise => request.retained_denoise,
                _ => None,
            };
            if let Some(retained) = retained {
                effective = SettingValue::Bool(retained);
            }
        }
        let mut outcome = if requested == default {
            SettingOutcome::NotRequested
        } else if let Some(reason) = reason {
            SettingOutcome::Ignored(reason)
        } else {
            SettingOutcome::Applied
        };
        if key == SettingKey::ExcludePids && reason.is_none() {
            let mut pids = c.exclude_pids.clone();
            if c.exclude_self {
                pids.push(self_pid);
            }
            // Retain first-occurrence order, including Windows' existing root selection.
            let mut seen = std::collections::BTreeSet::new();
            pids.retain(|pid| seen.insert(*pid));
            effective = SettingValue::Pids(pids);
            if effective != requested && requested != default {
                outcome = SettingOutcome::Adjusted(AdjustmentReason::ExclusionSetNormalized);
            }
        }
        if matches!(outcome, SettingOutcome::Applied) && request.legacy_sentinels.contains(&key) {
            outcome = SettingOutcome::Adjusted(AdjustmentReason::LegacySentinel);
        }
        entries.push(SettingResolution {
            setting: key,
            requested,
            effective,
            outcome,
        });
    }
    Ok(SettingsReport { revision, entries })
}

fn ignored_reason(
    key: SettingKey,
    c: &StreamConfig,
    platform: SettingsPlatform,
    switch: bool,
    fixed_processing: bool,
) -> Option<IgnoreReason> {
    use IgnoreReason::*;
    use SettingKey::*;
    let system = matches!(c.kind, SourceKind::SystemLoopback | SourceKind::Mix);
    let exclusions = c.exclude_self || !c.exclude_pids.is_empty();
    match key {
        Gain | RingCapacityChunks if switch => Some(FixedAtOpen),
        Denoise | Vad if switch && fixed_processing => Some(FixedAtOpen),
        DeviceId if matches!(c.kind, SourceKind::ProcessLoopback | SourceKind::Mix) => {
            Some(WrongSourceKind)
        }
        TargetPid | Mode if c.kind != SourceKind::ProcessLoopback => Some(WrongSourceKind),
        ExcludeSelf | ExcludePids if !system => Some(WrongSourceKind),
        MixMicDeviceId | MixSystemDeviceId | MixMicGain | MixSystemGain
            if c.kind != SourceKind::Mix =>
        {
            Some(WrongSourceKind)
        }
        SilenceProbe if c.kind == SourceKind::Mic => Some(WrongSourceKind),
        SilenceProbe if platform != SettingsPlatform::MacOs => Some(UnsupportedPlatform),
        WindowsClassicLoopbackKeepalive if !system => Some(WrongSourceKind),
        WindowsClassicLoopbackKeepalive if platform != SettingsPlatform::Windows => {
            Some(UnsupportedPlatform)
        }
        WindowsClassicLoopbackKeepalive if exclusions => Some(ProcessLoopbackPath),
        DeviceId
            if c.kind == SourceKind::SystemLoopback
                && exclusions
                && matches!(
                    platform,
                    SettingsPlatform::Linux | SettingsPlatform::Windows
                ) =>
        {
            Some(ExclusionDeviceUnscoped)
        }
        MixSystemDeviceId
            if c.kind == SourceKind::Mix
                && exclusions
                && matches!(
                    platform,
                    SettingsPlatform::Linux | SettingsPlatform::Windows
                ) =>
        {
            Some(ExclusionDeviceUnscoped)
        }
        _ => None,
    }
}

fn value(key: SettingKey, c: &StreamConfig, r: &SettingsRequest) -> SettingValue {
    use SettingKey::*;
    let text = |s: &Option<String>| {
        s.clone()
            .map(SettingValue::Text)
            .unwrap_or(SettingValue::Absent)
    };
    match key {
        Kind => SettingValue::Text(
            match c.kind {
                SourceKind::Mic => "mic",
                SourceKind::SystemLoopback => "system",
                SourceKind::ProcessLoopback => "process",
                SourceKind::Mix => "mix",
            }
            .into(),
        ),
        DeviceId => text(&c.device_id),
        TargetPid => c
            .target_pid
            .map(|pid| SettingValue::Unsigned(u64::from(pid)))
            .unwrap_or(SettingValue::Absent),
        Mode => SettingValue::Text(
            match c.mode {
                ProcessMode::Include => "include",
                ProcessMode::Exclude => "exclude",
            }
            .into(),
        ),
        ChunkMs => SettingValue::Unsigned(u64::from(c.chunk_ms)),
        RingCapacityChunks => SettingValue::Unsigned(c.ring_capacity_chunks as u64),
        ExcludeSelf => SettingValue::Bool(c.exclude_self),
        ExcludePids => SettingValue::Pids(c.exclude_pids.clone()),
        Output => SettingValue::Format(c.output),
        SecondaryOutput => c
            .secondary_output
            .map(SettingValue::Format)
            .unwrap_or(SettingValue::Absent),
        SecondaryEncoding => SettingValue::Text(r.secondary_encoding.clone()),
        Gain => SettingValue::Float(c.gain),
        MixMicDeviceId => text(&c.mix_mic_device_id),
        MixSystemDeviceId => text(&c.mix_system_device_id),
        MixMicGain => SettingValue::Float(c.mix_mic_gain),
        MixSystemGain => SettingValue::Float(c.mix_system_gain),
        SilenceProbe => SettingValue::Bool(c.silence_probe),
        WindowsClassicLoopbackKeepalive => SettingValue::Bool(
            r.retained_windows_classic_loopback_keepalive.is_some()
                || r.windows_classic_loopback_keepalive,
        ),
        Denoise => SettingValue::Bool(r.retained_denoise.is_none() && r.denoise),
        Vad => {
            if r.vad {
                SettingValue::Bool(true)
            } else {
                SettingValue::Absent
            }
        }
    }
}
