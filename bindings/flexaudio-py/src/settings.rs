//! Python capture snapshots and static applicability, before native or model work.
use ::flexaudio as fa;
use fa::{IgnoreReason as I, SettingKey as K, SettingOutcome as O, SettingValue as V};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

#[derive(Default)]
pub(crate) struct SettingsState {
    pub(crate) report: Option<fa::SettingsReport>,
    pub(crate) strict: bool,
}

pub(crate) struct Request<'a> {
    pub(crate) config: &'a fa::StreamConfig,
    pub(crate) keepalive: Option<bool>,
    pub(crate) denoise: bool,
    pub(crate) vad: bool,
    pub(crate) secondary: Option<&'a crate::secondary::SecondaryOutputOptions>,
}

impl Request<'_> {
    pub(crate) fn resolve(
        &self,
        previous: Option<&fa::SettingsReport>,
        strict: bool,
    ) -> fa::Result<fa::SettingsReport> {
        let c = self.config;
        let system = matches!(c.kind, fa::SourceKind::SystemLoopback | fa::SourceKind::Mix);
        let exclusions = c.exclude_self || !c.exclude_pids.is_empty();
        let text = |value: &Option<String>| value.clone().map(V::Text).unwrap_or(V::Absent);
        let format = |value: Option<fa::OutputFormat>| value.map(V::Format).unwrap_or(V::Absent);
        let wrong = |condition: bool| condition.then_some(I::WrongSourceKind);
        let fixed = previous.map(|_| I::FixedAtOpen);
        let mut rows = Vec::new();
        let mut add = |key, requested: V, default: V, reason: Option<I>| {
            let effective = match reason {
                Some(I::FixedAtOpen) => previous
                    .and_then(|r| r.get(key))
                    .map(|r| r.effective.clone())
                    .unwrap_or(default.clone()),
                Some(_) => V::Absent,
                None => requested.clone(),
            };
            let outcome = if requested == default {
                O::NotRequested
            } else if let Some(reason) = reason {
                O::Ignored(reason)
            } else {
                O::Applied
            };
            rows.push((key, requested, effective, outcome));
        };
        add(
            K::Kind,
            V::Text(crate::source_kind_str(c.kind).into()),
            V::Text("mic".into()),
            None,
        );
        let unscoped = exclusions && cfg!(any(target_os = "linux", target_os = "windows"));
        let device_reason = if matches!(
            c.kind,
            fa::SourceKind::ProcessLoopback | fa::SourceKind::Mix
        ) {
            Some(I::WrongSourceKind)
        } else if system && unscoped {
            Some(I::ExclusionDeviceUnscoped)
        } else {
            None
        };
        add(K::DeviceId, text(&c.device_id), V::Absent, device_reason);
        add(
            K::TargetPid,
            c.target_pid
                .map(|p| V::Unsigned(p.into()))
                .unwrap_or(V::Absent),
            V::Absent,
            wrong(c.kind != fa::SourceKind::ProcessLoopback),
        );
        add(
            K::Mode,
            V::Text(
                if c.mode == fa::ProcessMode::Include {
                    "include"
                } else {
                    "exclude"
                }
                .into(),
            ),
            V::Text("include".into()),
            wrong(c.kind != fa::SourceKind::ProcessLoopback),
        );
        add(
            K::ChunkMs,
            V::Unsigned(c.chunk_ms.into()),
            V::Unsigned(20),
            None,
        );
        add(
            K::RingCapacityChunks,
            V::Unsigned(c.ring_capacity_chunks as u64),
            V::Unsigned(50),
            fixed,
        );
        add(
            K::ExcludeSelf,
            V::Bool(c.exclude_self),
            V::Bool(false),
            wrong(!system),
        );
        add(
            K::ExcludePids,
            V::Pids(c.exclude_pids.clone()),
            V::Pids(Vec::new()),
            wrong(!system),
        );
        add(
            K::Output,
            V::Format(c.output),
            V::Format(fa::OutputFormat::default()),
            None,
        );
        // These attachments have no switch arguments: report a default request and retained routing.
        add(
            K::SecondaryOutput,
            if previous.is_some() {
                V::Absent
            } else {
                format(c.secondary_output)
            },
            V::Absent,
            None,
        );
        let encoding = self.secondary.map(|s| s.encoding.name()).unwrap_or("f32");
        add(
            K::SecondaryEncoding,
            V::Text(if previous.is_some() { "f32" } else { encoding }.into()),
            V::Text("f32".into()),
            None,
        );
        add(K::Gain, V::Float(c.gain), V::Float(1.0), fixed);
        add(
            K::MixMicDeviceId,
            text(&c.mix_mic_device_id),
            V::Absent,
            wrong(c.kind != fa::SourceKind::Mix),
        );
        add(
            K::MixSystemDeviceId,
            text(&c.mix_system_device_id),
            V::Absent,
            if c.kind != fa::SourceKind::Mix {
                Some(I::WrongSourceKind)
            } else if unscoped {
                Some(I::ExclusionDeviceUnscoped)
            } else {
                None
            },
        );
        add(
            K::MixMicGain,
            V::Float(c.mix_mic_gain),
            V::Float(1.0),
            wrong(c.kind != fa::SourceKind::Mix),
        );
        add(
            K::MixSystemGain,
            V::Float(c.mix_system_gain),
            V::Float(1.0),
            wrong(c.kind != fa::SourceKind::Mix),
        );
        add(
            K::SilenceProbe,
            V::Bool(c.silence_probe),
            V::Bool(true),
            if c.kind == fa::SourceKind::Mic {
                Some(I::WrongSourceKind)
            } else if !cfg!(target_os = "macos") {
                Some(I::UnsupportedPlatform)
            } else {
                None
            },
        );
        add(
            K::WindowsClassicLoopbackKeepalive,
            V::Bool(self.keepalive.unwrap_or(true)),
            V::Bool(true),
            if !system {
                Some(I::WrongSourceKind)
            } else if !cfg!(target_os = "windows") {
                Some(I::UnsupportedPlatform)
            } else if exclusions {
                Some(I::ProcessLoopbackPath)
            } else {
                None
            },
        );
        add(K::Denoise, V::Bool(self.denoise), V::Bool(false), None);
        add(
            K::Vad,
            if previous.is_none() && self.vad {
                V::Bool(true)
            } else {
                V::Absent
            },
            V::Absent,
            None,
        );
        for (key, requested, effective, outcome) in &mut rows {
            if *key == K::ExcludePids && system {
                let mut pids = c.exclude_pids.clone();
                if c.exclude_self {
                    pids.push(std::process::id());
                }
                let mut seen = std::collections::BTreeSet::new();
                pids.retain(|pid| seen.insert(*pid));
                *effective = V::Pids(pids);
                if *requested != V::Pids(Vec::new()) && requested != effective {
                    *outcome = O::Adjusted(fa::AdjustmentReason::ExclusionSetNormalized);
                }
            }
            let retained = matches!(key, K::SecondaryOutput | K::SecondaryEncoding | K::Vad)
                || (*key == K::WindowsClassicLoopbackKeepalive
                    && self.keepalive.is_none()
                    && *effective != V::Absent);
            if retained {
                if let Some(entry) = previous.and_then(|r| r.get(*key)) {
                    *effective = entry.effective.clone();
                }
            }
        }
        let revision = match previous {
            Some(r) => r
                .revision
                .checked_add(1)
                .ok_or_else(|| fa::Error::InvalidState("settings revision exhausted".into()))?,
            None => 0,
        };
        let report = fa::SettingsReport::new(revision, rows)?;
        if strict && report.has_ignored() {
            Err(fa::Error::IgnoredSettings { report })
        } else {
            Ok(report)
        }
    }
}

