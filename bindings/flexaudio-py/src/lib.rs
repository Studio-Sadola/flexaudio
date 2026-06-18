//! flexaudio-py — Python バインディング (PyO3 + maturin)。flexaudio を直接リンクする。
//!
//! Python アプリが flexaudio をインプロセスで使うためのバインディング。flexaudio-napi
//! （Node 向け）と同じ作法を PyO3 へ翻訳したもの。
//!
//! 設計:
//! - `open(...)` で `flexaudio::open` し、そのまま `start()` まで済ませて [`Stream`] を返す
//!   （napi の `open_stream` と同じく open で start まで行う）。
//! - poll 系（`poll_chunk` / `poll_event`）は非ブロッキングで速いので GIL は解放しない。
//!   pyclass のメソッドは GIL 保持下で呼ばれるので、内部 `flexaudio::Stream` への同時
//!   アクセスは起きない（napi のような bridge スレッドは持たない）。
//! - チャンクの `data` は interleaved `f32` をリトルエンディアン生バイト（`bytes`）で渡す。
//!   numpy 利用者は `np.frombuffer(chunk.data, dtype=np.float32)` で読む。
//!
//! 実行時にネットワーク通信はしない（PyO3 は Python 拡張ブリッジのみ）。

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;

// 依存クレート `flexaudio` を `fa` で参照する。この cdylib の `[lib] name` と
// `#[pymodule] fn flexaudio` が同名 `flexaudio` を作るため、素の `flexaudio::` は
// クレートとモジュールで衝突する。別名にして曖昧さを断つ。
use ::flexaudio as fa;
use fa::{AudioChunk, DeviceInfo, Event, OutputFormat, ProcessMode, SourceKind, StreamConfig};

// ---------------------------------------------------------------------------
// エラー変換
// ---------------------------------------------------------------------------

/// flexaudio::Error → Python 例外。引数系は `ValueError`、それ以外は `RuntimeError`。
/// いずれもメッセージ（Display）を保持する。
fn to_py_err(err: fa::Error) -> PyErr {
    let msg = err.to_string();
    match err {
        fa::Error::InvalidArg(_) | fa::Error::UnsupportedFormat(_) => PyValueError::new_err(msg),
        _ => PyRuntimeError::new_err(msg),
    }
}

// ---------------------------------------------------------------------------
// 列挙体 ↔ 文字列の変換ヘルパ
// ---------------------------------------------------------------------------

/// [`SourceKind`] を Python 向け文字列（"mic"|"system"|"process"）へ。
fn source_kind_str(k: SourceKind) -> &'static str {
    match k {
        SourceKind::Mic => "mic",
        SourceKind::SystemLoopback => "system",
        SourceKind::ProcessLoopback => "process",
    }
}

/// "mic"|"system"|"process" を [`SourceKind`] へ。不正値は `ValueError`。
fn parse_source_kind(s: &str) -> PyResult<SourceKind> {
    match s {
        "mic" => Ok(SourceKind::Mic),
        "system" => Ok(SourceKind::SystemLoopback),
        "process" => Ok(SourceKind::ProcessLoopback),
        other => Err(PyValueError::new_err(format!(
            "unknown kind: {other:?} (expected mic|system|process)"
        ))),
    }
}

/// "include"|"exclude" を [`ProcessMode`] へ（process 専用）。既定は Include。
fn parse_process_mode(s: &str) -> PyResult<ProcessMode> {
    match s {
        "include" => Ok(ProcessMode::Include),
        "exclude" => Ok(ProcessMode::Exclude),
        other => Err(PyValueError::new_err(format!(
            "unknown mode: {other:?} (expected include|exclude)"
        ))),
    }
}

// ---------------------------------------------------------------------------
// StreamConfig の構築
// ---------------------------------------------------------------------------

