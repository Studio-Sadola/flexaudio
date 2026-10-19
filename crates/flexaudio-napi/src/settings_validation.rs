//! Preserve argument errors before strict settings applicability rejection.
use super::*;
use flexaudio::{SettingKey as K, SettingValue as V};

pub(super) fn validate(
    config: &StreamConfig,
    previous: Option<&flexaudio::SettingsReport>,
) -> flexaudio::Result<()> {
    let c = config;
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
    Ok(())
}
