//! One projection for typed errors, exception classes, loss, and shutdown outcomes.
use ::flexaudio as fa;
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::PyDict;

pub(crate) fn kind_name(kind: fa::ErrorKind) -> &'static str {
    match kind {
        fa::ErrorKind::InvalidArg => "invalidArg",
        fa::ErrorKind::InvalidState => "invalidState",
        fa::ErrorKind::DeviceNotFound => "deviceNotFound",
        fa::ErrorKind::PermissionDenied => "permissionDenied",
        fa::ErrorKind::UnsupportedOsVersion => "unsupportedOsVersion",
        fa::ErrorKind::DeviceLost => "deviceLost",
        fa::ErrorKind::Backend => "backend",
        fa::ErrorKind::UnsupportedFormat => "unsupportedFormat",
        fa::ErrorKind::NativeFormatChanged => "nativeFormatChanged",
        fa::ErrorKind::Unsupported => "unsupported",
        fa::ErrorKind::AmbiguousDeviceName => "ambiguousDeviceName",
        fa::ErrorKind::IgnoredSettings => "ignoredSettings",
        _ => "backend",
    }
}
pub(crate) fn lane_name(lane: fa::MixLane) -> PyResult<&'static str> {
    Ok(match lane {
        fa::MixLane::Microphone => "microphone",
        fa::MixLane::SystemAudio => "systemAudio",
        _ => {
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "unsupported Mix lane",
            ))
        }
    })
}

#[pyclass(
    module = "flexaudio",
    name = "NativeFormat",
    frozen,
    skip_from_py_object
)]
#[derive(Clone)]
pub(crate) struct NativeFormat {
    #[pyo3(get)]
    sample_rate: u32,
    #[pyo3(get)]
    channels: u16,
}
#[pymethods]
impl NativeFormat {
    fn to_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        d.set_item("sample_rate", self.sample_rate)?;
        d.set_item("channels", self.channels)?;
        Ok(d)
    }
}
impl From<(u32, u16)> for NativeFormat {
    fn from((sample_rate, channels): (u32, u16)) -> Self {
        Self {
            sample_rate,
            channels,
        }
    }
}

#[pyclass(
    module = "flexaudio",
    name = "ErrorContext",
    frozen,
    skip_from_py_object
)]
#[derive(Clone)]
pub(crate) struct ErrorContext(fa::ErrorContext);
#[pymethods]
impl ErrorContext {
    #[getter]
    fn operation(&self) -> String {
        self.0.operation().to_string()
    }
    #[getter]
    fn lane(&self) -> PyResult<Option<&'static str>> {
        self.0.lane().map(lane_name).transpose()
    }
    #[getter]
    fn native_status<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyDict>>> {
        let Some(status) = self.0.native_status() else {
            return Ok(None);
        };
        let d = PyDict::new(py);
        match status {
            fa::NativeStatus::HResult { call, bits } => {
                d.set_item("type", "hresult")?;
                d.set_item("call", call)?;
                d.set_item("bits", bits)?;
            }
            fa::NativeStatus::OsStatus { call, value } => {
                d.set_item("type", "osStatus")?;
                d.set_item("call", call)?;
                d.set_item("value", value)?;
            }
            _ => {
                return Err(pyo3::exceptions::PyRuntimeError::new_err(
                    "unsupported native status",
                ))
            }
        }
        Ok(Some(d))
    }
    fn to_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        d.set_item("operation", self.operation())?;
        d.set_item("lane", self.lane()?)?;
        d.set_item("native_status", self.native_status(py)?)?;
        Ok(d)
    }
}