/// Python 引数から [`StreamConfig`] を組む。`ring_capacity_chunks` は既定値を使う。
/// napi の `build_config` と同じく kind/device_id/process_id/mode/exclude_self/
/// output_rate/output_channels/chunk_ms だけを受ける。
#[allow(clippy::too_many_arguments)]
fn build_config(
    kind: &str,
    device_id: Option<String>,
    process_id: Option<u32>,
    mode: &str,
    exclude_self: bool,
    output_rate: u32,
    output_channels: u16,
    chunk_ms: u32,
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
        // ring_capacity_chunks は既定値を使う。
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------
// DeviceInfo（pyclass・getter）
// ---------------------------------------------------------------------------

/// `devices()` が返すデバイス情報。`source_kind` は文字列（"mic"|"system"|"process"）。
#[pyclass(module = "flexaudio", name = "DeviceInfo", frozen)]
pub struct PyDeviceInfo {
    #[pyo3(get)]
    id: String,
    #[pyo3(get)]
    name: String,
    #[pyo3(get)]
    source_kind: String,
    #[pyo3(get)]
    sample_rate: u32,
    #[pyo3(get)]
    channels: u16,
    #[pyo3(get)]
    is_loopback: bool,
    #[pyo3(get)]
    is_default: bool,
}

#[pymethods]
impl PyDeviceInfo {
    fn __repr__(&self) -> String {
        format!(
            "DeviceInfo(id={:?}, name={:?}, source_kind={:?}, sample_rate={}, channels={}, is_loopback={}, is_default={})",
            self.id,
            self.name,
            self.source_kind,
            self.sample_rate,
            self.channels,
            bool_repr(self.is_loopback),
            bool_repr(self.is_default),
        )
    }
}

fn device_info_to_py(info: DeviceInfo) -> PyDeviceInfo {
    PyDeviceInfo {
        id: info.id,
        name: info.name,
        source_kind: source_kind_str(info.source_kind).to_string(),
        sample_rate: info.sample_rate,
        channels: info.channels,
        is_loopback: info.is_loopback,
        is_default: info.is_default,
    }
}

// ---------------------------------------------------------------------------
// AudioChunk（pyclass・getter）
// ---------------------------------------------------------------------------

/// 1 チャンク分の録音データ。`data` は interleaved f32 のリトルエンディアン生バイト
/// （len = frames * channels * 4）。numpy では `np.frombuffer(chunk.data, dtype=np.float32)`。
#[pyclass(module = "flexaudio", name = "AudioChunk", frozen)]
pub struct PyAudioChunk {
    // interleaved f32 サンプル。生バイトは `data` getter でリトルエンディアン化して渡す。
    samples: Vec<f32>,
    #[pyo3(get)]
    frames: usize,
    #[pyo3(get)]
    pts_ns: i64,
    #[pyo3(get)]
    seq: u64,
    #[pyo3(get)]
    flags: u32,
    #[pyo3(get)]
    dropped_before: u32,
    #[pyo3(get)]
    peak: f32,
    #[pyo3(get)]
    rms: f32,
}

#[pymethods]
impl PyAudioChunk {
    /// interleaved f32 サンプルをリトルエンディアン生バイトで返す。
    #[getter]
    fn data<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        // f32 をリトルエンディアン 4 バイトずつ並べる（bytemuck を使わず安全に書く）。
        let mut buf = Vec::with_capacity(self.samples.len() * 4);
        for s in &self.samples {
            buf.extend_from_slice(&s.to_le_bytes());
        }
        PyBytes::new(py, &buf)
    }

    fn __repr__(&self) -> String {
        format!(
            "AudioChunk(frames={}, seq={}, pts_ns={}, flags={}, dropped_before={}, peak={}, rms={})",
            self.frames, self.seq, self.pts_ns, self.flags, self.dropped_before, self.peak, self.rms,
        )
    }
}

fn chunk_to_py(chunk: AudioChunk) -> PyAudioChunk {
    PyAudioChunk {
        frames: chunk.frames,
        pts_ns: chunk.pts_ns,
        seq: chunk.seq,
        flags: chunk.flags.bits(),
        dropped_before: chunk.dropped_before,
        peak: chunk.peak,
        rms: chunk.rms,
        samples: chunk.data,
    }
}

// ---------------------------------------------------------------------------
// StreamEvent（pyclass・getter）
// ---------------------------------------------------------------------------

/// ストリーム実行中のイベント。`type` で種別、`count`/`message` は種別により任意。
#[pyclass(module = "flexaudio", name = "StreamEvent", frozen)]
pub struct PyStreamEvent {
    #[pyo3(get, name = "type")]
    kind: String,
    #[pyo3(get)]
    count: Option<u64>,
    #[pyo3(get)]
    message: Option<String>,
}

#[pymethods]
impl PyStreamEvent {
    fn __repr__(&self) -> String {
        format!(
            "StreamEvent(type={:?}, count={:?}, message={:?})",
            self.kind, self.count, self.message
        )
    }
}

fn event_to_py(ev: Event) -> PyStreamEvent {
    match ev {
        Event::ChunkDropped { count } => PyStreamEvent {
            kind: "chunkDropped".to_string(),
            count: Some(count),
            message: None,
        },
        Event::StreamStalled => PyStreamEvent {
            kind: "stalled".to_string(),
            count: None,
            message: None,
        },
        Event::StreamRecovered => PyStreamEvent {
            kind: "recovered".to_string(),
            count: None,
            message: None,
        },
        Event::PermissionDenied => PyStreamEvent {
            kind: "permissionDenied".to_string(),
            count: None,
            message: None,
        },
        Event::DeviceLost => PyStreamEvent {
            kind: "deviceLost".to_string(),
            count: None,
            message: None,
        },
        Event::Error(msg) => PyStreamEvent {
            kind: "error".to_string(),
            count: None,
            message: Some(msg),
        },
        // Event は #[non_exhaustive]。将来のバリアント追加に備えて、未知種別は "unknown"
        // + デバッグ表現で Python へ渡す（握り潰さない）。
        other => PyStreamEvent {
            kind: "unknown".to_string(),
            count: None,
            message: Some(format!("unknown event: {other:?}")),
        },
    }
}

