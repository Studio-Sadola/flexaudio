//! Data types passed to Python, and their conversions.
//!
//! Collected here are the frozen "value carrier only" pyclasses (DeviceInfo / AudioChunk /
//! StreamEvent / VadEvent / DeviceEvent) and the functions converting core types into them.
//! Classes with behavior (Stream / Vad / Denoiser / FlacEncoder / DeviceWatcher) live in
//! other modules.

use pyo3::prelude::*;
use pyo3::types::PyBytes;

use ::flexaudio as fa;
use fa::{AudioChunk, DeviceEvent, DeviceInfo, Event, ProcessInfo};

use crate::{bool_repr, source_kind_str};

// ---------------------------------------------------------------------------
// DeviceInfo (pyclass, getters)
// ---------------------------------------------------------------------------

/// Device information returned by `devices()`. `source_kind` is a string
/// ("mic"|"system"|"process").
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
// ProcessInfo (pyclass, getters)
// ---------------------------------------------------------------------------

/// Process information returned by `processes()` (candidate targets for per-process capture).
///
/// Passing `pid` to `open("process", process_id=pid)` records that process.
/// `executable` / `bundle_id` are `None` when unavailable, and `is_output_active` is `None`
/// when the OS does not expose the state.
#[pyclass(module = "flexaudio", name = "ProcessInfo", frozen)]
pub struct PyProcessInfo {
    #[pyo3(get)]
    pid: u32,
    #[pyo3(get)]
    name: String,
    #[pyo3(get)]
    executable: Option<String>,
    #[pyo3(get)]
    bundle_id: Option<String>,
    #[pyo3(get)]
    is_output_active: Option<bool>,
}

/// Writes an `Option<String>` in Python repr style (`None` or `'...'`).
fn optional_str_repr(value: &Option<String>) -> String {
    match value {
        Some(v) => format!("{v:?}"),
        None => "None".to_string(),
    }
}

/// Writes an `Option<bool>` in Python repr style (`None` / `True` / `False`).
fn optional_bool_repr(value: Option<bool>) -> &'static str {
    match value {
        Some(b) => bool_repr(b),
        None => "None",
    }
}

#[pymethods]
impl PyProcessInfo {
    fn __repr__(&self) -> String {
        format!(
            "ProcessInfo(pid={}, name={:?}, executable={}, bundle_id={}, is_output_active={})",
            self.pid,
            self.name,
            optional_str_repr(&self.executable),
            optional_str_repr(&self.bundle_id),
            optional_bool_repr(self.is_output_active),
        )
    }
}

pub(crate) fn process_info_to_py(info: ProcessInfo) -> PyProcessInfo {
    PyProcessInfo {
        pid: info.pid,
        name: info.name,
        executable: info.executable,
        bundle_id: info.bundle_id,
        is_output_active: info.is_output_active,
    }
}

// ---------------------------------------------------------------------------
// AudioChunk (pyclass, getters)
// ---------------------------------------------------------------------------

/// One chunk of recorded data. `data` is interleaved f32 as raw little-endian bytes
/// (len = frames * channels * 4). With numpy: `np.frombuffer(chunk.data, dtype=np.float32)`.
///
/// `vad_events` is the list of [`VadEvent`](PyVadEvent)s finalized in this chunk when the
/// integrated VAD (`open(..., vad=...)`) is enabled (an empty list when disabled or when there
/// are no events). With denoise enabled, `data` is already the noise-suppressed audio (the
/// order is denoise → VAD).
///
/// Note: `peak` / `rms` are the values computed by the core **before denoise**. With denoise
/// enabled they may not match the actual signal in `data` (after denoise) (the core's
/// statistics are carried as-is).
#[pyclass(module = "flexaudio", name = "AudioChunk", frozen)]
pub struct PyAudioChunk {
    // Interleaved f32 samples. The `data` getter converts them to raw little-endian bytes.
    // When the integrated denoise is enabled, they have been overwritten in poll with the
    // noise-suppressed samples.
    samples: Vec<f32>,
    // Events finalized by the integrated VAD (whether the kind is start, absolute sample
    // position). The getter turns them into PyVadEvent. Empty when disabled.
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
    /// Returns the interleaved f32 samples as raw little-endian bytes.
    #[getter]
    fn data<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        // Lay out each f32 as 4 little-endian bytes (written safely, without bytemuck).
        let mut buf = Vec::with_capacity(self.samples.len() * 4);
        for s in &self.samples {
            buf.extend_from_slice(&s.to_le_bytes());
        }
        PyBytes::new(py, &buf)
    }

    /// List of integrated VAD events finalized in this chunk. An empty list when VAD is
    /// disabled.
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
    /// Mutable reference to the samples for in-place processing by denoise (used from poll).
    pub(crate) fn samples_mut(&mut self) -> &mut [f32] {
        &mut self.samples
    }

    /// Reference to the samples for VAD to read (used from poll).
    pub(crate) fn samples(&self) -> &[f32] {
        &self.samples
    }

    /// Inserts the events finalized by the integrated VAD (start flag, absolute sample
    /// position).
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
// StreamEvent (pyclass, getters)
// ---------------------------------------------------------------------------

/// An event during stream execution. `type` gives the kind; `count`/`message` are optional
/// depending on the kind.
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
        // Event is #[non_exhaustive]. To prepare for variants added in the future, unknown
        // kinds are passed to Python as "unknown" + the debug representation (not swallowed).
        other => PyStreamEvent {
            kind: "unknown".to_string(),
            count: None,
            message: Some(format!("unknown event: {other:?}")),
        },
    }
}

// ---------------------------------------------------------------------------
// VadEvent (pyclass, getters)
// ---------------------------------------------------------------------------

/// A speech boundary event finalized by the VAD. `type` is "speech_start" or "speech_end".
///
/// `at_sample` is an absolute sample position **in terms of the VAD's internal rate (16000 or
/// 8000)**, not in terms of the input samples (the same for both `Vad.process` and the
/// integrated VAD). To convert to seconds: `at_sample / sample_rate`.
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
    /// Builds from a start flag and an absolute sample position (`true`=speech_start).
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
// DeviceEvent (pyclass, getters)
// ---------------------------------------------------------------------------

/// A device hotplug / default change event (returned by `DeviceWatcher.poll_event`).
///
/// `type` is "added" | "removed" | "defaultChanged". Depending on the kind, the following are
/// attached:
/// - added: `device` ([`DeviceInfo`](PyDeviceInfo)).
/// - removed: `id` (the stable ID of the removed device).
/// - defaultChanged: `id` (the ID of the new default device) and `source_kind`
///   ("mic"|"system").
#[pyclass(module = "flexaudio", name = "DeviceEvent", frozen)]
pub struct PyDeviceEvent {
    #[pyo3(get, name = "type")]
    kind: String,
    // Some only for added. The getter turns it into PyDeviceInfo (kept as the core type).
    device: Option<DeviceInfo>,
    #[pyo3(get)]
    id: Option<String>,
    #[pyo3(get)]
    source_kind: Option<String>,
}

#[pymethods]
impl PyDeviceEvent {
    /// Device information of an added event (`None` otherwise).
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
        // DeviceEvent is #[non_exhaustive]. To prepare for variants added in the future,
        // unknown kinds are passed as "unknown" (not swallowed).
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
    //! Checks only the pure conversions that do not depend on a Python runtime (excluding the
    //! PyBytes path that creates pyclasses).

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
            seq: 9_007_199_254_740_993, // 2^53 + 1 (a digit lost in f64).
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
        // By default, the integrated VAD events are empty.
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
