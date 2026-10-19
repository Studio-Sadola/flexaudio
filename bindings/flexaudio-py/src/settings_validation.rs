//! Argument validation before strict settings applicability rejection.
use ::flexaudio as fa;

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
