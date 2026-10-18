//! Snake-case projection of the shared capability snapshot.
use ::flexaudio as fa;
use pyo3::prelude::*;
use pyo3::types::PyDict;

#[pyfunction]
pub(crate) fn capabilities(py: Python<'_>) -> PyResult<Bound<'_, PyDict>> {
    let snapshot = py.detach(fa::capabilities);
    project(py, &snapshot)
}

fn version<'py>(py: Python<'py>, value: &fa::OsVersion) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("major", value.major)?;
    d.set_item("minor", value.minor)?;
    d.set_item("patch", value.patch)?;
    d.set_item("build", value.build)?;
    Ok(d)
}

fn backend(value: &fa::BackendKind) -> &'static str {
    match value {
        fa::BackendKind::Cpal => "cpal",
        fa::BackendKind::PipeWire => "pipeWire",
        fa::BackendKind::WasapiLoopback => "wasapiLoopback",
        fa::BackendKind::WasapiProcessLoopback => "wasapiProcessLoopback",
        fa::BackendKind::CoreAudioProcessTap => "coreAudioProcessTap",
        fa::BackendKind::Composite => "composite",
        fa::BackendKind::Unsupported => "unsupported",
        _ => "other",
    }
}

fn scope(value: &fa::PidScope) -> &'static str {
    match value {
        fa::PidScope::ExactPid => "exactPid",
        fa::PidScope::ProcessTree => "processTree",
        fa::PidScope::NotApplicable => "notApplicable",
        _ => "other",
    }
}

fn availability<'py>(py: Python<'py>, value: &fa::Availability) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    match value {
        fa::Availability::Supported => d.set_item("status", "supported")?,
        fa::Availability::Unknown { error } => {
            d.set_item("status", "unknown")?;
            d.set_item(
                "error",
                crate::errors::AudioError(error.clone()).to_dict(py)?,
            )?;
        }
        fa::Availability::Unavailable { reason } => {
            d.set_item("status", "unavailable")?;
            let r = PyDict::new(py);
            let kind = match reason {
                fa::UnavailableReason::UnsupportedOs => "unsupportedOs",
                fa::UnavailableReason::PipeWireUnavailable => "pipeWireUnavailable",
                fa::UnavailableReason::NotApplicable => "notApplicable",
                fa::UnavailableReason::OsVersionTooOld { required, actual } => {
                    r.set_item("required", version(py, required)?)?;
                    r.set_item("actual", version(py, actual)?)?;
                    "osVersionTooOld"
                }
                other => return other_unavailable(py, other),
            };
            r.set_item("kind", kind)?;
            d.set_item("reason", r)?;
        }
        _ => {
            d.set_item("status", "unknown")?;
            d.set_item(
                "error",
                crate::errors::AudioError(fa::Error::Unsupported).to_dict(py)?,
            )?;
        }
    }
    Ok(d)
}

fn other_unavailable<'py>(
    py: Python<'py>,
    reason: &impl std::fmt::Debug,
) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    let reason_dict = PyDict::new(py);
    reason_dict.set_item("kind", "other")?;
    reason_dict.set_item("code", format!("{reason:?}"))?;
    d.set_item("status", "unavailable")?;
    d.set_item("reason", reason_dict)?;
    Ok(d)
}

fn source<'py>(py: Python<'py>, value: &fa::SourceCapability) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("backend", backend(&value.backend))?;
    d.set_item("availability", availability(py, &value.availability)?)?;
    Ok(d)
}

fn side_effect<'py>(
    py: Python<'py>,
    value: &fa::SideEffectCapability,
) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("availability", availability(py, &value.availability)?)?;
    d.set_item("enabled_by_default", value.enabled_by_default)?;
    d.set_item(
        "sources",
        value
            .sources
            .iter()
            .map(|v| crate::source_kind_str(*v))
            .collect::<Vec<_>>(),
    )?;
    d.set_item(
        "condition",
        match value.condition {
            fa::SideEffectCondition::SilentActiveSystemAudio => "silentActiveSystemAudio",
            fa::SideEffectCondition::ClassicLoopbackWithoutExclusions => {
                "classicLoopbackWithoutExclusions"
            }
            _ => "other",
        },
    )?;
    Ok(d)
}

