//! Pure default comparison and static applicability; never queries native capabilities.

use crate::ProcessMode;
use crate::{
    AdjustmentReason, IgnoreReason, SettingKey, SettingOutcome, SettingValue, SettingsReport,
};
use crate::{Error, Result, SettingsRequest, SourceKind, StreamConfig};

/// Platform used for static setting applicability, independent of the build target.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsPlatform {
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
    pub(crate) fn current() -> Self {
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

/// Resolve every setting without native access, allocation of models, or capture mutation.
///
/// The platform and current process ID are selected internally. Switch request values
/// are compared with documented defaults, never with retained values. The returned
/// report includes every [`SettingKey`], including processing attachments.
///
/// This does not validate capture arguments or enforce strictness. After existing
/// argument validation, callers may reject [`SettingsReport::has_ignored`] before
/// loading models or touching native resources.
///
/// # Errors
/// Returns [`Error::InvalidState`] if the previous revision cannot be incremented.
pub fn resolve_settings(request: &SettingsRequest) -> Result<SettingsReport> {
    resolve_for_platform(request, SettingsPlatform::current(), std::process::id())
}

pub(crate) fn resolve_for_platform(
    request: &SettingsRequest,
    platform: SettingsPlatform,
    self_pid: u32,
) -> Result<SettingsReport> {
    let previous = request.previous.as_ref();
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
        let default = value(key, &d, &defaults)?;
        let omitted_on_switch =
            previous.is_some() && request.attachments.omitted_on_switch.contains(&key);
        let requested = if omitted_on_switch {
            default.clone()
        } else {
            value(key, c, request)?
        };
        let reason = ignored_reason(
            key,
            c,
            platform,
            previous.is_some(),
            &request.attachments.fixed_at_open,
        );
        let omitted = match key {
            SettingKey::WindowsClassicLoopbackKeepalive => request.keepalive.is_none(),
            SettingKey::Denoise => request.attachments.denoise.is_none(),
            SettingKey::Vad => request.attachments.vad.is_none(),
            SettingKey::SecondaryEncoding => request.attachments.secondary_encoding.is_none(),
            _ => false,
        };
        let retain_effective = previous.is_some()
            && (omitted
                || omitted_on_switch
                || request.attachments.retained_on_switch.contains(&key));
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
        } else if retain_effective {
            if let Some(entry) = previous.and_then(|report| report.get(key)) {
                effective = entry.effective.clone();
            }
        }
        let mut outcome = if requested == default {
            SettingOutcome::NotRequested
        } else if let Some(reason) = reason {
            SettingOutcome::Ignored(reason)
        } else {
            SettingOutcome::Applied
        };
        if key == SettingKey::ExcludePids && reason.is_none() && !retain_effective {
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
        if matches!(outcome, SettingOutcome::Applied)
            && request.attachments.legacy_sentinels.contains(&key)
        {
            outcome = SettingOutcome::Adjusted(AdjustmentReason::LegacySentinel);
        }
        entries.push((key, requested, effective, outcome));
    }
    SettingsReport::new(revision, entries)
}

fn ignored_reason(
    key: SettingKey,
    c: &StreamConfig,
    platform: SettingsPlatform,
    switch: bool,
    fixed_at_open: &[SettingKey],
) -> Option<IgnoreReason> {
    use IgnoreReason::*;
    use SettingKey::*;
    let system = matches!(c.kind, SourceKind::SystemLoopback | SourceKind::Mix);
    let exclusions = c.exclude_self || !c.exclude_pids.is_empty();
    match key {
        Gain | RingCapacityChunks if switch => Some(FixedAtOpen),
        key if switch && fixed_at_open.contains(&key) => Some(FixedAtOpen),
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

fn value(key: SettingKey, c: &StreamConfig, r: &SettingsRequest) -> Result<SettingValue> {
    use SettingKey::*;
    let text = |s: &Option<String>| {
        s.clone()
            .map(SettingValue::Text)
            .unwrap_or(SettingValue::Absent)
    };
    Ok(match key {
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
        SecondaryEncoding => SettingValue::Text(
            r.attachments
                .secondary_encoding
                .clone()
                .unwrap_or_else(|| "f32".into()),
        ),
        Gain => SettingValue::Float(c.gain),
        MixMicDeviceId => text(&c.mix_mic_device_id),
        MixSystemDeviceId => text(&c.mix_system_device_id),
        MixMicGain => SettingValue::Float(c.mix_mic_gain),
        MixSystemGain => SettingValue::Float(c.mix_system_gain),
        SilenceProbe => SettingValue::Bool(c.silence_probe),
        WindowsClassicLoopbackKeepalive => SettingValue::Bool(r.keepalive.unwrap_or(true)),
        Denoise => SettingValue::Bool(r.attachments.denoise.unwrap_or(false)),
        Vad => {
            if r.attachments.vad.unwrap_or(false) {
                SettingValue::Bool(true)
            } else {
                SettingValue::Absent
            }
        }
        _ => return Err(Error::InvalidArg("unrecognized setting key".into())),
    })
}
