//! Validate, copy canonical input, and delegate to the shared Rust session.
use crate::vad_marshal::{self as marshal, FrameProbabilities, SpeechSegment};
use flexaudio_vad::{
    Vad as CoreVad, VadError, VadFailure, VadOptions as Params, VadPostProcessor as Processor,
    VadRuntimeOptions,
};
use pyo3::exceptions::{PyRuntimeError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyTuple};

pyo3::create_exception!(flexaudio, VadValidationError, PyValueError);
pyo3::create_exception!(flexaudio, VadRuntimeError, PyRuntimeError);

fn code(e: &VadError) -> &'static str {
    match e.root() {
        VadError::InvalidParameter { .. } => "InvalidParameter",
        VadError::InvalidProbability { .. } => "InvalidProbability",
        VadError::InvalidPcm { .. } => "InvalidPcm",
        VadError::Overflow { .. } => "Overflow",
        VadError::SessionFinished => "SessionFinished",
        VadError::Inference => "Inference",
        VadError::ModelLoad => "ModelLoad",
        VadError::FailedSession => "FailedSession",
        VadError::Cause { primary, .. } => code(primary),
    }
}
pub(crate) fn failure_to_py(py: Python<'_>, f: VadFailure) -> PyErr {
    let error = match f.error.root() {
        VadError::InvalidParameter { .. }
        | VadError::InvalidProbability { .. }
        | VadError::InvalidPcm { .. }
        | VadError::Overflow { .. } => VadValidationError::new_err(f.error.to_string()),
        _ => VadRuntimeError::new_err(f.error.to_string()),
    };
    if let Err(e) = error.value(py).setattr("code", code(&f.error)) {
        return e;
    }
    match Py::new(py, crate::errors::AudioError(f.error.audio_error())) {
        Ok(audio_error) => {
            if let Err(error) = error.value(py).setattr("audio_error", audio_error) {
                return error;
            }
        }
        Err(error) => return error,
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
    let error = VadValidationError::new_err(message.to_string());
    if let Err(e) = error.value(py).setattr("code", code) {
        return e;
    }
    match Py::new(
        py,
        crate::errors::AudioError(::flexaudio::Error::InvalidArg(message.to_owned())),
    ) {
        Ok(audio_error) => {
            if let Err(error) = error.value(py).setattr("audio_error", audio_error) {
                return error;
            }
        }
        Err(error) => return error,
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
    crate::pcm_buffer::with_samples(samples, run)
}

fn float(value: &Bound<'_, PyAny>, upper: f64) -> PyResult<f32> {
    if value.is_instance_of::<PyBool>() {
        return Err(validation(value.py(), "float parameter cannot be bool"));
    }
    let v = value.extract::<f64>()?;
    if !v.is_finite() || !(0.0..=upper).contains(&v) {
        return Err(validation(
            value.py(),
            "float parameter outside the supported domain",
        ));
    }
    // The original f64 was checked before this intentional canonical f32 conversion.
    Ok(v as f32)
}
fn milliseconds(value: &Bound<'_, PyAny>, upper: u32) -> PyResult<u32> {
    if value.is_instance_of::<PyBool>() {
        return Err(validation(value.py(), "duration cannot be bool"));
    }
    let v = value.extract::<i64>()?;
    if !(0..=i64::from(upper)).contains(&v) {
        return Err(validation(
            value.py(),
            "duration is outside the supported range",
        ));
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
    "min_speech_ms",
    "min_silence_ms",
    "max_speech_ms",
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
            .ok_or_else(|| validation(py, "unknown VAD parameter"))?;
        if seen[index] {
            return Err(PyTypeError::new_err("multiple values for parameter"));
        }
        seen[index] = true;
        match index {
            0 => params.threshold = float(value, 1.0)?,
            1 => params.min_speech_ms = milliseconds(value, 134_217)?,
            2 => params.min_silence_ms = milliseconds(value, 134_217)?,
            3 => params.max_speech_ms = milliseconds(value, u32::MAX)?,
            4 => params.speech_pad_ms = milliseconds(value, 134_217)?,
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
pub struct VadOptions {
    pub(crate) inner: Params,
}
#[pymethods]
impl VadOptions {
    #[new]
    #[pyo3(signature = (**kwargs), text_signature = "(*, threshold=0.5, min_speech_ms=250, min_silence_ms=100, max_speech_ms=0, speech_pad_ms=30)")]
    fn new(py: Python<'_>, kwargs: Option<&Bound<'_, PyDict>>) -> PyResult<Self> {
        let (inner, _) = parse_params(&PyTuple::empty(py), kwargs, false)?;
        Ok(Self { inner })
    }
    #[getter]
    fn threshold(&self) -> f32 {
        self.inner.threshold
    }
    #[getter]
    fn min_speech_ms(&self) -> u32 {
        self.inner.min_speech_ms
    }
    #[getter]
    fn min_silence_ms(&self) -> u32 {
        self.inner.min_silence_ms
    }
    #[getter]
    fn max_speech_ms(&self) -> u32 {
        self.inner.max_speech_ms
    }
    #[getter]
    fn speech_pad_ms(&self) -> u32 {
        self.inner.speech_pad_ms
    }
}

#[pyclass(module = "flexaudio", unsendable)]
pub struct Vad {
    inner: CoreVad,
}
#[pymethods]
impl Vad {
    #[new]
    #[pyo3(signature = (*args, **kwargs), text_signature = "(threshold=0.5, min_speech_ms=250, min_silence_ms=100, max_speech_ms=0, speech_pad_ms=30, provisional=False)")]
    fn new(args: &Bound<'_, PyTuple>, kwargs: Option<&Bound<'_, PyDict>>) -> PyResult<Self> {
        let (params, provisional) = parse_params(args, kwargs, true)?;
        let inner = CoreVad::new(params, VadRuntimeOptions { provisional })
            .map_err(|e| failure_to_py(args.py(), e.into()))?;
        Ok(Self { inner })
    }
    /// Convert input with delay-compensated event times; finite resampler overshoot
    /// is clipped internally to [-1, 1] after raw input validation.
    #[pyo3(signature = (samples, input_sample_rate=16_000, input_channels=1))]
    fn process(
        &mut self,
        py: Python<'_>,
        samples: &Bound<'_, PyAny>,
        input_sample_rate: u32,
        input_channels: u16,
    ) -> PyResult<Vec<Py<PyAny>>> {
        let result = with_samples(samples, |values| {
            self.inner
                .process_pcm(values, input_sample_rate, input_channels)
        })?;
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
pub struct VadPostProcessor {
    inner: Processor,
}
#[pymethods]
impl VadPostProcessor {
    #[new]
    #[pyo3(signature = (params=None))]
    fn new(py: Python<'_>, params: Option<&VadOptions>) -> PyResult<Self> {
        Ok(Self {
            inner: Processor::new(params.map(|p| p.inner.clone()).unwrap_or_default())
                .map_err(|e| failure_to_py(py, e.into()))?,
        })
    }
    fn process(
        &mut self,
        py: Python<'_>,
        probabilities: &Bound<'_, PyAny>,
    ) -> PyResult<Vec<SpeechSegment>> {
        let result = with_samples(probabilities, |values| self.inner.process(values))?;
        Ok(marshal::segments_to_py(
            result.map_err(|e| failure_to_py(py, e.into()))?,
        ))
    }
    fn finish(&mut self, py: Python<'_>) -> PyResult<Vec<SpeechSegment>> {
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
fn speech_segments(
    py: Python<'_>,
    samples: &Bound<'_, PyAny>,
    params: Option<&VadOptions>,
) -> PyResult<Vec<SpeechSegment>> {
    let params = params.map(|p| p.inner.clone()).unwrap_or_default();
    let result = with_samples(samples, |values| {
        flexaudio_vad::speech_segments(values, &params)
    })?;
    Ok(marshal::segments_to_py(
        result.map_err(|e| failure_to_py(py, e.into()))?,
    ))
}

/// Immutable primary-tap options; secondary is explicitly unsupported by this binding.
#[pyclass(module = "flexaudio", frozen, skip_from_py_object)]
#[derive(Clone)]
pub struct StreamVadOptions {
    #[pyo3(get)]
    pub(crate) params: VadOptions,
    #[pyo3(get)]
    pub(crate) provisional: bool,
    #[pyo3(get)]
    pub(crate) tap: String,
}
#[pymethods]
impl StreamVadOptions {
    #[new]
    #[pyo3(signature = (*, params=None, provisional=false, tap="primary"))]
    fn new(
        py: Python<'_>,
        params: Option<&VadOptions>,
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
            params: params.cloned().unwrap_or(VadOptions {
                inner: Params::default(),
            }),
            provisional,
            tap: tap.to_string(),
        })
    }
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Vad>()?;
    m.add_class::<VadOptions>()?;
    m.add_class::<VadPostProcessor>()?;
    m.add_class::<StreamVadOptions>()?;
    m.add(
        "VadValidationError",
        m.py().get_type::<VadValidationError>(),
    )?;
    m.add("VadRuntimeError", m.py().get_type::<VadRuntimeError>())?;
    m.add_function(wrap_pyfunction!(speech_segments, m)?)?;
    marshal::register(m)
}

#[cfg(test)]
#[path = "../tests/rust/vad.rs"]
mod tests;

pub(crate) fn tap_error(error: flexaudio_vad::VadTapError) -> PyErr {
    Python::attach(|py| {
        let error_code = match error.root() {
            flexaudio_vad::VadTapError::Cause { primary, .. } => primary.code().as_str(),
            flexaudio_vad::VadTapError::Vad(error) => code(error),
            flexaudio_vad::VadTapError::InvalidStereoLength => "InvalidStereoLength",
            flexaudio_vad::VadTapError::InvalidPcm { .. } => "InvalidPcm",
            flexaudio_vad::VadTapError::CaptureSampleOverflow => "CaptureSampleOverflow",
            flexaudio_vad::VadTapError::PtsOutOfRange => "PtsOutOfRange",
            flexaudio_vad::VadTapError::UnsupportedConversionClock => "UnsupportedConversionClock",
            flexaudio_vad::VadTapError::Conversion => "Conversion",
            flexaudio_vad::VadTapError::Stopped => "Stopped",
            flexaudio_vad::VadTapError::FailedSession => "FailedSession",
        };
        let result = VadRuntimeError::new_err(error.to_string());
        if let Err(error) = result.value(py).setattr("code", error_code) {
            return error;
        }
        let audio_error = match Py::new(py, crate::errors::AudioError(error.audio_error())) {
            Ok(error) => error,
            Err(error) => return error,
        };
        if let Err(error) = result.value(py).setattr("audio_error", audio_error) {
            return error;
        }
        if let Err(error) = result
            .value(py)
            .setattr("terminal_events", Vec::<Py<PyAny>>::new())
        {
            return error;
        }
        result
    })
}
