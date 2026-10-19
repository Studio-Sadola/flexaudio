//! Device-independent validation shared by facade and custom-backend opens.

use crate::{Error, Result, StreamConfig};

pub(crate) fn open_config(config: &StreamConfig) -> Result<()> {
    flexaudio_core::validate_chunk_ms(config.chunk_ms)?;
    if config.ring_capacity_chunks == 0 {
        return Err(Error::InvalidArg("ring_capacity_chunks must be > 0".into()));
    }
    if !config.gain.is_finite() || config.gain < 0.0 {
        return Err(Error::InvalidArg(format!(
            "gain must be finite and >= 0.0, got {}",
            config.gain
        )));
    }
    config.output.validate()?;
    crate::validate_exclude_pids(config)?;
    if let Some(secondary) = config.secondary_output {
        secondary.validate()?;
    }
    Ok(())
}

pub(crate) fn source_config(config: &StreamConfig) -> Result<()> {
    crate::validate_exclude_pids(config)?;
    match config.kind {
        crate::SourceKind::Mix => crate::validate_mix_gains(config),
        #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
        crate::SourceKind::ProcessLoopback if config.target_pid.is_none() => Err(
            Error::InvalidArg("ProcessLoopback requires target_pid".into()),
        ),
        _ => Ok(()),
    }
}
