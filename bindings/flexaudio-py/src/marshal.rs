//! Data types passed to Python and their conversions.
//!
//! This module contains frozen pyclasses that only carry values (DeviceInfo / AudioChunk /
//! StreamEvent / VadEvent / DeviceEvent) and conversion functions from core types. Classes with behavior
//! (Stream / Vad / Denoiser / FlacEncoder / DeviceWatcher) live in separate modules.

use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};

use ::flexaudio as fa;
use fa::{AudioChunk, DeviceEvent, DeviceInfo, Event, ProcessInfo};

use crate::{bool_repr, source_kind_str};

// ---------------------------------------------------------------------------
// DeviceInfo (pyclass and getters)
// ---------------------------------------------------------------------------

/// Device information returned by `devices()`. `source_kind` is a string ("mic"|"system"|"process").
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
// ProcessInfo (pyclass and getters)
// ---------------------------------------------------------------------------

/// Process information returned by `processes()` (candidates for per-process capture).
///
/// Pass `pid` to `open("process", process_id=pid)` to capture that process.
/// `executable` / `bundle_id` are `None` when unavailable; `is_output_active` is `None` when the OS
/// does not expose the state.
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

/// Format `Option<String>` like Python repr (`None` or `'...'`).
fn optional_str_repr(value: &Option<String>) -> String {
    match value {
        Some(v) => format!("{v:?}"),
        None => "None".to_string(),
    }
}

/// Format `Option<bool>` like Python repr (`None` / `True` / `False`).
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
// AudioChunk (pyclass and getters)
// ---------------------------------------------------------------------------

/// One chunk of recorded audio. `data` is raw little-endian interleaved f32 bytes
/// (len = frames * channels * 4). In numpy, use `np.frombuffer(chunk.data, dtype=np.float32)`.
///
/// `vad_events` contains [`VadEvent`](PyVadEvent) events finalized for this chunk when integrated
/// VAD (`open(..., vad=...)`) is enabled (empty when disabled or no events). When denoise is enabled,
/// `data` already contains denoised audio (order: denoise → VAD).
///
/// `peak` / `rms` describe the final delivered float samples after denoise and gain,
/// before any integer encoding.
#[pyclass(module = "flexaudio", name = "AudioChunk", frozen)]
pub struct PyAudioChunk {
    // Interleaved f32 samples. The `data` getter converts them to little-endian bytes.
    // When integrated denoise is enabled, poll replaces these with the denoised samples.
    samples: Vec<f32>,
    // Events finalized by integrated VAD (start/end and absolute sample position). The getter
    // converts them to PyVadEvent. Empty when disabled.
    vad_events: Vec<(bool, u64)>,
    whisper_events: Option<Vec<flexaudio_vad::AttachedWhisperVadEvent>>,
    #[pyo3(get)]
    frame_index: u64,
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
    /// Return interleaved f32 samples as raw little-endian bytes.
    #[getter]
    fn data<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        // Write each f32 as four little-endian bytes without using bytemuck.
        let mut buf = Vec::with_capacity(self.samples.len() * 4);
        for s in &self.samples {
            buf.extend_from_slice(&s.to_le_bytes());
        }
        PyBytes::new(py, &buf)
    }

    /// New-mode events are absent when whisper attachment is disabled.
    #[getter]
    fn whisper_vad_events(&self, py: Python<'_>) -> PyResult<Option<Vec<Py<PyAny>>>> {
        self.whisper_events
            .as_ref()
            .map(|events| {
                events
                    .iter()
                    .cloned()
                    .map(|event| crate::whisper_marshal::attached_to_py(py, event))
                    .collect()
            })
            .transpose()
    }

    /// Integrated VAD events finalized for this chunk. Empty when VAD is disabled.
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
    pub(crate) fn set_whisper_events(
        &mut self,
        events: Vec<flexaudio_vad::AttachedWhisperVadEvent>,
    ) {
        self.whisper_events = Some(events);
    }

    pub(crate) fn extend_vad_events(&mut self, events: Vec<(bool, u64)>) {
        self.vad_events.extend(events);
    }
    pub(crate) fn has_audio_or_vad(&self) -> bool {
        !self.samples.is_empty() || !self.vad_events.is_empty()
    }

    /// Update metrics from the final delivered float PCM.
    pub(crate) fn update_metrics(&mut self) {
        self.peak = self
            .samples
            .iter()
            .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
        self.rms = if self.samples.is_empty() {
            0.0
        } else {
            (self
                .samples
                .iter()
                .map(|&sample| f64::from(sample).powi(2))
                .sum::<f64>()
                / self.samples.len() as f64)
                .sqrt() as f32
        };
    }

    /// Mutable sample reference for in-place denoising (used during poll).
    pub(crate) fn samples_mut(&mut self) -> &mut [f32] {
        &mut self.samples
    }

    /// Sample reference for VAD to read (used during poll).
    pub(crate) fn samples(&self) -> &[f32] {
        &self.samples
    }

    /// Set events finalized by integrated VAD (start flag and absolute sample position).
    pub(crate) fn set_vad_events(&mut self, events: Vec<(bool, u64)>) {
        self.vad_events = events;
    }
}