fn project<'py>(py: Python<'py>, snapshot: &fa::Capabilities) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item(
        "os_version",
        snapshot
            .os_version
            .as_ref()
            .map(|v| version(py, v))
            .transpose()?,
    )?;
    let sources = PyDict::new(py);
    for (name, value) in [
        ("mic", &snapshot.sources.mic),
        ("system", &snapshot.sources.system),
        ("process", &snapshot.sources.process),
        ("mix", &snapshot.sources.mix),
    ] {
        sources.set_item(name, source(py, value)?)?;
    }
    d.set_item("sources", sources)?;
    let exclusions = &snapshot.system_exclusions;
    let e = PyDict::new(py);
    e.set_item("availability", availability(py, &exclusions.availability)?)?;
    e.set_item("backend", backend(&exclusions.backend))?;
    e.set_item("scope", scope(&exclusions.scope))?;
    e.set_item("max_distinct_pids", exclusions.max_distinct_pids)?;
    e.set_item(
        "device_selection",
        match exclusions.device_selection {
            fa::DeviceSelectionBehavior::Honored => "honored",
            fa::DeviceSelectionBehavior::Ignored => "ignored",
            fa::DeviceSelectionBehavior::NotApplicable => "notApplicable",
            _ => "other",
        },
    )?;
    d.set_item("system_exclusions", e)?;
    let process = PyDict::new(py);
    process.set_item("include_scope", scope(&snapshot.process.include_scope))?;
    process.set_item("exclude_scope", scope(&snapshot.process.exclude_scope))?;
    d.set_item("process", process)?;
    let watcher = PyDict::new(py);
    watcher.set_item(
        "availability",
        availability(py, &snapshot.device_watcher.availability)?,
    )?;
    watcher.set_item(
        "mode",
        match snapshot.device_watcher.mode {
            fa::WatcherMode::Native => "native",
            fa::WatcherMode::Noop => "noop",
            _ => "other",
        },
    )?;
    watcher.set_item(
        "scopes",
        snapshot
            .device_watcher
            .scopes
            .iter()
            .map(|v| match v {
                fa::WatchScope::SystemOutputs => "systemOutputs",
                fa::WatchScope::DefaultSystemOutput => "defaultSystemOutput",
                _ => "other",
            })
            .collect::<Vec<_>>(),
    )?;
    d.set_item("device_watcher", watcher)?;
    let effects = PyDict::new(py);
    effects.set_item(
        "silence_probe",
        side_effect(py, &snapshot.side_effects.silence_probe)?,
    )?;
    effects.set_item(
        "classic_loopback_keepalive",
        side_effect(py, &snapshot.side_effects.classic_loopback_keepalive)?,
    )?;
    d.set_item("side_effects", effects)?;
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn future_unavailable_reason_preserves_debug_code() {
        #[derive(Debug)]
        enum FutureReason {
            BackendUnavailable,
        }
        Python::initialize();
        Python::attach(|py| {
            let value = other_unavailable(py, &FutureReason::BackendUnavailable).unwrap();
            assert_eq!(value.len(), 2);
            assert_eq!(
                value
                    .get_item("status")
                    .unwrap()
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "unavailable"
            );
            let projected = value.get_item("reason").unwrap().unwrap();
            assert_eq!(projected.len().unwrap(), 2);
            assert_eq!(
                projected
                    .get_item("kind")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "other"
            );
            assert_eq!(
                projected
                    .get_item("code")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "BackendUnavailable"
            );
        });
    }

    #[test]
    fn availability_shapes_preserve_typed_errors_and_version_payloads() {
        Python::initialize();
        Python::attach(|py| {
            let supported = availability(py, &fa::Availability::Supported).unwrap();
            assert_eq!(supported.len(), 1);
            let unknown = availability(
                py,
                &fa::Availability::Unknown {
                    error: fa::Error::DeviceLost,
                },
            )
            .unwrap();
            assert_eq!(
                unknown
                    .get_item("error")
                    .unwrap()
                    .unwrap()
                    .get_item("kind")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "deviceLost"
            );
            for reason in [
                fa::UnavailableReason::UnsupportedOs,
                fa::UnavailableReason::NotApplicable,
                fa::UnavailableReason::PipeWireUnavailable,
                fa::UnavailableReason::OsVersionTooOld {
                    required: fa::OsVersion::new(14, 4, None, None),
                    actual: fa::OsVersion::new(13, 0, Some(1), Some(2)),
                },
            ] {
                let value = availability(py, &fa::Availability::Unavailable { reason }).unwrap();
                assert_eq!(value.len(), 2);
                assert!(value
                    .get_item("reason")
                    .unwrap()
                    .unwrap()
                    .get_item("kind")
                    .is_ok());
            }
        });
    }
}
