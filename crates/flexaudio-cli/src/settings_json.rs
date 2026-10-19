//! Stable JSON projection for successful and rejected capture settings.
use flexaudio::{Error, SettingKey, SettingOutcome, SettingValue, SettingsReport, Stream};
use serde_json::{json, Value};

pub(super) fn project(report: &SettingsReport, denoise: bool) -> Value {
    let entries: Vec<_> = report
        .entries
        .iter()
        .map(|entry| {
            // The facade's shared-reference denoise setter cannot update its open snapshot.
            // This CLI owns the explicit denoise request and applies it before start.
            let (requested, effective, outcome) = if entry.setting == SettingKey::Denoise {
                (
                    json!(denoise),
                    json!(denoise),
                    json!({"status": if denoise { "applied" } else { "notRequested" }}),
                )
            } else {
                (
                    value(&entry.requested),
                    value(&entry.effective),
                    outcome(&entry.outcome),
                )
            };
            json!({"setting": entry.setting.as_str(), "requested": requested,
            "effective": effective, "outcome": outcome})
        })
        .collect();
    json!({"revision": report.revision, "entries": entries})
}

fn value(value: &SettingValue) -> Value {
    match value {
        SettingValue::Absent => Value::Null,
        SettingValue::Bool(value) => json!(value),
        SettingValue::Unsigned(value) => json!(value),
        SettingValue::Float(value) if value.is_nan() => json!("nan"),
        SettingValue::Float(value) if *value == f32::INFINITY => json!("positiveInfinity"),
        SettingValue::Float(value) if *value == f32::NEG_INFINITY => json!("negativeInfinity"),
        SettingValue::Float(value) => json!(value),
        SettingValue::Text(value) => json!(value),
        SettingValue::Pids(value) => json!(value),
        SettingValue::Format(value) => {
            json!({"sampleRate":value.sample_rate,"channels":value.channels})
        }
        _ => Value::Null,
    }
}

fn outcome(value: &SettingOutcome) -> Value {
    match value {
        SettingOutcome::NotRequested => json!({"status":"notRequested"}),
        SettingOutcome::Applied => json!({"status":"applied"}),
        SettingOutcome::Ignored(reason) => json!({"status":"ignored","reason":reason.as_str()}),
        SettingOutcome::Adjusted(reason) => json!({"status":"adjusted","reason":reason.as_str()}),
        _ => json!({"status":"unknown"}),
    }
}

pub(super) fn emit_stream(stream: &Stream, enabled: bool, denoise: bool) {
    if enabled {
        if let Some(report) = stream.effective_settings() {
            eprintln!("{}", project(report, denoise));
        }
    }
}

pub(super) fn emit_rejection(error: &Error, enabled: bool, denoise: bool) {
    if enabled {
        if let Some(report) = error.ignored_settings() {
            eprintln!("{}", project(report, denoise));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_values_escape_and_preserve_nonfinite_tags() {
        let text = "selector\"\\\n\t\u{0000}";
        let encoded = value(&SettingValue::Text(text.into())).to_string();
        assert_eq!(
            serde_json::from_str::<Value>(&encoded).unwrap(),
            json!(text)
        );
        assert_eq!(value(&SettingValue::Float(f32::NAN)), json!("nan"));
        assert_eq!(
            value(&SettingValue::Float(f32::INFINITY)),
            json!("positiveInfinity")
        );
        assert_eq!(
            value(&SettingValue::Float(f32::NEG_INFINITY)),
            json!("negativeInfinity")
        );
        assert_eq!(value(&SettingValue::Pids(vec![1, 2])), json!([1, 2]));
        assert_eq!(value(&SettingValue::Absent), Value::Null);
        assert_eq!(value(&SettingValue::Unsigned(u64::MAX)), json!(u64::MAX));
        assert_eq!(
            value(&SettingValue::Format(flexaudio::OutputFormat::default())),
            json!({"sampleRate":48000,"channels":2})
        );
        let report = flexaudio::resolve_settings(
            &flexaudio::SettingsRequest::new(flexaudio::StreamConfig {
                gain: 2.0,
                ..Default::default()
            })
            .attachments(
                flexaudio::SettingsAttachments::new().legacy_sentinels([SettingKey::Gain]),
            ),
        )
        .unwrap();
        assert_eq!(
            outcome(&report.get(SettingKey::Gain).unwrap().outcome),
            json!({"status":"adjusted","reason":"legacySentinel"})
        );
    }
}
