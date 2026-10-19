//! Binding-owned capture snapshots, including processing attachments and encoding.
use super::*;
use flexaudio::{IgnoreReason as I, SettingKey as K, SettingOutcome as O, SettingValue as V};
use napi::bindgen_prelude::{Null, ToNapiValue};

pub(super) struct Request {
    pub config: StreamConfig,
    pub keepalive: Option<bool>,
    pub retained_keepalive: Option<bool>,
    pub strict: Option<bool>,
    pub denoise: bool,
    pub vad: bool,
    pub encoding: String,
}

impl Request {
    pub fn new(options: &OpenOptions, config: StreamConfig) -> napi::Result<Self> {
        let encoding = match options.secondary_output.as_ref() {
            Some(output) => match parse_sec_encoding(output.encoding.as_deref())? {
                SecEncoding::F32 => "f32",
                SecEncoding::S16 => "s16",
            },
            None => "f32",
        };
        Ok(Self {
            config,
            keepalive: options.windows_classic_loopback_keepalive,
            retained_keepalive: None,
            strict: options.strict,
            denoise: options.denoise.unwrap_or(false),
            vad: options.vad.is_some(),
            encoding: encoding.into(),
        })
    }

    pub fn resolve(
        &self,
        previous: Option<&flexaudio::SettingsReport>,
        strict: bool,
    ) -> flexaudio::Result<flexaudio::SettingsReport> {
        let c = &self.config;
        c.output.validate()?;
        if let Some(secondary) = c.secondary_output {
            secondary.validate()?;
        }
        if previous.is_none() && (!c.gain.is_finite() || c.gain < 0.0) {
            return Err(flexaudio::Error::InvalidArg(
                "gain must be finite and nonnegative".into(),
            ));
        }
        if c.kind == SourceKind::ProcessLoopback && c.target_pid.is_none() {
            return Err(flexaudio::Error::InvalidArg(
                "process capture requires processId".into(),
            ));
        }
        if c.kind == SourceKind::Mix
            && [c.mix_mic_gain, c.mix_system_gain]
                .iter()
                .any(|v| !v.is_finite() || *v < 0.0)
        {
            return Err(flexaudio::Error::InvalidArg(
                "mix gains must be finite and nonnegative".into(),
            ));
        }
        if let Some(previous) = previous {
            for (key, requested) in [
                (K::Output, V::Format(c.output)),
                (
                    K::SecondaryOutput,
                    c.secondary_output.map(V::Format).unwrap_or(V::Absent),
                ),
            ] {
                if previous
                    .get(key)
                    .is_some_and(|entry| entry.effective != requested)
                {
                    return Err(flexaudio::Error::InvalidArg(
                        "output formats cannot change during a source switch".into(),
                    ));
                }
            }
        }
        let system = matches!(c.kind, SourceKind::SystemLoopback | SourceKind::Mix);
        let exclusions = c.exclude_self || !c.exclude_pids.is_empty();
        let text = |v: &Option<String>| v.clone().map(V::Text).unwrap_or(V::Absent);
        let format = |v: Option<OutputFormat>| v.map(V::Format).unwrap_or(V::Absent);
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
        let wrong = |condition: bool| condition.then_some(I::WrongSourceKind);
        let fixed = previous.map(|_| I::FixedAtOpen);
        add(
            K::Kind,
            V::Text(source_kind_str(c.kind)),
            V::Text("mic".into()),
            None,
        );
        let device_reason = if matches!(c.kind, SourceKind::ProcessLoopback | SourceKind::Mix) {
            Some(I::WrongSourceKind)
        } else if system && exclusions && cfg!(any(target_os = "linux", target_os = "windows")) {
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
            wrong(c.kind != SourceKind::ProcessLoopback),
        );
        add(
            K::Mode,
            V::Text(
                if c.mode == ProcessMode::Include {
                    "include"
                } else {
                    "exclude"
                }
                .into(),
            ),
            V::Text("include".into()),
            wrong(c.kind != SourceKind::ProcessLoopback),
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
            V::Format(OutputFormat {
                sample_rate: 48000,
                channels: 2,
            }),
            None,
        );
        add(
            K::SecondaryOutput,
            format(c.secondary_output),
            V::Absent,
            None,
        );
        add(
            K::SecondaryEncoding,
            V::Text(self.encoding.clone()),
            V::Text("f32".into()),
            None,
        );
        add(K::Gain, V::Float(c.gain), V::Float(1.0), fixed);
        add(
            K::MixMicDeviceId,
            text(&c.mix_mic_device_id),
            V::Absent,
            wrong(c.kind != SourceKind::Mix),
        );
        add(
            K::MixSystemDeviceId,
            text(&c.mix_system_device_id),
            V::Absent,
            if c.kind != SourceKind::Mix {
                Some(I::WrongSourceKind)
            } else if exclusions && cfg!(any(target_os = "linux", target_os = "windows")) {
                Some(I::ExclusionDeviceUnscoped)
            } else {
                None
            },
        );
        add(
            K::MixMicGain,
            V::Float(c.mix_mic_gain),
            V::Float(1.0),
            wrong(c.kind != SourceKind::Mix),
        );
        add(
            K::MixSystemGain,
            V::Float(c.mix_system_gain),
            V::Float(1.0),
            wrong(c.kind != SourceKind::Mix),
        );
        add(
            K::SilenceProbe,
            V::Bool(c.silence_probe),
            V::Bool(true),
            if c.kind == SourceKind::Mic {
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
        add(K::Denoise, V::Bool(self.denoise), V::Bool(false), fixed);
        add(
            K::Vad,
            if self.vad { V::Bool(true) } else { V::Absent },
            V::Absent,
            fixed,
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
                    *outcome = O::Adjusted(flexaudio::AdjustmentReason::ExclusionSetNormalized);
                }
            }
            if *key == K::WindowsClassicLoopbackKeepalive
                && self.keepalive.is_none()
                && matches!(outcome, O::NotRequested)
                && *effective != V::Absent
            {
                if let Some(retained) = self.retained_keepalive {
                    *effective = V::Bool(retained);
                }
            }
            if *key == K::SecondaryEncoding {
                if let Some(entry) = previous.and_then(|r| r.get(*key)) {
                    *effective = entry.effective.clone();
                }
            }
        }
        let revision = match previous {
            Some(r) => r.revision.checked_add(1).ok_or_else(|| {
                flexaudio::Error::InvalidState("settings revision exhausted".into())
            })?,
            None => 0,
        };
        let report = flexaudio::SettingsReport::new(revision, rows)?;
        if strict && report.has_ignored() {
            Err(flexaudio::Error::IgnoredSettings { report })
        } else {
            Ok(report)
        }
    }
}

