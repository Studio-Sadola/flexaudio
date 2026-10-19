//! Translation between Python options and the shared settings resolver.
use ::flexaudio as fa;
use fa::{SettingKey as K, SettingOutcome as O, SettingValue as V};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

#[derive(Default)]
pub(crate) struct SettingsState {
    // Device-free custom-backend fixtures have no facade report.
    #[cfg(test)]
    pub(crate) report: Option<fa::SettingsReport>,
    pub(crate) strict: bool,
}

pub(crate) struct Request<'a> {
    pub(crate) config: &'a fa::StreamConfig,
    pub(crate) keepalive: Option<bool>,
    pub(crate) denoise: bool,
    pub(crate) vad: bool,
    pub(crate) secondary: Option<&'a crate::secondary::SecondaryOutputOptions>,
}

impl Request<'_> {
    pub(crate) fn attachments(&self, switching: bool) -> fa::SettingsAttachments {
        let attachments = fa::SettingsAttachments::new()
            .denoise(self.denoise)
            .windows_classic_loopback_keepalive(self.keepalive);
        if switching {
            attachments.omitted_on_switch([K::SecondaryOutput, K::SecondaryEncoding, K::Vad])
        } else {
            attachments
                .vad(self.vad)
                .secondary_encoding(self.secondary.map(|s| s.encoding.name()).unwrap_or("f32"))
        }
    }

    pub(crate) fn resolve(
        &self,
        previous: Option<&fa::SettingsReport>,
        strict: bool,
    ) -> fa::Result<fa::SettingsReport> {
        let mut request = fa::SettingsRequest::new(self.config.clone())
            .attachments(self.attachments(previous.is_some()))
            .windows_classic_loopback_keepalive(self.keepalive);
        if let Some(previous) = previous {
            request = request.previous(previous);
        }
        let report = fa::resolve_settings(&request)?;
        if strict && report.has_ignored() {
            Err(fa::Error::IgnoredSettings { report })
        } else {
            Ok(report)
        }
    }
}

fn value<'py>(py: Python<'py>, value: &V) -> PyResult<Bound<'py, PyAny>> {
    Ok(match value {
        V::Absent => py.None().into_bound(py),
        V::Bool(v) => v.into_pyobject(py)?.to_owned().into_any(),
        V::Unsigned(v) => v.into_pyobject(py)?.into_any(),
        V::Float(v) => v.into_pyobject(py)?.into_any(),
        V::Text(v) => v.into_pyobject(py)?.into_any(),
        V::Pids(v) => v.into_pyobject(py)?.into_any(),
        V::Format(v) => (v.sample_rate, v.channels).into_pyobject(py)?.into_any(),
        _ => {
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "unsupported setting value",
            ))
        }
    })
}

pub(crate) fn to_py<'py>(
    py: Python<'py>,
    report: &fa::SettingsReport,
) -> PyResult<Bound<'py, PyDict>> {
    let result = PyDict::new(py);
    result.set_item("revision", report.revision)?;
    let entries = PyList::empty(py);
    for entry in &report.entries {
        let row = PyDict::new(py);
        row.set_item("setting", entry.setting.as_str())?;
        row.set_item("requested", value(py, &entry.requested)?)?;
        row.set_item("effective", value(py, &entry.effective)?)?;
        let outcome = PyDict::new(py);
        let (status, reason) = match entry.outcome {
            O::NotRequested => ("notRequested", None),
            O::Applied => ("applied", None),
            O::Ignored(reason) => ("ignored", Some(reason.as_str())),
            O::Adjusted(reason) => ("adjusted", Some(reason.as_str())),
            _ => {
                return Err(pyo3::exceptions::PyRuntimeError::new_err(
                    "unsupported setting outcome",
                ))
            }
        };
        outcome.set_item("status", status)?;
        outcome.set_item("reason", reason)?;
        row.set_item("outcome", outcome)?;
        entries.append(row)?;
    }
    result.set_item("entries", entries)?;
    Ok(result)
}
