//! Convert and validate Python arguments for core configuration types.
//!
//! - [`build_config`]: Build [`StreamConfig`] from `open` / `switch_source` arguments.
//! - [`validate_denoise`]: Validate the 48 kHz requirement for integrated denoise.

use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyBytes, PyDict, PyInt, PySequence, PySequenceMethods, PyString};

use ::flexaudio as fa;
use fa::{OutputFormat, StreamConfig};

use crate::{parse_process_mode, parse_source_kind};

pub(crate) fn parse_ring_capacity(value: Option<&Bound<'_, PyAny>>) -> PyResult<usize> {
    let Some(value) = value.filter(|value| !value.is_none()) else {
        return Ok(50);
    };
    if value.is_instance_of::<PyBool>() || !value.is_instance_of::<PyInt>() {
        return Err(PyTypeError::new_err(
            "ring_capacity_chunks must be an integer or None",
        ));
    }
    match value.extract::<usize>() {
        Ok(value) if value > 0 => Ok(value),
        _ => Err(PyValueError::new_err(
            "ring_capacity_chunks must be positive and fit native usize",
        )),
    }
}

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
        let message = || -> PyResult<String> {
            let repr = match item.repr() {
                Ok(repr) => repr.to_string_lossy().into_owned(),
                Err(error) if item.is_instance_of::<PyInt>() => {
                    // Python limits decimal integer formatting; hexadecimal remains exact.
                    let _ = error;
                    item.py()
                        .import("builtins")?
                        .getattr("hex")?
                        .call1((&item,))?
                        .extract::<String>()?
                }
                Err(error) => return Err(error),
            };
            Ok(format!(
                "exclude_pids[{index}] must be a positive integer in 1..=4294967295, got {repr}"
            ))
        };
        if item.is_instance_of::<PyBool>() || !item.is_instance_of::<PyInt>() {
            return Err(PyTypeError::new_err(message()?));
        }
        let pid = match item.extract::<u32>() {
            Ok(pid) if pid != 0 => pid,
            _ => return Err(PyValueError::new_err(message()?)),
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
    ::flexaudio::core::validate_chunk_ms(chunk_ms).map_err(crate::to_py_err)?;
    let kind = parse_source_kind(kind)?;
    let mode = parse_process_mode(mode)?;
    let output = OutputFormat {
        sample_rate: output_rate,
        channels: output_channels,
    };
    Ok({
        let mut config_value = StreamConfig::new(kind);
        config_value.output = output;
        config_value.device_id = device_id;
        config_value.target_pid = process_id;
        // mode is process-only; exclusions apply to system capture and the system side of mix.
        config_value.mode = mode;
        config_value.exclude_self = exclude_self;
        config_value.exclude_pids = exclude_pids;
        config_value.chunk_ms = chunk_ms;
        config_value.gain = gain;
        // Mix-only (the facade ignores these for other source kinds).
        config_value.mix_mic_device_id = mic_device_id;
        config_value.mix_system_device_id = system_device_id;
        config_value.mix_mic_gain = mic_gain;
        config_value.mix_system_gain = system_gain;
        config_value
    })
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
    //! Check configuration construction, exclusion parsing, and denoise validation.

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

    #[test]
    fn silent_failure_rejected_huge_pid_reports_the_value_instead_of_a_placeholder() {
        Python::initialize();
        Python::attach(|py| {
            let value = py
                .eval(pyo3::ffi::c_str!("[10 ** 5000]"), None, None)
                .unwrap();
            let error = parse_exclude_pids(Some(&value)).unwrap_err();
            let message = error.to_string();
            assert!(!message.contains("<unrepresentable value>"));
            assert!(message.contains("0x") || message.contains(&"0".repeat(5000)));
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