#[pyclass(module = "flexaudio", name = "AudioError", frozen, skip_from_py_object)]
#[derive(Clone)]
pub(crate) struct AudioError(pub(crate) fa::Error);
impl AudioError {
    fn collect(
        error: &fa::Error,
        contexts: &mut Vec<ErrorContext>,
        secondary: &mut Vec<AudioError>,
    ) {
        match error {
            fa::Error::Diagnostic { source, .. } => Self::collect(source, contexts, secondary),
            fa::Error::Context { source, context } => {
                contexts.push(ErrorContext(*context));
                Self::collect(source, contexts, secondary);
            }
            fa::Error::Multiple(group) => {
                Self::collect(group.primary(), contexts, secondary);
                secondary.extend(group.secondary().cloned().map(AudioError));
            }
            _ => {}
        }
    }
}
#[pymethods]
impl AudioError {
    #[getter]
    fn kind(&self) -> &'static str {
        kind_name(self.0.kind())
    }
    #[getter]
    fn message(&self) -> String {
        self.0.to_string()
    }
    #[getter]
    fn contexts(&self) -> Vec<ErrorContext> {
        let mut contexts = Vec::new();
        Self::collect(&self.0, &mut contexts, &mut Vec::new());
        contexts
    }
    #[getter]
    fn secondary(&self) -> Vec<AudioError> {
        let mut secondary = Vec::new();
        Self::collect(&self.0, &mut Vec::new(), &mut secondary);
        secondary
    }
    #[getter]
    fn permission(&self) -> Option<&'static str> {
        self.0.permission().map(fa::Permission::as_str)
    }
    #[getter]
    fn vad_code(&self) -> Option<&'static str> {
        self.0.vad_code().map(fa::VadErrorCode::as_str)
    }
    #[getter]
    fn diagnostic_details(&self) -> Vec<String> {
        self.0
            .diagnostic_details()
            .into_iter()
            .map(str::to_owned)
            .collect()
    }
    #[getter]
    fn addon_kind(&self) -> Option<&'static str> {
        self.0.addon_kind().map(fa::core::AddonErrorKind::as_str)
    }
    #[getter]
    fn addon_message(&self) -> Option<String> {
        match self.0.root() {
            fa::Error::Addon { message, .. } => Some(message.clone()),
            _ => None,
        }
    }
    #[getter]
    fn advertised(&self) -> Option<NativeFormat> {
        match self.0.root() {
            fa::Error::NativeFormatChanged { advertised, .. } => Some((*advertised).into()),
            _ => None,
        }
    }
    #[getter]
    fn actual(&self) -> Option<NativeFormat> {
        match self.0.root() {
            fa::Error::NativeFormatChanged { actual, .. } => Some((*actual).into()),
            _ => None,
        }
    }
    pub(crate) fn to_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        d.set_item("kind", self.kind())?;
        d.set_item("message", self.message())?;
        let contexts = self
            .contexts()
            .iter()
            .map(|c| c.to_dict(py))
            .collect::<PyResult<Vec<_>>>()?;
        let secondary = self
            .secondary()
            .iter()
            .map(|e| e.to_dict(py))
            .collect::<PyResult<Vec<_>>>()?;
        d.set_item("diagnostic_details", self.diagnostic_details())?;
        d.set_item("addon_kind", self.addon_kind())?;
        d.set_item("addon_message", self.addon_message())?;
        d.set_item("contexts", contexts)?;
        d.set_item("secondary", secondary)?;
        if let Some(permission) = self.permission() {
            d.set_item("permission", permission)?;
        }
        if let Some(code) = self.vad_code() {
            d.set_item("vad_code", code)?;
        }
        if let (Some(advertised), Some(actual)) = (self.advertised(), self.actual()) {
            d.set_item("advertised", advertised.to_dict(py)?)?;
            d.set_item("actual", actual.to_dict(py)?)?;
        }
        Ok(d)
    }
}

