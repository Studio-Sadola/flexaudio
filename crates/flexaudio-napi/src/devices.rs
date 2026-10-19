//! Asynchronous provider-provenance inventory projection.
use super::*;

#[napi(object)]
pub struct JsNamespacedDevice {
    pub device: JsDeviceInfo,
    pub id_namespace: String,
}

pub struct NamespacedDevicesTask;
impl Task for NamespacedDevicesTask {
    type Output = flexaudio::Result<Vec<flexaudio::NamespacedDevice>>;
    type JsValue = Vec<JsNamespacedDevice>;

    fn compute(&mut self) -> napi::Result<Self::Output> {
        Ok(flexaudio::devices_with_namespaces())
    }

    fn resolve(&mut self, env: Env, devices: Self::Output) -> napi::Result<Self::JsValue> {
        let devices = devices.map_err(|error| boundary::async_error(&env, error))?;
        Ok(devices
            .into_iter()
            .map(|entry| JsNamespacedDevice {
                device: device_info_to_js(entry.device),
                id_namespace: {
                    use flexaudio::DeviceIdNamespace::*;
                    match entry.id_namespace {
                        CpalDeviceName
                        | PipeWireNodeName
                        | WasapiOutputSelector
                        | CoreAudioOutputSelector => entry.id_namespace.as_str(),
                        _ => "unknown",
                    }
                    .into()
                },
            })
            .collect())
    }
}

#[napi(ts_return_type = "Promise<NamespacedDevice[]>")]
pub fn devices_with_namespaces() -> AsyncTask<NamespacedDevicesTask> {
    AsyncTask::new(NamespacedDevicesTask)
}
