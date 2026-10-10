//! Read-only Python variants for the whisper-compatible event contract.
use flexaudio_vad::{
    AttachedWhisperVadEvent, EpochEndReason, PreviewCloseReason, PreviewCutReason, WhisperVadEvent,
    WhisperVadEventKind,
};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyMemoryView};

#[pyclass(module = "flexaudio", frozen, skip_from_py_object)]
#[derive(Clone)]
pub struct WhisperSpeechSegment {
    #[pyo3(get)]
    pub(crate) start_ms: u64,
    #[pyo3(get)]
    pub(crate) end_ms: u64,
}
#[pyclass(module = "flexaudio", frozen, skip_from_py_object)]
#[derive(Clone)]
pub struct SegmentEvent {
    #[pyo3(get)]
    pub(crate) epoch: u32,
    #[pyo3(get)]
    pub(crate) seq: u64,
    #[pyo3(get)]
    pub(crate) start_ms: u64,
    #[pyo3(get)]
    pub(crate) end_ms: u64,
}
#[pymethods]
impl SegmentEvent {
    #[getter]
    fn r#type(&self) -> &'static str {
        "segment"
    }
}
#[pyclass(module = "flexaudio", frozen, skip_from_py_object)]
#[derive(Clone)]
pub struct ProvisionalSpeechStartEvent {
    #[pyo3(get)]
    pub(crate) epoch: u32,
    #[pyo3(get)]
    pub(crate) seq: u64,
    #[pyo3(get)]
    pub(crate) at_ms: u64,
}
#[pymethods]
impl ProvisionalSpeechStartEvent {
    #[getter]
    fn r#type(&self) -> &'static str {
        "provisional_speech_start"
    }
}
#[pyclass(module = "flexaudio", frozen, skip_from_py_object)]
#[derive(Clone)]
pub struct ProvisionalSpeechEndEvent {
    #[pyo3(get)]
    pub(crate) epoch: u32,
    #[pyo3(get)]
    pub(crate) seq: u64,
    #[pyo3(get)]
    pub(crate) at_ms: u64,
    #[pyo3(get)]
    pub(crate) reason: &'static str,
}
#[pymethods]
impl ProvisionalSpeechEndEvent {
    #[getter]
    fn r#type(&self) -> &'static str {
        "provisional_speech_end"
    }
}
#[pyclass(module = "flexaudio", frozen, skip_from_py_object)]
#[derive(Clone)]
pub struct ProvisionalCutEvent {
    #[pyo3(get)]
    pub(crate) epoch: u32,
    #[pyo3(get)]
    pub(crate) seq: u64,
    #[pyo3(get)]
    pub(crate) start_ms: u64,
    #[pyo3(get)]
    pub(crate) end_ms: u64,
    #[pyo3(get)]
    pub(crate) reason: &'static str,
}
#[pymethods]
impl ProvisionalCutEvent {
    #[getter]
    fn r#type(&self) -> &'static str {
        "provisional_cut"
    }
}
#[pyclass(module = "flexaudio", frozen, skip_from_py_object)]
#[derive(Clone)]
pub struct EpochEndEvent {
    #[pyo3(get)]
    pub(crate) epoch: u32,
    #[pyo3(get)]
    pub(crate) seq: u64,
    #[pyo3(get)]
    pub(crate) reason: &'static str,
}
#[pymethods]
impl EpochEndEvent {
    #[getter]
    fn r#type(&self) -> &'static str {
        "epoch_end"
    }
}
#[pyclass(module = "flexaudio", frozen, skip_from_py_object)]
#[derive(Clone)]
pub struct EpochStartEvent {
    #[pyo3(get)]
    pub(crate) epoch: u32,
    #[pyo3(get)]
    pub(crate) seq: u64,
    #[pyo3(get)]
    pub(crate) capture_sample: u64,
    #[pyo3(get)]
    pub(crate) pts_ns: i64,
}
#[pymethods]
impl EpochStartEvent {
    #[getter]
    fn r#type(&self) -> &'static str {
        "epoch_start"
    }
}