#[pyclass(module = "flexaudio", name = "AudioLoss", frozen, skip_from_py_object)]
#[derive(Clone)]
pub(crate) struct AudioLoss(pub(crate) fa::AudioLoss);
#[pymethods]
impl AudioLoss {
    #[getter]
    fn path<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        match self.0.path() {
            fa::AudioPath::Capture { lane } => {
                d.set_item("type", "capture")?;
                d.set_item("lane", lane.map(lane_name).transpose()?)?;
            }
            fa::AudioPath::MixFifo { lane } => {
                d.set_item("type", "mixFifo")?;
                d.set_item("lane", lane_name(lane)?)?;
            }
            fa::AudioPath::Output { tap } => {
                d.set_item("type", "output")?;
                d.set_item(
                    "tap",
                    match tap {
                        fa::OutputTap::Primary => "primary",
                        fa::OutputTap::Secondary => "secondary",
                        _ => {
                            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                                "unsupported output tap",
                            ))
                        }
                    },
                )?;
            }
            _ => {
                return Err(pyo3::exceptions::PyRuntimeError::new_err(
                    "unsupported loss path",
                ))
            }
        }
        Ok(d)
    }
    #[getter]
    fn reason(&self) -> PyResult<&'static str> {
        Ok(match self.0.reason() {
            fa::LossReason::RawOverflow => "rawOverflow",
            fa::LossReason::MixFifoOverflow => "mixFifoOverflow",
            fa::LossReason::CorruptBuffer => "corruptBuffer",
            fa::LossReason::MalformedBuffer => "malformedBuffer",
            fa::LossReason::CallbackRejected => "callbackRejected",
            fa::LossReason::OutputOverflow => "outputOverflow",
            fa::LossReason::GenerationEnded => "generationEnded",
            _ => {
                return Err(pyo3::exceptions::PyRuntimeError::new_err(
                    "unsupported loss reason",
                ))
            }
        })
    }
    #[getter]
    fn samples(&self) -> Option<u64> {
        self.0.samples().map(std::num::NonZeroU64::get)
    }
    #[getter]
    fn sample_rate(&self) -> u32 {
        self.0.sample_rate()
    }
    #[getter]
    fn channels(&self) -> u16 {
        self.0.channels()
    }
    pub(crate) fn to_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        d.set_item("path", self.path(py)?)?;
        d.set_item("reason", self.reason()?)?;
        d.set_item("samples", self.samples())?;
        d.set_item("sample_rate", self.sample_rate())?;
        d.set_item("channels", self.channels())?;
        Ok(d)
    }
}

#[pyclass(
    module = "flexaudio",
    name = "ShutdownReport",
    frozen,
    skip_from_py_object
)]
#[derive(Clone)]
pub(crate) struct ShutdownReport(pub(crate) fa::ShutdownReport);
#[pymethods]
impl ShutdownReport {
    #[getter]
    fn primary(&self) -> Option<AudioError> {
        self.0.primary().cloned().map(AudioError)
    }
    #[getter]
    fn cleanup_errors(&self) -> Vec<AudioError> {
        self.0.cleanup().iter().cloned().map(AudioError).collect()
    }
    fn to_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        d.set_item(
            "primary",
            self.primary().map(|e| e.to_dict(py)).transpose()?,
        )?;
        d.set_item(
            "cleanup_errors",
            self.cleanup_errors()
                .iter()
                .map(|e| e.to_dict(py))
                .collect::<PyResult<Vec<_>>>()?,
        )?;
        Ok(d)
    }
}