pub(crate) fn chunk_to_py(chunk: AudioChunk) -> PyAudioChunk {
    PyAudioChunk {
        frame_index: chunk.frame_index,
        whisper_events: None,
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
// StreamEvent (pyclass and getters)
// ---------------------------------------------------------------------------

/// Stream notification with compatible optional properties and a closed serialization view.
#[pyclass(module = "flexaudio", name = "StreamEvent", frozen)]
pub struct PyStreamEvent {
    #[pyo3(get)]
    permission: Option<String>,
    #[pyo3(get, name = "type")]
    kind: String,
    #[pyo3(get)]
    count: Option<u64>,
    #[pyo3(get)]
    message: Option<String>,
    #[pyo3(get)]
    error: Option<crate::errors::AudioError>,
    #[pyo3(get)]
    loss: Option<crate::errors::AudioLoss>,
}
#[pymethods]
impl PyStreamEvent {
    fn __repr__(&self) -> String {
        format!("StreamEvent(type={:?})", self.kind)
    }
    fn to_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        d.set_item("type", &self.kind)?;
        if let Some(permission) = &self.permission {
            d.set_item("permission", permission)?;
        }
        if let Some(count) = self.count {
            d.set_item("count", count)?;
        }
        if let Some(message) = &self.message {
            d.set_item("message", message)?;
        }
        if let Some(error) = &self.error {
            d.set_item("error", error.to_dict(py)?)?;
        }
        if let Some(loss) = &self.loss {
            d.set_item("loss", loss.to_dict(py)?)?;
        }
        Ok(d)
    }
}
pub(crate) fn event_to_py(ev: Event) -> PyStreamEvent {
    let mut result = PyStreamEvent {
        kind: String::new(),
        permission: None,
        count: None,
        message: None,
        error: None,
        loss: None,
    };
    result.kind = match ev {
        Event::TerminalError { error } => {
            result.error = Some(crate::errors::AudioError(error));
            "terminalError"
        }
        Event::RecoverableError { error } => {
            result.error = Some(crate::errors::AudioError(error));
            "recoverableError"
        }
        Event::ShutdownError { error } => {
            result.error = Some(crate::errors::AudioError(error));
            "shutdownError"
        }
        Event::AudioLoss { loss } => {
            result.loss = Some(crate::errors::AudioLoss(loss));
            "audioLoss"
        }
        Event::Clipped => "clipped",
        Event::PermissionGranted => {
            result.permission = Some("microphone".into());
            "permissionGranted"
        }
        Event::ChunkDropped { count } => {
            result.count = Some(count);
            "chunkDropped"
        }
        Event::StreamStalled => "stalled",
        Event::StreamRecovered => "recovered",
        Event::PermissionPending { permission, .. } => {
            result.permission = Some(permission.as_str().into());
            result.message = Some(format!("Recording permission is pending. {}", permission.guidance()));
            "permissionPending"
        }
        Event::PermissionDenied { permission, detail } => {
            result.permission = Some(permission.as_str().into());
            result.message = Some(fa::Error::PermissionDenied { permission, detail }.to_string());
            "permissionDenied"
        }
        Event::SilenceWhileSourceActive { .. } => {
            result.message = Some("Capture is silent while the source is active; check recording permissions and source access.".into());
            "silenceWhileSourceActive"
        }
        Event::DeviceLost => "deviceLost",
        Event::Error(_) => {
            result.message = Some("capture failed".into());
            "error"
        }
        _ => {
            result.message = Some("unknown event".into());
            "unknown"
        }
    }
    .into();
    result
}

// ---------------------------------------------------------------------------
// VadEvent (pyclass and getters)
// ---------------------------------------------------------------------------

/// Speech-boundary event finalized by VAD. `type` is "speech_start" or "speech_end".
///
/// `at_sample` is an absolute sample index at the **VAD internal rate (16000 or 8000)**, not an
/// index into the input samples (same for `Vad.process` and integrated VAD). Convert to seconds with
/// `at_sample / sample_rate`.
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
    /// Create from the start flag and absolute sample position (`true` = speech_start).
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
// DeviceEvent (pyclass and getters)
// ---------------------------------------------------------------------------

/// Device hotplug or default-device change event (returned by `DeviceWatcher.poll_event`).
///
/// `type` is "added" | "removed" | "defaultChanged". Fields depend on the event:
/// - added: `device` ([`DeviceInfo`](PyDeviceInfo)).
/// - removed: `id` (stable ID of the removed device).
/// - defaultChanged: `id` (ID of the new default device) and `source_kind` ("mic"|"system").
#[pyclass(module = "flexaudio", name = "DeviceEvent", frozen)]
pub struct PyDeviceEvent {
    #[pyo3(get, name = "type")]
    kind: String,
    // Some only for added events. The getter converts it to PyDeviceInfo (stored as the core type).
    device: Option<DeviceInfo>,
    #[pyo3(get)]
    id: Option<String>,
    #[pyo3(get)]
    source_kind: Option<String>,
    #[pyo3(get)]
    dropped_events: Option<u64>,
    #[pyo3(get)]
    message: Option<String>,
}

#[pymethods]
impl PyDeviceEvent {
    fn to_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        d.set_item("type", &self.kind)?;
        if let Some(info) = &self.device {
            let device = PyDict::new(py);
            device.set_item("id", &info.id)?;
            device.set_item("name", &info.name)?;
            device.set_item("source_kind", source_kind_str(info.source_kind))?;
            device.set_item("sample_rate", info.sample_rate)?;
            device.set_item("channels", info.channels)?;
            device.set_item("is_loopback", info.is_loopback)?;
            device.set_item("is_default", info.is_default)?;
            d.set_item("device", device)?;
        }
        if let Some(id) = &self.id {
            d.set_item("id", id)?;
        }
        if let Some(kind) = &self.source_kind {
            d.set_item("source_kind", kind)?;
        }
        if let Some(count) = self.dropped_events {
            d.set_item("dropped_events", count)?;
        }
        if let Some(message) = &self.message {
            d.set_item("message", message)?;
        }
        Ok(d)
    }
    /// Device info for an added event (`None` otherwise).
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
    let mut result = PyDeviceEvent {
        kind: String::new(),
        device: None,
        id: None,
        source_kind: None,
        dropped_events: None,
        message: None,
    };
    result.kind = match ev {
        DeviceEvent::Added(info) => {
            result.device = Some(info);
            "added"
        }
        DeviceEvent::Removed { id } => {
            result.id = Some(id);
            "removed"
        }
        DeviceEvent::DefaultChanged { kind, id } => {
            result.source_kind = Some(source_kind_str(kind.into()).into());
            result.id = Some(id);
            "defaultChanged"
        }
        DeviceEvent::DefaultCleared { kind } => {
            result.source_kind = Some(source_kind_str(kind.into()).into());
            "defaultCleared"
        }
        DeviceEvent::RescanRequired { dropped_events } => {
            result.dropped_events = Some(dropped_events);
            "rescanRequired"
        }
        _ => {
            result.message = Some("unknown device event; rescan required".into());
            "unknown"
        }
    }
    .into();
    result
}