// ---------------------------------------------------------------------------
// Stream（pyclass）。内部に flexaudio::Stream を持つ。
// ---------------------------------------------------------------------------

/// 録音ストリームのハンドル。内部の `flexaudio::Stream` を直接 poll する。
///
/// `open(...)` が `start()` まで済ませて返す。利用側は `poll_chunk` / `poll_event` を
/// 周期的に呼ぶ。`stop()` で停止し、context manager（`with`）では `__exit__` で stop する。
#[pyclass(module = "flexaudio")]
pub struct Stream {
    inner: fa::Stream,
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

    /// 取り出せるチャンクがあれば返す。無ければ `None`（非ブロッキング）。
    fn poll_chunk(&mut self) -> Option<PyAudioChunk> {
        self.inner.poll_chunk().map(chunk_to_py)
    }

    /// 取り出せるイベントがあれば返す。無ければ `None`（非ブロッキング）。
    fn poll_event(&mut self) -> Option<PyStreamEvent> {
        self.inner.poll_event().map(event_to_py)
    }

    /// 録音を止めずに入力ソース（mic/system/process）をホットスワップする。
    ///
    /// 出力フォーマット（output_rate/output_channels）は切替では変えられない。変更を
    /// 要求すると `switch_source` がエラーを返し、ここで例外になる。
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
    ) -> PyResult<()> {
        let config = build_config(
            kind,
            device_id,
            process_id,
            &mode,
            exclude_self,
            output_rate,
            output_channels,
            chunk_ms,
        )?;
        self.inner.switch_source(config).map_err(to_py_err)
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

// ---------------------------------------------------------------------------
// モジュール関数
// ---------------------------------------------------------------------------

/// 利用可能なデバイスを列挙する。ヘッドレス環境では空リストでも例外にしない。
#[pyfunction]
fn devices() -> PyResult<Vec<PyDeviceInfo>> {
    let list = fa::devices().map_err(to_py_err)?;
    Ok(list.into_iter().map(device_info_to_py).collect())
}

/// ストリームを開いて `start()` まで済ませ、[`Stream`] を返す。
///
/// `kind` は "mic"|"system"|"process"。不正値は `ValueError`。デバイスが無い環境では
/// open / start が flexaudio のエラーを上げる（`RuntimeError` 等に変換される）。
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
))]
#[allow(clippy::too_many_arguments)]
fn open(
    kind: &str,
    device_id: Option<String>,
    process_id: Option<u32>,
    mode: String,
    exclude_self: bool,
    output_rate: u32,
    output_channels: u16,
    chunk_ms: u32,
) -> PyResult<Stream> {
    let config = build_config(
        kind,
        device_id,
        process_id,
        &mode,
        exclude_self,
        output_rate,
        output_channels,
        chunk_ms,
    )?;
    let mut stream = fa::open(config).map_err(to_py_err)?;
    stream.start().map_err(to_py_err)?;
    Ok(Stream { inner: stream })
}

// ---------------------------------------------------------------------------
// 小物
// ---------------------------------------------------------------------------

/// Python 風に bool を "True"/"False" で表す（__repr__ 用）。
fn bool_repr(b: bool) -> &'static str {
    if b {
        "True"
    } else {
        "False"
    }
}

// ---------------------------------------------------------------------------
// モジュール定義
// ---------------------------------------------------------------------------

