//! Python 引数 → コアの各種 config への変換と検証。
//!
//! - [`build_config`]: `open` / `switch_source` の引数から [`StreamConfig`] を組む。
//! - [`make_vad_config`] / [`vad_config_from_dict`]: VAD の設定を組む（独立 [`Vad`] 用は
//!   明示引数、統合 VAD 用は Python の dict から）。
//! - [`validate_denoise`]: 統合 denoise の 48kHz 前提を検証する。
//!
//! [`Vad`]: crate::vad::Vad

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyDictMethods};

use ::flexaudio as fa;
use fa::{OutputFormat, StreamConfig};
use flexaudio_vad::VadConfig;

use crate::{parse_process_mode, parse_source_kind};

/// Python 引数から [`StreamConfig`] を組む。`ring_capacity_chunks` は既定値を使う。
/// napi の `build_config` と同じく kind/device_id/process_id/mode/exclude_self/
/// output_rate/output_channels/chunk_ms/gain と mix 専用の mic_device_id/
/// system_device_id/mic_gain/system_gain を受ける。
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
        // mode は process 専用 / exclude_self は system 専用。混ぜないのは facade 側が見る。
        mode,
        exclude_self,
        chunk_ms,
        gain,
        // mix 専用（mix 以外では facade が無視する）。
        mix_mic_device_id: mic_device_id,
        mix_system_device_id: system_device_id,
        mix_mic_gain: mic_gain,
        mix_system_gain: system_gain,
        // ring_capacity_chunks は既定値を使う。
        ..Default::default()
    })
}

/// 明示引数から [`VadConfig`] を組む（独立 [`Vad`](crate::vad::Vad) の構築に使う）。
///
/// 妥当性検証（サンプルレート 8k/16k・しきい値域）は [`flexaudio_vad::Vad::new`] が行う
/// ので、ここでは値を詰めるだけ。
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

/// 統合 VAD 用に Python の dict から [`VadConfig`] を組む。
///
/// キーは独立 [`Vad`](crate::vad::Vad) の引数と同じ（`threshold` / `neg_threshold` /
/// `min_speech_ms` / `min_silence_ms` / `speech_pad_ms` / `max_speech_ms` /
/// `sample_rate`）。未指定のキーは [`VadConfig::default`]（silero 既定）を使う。未知の
/// キーは無視する（前方互換）。
pub(crate) fn vad_config_from_dict(dict: &Bound<'_, PyDict>) -> PyResult<VadConfig> {
    let d = VadConfig::default();
    Ok(make_vad_config(
        get_f32(dict, "threshold")?.unwrap_or(d.threshold),
        // neg_threshold は「キーが無い」も「明示 None」も既定（None）に倒す。
        get_opt_f32(dict, "neg_threshold")?.flatten(),
        get_u32(dict, "min_speech_ms")?.unwrap_or(d.min_speech_ms),
        get_u32(dict, "min_silence_ms")?.unwrap_or(d.min_silence_ms),
        get_u32(dict, "speech_pad_ms")?.unwrap_or(d.speech_pad_ms),
        get_u32(dict, "max_speech_ms")?.unwrap_or(d.max_speech_ms),
        get_u32(dict, "sample_rate")?.unwrap_or(d.sample_rate),
    ))
}

// dict から特定型のキーを取り出す小さなヘルパ群。pyo3 0.29 の FromPyObject は 2
// ライフタイム + 関連 Error 型を持ち、ジェネリックにすると `?` の変換がトレイトソルバの
// 制限に当たる。型ごとに具体化して素直に extract する（型不一致は ValueError で上がる）。

/// キーが無ければ `None`。あれば f32 へ。
fn get_f32(dict: &Bound<'_, PyDict>, key: &str) -> PyResult<Option<f32>> {
    match dict.get_item(key)? {
        Some(v) => Ok(Some(v.extract::<f32>()?)),
        None => Ok(None),
    }
}

/// キーが無ければ `None`。あれば u32 へ。
fn get_u32(dict: &Bound<'_, PyDict>, key: &str) -> PyResult<Option<u32>> {
    match dict.get_item(key)? {
        Some(v) => Ok(Some(v.extract::<u32>()?)),
        None => Ok(None),
    }
}

/// neg_threshold 用: キーが無ければ `None`、あれば `Option<f32>`（Python の None も許す）。
fn get_opt_f32(dict: &Bound<'_, PyDict>, key: &str) -> PyResult<Option<Option<f32>>> {
    match dict.get_item(key)? {
        Some(v) => Ok(Some(v.extract::<Option<f32>>()?)),
        None => Ok(None),
    }
}

/// 統合 denoise の前提（48kHz 出力）を検証する。denoise 有効かつ出力レートが 48000 で
/// なければ `ValueError`。RNNoise が 48kHz 固定フレームでしか動かないため。
pub(crate) fn validate_denoise(denoise: bool, output_rate: u32) -> PyResult<()> {
    if denoise && output_rate != 48_000 {
        return Err(PyValueError::new_err(format!(
            "denoise は 48000 Hz 出力専用です（output_rate={output_rate}）。\
             denoise=True のときは output_rate を 48000 にしてください。"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! StreamConfig / VadConfig の組み立てと denoise 検証を Python ランタイム無しで見る
    //! （dict 経路は PyDict = Python ホストが要るので、明示引数版 make_vad_config で代替）。

    use super::*;
    use fa::{ProcessMode, SourceKind};

    /// 既定引数相当で build_config を呼ぶヘルパ（open/switch_source の既定と揃える）。
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
        // 引数既定（Vad の __new__ 既定）と VadConfig::default が食い違っていないこと。
        let d = VadConfig::default();
        let cfg = make_vad_config(0.5, None, 250, 100, 30, 0, 16000);
        assert_eq!(cfg, d);
    }

    #[test]
    fn validate_denoise_gate() {
        // denoise 無効ならどのレートでも通る。
        assert!(validate_denoise(false, 16_000).is_ok());
        assert!(validate_denoise(false, 48_000).is_ok());
        // denoise 有効は 48000 のみ通る。
        assert!(validate_denoise(true, 48_000).is_ok());
        assert!(validate_denoise(true, 16_000).is_err());
        assert!(validate_denoise(true, 44_100).is_err());
    }
}
