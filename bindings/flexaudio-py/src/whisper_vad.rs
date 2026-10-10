//! Validate, borrow canonical input, and delegate to the shared Rust session.
use crate::whisper_marshal::{self as marshal, FrameProbabilities, WhisperSpeechSegment};
use flexaudio_vad::{
    WhisperVad as CoreVad, WhisperVadError, WhisperVadFailure, WhisperVadOptions,
    WhisperVadParams as Params, WhisperVadPostProcessor as Processor,
};
use pyo3::exceptions::{PyRuntimeError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyTuple};

pyo3::create_exception!(flexaudio, WhisperVadValidationError, PyValueError);
pyo3::create_exception!(flexaudio, WhisperVadRuntimeError, PyRuntimeError);

fn code(e: &WhisperVadError) -> &'static str {
    match e {
        WhisperVadError::InvalidParameter { .. } => "InvalidParameter",
        WhisperVadError::InvalidProbability { .. } => "InvalidProbability",
        WhisperVadError::InvalidPcm { .. } => "InvalidPcm",
        WhisperVadError::Overflow { .. } => "Overflow",
        WhisperVadError::SessionFinished => "SessionFinished",
        WhisperVadError::Inference => "Inference",
        WhisperVadError::ModelLoad => "ModelLoad",
        WhisperVadError::FailedSession => "FailedSession",
    }
}
pub(crate) fn failure_to_py(py: Python<'_>, f: WhisperVadFailure) -> PyErr {
    let error = match f.error {
        WhisperVadError::InvalidParameter { .. }
        | WhisperVadError::InvalidProbability { .. }
        | WhisperVadError::InvalidPcm { .. }
        | WhisperVadError::Overflow { .. } => {
            WhisperVadValidationError::new_err(f.error.to_string())
        }
        _ => WhisperVadRuntimeError::new_err(f.error.to_string()),
    };
    if let Err(e) = error.value(py).setattr("code", code(&f.error)) {
        return e;
    }
    match marshal::events_to_py(py, f.terminal_events) {
        Ok(events) => {
            if let Err(e) = error.value(py).setattr("terminal_events", events) {
                return e;
            }
        }
        Err(e) => return e,
    }
    error
}
pub(crate) fn boundary_error(py: Python<'_>, code: &str, message: &str) -> PyErr {
    let error = WhisperVadValidationError::new_err(message.to_string());
    if let Err(e) = error.value(py).setattr("code", code) {
        return e;
    }
    if let Err(e) = error
        .value(py)
        .setattr("terminal_events", Vec::<Py<PyAny>>::new())
    {
        return e;
    }
    error
}

fn validation(py: Python<'_>, message: &str) -> PyErr {
    boundary_error(py, "InvalidParameter", message)
}

fn with_samples<T>(samples: &Bound<'_, PyAny>, run: impl FnOnce(&[f32]) -> T) -> PyResult<T> {
    crate::whisper_buffer::with_samples(samples, run)
}

fn float(value: &Bound<'_, PyAny>, upper: f64) -> PyResult<f32> {
    if value.is_instance_of::<PyBool>() {
        return Err(validation(value.py(), "float parameter cannot be bool"));
    }
    let v = value.extract::<f64>()?;
    if !v.is_finite() || !(0.0..=upper).contains(&v) {
        return Err(validation(
            value.py(),
            "float parameter outside the pinned domain",
        ));
    }
    // The original f64 was checked before this intentional canonical f32 conversion.
    Ok(v as f32)
}
fn duration(value: &Bound<'_, PyAny>) -> PyResult<u32> {
    if value.is_instance_of::<PyBool>() {
        return Err(validation(value.py(), "duration cannot be bool"));
    }
    let v = value.extract::<i64>()?;
    if !(0..=134_217).contains(&v) {
        return Err(validation(value.py(), "duration must be in 0..134217"));
    }
    u32::try_from(v).map_err(|_| validation(value.py(), "invalid duration"))
}
fn boolean(value: &Bound<'_, PyAny>) -> PyResult<bool> {
    if !value.is_instance_of::<PyBool>() {
        return Err(validation(value.py(), "provisional must be bool"));
    }
    value.extract()
}

