//! Convert and validate Python arguments for core configuration types.
//!
//! - [`build_config`]: Build [`StreamConfig`] from `open` / `switch_source` arguments.
//! - [`make_vad_config`] / [`vad_config_from_dict`]: Build VAD configuration (explicit
//!   arguments for standalone [`Vad`], or a Python dict for integrated VAD).
//! - [`validate_denoise`]: Validate the 48 kHz requirement for integrated denoise.
//!
//! [`Vad`]: crate::vad::Vad

use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{
    PyBool, PyBytes, PyDict, PyDictMethods, PyInt, PySequence, PySequenceMethods, PyString,
};

use ::flexaudio as fa;
use fa::{OutputFormat, StreamConfig};
use flexaudio_vad::VadConfig;

use crate::{parse_process_mode, parse_source_kind};

/// Validate exclusion at the Python boundary before addons or device acquisition.
/// Keep the sequence order and duplicates; integer extraction follows explicit type checks
/// so bools and objects implementing only __index__ cannot become PIDs.
pub(crate) fn parse_exclude_pids(value: Option<&Bound<'_, PyAny>>) -> PyResult<Vec<u32>> {
    let Some(value) = value.filter(|value| !value.is_none()) else {
        return Ok(Vec::new());
    };
    let sequence_error =
        || PyTypeError::new_err("exclude_pids must be a sequence of integers or None");
    if value.is_instance_of::<PyString>()
        || value.is_instance_of::<PyBytes>()
        || value.is_instance_of::<PyDict>()
    {
        return Err(sequence_error());
    }
    let sequence = value.cast::<PySequence>().map_err(|_| sequence_error())?;
    let len = sequence.len()?;
    if len > 4096 {
        return Err(PyValueError::new_err(
            "exclude_pids: too many entries (max 4096)",
        ));
    }
    let mut pids = Vec::with_capacity(len);
    for index in 0..len {
        let item = sequence.get_item(index)?;
        let message = || {
            format!(
                "exclude_pids[{index}] must be a positive integer in 1..=4294967295, got {}",
                match item.repr() {
                    Ok(repr) => repr.to_string_lossy().into_owned(),
                    // Python may refuse decimal formatting of extremely large integers.
                    Err(_) => "<unrepresentable value>".to_string(),
                }
            )
        };
        if item.is_instance_of::<PyBool>() || !item.is_instance_of::<PyInt>() {
            return Err(PyTypeError::new_err(message()));
        }
        let pid = match item.extract::<u32>() {
            Ok(pid) if pid != 0 => pid,
            _ => return Err(PyValueError::new_err(message())),
        };
        pids.push(pid);
    }
    Ok(pids)
}

/// Build [`StreamConfig`] from Python arguments. Use the default for `ring_capacity_chunks`.
/// Like napi's `build_config`, accepts kind/device_id/process_id/mode/exclude_self/
/// output_rate/output_channels/chunk_ms/gain and the mix-specific mic_device_id/
/// system_device_id/mic_gain/system_gain.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_config(
    kind: &str,
    device_id: Option<String>,
    process_id: Option<u32>,
    mode: &str,
    exclude_self: bool,
    exclude_pids: Vec<u32>,
    output_rate: u32,
    output_channels: u16,
    chunk_ms: u32,
    gain: f32,
    mic_device_id: Option<String>,
    system_device_id: Option<String>,
    mic_gain: f32,
    system_gain: f32,
) -> PyResult<StreamConfig> {
    if chunk_ms != 20 {
        return Err(crate::to_py_err(::flexaudio::Error::InvalidArg(
            "chunk_ms must be 20".into(),
        )));
    }
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
        // mode is process-only; exclusions apply to system capture and the system side of mix.
        mode,
        exclude_self,
        exclude_pids,
        chunk_ms,
        gain,
        // Mix-only (the facade ignores these for other source kinds).
        mix_mic_device_id: mic_device_id,
        mix_system_device_id: system_device_id,
        mix_mic_gain: mic_gain,
        mix_system_gain: system_gain,
        // Use the default for ring_capacity_chunks.
        ..Default::default()
    })
}

/// Build [`VadConfig`] from explicit arguments (used to construct standalone [`Vad`](crate::vad::Vad)).
///
/// [`flexaudio_vad::Vad::new`] validates the sample rate (8k/16k) and threshold ranges, so
/// this function only assigns the values.
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

/// Build [`VadConfig`] from a Python dict for integrated VAD.
///
/// Keys match the arguments for standalone [`Vad`](crate::vad::Vad) (`threshold` / `neg_threshold` /
/// `min_speech_ms` / `min_silence_ms` / `speech_pad_ms` / `max_speech_ms` /
/// `sample_rate`). Missing keys use [`VadConfig::default`] (Silero defaults). Unknown keys
/// are ignored for forward compatibility.
pub(crate) fn vad_config_from_dict(dict: &Bound<'_, PyDict>) -> PyResult<VadConfig> {
    let d = VadConfig::default();
    Ok(make_vad_config(
        get_f32(dict, "threshold")?.unwrap_or(d.threshold),
        // Both a missing neg_threshold key and an explicit None use the default (None).
        get_opt_f32(dict, "neg_threshold")?.flatten(),
        get_u32(dict, "min_speech_ms")?.unwrap_or(d.min_speech_ms),
        get_u32(dict, "min_silence_ms")?.unwrap_or(d.min_silence_ms),
        get_u32(dict, "speech_pad_ms")?.unwrap_or(d.speech_pad_ms),
        get_u32(dict, "max_speech_ms")?.unwrap_or(d.max_speech_ms),
        get_u32(dict, "sample_rate")?.unwrap_or(d.sample_rate),
    ))
}

