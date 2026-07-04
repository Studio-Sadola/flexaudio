//! 録音ストリーム [`Stream`] と入口 `open()`、VAD / denoise の統合。
//!
//! Python の [`Stream`] は pull/poll 型。napi のような bridge スレッドは持たず、利用側が
//! `poll_chunk` / `poll_event` を周期的に呼ぶ。統合 VAD / denoise の加工も poll_chunk が
//! 呼ばれたその場（GIL 下）で行う（呼ばれた時に処理する）。

use pyo3::prelude::*;
use pyo3::types::PyDict;

use ::flexaudio as fa;
use flexaudio_denoise::Denoiser as CoreDenoiser;
use flexaudio_vad::Vad as CoreVad;

use crate::config::{build_config, vad_config_from_dict, validate_denoise};
use crate::marshal::{chunk_to_py, event_to_py, PyAudioChunk, PyStreamEvent};
use crate::{denoise_err_to_py, to_py_err, vad_err_to_py};

/// 録音ストリームのハンドル。内部の `flexaudio::Stream` を直接 poll する。
///
/// `open(...)` が `start()` まで済ませて返す。利用側は `poll_chunk` / `poll_event` を
/// 周期的に呼ぶ。`stop()` で停止し、context manager（`with`）では `__exit__` で stop する。
///
/// 統合アドオン:
/// - `denoise=True` で開くと、`poll_chunk` が返す前にチャンクの音声を RNNoise で上書きする
///   （48kHz 出力専用。先頭 480 サンプル/ch は denoise の遅延で無音になる）。
/// - `vad={...}` で開くと、各チャンクの音声を VAD にかけ、確定した発話境界を
///   `chunk.vad_events` に添える。加工順は denoise → VAD。
///
/// 統合 VAD が持つ rubato リサンプラが !Sync なので pyclass の Send+Sync 既定を満たせない。
/// Python は poll 型の単一スレッド利用（GIL 下）が前提なので unsendable にして生成スレッドに
/// 固定する（VAD を使わない場合も一律 unsendable）。
#[pyclass(module = "flexaudio", unsendable)]
pub struct Stream {
    inner: fa::Stream,
    // 統合 denoise（48kHz 前提）。無効なら None。
    denoiser: Option<CoreDenoiser>,
    // 統合 VAD。無効なら None。
    vad: Option<CoreVad>,
    // 出力フォーマット。VAD の process_pcm に渡し、denoise の 48kHz 前提判定にも使う。
    // switch_source では出力フォーマットを変えられないので、開いたときの値のまま。
    output_rate: u32,
    output_channels: u16,
}

impl Stream {
    /// アドオン状態（VAD / denoise）を Python 引数から組む。denoise の 48kHz 前提と
    /// Denoiser / Vad の構築失敗をここで検証・変換する。
    fn build_addons(
        vad: Option<&Bound<'_, PyDict>>,
        denoise: bool,
        output_rate: u32,
        output_channels: u16,
    ) -> PyResult<(Option<CoreVad>, Option<CoreDenoiser>)> {
        validate_denoise(denoise, output_rate)?;
        let denoiser = if denoise {
            Some(CoreDenoiser::new(output_channels).map_err(denoise_err_to_py)?)
        } else {
            None
        };
        let vad_state = match vad {
            Some(d) => Some(CoreVad::new(vad_config_from_dict(d)?).map_err(vad_err_to_py)?),
            None => None,
        };
        Ok((vad_state, denoiser))
    }
}

#[pymethods]
impl Stream {
    /// 録音を停止する。二重呼び出し安全（flexaudio 側が冪等）。
    fn stop(&mut self) {
        self.inner.stop();
    }

    /// 録音を止めずに配信だけ一時停止する。`resume` で再開。
    fn pause(&self) {
        self.inner.pause();
    }

    /// 一時停止を解除して配信を再開する。
    fn resume(&self) {
        self.inner.resume();
    }

    /// 一時停止中かどうかを返す。
    fn is_paused(&self) -> bool {
        self.inner.is_paused()
    }

    /// 入力ゲイン（線形倍率）を変更する。1.0 でそのまま、2.0 で約 +6dB、0.0 で無音。
    /// 録音中いつでも呼べて、次のチャンクから効く（20ms 粒度）。乗算後のサンプルは
    /// ±1.0 にクランプされる。有限かつ 0 以上でなければ `ValueError`。
    fn set_gain(&self, gain: f32) -> PyResult<()> {
        self.inner.set_gain(gain).map_err(to_py_err)
    }

