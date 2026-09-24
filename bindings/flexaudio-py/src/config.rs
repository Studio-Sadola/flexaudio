//! Conversion and validation from Python arguments to the core's various configs.
//!
//! - [`build_config`]: builds a [`StreamConfig`] from the arguments of `open` / `switch_source`.
//! - [`make_vad_config`] / [`vad_config_from_dict`]: build the VAD config (from explicit
//!   arguments for the standalone [`Vad`], from a Python dict for the integrated VAD).
//! - [`validate_denoise`]: validates the 48kHz precondition of the integrated denoise.
//!
//! [`Vad`]: crate::vad::Vad

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyDictMethods};

use ::flexaudio as fa;
use fa::{OutputFormat, StreamConfig};
use flexaudio_vad::VadConfig;

use crate::{parse_process_mode, parse_source_kind};

/// Builds a [`StreamConfig`] from Python arguments. `ring_capacity_chunks` uses the default.
/// Like napi's `build_config`, it takes kind/device_id/process_id/mode/exclude_self/
/// output_rate/output_channels/chunk_ms/gain plus the mix-only mic_device_id/
/// system_device_id/mic_gain/system_gain.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_config(
    kind: &str,
    device_id: Option<String>,
    process_id: Option<u32>,
    mode: &str,
    exclude_self: bool,
    output_rate: u32,
    output_channels: u16,
    chunk_ms: u32,
    gain: f32,
    mic_device_id: Option<String>,
    system_device_id: Option<String>,
    mic_gain: f32,
    system_gain: f32,
) -> PyResult<StreamConfig> {
    let kind = parse_source_kind(kind)?;
    let mode = parse_process_mode(mode)?;
    let output = OutputFormat {
        sample_rate: output_rate,
        channels: output_channels,
    };
    Ok(StreamConfig {
        kind,
        output,
        device_id,
        target_pid: process_id,
        // mode is process-only / exclude_self is system-only. The facade enforces not mixing them.
        mode,
        exclude_self,
        chunk_ms,
        gain,
        // Mix-only (the facade ignores these for anything other than mix).
        mix_mic_device_id: mic_device_id,
        mix_system_device_id: system_device_id,
        mix_mic_gain: mic_gain,
        mix_system_gain: system_gain,
        // ring_capacity_chunks uses the default.
        ..Default::default()
    })
}

/// Builds a [`VadConfig`] from explicit arguments (used to construct the standalone
/// [`Vad`](crate::vad::Vad)).
///
/// Validation (sample rate 8k/16k, threshold range) is done by [`flexaudio_vad::Vad::new`],
/// so this only fills in the values.
#[allow(clippy::too_many_arguments)]
pub(crate) fn make_vad_config(
    threshold: f32,
    neg_threshold: Option<f32>,
    min_speech_ms: u32,
    min_silence_ms: u32,
    speech_pad_ms: u32,
    max_speech_ms: u32,
    sample_rate: u32,
) -> VadConfig {
    VadConfig {
        threshold,
        neg_threshold,
        min_speech_ms,
        min_silence_ms,
        speech_pad_ms,
        max_speech_ms,
        sample_rate,
    }
}

/// Builds a [`VadConfig`] from a Python dict for the integrated VAD.
///
/// The keys are the same as the arguments of the standalone [`Vad`](crate::vad::Vad)
/// (`threshold` / `neg_threshold` / `min_speech_ms` / `min_silence_ms` / `speech_pad_ms` /
/// `max_speech_ms` / `sample_rate`). Unspecified keys use [`VadConfig::default`] (the silero
/// defaults). Unknown keys are ignored (forward compatibility).
pub(crate) fn vad_config_from_dict(dict: &Bound<'_, PyDict>) -> PyResult<VadConfig> {
    let d = VadConfig::default();
    Ok(make_vad_config(
        get_f32(dict, "threshold")?.unwrap_or(d.threshold),
        // For neg_threshold, both "key missing" and "explicit None" fall back to the default
        // (None).
        get_opt_f32(dict, "neg_threshold")?.flatten(),
        get_u32(dict, "min_speech_ms")?.unwrap_or(d.min_speech_ms),
        get_u32(dict, "min_silence_ms")?.unwrap_or(d.min_silence_ms),
        get_u32(dict, "speech_pad_ms")?.unwrap_or(d.speech_pad_ms),
        get_u32(dict, "max_speech_ms")?.unwrap_or(d.max_speech_ms),
        get_u32(dict, "sample_rate")?.unwrap_or(d.sample_rate),
    ))
}

// Small helpers that extract a key of a specific type from a dict. pyo3 0.29's FromPyObject
// has 2 lifetimes + an associated Error type, and making this generic hits trait-solver
// limits on the `?` conversion. So each type is spelled out and extracted plainly (a type
// mismatch surfaces as a ValueError).

/// `None` if the key is missing; otherwise converted to f32.
fn get_f32(dict: &Bound<'_, PyDict>, key: &str) -> PyResult<Option<f32>> {
    match dict.get_item(key)? {
        Some(v) => Ok(Some(v.extract::<f32>()?)),
        None => Ok(None),
    }
}

/// `None` if the key is missing; otherwise converted to u32.
fn get_u32(dict: &Bound<'_, PyDict>, key: &str) -> PyResult<Option<u32>> {
    match dict.get_item(key)? {
        Some(v) => Ok(Some(v.extract::<u32>()?)),
        None => Ok(None),
    }
}