#[cfg(test)]
mod tests {
    //! Test only pure conversions that do not require a Python runtime (excluding the PyBytes pyclass path).

    use super::*;
    use fa::SourceKind;

    #[test]
    fn terminal_backend_events_keep_error_kind_and_cause() {
        let error = fa::Error::Backend("authorization query failed".into());
        let event = event_to_py(Event::TerminalError {
            error: error.clone(),
        });
        assert_eq!(event.kind, "terminalError");
        assert_eq!(event.error.unwrap().0, error);
        assert_eq!(event.message, None);
        assert_eq!(event.permission, None);
    }

    #[test]
    fn permission_pending_preserves_permission_and_advisory_message() {
        for permission in [fa::Permission::Microphone, fa::Permission::SystemAudio] {
            let mapped = event_to_py(Event::PermissionPending {
                permission,
                detail: "Permission is pending; capture may remain silent until granted".into(),
            });
            assert_eq!(mapped.kind, "permissionPending");
            assert_eq!(mapped.permission.as_deref(), Some(permission.as_str()));
            assert_eq!(mapped.count, None);
            assert_eq!(
                mapped.message,
                Some(format!(
                    "Recording permission is pending. {}",
                    permission.guidance()
                ))
            );
        }
    }

    #[test]
    fn permission_events_preserve_cause_and_advisory_kind() {
        for permission in [fa::Permission::Microphone, fa::Permission::SystemAudio] {
            let expected = fa::Error::PermissionDenied {
                permission,
                detail: "denied by user".into(),
            }
            .to_string();
            let mapped = event_to_py(Event::PermissionDenied {
                permission,
                detail: "denied by user".into(),
            });
            assert_eq!(mapped.kind, "permissionDenied");
            assert_eq!(mapped.permission.as_deref(), Some(permission.as_str()));
            assert_eq!(mapped.message.as_deref(), Some(expected.as_str()));
        }
        let advisory = event_to_py(Event::SilenceWhileSourceActive {
            detail: "check recording privacy settings".into(),
        });
        assert_eq!(advisory.kind, "silenceWhileSourceActive");
        assert_eq!(advisory.permission, None);
        assert_eq!(
            advisory.message.as_deref(),
            Some("Capture is silent while the source is active; check recording permissions and source access.")
        );
    }