/// Preserve existing core argument errors before strict applicability rejection.
pub(crate) fn validate(config: &fa::StreamConfig, opening: bool) -> fa::Result<()> {
    config.output.validate()?;
    if let Some(format) = config.secondary_output {
        format.validate()?;
    }
    if opening && (!config.gain.is_finite() || config.gain < 0.0) {
        return Err(fa::Error::InvalidArg(format!(
            "gain must be finite and >= 0.0, got {}",
            config.gain
        )));
    }
    if config.kind == fa::SourceKind::ProcessLoopback && config.target_pid.is_none() {
        return Err(fa::Error::InvalidArg(
            "ProcessLoopback requires target_pid".into(),
        ));
    }
    if config.kind == fa::SourceKind::Mix {
        for (name, gain) in [
            ("mix_mic_gain", config.mix_mic_gain),
            ("mix_system_gain", config.mix_system_gain),
        ] {
            if !gain.is_finite() || gain < 0.0 {
                return Err(fa::Error::InvalidArg(format!(
                    "{name} must be finite and >= 0.0, got {gain}"
                )));
            }
        }
    }
    Ok(())
}

fn value<'py>(py: Python<'py>, value: &V) -> PyResult<Bound<'py, PyAny>> {
    Ok(match value {
        V::Absent => py.None().into_bound(py),
        V::Bool(v) => v.into_pyobject(py)?.to_owned().into_any(),
        V::Unsigned(v) => v.into_pyobject(py)?.into_any(),
        V::Float(v) => v.into_pyobject(py)?.into_any(),
        V::Text(v) => v.into_pyobject(py)?.into_any(),
        V::Pids(v) => v.into_pyobject(py)?.into_any(),
        V::Format(v) => (v.sample_rate, v.channels).into_pyobject(py)?.into_any(),
        _ => {
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "unsupported setting value",
            ))
        }
    })
}

pub(crate) fn to_py<'py>(
    py: Python<'py>,
    report: &fa::SettingsReport,
) -> PyResult<Bound<'py, PyDict>> {
    let result = PyDict::new(py);
    result.set_item("revision", report.revision)?;
    let entries = PyList::empty(py);
    for entry in &report.entries {
        let row = PyDict::new(py);
        row.set_item("setting", entry.setting.as_str())?;
        row.set_item("requested", value(py, &entry.requested)?)?;
        row.set_item("effective", value(py, &entry.effective)?)?;
        let outcome = PyDict::new(py);
        let (status, reason) = match entry.outcome {
            O::NotRequested => ("notRequested", None),
            O::Applied => ("applied", None),
            O::Ignored(reason) => ("ignored", Some(reason.as_str())),
            O::Adjusted(reason) => ("adjusted", Some(reason.as_str())),
            _ => {
                return Err(pyo3::exceptions::PyRuntimeError::new_err(
                    "unsupported setting outcome",
                ))
            }
        };
        outcome.set_item("status", status)?;
        outcome.set_item("reason", reason)?;
        row.set_item("outcome", outcome)?;
        entries.append(row)?;
    }
    result.set_item("entries", entries)?;
    Ok(result)
}