/// Python モジュール `flexaudio`。
#[pymodule]
fn flexaudio(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(open, m)?)?;
    m.add_function(wrap_pyfunction!(devices, m)?)?;
    m.add_class::<Stream>()?;
    m.add_class::<PyAudioChunk>()?;
    m.add_class::<PyStreamEvent>()?;
    m.add_class::<PyDeviceInfo>()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! marshalling の純粋部分を Python ランタイム無しで検証する。
    //!
    //! pyclass を生成する経路（PyBytes 等）は Python ホストが要るのでここでは見ない。
    //! ここで見るのは Python 非依存の純変換だけ:
    //! - `parse_source_kind` / `source_kind_str`（往復）
    //! - `parse_process_mode`（既定/明示/未知）
    //! - `build_config`（引数 → StreamConfig の既定・反映）
    //! - `to_py_err`（flexaudio::Error → 例外の種別とメッセージは Python ランタイム要のため
    //!   ここでは Error の Display のみ間接確認）

    use super::*;

    #[test]
    fn source_kind_roundtrips() {
        for (s, k) in [
            ("mic", SourceKind::Mic),
            ("system", SourceKind::SystemLoopback),
            ("process", SourceKind::ProcessLoopback),
        ] {
            assert_eq!(parse_source_kind(s).unwrap(), k);
            assert_eq!(source_kind_str(k), s);
        }
    }

    #[test]
    fn parse_source_kind_rejects_unknown() {
        assert!(parse_source_kind("bogus").is_err());
    }

    #[test]
    fn parse_process_mode_defaults_and_explicit() {
        assert_eq!(parse_process_mode("include").unwrap(), ProcessMode::Include);
        assert_eq!(parse_process_mode("exclude").unwrap(), ProcessMode::Exclude);
        assert!(parse_process_mode("nope").is_err());
    }

    #[test]
    fn build_config_defaults() {
        let cfg = build_config("mic", None, None, "include", false, 48_000, 2, 20).unwrap();
        assert_eq!(cfg.kind, SourceKind::Mic);
        assert_eq!(cfg.output.sample_rate, 48_000);
        assert_eq!(cfg.output.channels, 2);
        assert_eq!(cfg.mode, ProcessMode::Include);
        assert!(!cfg.exclude_self);
        assert_eq!(cfg.target_pid, None);
        assert_eq!(cfg.device_id, None);
        assert_eq!(cfg.chunk_ms, 20);
        // 指定外の ring_capacity_chunks は StreamConfig 既定（50）。
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
        )
        .unwrap();
        assert_eq!(cfg.kind, SourceKind::ProcessLoopback);
        assert_eq!(cfg.device_id.as_deref(), Some("dev-x"));
        assert_eq!(cfg.target_pid, Some(9999));
        assert_eq!(cfg.mode, ProcessMode::Exclude);
        assert!(cfg.exclude_self);
        assert_eq!(cfg.output.sample_rate, 16_000);
        assert_eq!(cfg.output.channels, 1);
    }

    #[test]
    fn build_config_rejects_unknown_kind() {
        assert!(build_config("speaker", None, None, "include", false, 48_000, 2, 20).is_err());
    }

    #[test]
    fn to_py_err_preserves_message() {
        // 種別判定は Python ランタイム不要（match のみ）。メッセージは Display 由来。
        let err = fa::Error::DeviceNotFound;
        let msg = err.to_string();
        assert!(msg.contains("device not found"));
        // 変換自体が panic しないことだけ確認（PyErr の中身は Python 要）。
        let _ = to_py_err(fa::Error::DeviceNotFound);
        let _ = to_py_err(fa::Error::InvalidArg("x".to_string()));
    }

    #[test]
    fn event_to_py_maps_each_variant() {
        let dropped = event_to_py(Event::ChunkDropped { count: 7 });
        assert_eq!(dropped.kind, "chunkDropped");
        assert_eq!(dropped.count, Some(7));
        assert_eq!(event_to_py(Event::StreamStalled).kind, "stalled");
        assert_eq!(event_to_py(Event::StreamRecovered).kind, "recovered");
        assert_eq!(
            event_to_py(Event::PermissionDenied).kind,
            "permissionDenied"
        );
        assert_eq!(event_to_py(Event::DeviceLost).kind, "deviceLost");
        let errev = event_to_py(Event::Error("boom".to_string()));
        assert_eq!(errev.kind, "error");
        assert_eq!(errev.message.as_deref(), Some("boom"));
    }

    #[test]
    fn chunk_to_py_carries_fields() {
        let chunk = AudioChunk {
            data: vec![0.0, 1.0, -1.0, 0.5],
            frames: 2,
            pts_ns: 123,
            seq: 9_007_199_254_740_993, // 2^53 + 1（f64 では落ちる桁）。
            flags: fa::ChunkFlags::empty(),
            dropped_before: 3,
            peak: 1.0,
            rms: 0.5,
        };
        let py = chunk_to_py(chunk);
        assert_eq!(py.frames, 2);
        assert_eq!(py.pts_ns, 123);
        assert_eq!(py.seq, 9_007_199_254_740_993);
        assert_eq!(py.dropped_before, 3);
        assert_eq!(py.samples, vec![0.0, 1.0, -1.0, 0.5]);
    }

    #[test]
    fn device_info_to_py_maps_all_fields() {
        let info = DeviceInfo {
            id: "id-x".to_string(),
            name: "Name X".to_string(),
            source_kind: SourceKind::SystemLoopback,
            sample_rate: 44_100,
            channels: 1,
            is_loopback: true,
            is_default: false,
        };
        let py = device_info_to_py(info);
        assert_eq!(py.id, "id-x");
        assert_eq!(py.source_kind, "system");
        assert_eq!(py.sample_rate, 44_100);
        assert!(py.is_loopback);
        assert!(!py.is_default);
    }
}