fn value(env: &Env, value: &V) -> napi::Result<napi::JsUnknown> {
    let raw = match value {
        V::Absent => unsafe { Null::to_napi_value(env.raw(), Null)? },
        V::Bool(v) => unsafe { bool::to_napi_value(env.raw(), *v)? },
        V::Unsigned(v) => unsafe { BigInt::to_napi_value(env.raw(), BigInt::from(*v))? },
        V::Float(v) => unsafe { f64::to_napi_value(env.raw(), f64::from(*v))? },
        V::Text(v) => unsafe { String::to_napi_value(env.raw(), v.clone())? },
        V::Pids(v) => unsafe { Vec::<u32>::to_napi_value(env.raw(), v.clone())? },
        V::Format(v) => unsafe {
            JsNativeFormat::to_napi_value(
                env.raw(),
                JsNativeFormat {
                    sample_rate: v.sample_rate,
                    channels: v.channels,
                },
            )?
        },
        _ => {
            return Err(NapiError::new(
                Status::GenericFailure,
                "unrecognized setting value",
            ))
        }
    };
    Ok(unsafe { napi::JsUnknown::from_raw_unchecked(env.raw(), raw) })
}

pub(super) fn to_js(env: &Env, report: &flexaudio::SettingsReport) -> napi::Result<JsObject> {
    let mut object = env.create_object()?;
    object.set_named_property("revision", BigInt::from(report.revision))?;
    let mut entries = env.create_array_with_length(report.entries.len())?;
    for (index, entry) in report.entries.iter().enumerate() {
        let mut row = env.create_object()?;
        row.set_named_property("setting", entry.setting.as_str())?;
        row.set_named_property("requested", value(env, &entry.requested)?)?;
        row.set_named_property("effective", value(env, &entry.effective)?)?;
        let mut outcome = env.create_object()?;
        let (status, reason) = match entry.outcome {
            O::NotRequested => ("notRequested", None),
            O::Applied => ("applied", None),
            O::Ignored(r) => ("ignored", Some(r.as_str())),
            O::Adjusted(r) => ("adjusted", Some(r.as_str())),
            _ => {
                return Err(NapiError::new(
                    Status::GenericFailure,
                    "unrecognized setting outcome",
                ))
            }
        };
        outcome.set_named_property("status", status)?;
        if let Some(reason) = reason {
            outcome.set_named_property("reason", reason)?;
        }
        row.set_named_property("outcome", outcome)?;
        entries.set_element(index as u32, row)?;
    }
    object.set_named_property("entries", entries)?;
    Ok(object)
}