/// For neg_threshold: `None` if the key is missing, otherwise `Option<f32>` (Python's None is
/// also accepted).
fn get_opt_f32(dict: &Bound<'_, PyDict>, key: &str) -> PyResult<Option<Option<f32>>> {
    match dict.get_item(key)? {
        Some(v) => Ok(Some(v.extract::<Option<f32>>()?)),
        None => Ok(None),
    }
}

/// Validates the precondition of the integrated denoise (48kHz output). Returns `ValueError`
/// if denoise is enabled and the output rate is not 48000, because RNNoise only runs on
/// fixed 48kHz frames.
pub(crate) fn validate_denoise(denoise: bool, output_rate: u32) -> PyResult<()> {
    if denoise && output_rate != 48_000 {
        return Err(PyValueError::new_err(format!(
            "denoise supports 48000 Hz output only (output_rate={output_rate}). \
             Set output_rate to 48000 when denoise=True."
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Checks StreamConfig / VadConfig assembly and denoise validation without a Python
    //! runtime (the dict path needs a PyDict = a Python host, so the explicit-argument
    //! make_vad_config stands in for it).

    use super::*;
    use fa::{ProcessMode, SourceKind};

    /// Helper that calls build_config with the default arguments (matching the defaults of
    /// open/switch_source).
    fn build_config_with_defaults(kind: &str) -> PyResult<StreamConfig> {
        build_config(
            kind, None, None, "include", false, 48_000, 2, 20, 1.0, None, None, 1.0, 1.0,
        )
    }

    #[test]
    fn build_config_defaults() {
        let cfg = build_config_with_defaults("mic").unwrap();
        assert_eq!(cfg.kind, SourceKind::Mic);
        assert_eq!(cfg.output.sample_rate, 48_000);
        assert_eq!(cfg.output.channels, 2);
        assert_eq!(cfg.mode, ProcessMode::Include);
        assert!(!cfg.exclude_self);
        assert_eq!(cfg.target_pid, None);
        assert_eq!(cfg.device_id, None);
        assert_eq!(cfg.chunk_ms, 20);
        assert_eq!(cfg.gain, 1.0);
        assert_eq!(cfg.mix_mic_device_id, None);
        assert_eq!(cfg.mix_system_device_id, None);
        assert_eq!(cfg.mix_mic_gain, 1.0);
        assert_eq!(cfg.mix_system_gain, 1.0);
        assert_eq!(
            cfg.ring_capacity_chunks,
            StreamConfig::default().ring_capacity_chunks
        );
    }

    #[test]
    fn build_config_reflects_all_fields() {
        let cfg = build_config(
            "process",
            Some("dev-x".to_string()),
            Some(9999),
            "exclude",
            true,
            16_000,
            1,
            20,
            2.5,
            None,
            None,
            1.0,
            1.0,
        )
        .unwrap();
        assert_eq!(cfg.kind, SourceKind::ProcessLoopback);
        assert_eq!(cfg.device_id.as_deref(), Some("dev-x"));
        assert_eq!(cfg.target_pid, Some(9999));
        assert_eq!(cfg.mode, ProcessMode::Exclude);
        assert!(cfg.exclude_self);
        assert_eq!(cfg.output.sample_rate, 16_000);
        assert_eq!(cfg.output.channels, 1);
        assert_eq!(cfg.gain, 2.5);
    }

    #[test]
    fn build_config_reflects_mix_fields() {
        let cfg = build_config(
            "mix",
            None,
            None,
            "include",
            false,
            48_000,
            2,
            20,
            1.0,
            Some("mic-a".to_string()),
            Some("sink-b".to_string()),
            0.5,
            2.0,
        )
        .unwrap();
        assert_eq!(cfg.kind, SourceKind::Mix);
        assert_eq!(cfg.mix_mic_device_id.as_deref(), Some("mic-a"));
        assert_eq!(cfg.mix_system_device_id.as_deref(), Some("sink-b"));
        assert_eq!(cfg.mix_mic_gain, 0.5);
        assert_eq!(cfg.mix_system_gain, 2.0);
    }

    #[test]
    fn build_config_rejects_unknown_kind() {
        assert!(build_config_with_defaults("speaker").is_err());
    }

    #[test]
    fn make_vad_config_reflects_fields() {
        let cfg = make_vad_config(0.3, Some(0.2), 111, 222, 33, 444, 8000);
        assert_eq!(cfg.threshold, 0.3);
        assert_eq!(cfg.neg_threshold, Some(0.2));
        assert_eq!(cfg.min_speech_ms, 111);
        assert_eq!(cfg.min_silence_ms, 222);
        assert_eq!(cfg.speech_pad_ms, 33);
        assert_eq!(cfg.max_speech_ms, 444);
        assert_eq!(cfg.sample_rate, 8000);
    }

    #[test]
    fn make_vad_config_defaults_match_crate() {
        // The argument defaults (Vad's __new__ defaults) must not diverge from
        // VadConfig::default.
        let d = VadConfig::default();
        let cfg = make_vad_config(0.5, None, 250, 100, 30, 0, 16000);
        assert_eq!(cfg, d);
    }

    #[test]
    fn validate_denoise_gate() {
        // With denoise disabled, any rate passes.
        assert!(validate_denoise(false, 16_000).is_ok());
        assert!(validate_denoise(false, 48_000).is_ok());
        // With denoise enabled, only 48000 passes.
        assert!(validate_denoise(true, 48_000).is_ok());
        assert!(validate_denoise(true, 16_000).is_err());
        assert!(validate_denoise(true, 44_100).is_err());
    }
}