    /// 現在の入力ゲイン（線形倍率）を返す。
    fn gain(&self) -> f32 {
        self.inner.gain()
    }

    /// ソースのネイティブフォーマット `(sample_rate, channels)` を返す（第 1 段リサンプル
    /// 前の実入力の形）。ソース切替後は切替先の値になる。
    fn native_format(&self) -> (u32, u16) {
        self.inner.native_format()
    }

    /// リングが溢れて捨てたチャンクの累計数を返す（開始からの通算）。
    fn dropped_chunks(&self) -> u64 {
        self.inner.dropped_chunks()
    }

    /// 取り出せるチャンクがあれば返す。無ければ `None`（非ブロッキング）。
    ///
    /// 統合アドオンが有効なら、返す前にここで加工する（順序は denoise → VAD）。denoise は
    /// チャンクの音声を in-place で上書きし、VAD は加工後の音声で発話境界を判定して
    /// `chunk.vad_events` に添える。どちらも無効なら素通し。
    fn poll_chunk(&mut self) -> Option<PyAudioChunk> {
        let chunk = self.inner.poll_chunk()?;
        let mut py_chunk = chunk_to_py(chunk);

        // 1) denoise: チャンクの音声を in-place で上書き。長さは出力チャンネルの倍数
        //    （frames * channels）で必ず割り切れるのでエラーにはならないが、万一の失敗
        //    （長さ不整合）は best-effort で素通しに倒す（poll を止めない）。
        if let Some(dn) = self.denoiser.as_mut() {
            let _ = dn.process(py_chunk.samples_mut());
        }

        // 2) VAD: 加工後の音声で発話境界を判定して添える。process_pcm が内部で mono 化・
        //    VAD レートへのリサンプルを行うので、出力フォーマットのまま渡してよい。
        if let Some(vad) = self.vad.as_mut() {
            let events: Vec<(bool, u64)> = vad
                .process_pcm(py_chunk.samples(), self.output_rate, self.output_channels)
                .into_iter()
                .map(|ev| match ev {
                    flexaudio_vad::VadEvent::SpeechStart { at_sample } => (true, at_sample),
                    flexaudio_vad::VadEvent::SpeechEnd { at_sample } => (false, at_sample),
                })
                .collect();
            py_chunk.set_vad_events(events);
        }

        Some(py_chunk)
    }

    /// 取り出せるイベントがあれば返す。無ければ `None`（非ブロッキング）。
    fn poll_event(&mut self) -> Option<PyStreamEvent> {
        self.inner.poll_event().map(event_to_py)
    }

    /// 録音を止めずに入力ソース（mic/system/process/mix）をホットスワップする。
    ///
    /// 出力フォーマット（output_rate/output_channels）は切替では変えられない。変更を
    /// 要求すると `switch_source` がエラーを返し、ここで例外になる。
    /// `gain` も受けるがコアが無視する（ゲインはストリームの状態。変更は `set_gain`）。
    /// `mic_device_id`/`system_device_id`/`mic_gain`/`system_gain` は mix 専用
    /// （他ソースでは無視される）。
    ///
    /// `vad` / `denoise` は統合アドオンを再指定する。ソースが変わると音声が不連続になるので、
    /// 指定に応じてアドオンを作り直す（内部状態はリセット）。省略すると既定（`vad=None` /
    /// `denoise=False`）＝アドオン無効になる（open と同じ流儀で、切替のたびに明示する）。
    #[pyo3(signature = (
        kind,
        *,
        device_id = None,
        process_id = None,
        mode = "include".to_string(),
        exclude_self = false,
        output_rate = 48_000,
        output_channels = 2,
        chunk_ms = 20,
        gain = 1.0,
        mic_device_id = None,
        system_device_id = None,
        mic_gain = 1.0,
        system_gain = 1.0,
        vad = None,
        denoise = false,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn switch_source(
        &mut self,
        kind: &str,
        device_id: Option<String>,
        process_id: Option<u32>,
        mode: String,
        exclude_self: bool,
        output_rate: u32,
        output_channels: u16,
        chunk_ms: u32,
        gain: f32,
        mic_device_id: Option<String>,
        system_device_id: Option<String>,
        mic_gain: f32,
        system_gain: f32,
        vad: Option<Bound<'_, PyDict>>,
        denoise: bool,
    ) -> PyResult<()> {
        // アドオンは出力フォーマットに依存する。切替では出力フォーマットは変わらないので、
        // 開いたときの output_rate/output_channels を使って検証・構築する（引数の
        // output_rate は core の switch_source が形の一致確認に使う）。
        let (new_vad, new_denoiser) = Self::build_addons(
            vad.as_ref(),
            denoise,
            self.output_rate,
            self.output_channels,
        )?;

        let config = build_config(
            kind,
            device_id,
            process_id,
            &mode,
            exclude_self,
            output_rate,
            output_channels,
            chunk_ms,
            gain,
            mic_device_id,
            system_device_id,
            mic_gain,
            system_gain,
        )?;
        self.inner.switch_source(config).map_err(to_py_err)?;

        // 切替が成功してからアドオンを差し替える（失敗時は旧アドオンを保つ）。
        self.vad = new_vad;
        self.denoiser = new_denoiser;
        Ok(())
    }

