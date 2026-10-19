//! Translation between Node options and the shared settings resolver.
use super::*;
use flexaudio::{SettingKey as K, SettingOutcome as O, SettingValue as V};
use napi::bindgen_prelude::{Null, ToNapiValue};

pub(super) struct Request {
    pub config: StreamConfig,
    pub keepalive: Option<bool>,
    pub strict: Option<bool>,
    attachments: flexaudio::SettingsAttachments,
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
            strict: options.strict,
            attachments: flexaudio::SettingsAttachments::new()
                .denoise(options.denoise.unwrap_or(false))
                .vad(options.vad.is_some())
                .secondary_encoding(encoding)
                .fixed_at_open([K::Denoise, K::Vad])
                .retained_on_switch([K::SecondaryEncoding]),
        })
    }

    pub fn open_options(&self, options: flexaudio::OpenOptions) -> flexaudio::OpenOptions {
        options.settings_attachments(
            self.attachments
                .clone()
                .windows_classic_loopback_keepalive(self.keepalive),
        )
    }

    pub fn resolve(
        &self,
        previous: Option<&flexaudio::SettingsReport>,
        strict: bool,
    ) -> flexaudio::Result<flexaudio::SettingsReport> {
        super::settings_validation::validate(&self.config, previous)?;
        let mut request = flexaudio::SettingsRequest::new(self.config.clone())
            .attachments(self.attachments.clone())
            .windows_classic_loopback_keepalive(self.keepalive);
        if let Some(previous) = previous {
            request = request.previous(previous);
        }
        let report = flexaudio::resolve_settings(&request)?;
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
