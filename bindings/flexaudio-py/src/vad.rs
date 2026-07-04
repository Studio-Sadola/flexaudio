//! 独立した VAD（音声区間検出）クラス [`Vad`]。
//!
//! silero-VAD を ONNX でオフライン実行するアドオン（[`flexaudio_vad`]）の Python 露出。
//! 録音チャンク（任意フォーマットの interleaved f32）をそのまま [`Vad::process`] に流すと、
//! 内部で mono 化・VAD レートへリサンプルしてから発話境界を判定する。

use pyo3::prelude::*;

use flexaudio_vad::Vad as CoreVad;

use crate::config::make_vad_config;
use crate::marshal::{vad_event_to_py, PyVadEvent};
use crate::vad_err_to_py;

/// ストリーミング VAD。1 インスタンスが ONNX セッションを 1 つ持つ。
///
/// 任意フォーマットの録音チャンクを [`process`](Vad::process) に流すと、確定した発話境界を
/// [`VadEvent`](PyVadEvent) のリストで返す。`at_sample` は VAD 内部レート基準。
// rubato のリサンプラ（VAD の前段変換）が !Sync なので pyclass の Send+Sync 既定を満たせ
// ない。Python は poll 型の単一スレッド利用（GIL 下）が前提なので unsendable にして生成
// スレッドに固定する。
#[pyclass(module = "flexaudio", name = "Vad", unsendable)]
pub struct Vad {
    inner: CoreVad,
}

#[pymethods]
impl Vad {
    /// 設定を指定して VAD を構築する。既定値は silero-VAD の `get_speech_timestamps`
    /// （= [`VadConfig::default`](flexaudio_vad::VadConfig)）に揃えてある。`sample_rate` は
    /// 8000 か 16000 のみ（それ以外は `ValueError`）。`neg_threshold` は無音判定の負側
    /// しきい値で、`None` なら `max(threshold - 0.15, 0.01)`（silero 準拠）。
    #[new]
    #[pyo3(signature = (
        threshold = 0.5,
        min_speech_ms = 250,
        min_silence_ms = 100,
        speech_pad_ms = 30,
        max_speech_ms = 0,
        sample_rate = 16_000,
        neg_threshold = None,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        threshold: f32,
        min_speech_ms: u32,
        min_silence_ms: u32,
        speech_pad_ms: u32,
        max_speech_ms: u32,
        sample_rate: u32,
        neg_threshold: Option<f32>,
    ) -> PyResult<Self> {
        let config = make_vad_config(
            threshold,
            neg_threshold,
            min_speech_ms,
            min_silence_ms,
            speech_pad_ms,
            max_speech_ms,
            sample_rate,
        );
        let inner = CoreVad::new(config).map_err(vad_err_to_py)?;
        Ok(Vad { inner })
    }

    /// 任意フォーマット（`input_sample_rate` / `input_channels` の interleaved f32）の
    /// サンプル列を処理し、確定した [`VadEvent`](PyVadEvent) のリストを返す。
    ///
    /// 端数フレームは内部に持ち越すので、任意の位置で分割して連続で渡してよい（継ぎ目は
    /// 出ない）。`samples` は list / array.array / numpy 配列いずれも渡せる。
    fn process(
        &mut self,
        samples: Vec<f32>,
        input_sample_rate: u32,
        input_channels: u16,
    ) -> Vec<PyVadEvent> {
        self.inner
            .process_pcm(&samples, input_sample_rate, input_channels)
            .into_iter()
            .map(vad_event_to_py)
            .collect()
    }

    /// 状態・端数バッファ・リサンプラをすべて初期化する（別のストリームを続けて処理できる）。
    fn reset(&mut self) {
        self.inner.reset();
    }
}