    /// context manager 対応。`with flexaudio.open("mic") as s:` で使える。
    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    /// `with` ブロックを抜けるとき stop する。例外は握り潰さない（False を返す）。
    fn __exit__(
        &mut self,
        _exc_type: Option<Bound<'_, PyAny>>,
        _exc_value: Option<Bound<'_, PyAny>>,
        _traceback: Option<Bound<'_, PyAny>>,
    ) -> bool {
        self.inner.stop();
        false
    }
}

/// ストリームを開いて `start()` まで済ませ、[`Stream`] を返す。
///
/// `kind` は "mic"|"system"|"process"|"mix"。不正値は `ValueError`。デバイスが無い
/// 環境では open / start が flexaudio のエラーを上げる（`RuntimeError` 等に変換される）。
/// `mic_device_id`/`system_device_id`/`mic_gain`/`system_gain` は mix 専用で、mix の
/// mic 側 / system 側のデバイス選択と合成前倍率を決める（他ソースでは無視される。
/// 合成後にグローバル `gain` が掛かる）。
///
/// 統合アドオン:
/// - `vad`（dict・既定 None）: 指定すると統合 VAD が有効になる。キーは独立 `Vad` の引数と
///   同じ（`threshold` / `min_speech_ms` / `min_silence_ms` / `speech_pad_ms` /
///   `max_speech_ms` / `sample_rate` / `neg_threshold`）。各チャンクの `vad_events` に
///   発話境界が入る。
/// - `denoise`（bool・既定 False）: True で RNNoise によるノイズ抑制を有効化する。48kHz
///   出力専用で、`denoise=True` かつ `output_rate!=48000` は `ValueError`。加工順は
///   denoise → VAD。
#[pyfunction]
#[pyo3(signature = (
    kind,
    *,
    device_id = None,
    process_id = None,
    mode = "include".to_string(),
    exclude_self = false,
    output_rate = 48_000,
    output_channels = 2,
    chunk_ms = 20,
    gain = 1.0,
    mic_device_id = None,
    system_device_id = None,
    mic_gain = 1.0,
    system_gain = 1.0,
    vad = None,
    denoise = false,
))]
#[allow(clippy::too_many_arguments)]
pub fn open(
    kind: &str,
    device_id: Option<String>,
    process_id: Option<u32>,
    mode: String,
    exclude_self: bool,
    output_rate: u32,
    output_channels: u16,
    chunk_ms: u32,
    gain: f32,
    mic_device_id: Option<String>,
    system_device_id: Option<String>,
    mic_gain: f32,
    system_gain: f32,
    vad: Option<Bound<'_, PyDict>>,
    denoise: bool,
) -> PyResult<Stream> {
    // アドオンの検証・構築を先に済ませる（denoise の 48kHz 前提・VAD 設定不正はここで弾く。
    // デバイスを掴む前に失敗させたい）。
    let (vad_state, denoiser) =
        Stream::build_addons(vad.as_ref(), denoise, output_rate, output_channels)?;

    let config = build_config(
        kind,
        device_id,
        process_id,
        &mode,
        exclude_self,
        output_rate,
        output_channels,
        chunk_ms,
        gain,
        mic_device_id,
        system_device_id,
        mic_gain,
        system_gain,
    )?;
    let mut stream = fa::open(config).map_err(to_py_err)?;
    stream.start().map_err(to_py_err)?;
    Ok(Stream {
        inner: stream,
        denoiser,
        vad: vad_state,
        output_rate,
        output_channels,
    })
}
