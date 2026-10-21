//! Provider-assigned device selector provenance.
use pyo3::prelude::*;

#[pyclass(module = "flexaudio", name = "NamespacedDevice", frozen)]
pub(crate) struct NamespacedDevice(::flexaudio::NamespacedDevice);

#[pymethods]
impl NamespacedDevice {
    #[getter]
    fn device(&self) -> crate::marshal::PyDeviceInfo {
        crate::marshal::device_info_to_py(self.0.device.clone())
    }
    #[getter]
    fn id_namespace(&self) -> &'static str {
        self.0.id_namespace.as_str()
    }
}

#[pyfunction]
pub(crate) fn devices_with_namespaces() -> PyResult<Vec<NamespacedDevice>> {
    ::flexaudio::devices_with_namespaces()
        .map(|entries| entries.into_iter().map(NamespacedDevice).collect())
        .map_err(crate::to_py_err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::flexaudio as fa;

    #[test]
    fn provider_provenance_does_not_depend_on_device_id_spelling() {
        Python::initialize();
        Python::attach(|py| {
            for namespace in [
                fa::DeviceIdNamespace::CpalDeviceName,
                fa::DeviceIdNamespace::PipeWireNodeName,
                fa::DeviceIdNamespace::WasapiOutputSelector,
                fa::DeviceIdNamespace::CoreAudioOutputSelector,
                fa::DeviceIdNamespace::Unknown,
            ] {
                let mut device = fa::DeviceInfo::new(
                    "same-selector".into(),
                    "same-name".into(),
                    fa::SourceKind::SystemLoopback,
                );
                device.sample_rate = 48_000;
                device.channels = 2;
                device.format_available = true;
                device.is_loopback = true;
                device.is_default = false;
                let object = Py::new(
                    py,
                    NamespacedDevice(fa::NamespacedDevice::new(device, namespace)),
                )
                .unwrap();
                let object = object.bind(py);
                assert_eq!(
                    object
                        .getattr("id_namespace")
                        .unwrap()
                        .extract::<String>()
                        .unwrap(),
                    namespace.as_str()
                );
                assert_eq!(
                    object
                        .getattr("device")
                        .unwrap()
                        .getattr("id")
                        .unwrap()
                        .extract::<String>()
                        .unwrap(),
                    "same-selector"
                );
                assert!(object.setattr("id_namespace", "unknown").is_err());
            }
        });
    }
}
