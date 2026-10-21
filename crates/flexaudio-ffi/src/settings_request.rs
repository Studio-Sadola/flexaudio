//! Describe C attachment policy and decoded legacy sentinels to the shared resolver.
use crate::types::FlexConfig;
use flexaudio::{SettingKey as K, SettingsAttachments};

pub(crate) fn attachments(base: &FlexConfig, keepalive: u32) -> SettingsAttachments {
    decoded_attachments(base, keepalive, true)
}

pub(crate) fn literal_gain_attachments(base: &FlexConfig, keepalive: u32) -> SettingsAttachments {
    decoded_attachments(base, keepalive, false)
}

fn decoded_attachments(
    base: &FlexConfig,
    keepalive: u32,
    legacy_gains: bool,
) -> SettingsAttachments {
    let mut sentinels = Vec::new();
    for (sentinel, key) in [
        (base.process_id == 0, K::TargetPid),
        (
            base.output_rate == 0 || base.output_channels == 0,
            K::Output,
        ),
        (base.chunk_ms == 0, K::ChunkMs),
        (legacy_gains && base.gain == 0.0, K::Gain),
        (legacy_gains && base.mix_mic_gain == 0.0, K::MixMicGain),
        (
            legacy_gains && base.mix_system_gain == 0.0,
            K::MixSystemGain,
        ),
        (base.silence_probe == 0, K::SilenceProbe),
    ] {
        if sentinel {
            sentinels.push(key);
        }
    }
    SettingsAttachments::new()
        .denoise(base.denoise != 0)
        .windows_classic_loopback_keepalive(match keepalive {
            1 => Some(true),
            2 => Some(false),
            _ => None,
        })
        .fixed_at_open([K::Denoise])
        .retained_on_switch([K::Vad, K::SecondaryEncoding])
        .legacy_sentinels(sentinels)
}