const PARAM_NAMES: [&str; 5] = [
    "threshold",
    "min_speech_duration_ms",
    "min_silence_duration_ms",
    "max_speech_duration_s",
    "speech_pad_ms",
];
fn parse_params(
    args: &Bound<'_, PyTuple>,
    kwargs: Option<&Bound<'_, PyDict>>,
    preview: bool,
) -> PyResult<(Params, bool)> {
    let py = args.py();
    let mut params = Params::default();
    let mut provisional = false;
    let count = if preview { 6 } else { 5 };
    if args.len() > count {
        return Err(PyTypeError::new_err("too many positional arguments"));
    }
    let mut seen = [false; 6];
    let mut apply = |name: &str, value: &Bound<'_, PyAny>| -> PyResult<()> {
        let index = PARAM_NAMES
            .iter()
            .position(|key| *key == name)
            .or_else(|| (preview && name == "provisional").then_some(5))
            .ok_or_else(|| validation(py, "unknown whisper VAD parameter"))?;
        if seen[index] {
            return Err(PyTypeError::new_err("multiple values for parameter"));
        }
        seen[index] = true;
        match index {
            0 => params.threshold = float(value, 1.0)?,
            1 => params.min_speech_duration_ms = duration(value)?,
            2 => params.min_silence_duration_ms = duration(value)?,
            3 => params.max_speech_duration_s = float(value, f64::from(f32::MAX))?,
            4 => params.speech_pad_ms = duration(value)?,
            5 => provisional = boolean(value)?,
            _ => unreachable!(),
        }
        Ok(())
    };
    for (i, v) in args.iter().enumerate() {
        apply(
            if i == 5 {
                "provisional"
            } else {
                PARAM_NAMES[i]
            },
            &v,
        )?;
    }
    if let Some(kwargs) = kwargs {
        for (key, value) in kwargs.iter() {
            apply(&key.extract::<String>()?, &value)?;
        }
    }
    params.validate().map_err(|e| failure_to_py(py, e.into()))?;
    Ok((params, provisional))
}

#[pyclass(module = "flexaudio", frozen, skip_from_py_object)]
#[derive(Clone)]
pub struct WhisperVadParams {
    pub(crate) inner: Params,
}
#[pymethods]
impl WhisperVadParams {
    #[new]
    #[pyo3(signature = (**kwargs), text_signature = "(*, threshold=0.5, min_speech_duration_ms=250, min_silence_duration_ms=100, max_speech_duration_s=3.4028234663852886e38, speech_pad_ms=30)")]
    fn new(py: Python<'_>, kwargs: Option<&Bound<'_, PyDict>>) -> PyResult<Self> {
        let (inner, _) = parse_params(&PyTuple::empty(py), kwargs, false)?;
        Ok(Self { inner })
    }
    #[getter]
    fn threshold(&self) -> f32 {
        self.inner.threshold
    }
    #[getter]
    fn min_speech_duration_ms(&self) -> u32 {
        self.inner.min_speech_duration_ms
    }
    #[getter]
    fn min_silence_duration_ms(&self) -> u32 {
        self.inner.min_silence_duration_ms
    }
    #[getter]
    fn max_speech_duration_s(&self) -> f32 {
        self.inner.max_speech_duration_s
    }
    #[getter]
    fn speech_pad_ms(&self) -> u32 {
        self.inner.speech_pad_ms
    }
}

#[pyclass(module = "flexaudio", unsendable)]
pub struct WhisperVad {
    inner: CoreVad,
}
#[pymethods]
impl WhisperVad {
    #[new]
    #[pyo3(signature = (*args, **kwargs), text_signature = "(threshold=0.5, min_speech_duration_ms=250, min_silence_duration_ms=100, max_speech_duration_s=3.4028234663852886e38, speech_pad_ms=30, provisional=False)")]
    fn new(args: &Bound<'_, PyTuple>, kwargs: Option<&Bound<'_, PyDict>>) -> PyResult<Self> {
        let (params, provisional) = parse_params(args, kwargs, true)?;
        let inner = CoreVad::new(params, WhisperVadOptions { provisional })
            .map_err(|e| failure_to_py(args.py(), e.into()))?;
        Ok(Self { inner })
    }
    fn process(&mut self, py: Python<'_>, samples: &Bound<'_, PyAny>) -> PyResult<Vec<Py<PyAny>>> {
        let result = with_samples(samples, |values| self.inner.process(values))?;
        marshal::events_to_py(py, result.map_err(|e| failure_to_py(py, e))?)
    }
    fn finish(&mut self, py: Python<'_>) -> PyResult<Vec<Py<PyAny>>> {
        marshal::events_to_py(py, self.inner.finish().map_err(|e| failure_to_py(py, e))?)
    }
    fn reset(&mut self, py: Python<'_>) -> PyResult<Vec<Py<PyAny>>> {
        marshal::events_to_py(py, self.inner.reset().map_err(|e| failure_to_py(py, e))?)
    }
    fn last_frame_probabilities(&self) -> FrameProbabilities {
        let batch = self.inner.last_frame_probabilities();
        FrameProbabilities {
            first_frame_index: batch.first_frame_index,
            samples: batch.values.to_vec(),
        }
    }
}

