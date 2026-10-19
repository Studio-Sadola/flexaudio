//! Public preflight and facade snapshots must resolve the same decoded request.

use crate::*;

#[test]
fn public_preflight_and_stream_own_the_same_complete_attachment_report() {
    let config = StreamConfig {
        secondary_output: Some(OutputFormat {
            sample_rate: 16_000,
            channels: 1,
        }),
        ..Default::default()
    };
    let attachments = SettingsAttachments::new()
        .denoise(true)
        .vad(true)
        .secondary_encoding("s16")
        .fixed_at_open([SettingKey::Denoise, SettingKey::Vad]);
    let options = OpenOptions::new()
        .settings_attachments(attachments)
        .strict(true);
    let expected =
        resolve_settings(&SettingsRequest::new(config.clone()).open_options(&options)).unwrap();
    let stream = crate::open_using(config, options, |_, _| {
        Ok(Box::new(MockBackend::new(48_000, 2, 0.0)))
    })
    .unwrap();
    assert_eq!(stream.effective_settings(), Some(&expected));
    for (key, value) in [
        (SettingKey::Denoise, SettingValue::Bool(true)),
        (SettingKey::Vad, SettingValue::Bool(true)),
        (
            SettingKey::SecondaryEncoding,
            SettingValue::Text("s16".into()),
        ),
    ] {
        let entry = expected.get(key).unwrap();
        assert_eq!(entry.requested, value);
        assert_eq!(entry.effective, value);
        assert_eq!(entry.outcome, SettingOutcome::Applied);
    }
}

#[test]
fn fixed_attachment_policy_is_per_key_and_omission_retains_values() {
    let config = StreamConfig {
        secondary_output: Some(OutputFormat {
            sample_rate: 16_000,
            channels: 1,
        }),
        ..Default::default()
    };
    let initial = resolve_settings(
        &SettingsRequest::new(config).attachments(
            SettingsAttachments::new()
                .denoise(true)
                .vad(true)
                .secondary_encoding("s16"),
        ),
    )
    .unwrap();
    for fixed_key in [SettingKey::Denoise, SettingKey::Vad] {
        let report = resolve_settings(
            &SettingsRequest::new(StreamConfig::default())
                .previous(&initial)
                .attachments(
                    SettingsAttachments::new()
                        .denoise(true)
                        .vad(true)
                        .fixed_at_open([fixed_key])
                        .omitted_on_switch([
                            SettingKey::SecondaryOutput,
                            SettingKey::SecondaryEncoding,
                        ]),
                ),
        )
        .unwrap();
        assert_eq!(
            report.get(fixed_key).unwrap().outcome,
            SettingOutcome::Ignored(IgnoreReason::FixedAtOpen)
        );
        let other = if fixed_key == SettingKey::Denoise {
            SettingKey::Vad
        } else {
            SettingKey::Denoise
        };
        assert_eq!(report.get(other).unwrap().outcome, SettingOutcome::Applied);
        for key in [SettingKey::SecondaryOutput, SettingKey::SecondaryEncoding] {
            let entry = report.get(key).unwrap();
            assert_eq!(entry.outcome, SettingOutcome::NotRequested);
            assert_eq!(entry.effective, initial.get(key).unwrap().effective);
        }
    }
    let omitted =
        resolve_settings(&SettingsRequest::new(StreamConfig::default()).previous(&initial))
            .unwrap();
    for key in [
        SettingKey::Denoise,
        SettingKey::Vad,
        SettingKey::SecondaryEncoding,
    ] {
        assert_eq!(
            omitted.get(key).unwrap().outcome,
            SettingOutcome::NotRequested
        );
        assert_eq!(
            omitted.get(key).unwrap().effective,
            initial.get(key).unwrap().effective
        );
    }
}

#[test]
fn public_legacy_decoding_obeys_default_and_ignore_precedence() {
    for gain in [1.0, 2.0] {
        let request = SettingsRequest::new(StreamConfig {
            gain,
            ..Default::default()
        })
        .attachments(SettingsAttachments::new().legacy_sentinels([SettingKey::Gain]));
        let report = resolve_settings(&request).unwrap();
        assert_eq!(
            report.get(SettingKey::Gain).unwrap().outcome,
            if gain == 1.0 {
                SettingOutcome::NotRequested
            } else {
                SettingOutcome::Adjusted(AdjustmentReason::LegacySentinel)
            }
        );
        let switch = resolve_settings(&request.previous(&report)).unwrap();
        assert_eq!(
            switch.get(SettingKey::Gain).unwrap().outcome,
            if gain == 1.0 {
                SettingOutcome::NotRequested
            } else {
                SettingOutcome::Ignored(IgnoreReason::FixedAtOpen)
            }
        );
    }
}