// Small helpers to extract typed values from a dict. In pyo3 0.29, FromPyObject has two
// lifetimes and an associated Error type. Generic helpers hit trait-solver limits when
// converting with `?`, so use concrete helpers and call extract directly (type mismatches
// surface as ValueError).

/// Return `None` if the key is missing; otherwise extract an f32.
fn get_f32(dict: &Bound<'_, PyDict>, key: &str) -> PyResult<Option<f32>> {
    match dict.get_item(key)? {
        Some(v) => Ok(Some(v.extract::<f32>()?)),
        None => Ok(None),
    }
}

/// Return `None` if the key is missing; otherwise extract a u32.
fn get_u32(dict: &Bound<'_, PyDict>, key: &str) -> PyResult<Option<u32>> {
    match dict.get_item(key)? {
        Some(v) => Ok(Some(v.extract::<u32>()?)),
        None => Ok(None),
    }
}

/// For neg_threshold: return `None` if the key is missing; otherwise extract `Option<f32>`
/// (Python None is allowed).
fn get_opt_f32(dict: &Bound<'_, PyDict>, key: &str) -> PyResult<Option<Option<f32>>> {
    match dict.get_item(key)? {
        Some(v) => Ok(Some(v.extract::<Option<f32>>()?)),
        None => Ok(None),
    }
}

/// Validate the 48 kHz output requirement for integrated denoise. If denoise is enabled and
/// the output rate is not 48000, return `ValueError` because RNNoise only accepts fixed frames
/// at 48 kHz.
pub(crate) fn validate_denoise(denoise: bool, output_rate: u32) -> PyResult<()> {
    if denoise && output_rate != 48_000 {
        return Err(PyValueError::new_err(format!(
            "denoise requires 48000 Hz output (output_rate={output_rate}). \
             Set output_rate to 48000 when denoise=True."
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Check StreamConfig / VadConfig construction and denoise validation without a Python
    //! runtime (the dict path requires PyDict and a Python host, so use make_vad_config with
    //! explicit arguments instead).

    use super::*;
    use fa::{ProcessMode, SourceKind};

    #[test]
    fn exclusion_parser_preserves_valid_sequences() {
        Python::initialize();
        Python::attach(|py| {
            for expression in [
                pyo3::ffi::c_str!("[1, 4294967295, 1]"),
                pyo3::ffi::c_str!("(1, 4294967295, 1)"),
            ] {
                let value = py.eval(expression, None, None).expect("valid sequence");
                assert_eq!(
                    parse_exclude_pids(Some(&value)).expect("valid PIDs"),
                    vec![1, u32::MAX, 1]
                );
            }
            let value = py
                .eval(pyo3::ffi::c_str!("[1] * 4096"), None, None)
                .expect("maximum length sequence");
            assert_eq!(
                parse_exclude_pids(Some(&value))
                    .expect("valid length")
                    .len(),
                4096
            );
            assert!(parse_exclude_pids(None).expect("default").is_empty());
            let value = py.None().into_bound(py);
            assert!(parse_exclude_pids(Some(&value)).expect("None").is_empty());
        });
    }

    /// Helper that calls build_config with default-equivalent arguments, matching open/switch_source.
    fn build_config_with_defaults(kind: &str) -> PyResult<StreamConfig> {
        build_config(
            kind,
            None,
            None,
            "include",
            false,
            Vec::new(),
            48_000,
            2,
            20,
            1.0,
            None,
            None,
            1.0,
            1.0,
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
        assert!(cfg.exclude_pids.is_empty());
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
            vec![9999, 9999],
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
        assert_eq!(cfg.exclude_pids, vec![9999, 9999]);
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
            Vec::new(),
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
        // Ensure Vad.__new__ argument defaults match VadConfig::default.
        let d = VadConfig::default();
        let cfg = make_vad_config(0.5, None, 250, 100, 30, 0, 16000);
        assert_eq!(cfg, d);
    }

    #[test]
    fn validate_denoise_gate() {
        // Any rate is accepted when denoise is disabled.
        assert!(validate_denoise(false, 16_000).is_ok());
        assert!(validate_denoise(false, 48_000).is_ok());
        // When denoise is enabled, only 48000 is accepted.
        assert!(validate_denoise(true, 48_000).is_ok());
        assert!(validate_denoise(true, 16_000).is_err());
        assert!(validate_denoise(true, 44_100).is_err());
    }
}
