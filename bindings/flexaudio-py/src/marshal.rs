//! Python へ渡すデータ型と、その変換。
//!
//! ここに集めるのは「値を運ぶだけ」の frozen な pyclass 群（DeviceInfo / AudioChunk /
//! StreamEvent / VadEvent / DeviceEvent）と、コア型からそれらへの変換関数。挙動を持つ
//! クラス（Stream / Vad / Denoiser / FlacEncoder / DeviceWatcher）は別モジュールにある。

use pyo3::prelude::*;
use pyo3::types::PyBytes;

use ::flexaudio as fa;
use fa::{AudioChunk, DeviceEvent, DeviceInfo, Event};

use crate::{bool_repr, source_kind_str};

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

pub(crate) fn device_info_to_py(info: DeviceInfo) -> PyDeviceInfo {
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
///
/// `vad_events` は統合 VAD（`open(..., vad=...)`）が有効なときにこのチャンクで確定した
/// [`VadEvent`](PyVadEvent) のリスト（無効時・イベント無しなら空リスト）。denoise 有効時は
/// `data` が既にノイズ抑制後の音声になっている（順序は denoise → VAD）。
///
/// 補足: `peak` / `rms` はコアが算出した **denoise 前** の値。denoise 有効時は `data` の
/// 実信号（denoise 後）とは一致しないことがある（コアの統計をそのまま運ぶ）。
#[pyclass(module = "flexaudio", name = "AudioChunk", frozen)]
pub struct PyAudioChunk {
    // interleaved f32 サンプル。生バイトは `data` getter でリトルエンディアン化して渡す。
    // 統合 denoise が有効なときは poll 内でノイズ抑制後の列に上書きされている。
    samples: Vec<f32>,
    // 統合 VAD が確定したイベント（種別が開始か・絶対サンプル位置）。getter で
    // PyVadEvent 化する。無効時は空。
    vad_events: Vec<(bool, u64)>,
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

    /// このチャンクで確定した統合 VAD イベントのリスト。VAD 無効時は空リスト。
    #[getter]
    fn vad_events(&self) -> Vec<PyVadEvent> {
        self.vad_events
            .iter()
            .map(|&(is_start, at_sample)| PyVadEvent::new(is_start, at_sample))
            .collect()
    }

    fn __repr__(&self) -> String {
        format!(
            "AudioChunk(frames={}, seq={}, pts_ns={}, flags={}, dropped_before={}, peak={}, rms={}, vad_events={})",
            self.frames,
            self.seq,
            self.pts_ns,
            self.flags,
            self.dropped_before,
            self.peak,
            self.rms,
            self.vad_events.len(),
        )
    }
}

impl PyAudioChunk {
    /// denoise が in-place 加工するためのサンプル可変参照（poll 内から使う）。
    pub(crate) fn samples_mut(&mut self) -> &mut [f32] {
        &mut self.samples
    }

    /// VAD が読むためのサンプル参照（poll 内から使う）。
    pub(crate) fn samples(&self) -> &[f32] {
        &self.samples
    }

    /// 統合 VAD が確定したイベント（開始フラグ・絶対サンプル位置）を差し込む。
    pub(crate) fn set_vad_events(&mut self, events: Vec<(bool, u64)>) {
        self.vad_events = events;
    }
}

pub(crate) fn chunk_to_py(chunk: AudioChunk) -> PyAudioChunk {
    PyAudioChunk {
        frames: chunk.frames,
        pts_ns: chunk.pts_ns,
        seq: chunk.seq,
        flags: chunk.flags.bits(),
        dropped_before: chunk.dropped_before,
        peak: chunk.peak,
        rms: chunk.rms,
        samples: chunk.data,
        vad_events: Vec::new(),
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

pub(crate) fn event_to_py(ev: Event) -> PyStreamEvent {
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
// VadEvent（pyclass・getter）
// ---------------------------------------------------------------------------

/// VAD が確定した発話境界イベント。`type` は "speech_start" か "speech_end"。
///
/// `at_sample` は **VAD 内部レート（16000 か 8000）基準** の絶対サンプル位置で、入力
/// サンプル基準ではない（`Vad.process` / 統合 VAD いずれも同じ）。秒に直すなら
/// `at_sample / sample_rate`。
#[pyclass(module = "flexaudio", name = "VadEvent", frozen)]
pub struct PyVadEvent {
    #[pyo3(get, name = "type")]
    kind: String,
    #[pyo3(get)]
    at_sample: u64,
}

#[pymethods]
impl PyVadEvent {
    fn __repr__(&self) -> String {
        format!(
            "VadEvent(type={:?}, at_sample={})",
            self.kind, self.at_sample
        )
    }
}

impl PyVadEvent {
    /// 開始フラグと絶対サンプル位置から作る（`true`=speech_start）。
    pub(crate) fn new(is_start: bool, at_sample: u64) -> PyVadEvent {
        PyVadEvent {
            kind: if is_start {
                "speech_start"
            } else {
                "speech_end"
            }
            .to_string(),
            at_sample,
        }
    }
}

pub(crate) fn vad_event_to_py(ev: flexaudio_vad::VadEvent) -> PyVadEvent {
    match ev {
        flexaudio_vad::VadEvent::SpeechStart { at_sample } => PyVadEvent::new(true, at_sample),
        flexaudio_vad::VadEvent::SpeechEnd { at_sample } => PyVadEvent::new(false, at_sample),
    }
}

// ---------------------------------------------------------------------------
// DeviceEvent（pyclass・getter）
// ---------------------------------------------------------------------------

/// デバイス着脱・既定変更のイベント（`DeviceWatcher.poll_event` が返す）。
///
/// `type` は "added" | "removed" | "defaultChanged"。種別により以下が付く:
/// - added: `device`（[`DeviceInfo`](PyDeviceInfo)）。
/// - removed: `id`（取り外されたデバイスの安定 ID）。
/// - defaultChanged: `id`（新しい既定デバイスの ID）と `source_kind`（"mic"|"system"）。
#[pyclass(module = "flexaudio", name = "DeviceEvent", frozen)]
pub struct PyDeviceEvent {
    #[pyo3(get, name = "type")]
    kind: String,
    // added のときだけ Some。getter で PyDeviceInfo 化する（コア型のまま保持する）。
    device: Option<DeviceInfo>,
    #[pyo3(get)]
    id: Option<String>,
    #[pyo3(get)]
    source_kind: Option<String>,
}

#[pymethods]
impl PyDeviceEvent {
    /// added イベントのデバイス情報（それ以外は `None`）。
    #[getter]
    fn device(&self) -> Option<PyDeviceInfo> {
        self.device.clone().map(device_info_to_py)
    }

    fn __repr__(&self) -> String {
        format!(
            "DeviceEvent(type={:?}, device={}, id={:?}, source_kind={:?})",
            self.kind,
            if self.device.is_some() {
                "Some"
            } else {
                "None"
            },
            self.id,
            self.source_kind,
        )
    }
}

pub(crate) fn device_event_to_py(ev: DeviceEvent) -> PyDeviceEvent {
    match ev {
        DeviceEvent::Added(info) => PyDeviceEvent {
            kind: "added".to_string(),
            device: Some(info),
            id: None,
            source_kind: None,
        },
        DeviceEvent::Removed { id } => PyDeviceEvent {
            kind: "removed".to_string(),
            device: None,
            id: Some(id),
            source_kind: None,
        },
        DeviceEvent::DefaultChanged { kind, id } => PyDeviceEvent {
            kind: "defaultChanged".to_string(),
            device: None,
            id: Some(id),
            source_kind: Some(source_kind_str(kind).to_string()),
        },
        // DeviceEvent は #[non_exhaustive]。将来のバリアント追加に備えて、未知種別は
        // "unknown" で渡す（握り潰さない）。
        other => PyDeviceEvent {
            kind: "unknown".to_string(),
            device: None,
            id: Some(format!("{other:?}")),
            source_kind: None,
        },
    }
}

#[cfg(test)]
mod tests {
    //! Python ランタイム非依存の純変換だけを見る（pyclass 生成の PyBytes 経路は除く）。

    use super::*;
    use fa::SourceKind;

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
        // 既定では統合 VAD イベントは空。
        assert!(py.vad_events.is_empty());
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

    #[test]
    fn vad_event_to_py_maps_both_variants() {
        let start = vad_event_to_py(flexaudio_vad::VadEvent::SpeechStart { at_sample: 512 });
        assert_eq!(start.kind, "speech_start");
        assert_eq!(start.at_sample, 512);
        let end = vad_event_to_py(flexaudio_vad::VadEvent::SpeechEnd { at_sample: 1024 });
        assert_eq!(end.kind, "speech_end");
        assert_eq!(end.at_sample, 1024);
    }

    #[test]
    fn device_event_to_py_maps_each_variant() {
        let info = DeviceInfo {
            id: "mic-1".to_string(),
            name: "Mic".to_string(),
            source_kind: SourceKind::Mic,
            sample_rate: 48_000,
            channels: 2,
            is_loopback: false,
            is_default: true,
        };
        let added = device_event_to_py(DeviceEvent::Added(info));
        assert_eq!(added.kind, "added");
        assert!(added.device.is_some());
        assert_eq!(added.id, None);

        let removed = device_event_to_py(DeviceEvent::Removed {
            id: "gone".to_string(),
        });
        assert_eq!(removed.kind, "removed");
        assert_eq!(removed.id.as_deref(), Some("gone"));
        assert!(removed.device.is_none());

        let changed = device_event_to_py(DeviceEvent::DefaultChanged {
            kind: SourceKind::SystemLoopback,
            id: "new-default".to_string(),
        });
        assert_eq!(changed.kind, "defaultChanged");
        assert_eq!(changed.id.as_deref(), Some("new-default"));
        assert_eq!(changed.source_kind.as_deref(), Some("system"));
    }
}