static EXCEPTIONS: PyOnceLock<Py<PyModule>> = PyOnceLock::new();
fn exceptions<'py>(py: Python<'py>) -> PyResult<&'py Bound<'py, PyModule>> {
    EXCEPTIONS
        .get_or_try_init(py, || {
            let code = pyo3::ffi::c_str!(
                r#"
class _AudioException:
    def __init__(self, message, audio_error):
        super().__init__(message)
        self._audio_error = audio_error
    @property
    def audio_error(self):
        return self._audio_error
class InvalidArgumentError(_AudioException, ValueError): pass
class UnsupportedFormatError(_AudioException, ValueError): pass
class InvalidStateError(_AudioException, RuntimeError): pass
class DeviceNotFoundError(_AudioException, RuntimeError): pass
class RecordingPermissionError(_AudioException, RuntimeError): pass
class UnsupportedOsVersionError(_AudioException, RuntimeError): pass
class DeviceLostError(_AudioException, RuntimeError): pass
class BackendError(_AudioException, RuntimeError): pass
class _FlacIoError(_AudioException, OSError): pass
class NativeFormatChangedError(_AudioException, RuntimeError): pass
class UnsupportedError(_AudioException, RuntimeError): pass
class AmbiguousDeviceNameError(_AudioException, RuntimeError): pass
class IgnoredSettingsError(ValueError):
    def __init__(self, message, report):
        super().__init__(message)
        self._report = report
    @property
    def report(self):
        return self._report
for _exception in (InvalidArgumentError, UnsupportedFormatError, InvalidStateError,
                   DeviceNotFoundError, RecordingPermissionError, UnsupportedOsVersionError,
                   DeviceLostError, BackendError, NativeFormatChangedError, UnsupportedError,
                   AmbiguousDeviceNameError, IgnoredSettingsError):
    _exception.__module__ = 'flexaudio'
"#
            );
            Ok(PyModule::from_code(
                py,
                code,
                pyo3::ffi::c_str!("flexaudio_errors.py"),
                pyo3::ffi::c_str!("_flexaudio_errors"),
            )?
            .unbind())
        })
        .map(|m| m.bind(py))
}
pub(crate) fn to_py_err(error: fa::Error) -> PyErr {
    Python::attach(|py| {
        if let Some(report) = error.ignored_settings() {
            return match exceptions(py).and_then(|m| {
                m.getattr("IgnoredSettingsError")?
                    .call1((error.to_string(), crate::settings::to_py(py, report)?))
            }) {
                Ok(value) => PyErr::from_value(value),
                Err(error) => error,
            };
        }
        let name = if error.addon_kind() == Some(fa::AddonErrorKind::FlacIo) {
            "_FlacIoError"
        } else {
            match error.kind() {
                fa::ErrorKind::InvalidArg => "InvalidArgumentError",
                fa::ErrorKind::UnsupportedFormat => "UnsupportedFormatError",
                fa::ErrorKind::InvalidState => "InvalidStateError",
                fa::ErrorKind::DeviceNotFound => "DeviceNotFoundError",
                fa::ErrorKind::PermissionDenied => "RecordingPermissionError",
                fa::ErrorKind::UnsupportedOsVersion => "UnsupportedOsVersionError",
                fa::ErrorKind::DeviceLost => "DeviceLostError",
                fa::ErrorKind::Backend => "BackendError",
                fa::ErrorKind::NativeFormatChanged => "NativeFormatChangedError",
                fa::ErrorKind::Unsupported => "UnsupportedError",
                fa::ErrorKind::AmbiguousDeviceName => "AmbiguousDeviceNameError",
                _ => "BackendError",
            }
        };
        let result = exceptions(py).and_then(|m| {
            m.getattr(name)?
                .call1((error.to_string(), AudioError(error)))
        });
        match result {
            Ok(value) => PyErr::from_value(value),
            Err(error) => error,
        }
    })
}
pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    for name in [
        "InvalidArgumentError",
        "UnsupportedFormatError",
        "InvalidStateError",
        "DeviceNotFoundError",
        "RecordingPermissionError",
        "UnsupportedOsVersionError",
        "DeviceLostError",
        "BackendError",
        "NativeFormatChangedError",
        "UnsupportedError",
        "AmbiguousDeviceNameError",
        "IgnoredSettingsError",
    ] {
        m.add(name, exceptions(m.py())?.getattr(name)?)?;
    }
    m.add_class::<AudioError>()?;
    m.add_class::<ErrorContext>()?;
    m.add_class::<NativeFormat>()?;
    m.add_class::<AudioLoss>()?;
    m.add_class::<ShutdownReport>()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_core_error_kind_has_an_explicit_distinct_python_projection() {
        flexaudio_core::test_support::assert_explicit_error_kind_arms(
            include_str!("errors.rs"),
            "fn kind_name(",
        );
        // IgnoredSettings uses the earlier report-bearing exception branch.
        // Every other kind must name its exception explicitly, even Backend.
        let exception_mapping = include_str!("errors.rs")
            .split_once("pub(crate) fn to_py_err(")
            .unwrap()
            .1
            .split_once("pub(crate) fn register(")
            .unwrap()
            .0;
        for (kind, _, _) in flexaudio_core::test_support::error_cases() {
            if kind != fa::ErrorKind::IgnoredSettings {
                assert!(
                    exception_mapping.contains(&format!("ErrorKind::{kind:?} =>")),
                    "Python exception choice must explicitly map {kind:?}"
                );
            }
        }
        Python::initialize();
        Python::attach(|py| {
            let mut names = std::collections::HashSet::new();
            let mut classes = Vec::new();
            let stub = include_str!("../flexaudio.pyi");
            let literal = stub
                .split_once("def kind(self) -> Literal[")
                .unwrap()
                .1
                .split_once(']')
                .unwrap()
                .0;
            for (kind, expected, error) in flexaudio_core::test_support::error_cases() {
                assert_eq!(error.kind(), kind, "core error fixture must match its kind");
                let name = kind_name(kind);
                assert!(
                    names.insert(name),
                    "Python kind collision or fallback for {kind:?}"
                );
                assert_eq!(name, expected, "Python kind must explicitly map {kind:?}");
                assert!(
                    literal.contains(&format!("\"{name}\"")),
                    "Python kind Literal is missing {name}"
                );
                let exception = to_py_err(error);
                let class = exception.get_type(py);
                assert!(
                    classes
                        .iter()
                        .all(|previous: &Bound<'_, pyo3::types::PyType>| !previous.is(&class)),
                    "Python exception collision or fallback for {kind:?}"
                );
                classes.push(class);
            }
        });
    }

    #[test]
    fn shutdown_report_fields_match_core_and_python_stub() {
        Python::initialize();
        Python::attach(|py| {
            let core =
                fa::ShutdownReport::new(Some(fa::Error::DeviceLost), vec![fa::Error::Unsupported]);
            let mut expected: Vec<_> = flexaudio_core::test_support::shutdown_fields(&core)
                .into_iter()
                .map(|(field, _)| match field {
                    "primary" => "primary".to_owned(),
                    "cleanup" => "cleanup_errors".to_owned(),
                    _ => panic!("Python ShutdownReport field mapping missing {field}"),
                })
                .collect();
            expected.sort();
            let dict = ShutdownReport(core).to_dict(py).unwrap();
            let mut actual: Vec<String> = dict.keys().extract().unwrap();
            actual.sort();
            assert_eq!(
                actual, expected,
                "Python ShutdownReport dict fields differ from core"
            );
            assert_eq!(
                dict.get_item("primary")
                    .unwrap()
                    .unwrap()
                    .get_item("kind")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "deviceLost"
            );
            assert_eq!(
                dict.get_item("cleanup_errors")
                    .unwrap()
                    .unwrap()
                    .get_item(0)
                    .unwrap()
                    .get_item("kind")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "unsupported"
            );
            assert_eq!(
                flexaudio_core::test_support::declaration_fields(
                    include_str!("../flexaudio.pyi"),
                    "class ShutdownReportDict(TypedDict):",
                    "\nclass "
                ),
                expected,
                "Python ShutdownReportDict declaration fields differ from core"
            );
        });
    }

    #[test]
    fn every_root_has_distinct_exception_and_read_only_typed_payload() {
        Python::initialize();
        Python::attach(|py| {
            let roots = [
                fa::Error::InvalidArg("validation".into()),
                fa::Error::InvalidState("state".into()),
                fa::Error::DeviceNotFound,
                fa::Error::PermissionDenied {
                    permission: fa::Permission::Microphone,
                    detail: "private".into(),
                },
                fa::Error::UnsupportedOsVersion,
                fa::Error::DeviceLost,
                fa::Error::Backend("safe failure".into()),
                fa::Error::UnsupportedFormat("format".into()),
                fa::Error::NativeFormatChanged {
                    advertised: (48_000, 2),
                    actual: (44_100, 1),
                },
                fa::Error::Unsupported,
                fa::Error::AmbiguousDeviceName,
            ];
            let mut classes = Vec::new();
            for root in roots {
                let kind = kind_name(root.kind());
                let error =
                    to_py_err(root.with_context(fa::ErrorContext::new(fa::Operation::Start)));
                let class = error.get_type(py);
                assert!(classes
                    .iter()
                    .all(|previous: &Bound<'_, pyo3::types::PyType>| !previous.is(&class)));
                classes.push(class.clone());
                let payload = error.value(py).getattr("audio_error").unwrap();
                assert_eq!(
                    payload
                        .getattr("kind")
                        .unwrap()
                        .extract::<String>()
                        .unwrap(),
                    kind
                );
                assert!(error.value(py).setattr("audio_error", py.None()).is_err());
                assert!(payload.setattr("kind", "backend").is_err());
                assert!(!error.to_string().contains("private"));
            }
        });
    }

    #[test]
    fn nested_error_serialization_preserves_context_secondary_and_formats() {
        Python::initialize();
        Python::attach(|py| {
            let inner = fa::Error::NativeFormatChanged {
                advertised: (48_000, 2),
                actual: (44_100, 1),
            }
            .with_context(fa::ErrorContext::new(fa::Operation::Normalize));
            let secondary = fa::Error::PermissionDenied {
                permission: fa::Permission::SystemAudio,
                detail: "secret".into(),
            }
            .with_context(fa::ErrorContext::new(fa::Operation::Stop));
            let error = fa::Error::Multiple(fa::ErrorGroup::new(inner, secondary, vec![]))
                .with_context(
                    fa::ErrorContext::new(fa::Operation::Start)
                        .with_lane(fa::MixLane::Microphone)
                        .with_native_status(fa::NativeStatus::HResult {
                            call: "internal_call",
                            bits: 0x8000_0001,
                        }),
                );
            let payload = AudioError(error);
            let d = payload.to_dict(py).unwrap();
            assert_eq!(
                d.get_item("kind")
                    .unwrap()
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "nativeFormatChanged"
            );
            let contexts = d.get_item("contexts").unwrap().unwrap();
            assert_eq!(contexts.len().unwrap(), 2);
            assert_eq!(
                contexts
                    .get_item(0)
                    .unwrap()
                    .get_item("operation")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "start"
            );
            assert_eq!(
                contexts
                    .get_item(1)
                    .unwrap()
                    .get_item("operation")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "normalize"
            );
            assert_eq!(
                d.get_item("actual")
                    .unwrap()
                    .unwrap()
                    .get_item("sample_rate")
                    .unwrap()
                    .extract::<u32>()
                    .unwrap(),
                44_100
            );
            let secondary = d
                .get_item("secondary")
                .unwrap()
                .unwrap()
                .get_item(0)
                .unwrap();
            assert_eq!(
                secondary
                    .get_item("permission")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "systemAudio"
            );
            assert!(!payload.message().contains("secret"));
            assert!(!payload.message().contains("internal_call"));
        });
    }
}

#[cfg(test)]
mod retained_cause_tests {
    use super::*;
    #[test]
    fn addon_and_native_detail_preserve_contexts_and_safe_display() {
        let error = AudioError(
            fa::Error::Addon {
                kind: fa::core::AddonErrorKind::VadReset,
                message: "synthetic reset cause".into(),
            }
            .with_context(fa::ErrorContext::new(fa::Operation::Flush))
            .with_diagnostic_detail("synthetic native detail".into()),
        );
        assert_eq!(error.kind(), "backend");
        assert_eq!(error.addon_kind(), Some("vadReset"));
        assert_eq!(
            error.addon_message().as_deref(),
            Some("synthetic reset cause")
        );
        assert_eq!(error.diagnostic_details(), ["synthetic native detail"]);
        assert_eq!(error.contexts()[0].operation(), "flush");
        assert!(!error.message().contains("synthetic"));
    }
}

#[cfg(test)]
mod error_projection_tests {
    use super::*;
    #[test]
    fn typed_python_flac_exceptions_retain_shared_addon_projection() {
        Python::initialize();
        Python::attach(|py| {
            for cause in [
                flexaudio_encode::EncodeError::Unsupported("injected unsupported format".into()),
                flexaudio_encode::EncodeError::Io(std::io::Error::from(
                    std::io::ErrorKind::PermissionDenied,
                )),
                flexaudio_encode::EncodeError::Encoder("injected encoder failure".into()),
            ] {
                let expected = cause.audio_error();
                let exception = crate::encode_err_to_py(cause);
                match expected.addon_kind().unwrap() {
                    fa::AddonErrorKind::FlacIo => {
                        assert!(exception.is_instance_of::<pyo3::exceptions::PyOSError>(py))
                    }
                    fa::AddonErrorKind::FlacUnsupported => {
                        assert!(exception.is_instance_of::<pyo3::exceptions::PyValueError>(py))
                    }
                    _ => assert!(exception.is_instance_of::<pyo3::exceptions::PyRuntimeError>(py)),
                }
                let record = exception.value(py).getattr("audio_error").unwrap();
                assert_eq!(
                    record
                        .getattr("addon_kind")
                        .unwrap()
                        .extract::<String>()
                        .unwrap(),
                    expected.addon_kind().unwrap().as_str()
                );
                assert_eq!(
                    record
                        .getattr("addon_message")
                        .unwrap()
                        .extract::<String>()
                        .unwrap(),
                    match expected.root() {
                        fa::Error::Addon { message, .. } => message.as_str(),
                        _ => panic!("expected addon cause"),
                    }
                );
            }
        });
    }
}