#[pyclass(module = "flexaudio", frozen, skip_from_py_object)]
pub struct FrameProbabilities {
    #[pyo3(get)]
    pub(crate) first_frame_index: u64,
    pub(crate) samples: Vec<f32>,
}
#[pymethods]
impl FrameProbabilities {
    /// Owned, read-only, contiguous native float32 buffer, independent of future calls.
    #[getter]
    fn values<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let bytes: Vec<u8> = self.samples.iter().flat_map(|v| v.to_ne_bytes()).collect();
        PyMemoryView::from(PyBytes::new(py, &bytes).as_any())?.call_method1("cast", ("f",))
    }
}

fn close_reason(r: PreviewCloseReason) -> &'static str {
    match r {
        PreviewCloseReason::Hysteresis => "hysteresis",
        PreviewCloseReason::Finish => "finish",
        PreviewCloseReason::Reset => "reset",
        PreviewCloseReason::Error => "error",
    }
}
fn cut_reason(r: PreviewCutReason) -> &'static str {
    match r {
        PreviewCutReason::Limit => "limit",
        PreviewCutReason::Hysteresis => "hysteresis",
        PreviewCutReason::Finish => "finish",
        PreviewCutReason::Reset => "reset",
        PreviewCutReason::Error => "error",
    }
}
fn end_reason(r: EpochEndReason) -> &'static str {
    match r {
        EpochEndReason::Finish => "finish",
        EpochEndReason::Reset => "reset",
        EpochEndReason::Error => "error",
    }
}
pub(crate) fn event_to_py(py: Python<'_>, e: WhisperVadEvent) -> PyResult<Py<PyAny>> {
    let epoch = e.epoch;
    let seq = e.seq;
    Ok(match e.kind {
        WhisperVadEventKind::Segment(s) => Py::new(
            py,
            SegmentEvent {
                epoch,
                seq,
                start_ms: s.start_ms,
                end_ms: s.end_ms,
            },
        )?
        .into_any(),
        WhisperVadEventKind::ProvisionalSpeechStart { at_ms } => {
            Py::new(py, ProvisionalSpeechStartEvent { epoch, seq, at_ms })?.into_any()
        }
        WhisperVadEventKind::ProvisionalSpeechEnd { at_ms, reason } => Py::new(
            py,
            ProvisionalSpeechEndEvent {
                epoch,
                seq,
                at_ms,
                reason: close_reason(reason),
            },
        )?
        .into_any(),
        WhisperVadEventKind::ProvisionalCut {
            start_ms,
            end_ms,
            reason,
        } => Py::new(
            py,
            ProvisionalCutEvent {
                epoch,
                seq,
                start_ms,
                end_ms,
                reason: cut_reason(reason),
            },
        )?
        .into_any(),
        WhisperVadEventKind::EpochEnd { reason } => Py::new(
            py,
            EpochEndEvent {
                epoch,
                seq,
                reason: end_reason(reason),
            },
        )?
        .into_any(),
    })
}
pub(crate) fn events_to_py(
    py: Python<'_>,
    events: Vec<WhisperVadEvent>,
) -> PyResult<Vec<Py<PyAny>>> {
    events.into_iter().map(|e| event_to_py(py, e)).collect()
}
pub(crate) fn attached_to_py(py: Python<'_>, e: AttachedWhisperVadEvent) -> PyResult<Py<PyAny>> {
    match e {
        AttachedWhisperVadEvent::Vad(e) => event_to_py(py, e),
        AttachedWhisperVadEvent::EpochStart {
            epoch,
            seq,
            capture_sample,
            pts_ns,
        } => Ok(Py::new(
            py,
            EpochStartEvent {
                epoch,
                seq,
                capture_sample,
                pts_ns,
            },
        )?
        .into_any()),
    }
}
pub(crate) fn segments_to_py(
    items: Vec<flexaudio_vad::WhisperSpeechSegment>,
) -> Vec<WhisperSpeechSegment> {
    items
        .into_iter()
        .map(|s| WhisperSpeechSegment {
            start_ms: s.start_ms,
            end_ms: s.end_ms,
        })
        .collect()
}
pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<WhisperSpeechSegment>()?;
    m.add_class::<SegmentEvent>()?;
    m.add_class::<ProvisionalSpeechStartEvent>()?;
    m.add_class::<ProvisionalSpeechEndEvent>()?;
    m.add_class::<ProvisionalCutEvent>()?;
    m.add_class::<EpochEndEvent>()?;
    m.add_class::<EpochStartEvent>()?;
    m.add_class::<FrameProbabilities>()?;
    Ok(())
}