#[pyclass(module = "flexaudio", unsendable)]
pub struct WhisperVadPostProcessor {
    inner: Processor,
}
#[pymethods]
impl WhisperVadPostProcessor {
    #[new]
    #[pyo3(signature = (params=None))]
    fn new(py: Python<'_>, params: Option<&WhisperVadParams>) -> PyResult<Self> {
        Ok(Self {
            inner: Processor::new(params.map(|p| p.inner.clone()).unwrap_or_default())
                .map_err(|e| failure_to_py(py, e.into()))?,
        })
    }
    fn process(
        &mut self,
        py: Python<'_>,
        probabilities: &Bound<'_, PyAny>,
    ) -> PyResult<Vec<WhisperSpeechSegment>> {
        let result = with_samples(probabilities, |values| self.inner.process(values))?;
        Ok(marshal::segments_to_py(
            result.map_err(|e| failure_to_py(py, e.into()))?,
        ))
    }
    fn finish(&mut self, py: Python<'_>) -> PyResult<Vec<WhisperSpeechSegment>> {
        Ok(marshal::segments_to_py(
            self.inner
                .finish()
                .map_err(|e| failure_to_py(py, e.into()))?,
        ))
    }
    fn reset(&mut self) {
        self.inner.reset();
    }
}

#[pyfunction]
#[pyo3(signature = (samples, params=None))]
fn whisper_speech_segments(
    py: Python<'_>,
    samples: &Bound<'_, PyAny>,
    params: Option<&WhisperVadParams>,
) -> PyResult<Vec<WhisperSpeechSegment>> {
    let params = params.map(|p| p.inner.clone()).unwrap_or_default();
    let result = with_samples(samples, |values| {
        flexaudio_vad::whisper_speech_segments(values, &params)
    })?;
    Ok(marshal::segments_to_py(
        result.map_err(|e| failure_to_py(py, e.into()))?,
    ))
}

/// Immutable primary-tap options; secondary is explicitly unsupported by this binding.
#[pyclass(module = "flexaudio", frozen, skip_from_py_object)]
#[derive(Clone)]
pub struct WhisperVadStreamOptions {
    #[pyo3(get)]
    pub(crate) params: WhisperVadParams,
    #[pyo3(get)]
    pub(crate) provisional: bool,
    #[pyo3(get)]
    pub(crate) tap: String,
}
#[pymethods]
impl WhisperVadStreamOptions {
    #[new]
    #[pyo3(signature = (*, params=None, provisional=false, tap="primary"))]
    fn new(
        py: Python<'_>,
        params: Option<&WhisperVadParams>,
        provisional: bool,
        tap: &str,
    ) -> PyResult<Self> {
        if tap != "primary" {
            return Err(boundary_error(
                py,
                "UnsupportedTap",
                "only primary is supported",
            ));
        }
        Ok(Self {
            params: params.cloned().unwrap_or(WhisperVadParams {
                inner: Params::default(),
            }),
            provisional,
            tap: tap.to_string(),
        })
    }
}

/// Validate attachment before acquiring a device.
pub(crate) fn validate_attachment(
    options: Option<&WhisperVadStreamOptions>,
    legacy: bool,
) -> PyResult<()> {
    if options.is_none() {
        return Ok(());
    }
    Python::attach(|py| {
        if legacy {
            return Err(boundary_error(
                py,
                "ConflictingVad",
                "vad and whisper_vad are mutually exclusive",
            ));
        }
        Ok(())
    })
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<WhisperVad>()?;
    m.add_class::<WhisperVadParams>()?;
    m.add_class::<WhisperVadPostProcessor>()?;
    m.add_class::<WhisperVadStreamOptions>()?;
    m.add(
        "WhisperVadValidationError",
        m.py().get_type::<WhisperVadValidationError>(),
    )?;
    m.add(
        "WhisperVadRuntimeError",
        m.py().get_type::<WhisperVadRuntimeError>(),
    )?;
    m.add_function(wrap_pyfunction!(whisper_speech_segments, m)?)?;
    marshal::register(m)
}

#[cfg(test)]
#[path = "../tests/rust/whisper_vad.rs"]
mod tests;

pub(crate) fn tap_error(error: flexaudio_vad::WhisperVadTapError) -> PyErr {
    Python::attach(|py| {
        let error_code = match &error {
            flexaudio_vad::WhisperVadTapError::Vad(error) => code(error),
            flexaudio_vad::WhisperVadTapError::InvalidStereoLength => "InvalidStereoLength",
            flexaudio_vad::WhisperVadTapError::InvalidPcm { .. } => "InvalidPcm",
            flexaudio_vad::WhisperVadTapError::CaptureSampleOverflow => "CaptureSampleOverflow",
            flexaudio_vad::WhisperVadTapError::PtsOutOfRange => "PtsOutOfRange",
            flexaudio_vad::WhisperVadTapError::UnsupportedConversionClock => {
                "UnsupportedConversionClock"
            }
            flexaudio_vad::WhisperVadTapError::Conversion => "Conversion",
            flexaudio_vad::WhisperVadTapError::Stopped => "Stopped",
            flexaudio_vad::WhisperVadTapError::FailedSession => "FailedSession",
        };
        let result = WhisperVadRuntimeError::new_err(error.to_string());
        let _ = result.value(py).setattr("code", error_code);
        let _ = result
            .value(py)
            .setattr("terminal_events", Vec::<Py<PyAny>>::new());
        result
    })
}