    #[test]
    fn event_to_py_maps_each_variant() {
        let dropped = event_to_py(Event::ChunkDropped { count: 7 });
        assert_eq!(dropped.kind, "chunkDropped");
        assert_eq!(dropped.count, Some(7));
        assert_eq!(event_to_py(Event::StreamStalled).kind, "stalled");
        assert_eq!(event_to_py(Event::StreamRecovered).kind, "recovered");
        assert_eq!(
            event_to_py(Event::PermissionDenied {
                permission: fa::Permission::Microphone,
                detail: "denied by user".into()
            })
            .kind,
            "permissionDenied"
        );
        assert_eq!(event_to_py(Event::DeviceLost).kind, "deviceLost");
        let errev = event_to_py(Event::Error("boom".to_string()));
        assert_eq!(errev.kind, "error");
        assert_eq!(errev.message.as_deref(), Some("capture failed"));
    }

    #[test]
    fn chunk_to_py_carries_fields() {
        let chunk = AudioChunk {
            frame_index: 0,
            data: vec![0.0, 1.0, -1.0, 0.5],
            frames: 2,
            pts_ns: 123,
            seq: 9_007_199_254_740_993, // 2^53 + 1 (not exactly representable as f64).
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
        // Integrated VAD events are empty by default.
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
            kind: fa::DefaultDeviceKind::SystemAudio,
            id: "new-default".to_string(),
        });
        assert_eq!(changed.kind, "defaultChanged");
        assert_eq!(changed.id.as_deref(), Some("new-default"));
        assert_eq!(changed.source_kind.as_deref(), Some("system"));
    }
}

#[cfg(test)]
mod parity_tests {
    use super::*;

    #[test]
    fn every_new_event_has_exact_typed_dictionary_without_legacy_fields() {
        Python::initialize();
        Python::attach(|py| {
            for (event, tag, field) in [
                (
                    Event::TerminalError {
                        error: fa::Error::DeviceLost,
                    },
                    "terminalError",
                    "error",
                ),
                (
                    Event::RecoverableError {
                        error: fa::Error::Backend("retry".into()),
                    },
                    "recoverableError",
                    "error",
                ),
                (
                    Event::ShutdownError {
                        error: fa::Error::Backend("cleanup".into()),
                    },
                    "shutdownError",
                    "error",
                ),
                (Event::PermissionGranted, "permissionGranted", "permission"),
                (
                    Event::AudioLoss {
                        loss: fa::AudioLoss::raw_overflow(
                            Some(fa::MixLane::SystemAudio),
                            std::num::NonZeroU64::new(u64::MAX),
                            48_000,
                            2,
                        )
                        .unwrap(),
                    },
                    "audioLoss",
                    "loss",
                ),
            ] {
                let d = event_to_py(event).to_dict(py).unwrap();
                assert_eq!(d.len(), 2);
                assert_eq!(
                    d.get_item("type")
                        .unwrap()
                        .unwrap()
                        .extract::<String>()
                        .unwrap(),
                    tag
                );
                assert!(d.contains(field).unwrap());
                assert!(!d.contains("message").unwrap());
                assert!(!d.contains("count").unwrap());
                if field == "loss" {
                    assert_eq!(
                        d.get_item("loss")
                            .unwrap()
                            .unwrap()
                            .get_item("samples")
                            .unwrap()
                            .extract::<u64>()
                            .unwrap(),
                        u64::MAX
                    );
                }
            }
            assert_eq!(event_to_py(Event::Clipped).to_dict(py).unwrap().len(), 1);
            let loss = crate::errors::AudioLoss(
                fa::AudioLoss::output_overflow(fa::OutputTap::Secondary, None, 16_000, 1).unwrap(),
            );
            assert!(loss
                .to_dict(py)
                .unwrap()
                .get_item("samples")
                .unwrap()
                .unwrap()
                .is_none());
        });
    }

    #[test]
    fn default_clear_and_rescan_keep_exact_payloads_and_counts() {
        Python::initialize();
        Python::attach(|py| {
            for kind in [
                fa::DefaultDeviceKind::Microphone,
                fa::DefaultDeviceKind::SystemAudio,
            ] {
                let event = device_event_to_py(DeviceEvent::DefaultCleared { kind });
                assert_eq!(event.kind, "defaultCleared");
                assert_eq!(event.id, None);
                let d = event.to_dict(py).unwrap();
                assert_eq!(d.len(), 2);
                assert!(!d.contains("id").unwrap());
                assert_eq!(
                    d.get_item("source_kind")
                        .unwrap()
                        .unwrap()
                        .extract::<String>()
                        .unwrap(),
                    source_kind_str(kind.into())
                );
            }
            let d = device_event_to_py(DeviceEvent::RescanRequired {
                dropped_events: u64::MAX,
            })
            .to_dict(py)
            .unwrap();
            assert_eq!(
                d.get_item("dropped_events")
                    .unwrap()
                    .unwrap()
                    .extract::<u64>()
                    .unwrap(),
                u64::MAX
            );
            assert_eq!(d.len(), 2);
        });
    }
}
