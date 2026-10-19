//! Provenance-aware device listing without capture side effects.
use flexaudio::NamespacedDevice;
use serde_json::{json, Value};
use std::io::{self, Write};

pub(super) fn project(devices: &[NamespacedDevice]) -> Value {
    Value::Array(
        devices
            .iter()
            .map(|entry| {
                let device = &entry.device;
                json!({
                    "device": {
                        "id": device.id, "name": device.name,
                        "sourceKind": super::source_kind_label(device.source_kind),
                        "sampleRate": device.sample_rate, "channels": device.channels,
                        "formatAvailable": device.format_available,
                        "isLoopback": device.is_loopback, "isDefault": device.is_default,
                    },
                    "idNamespace": entry.id_namespace.as_str(),
                })
            })
            .collect(),
    )
}

pub(super) fn list() -> Result<(), String> {
    let devices = flexaudio::devices_with_namespaces()
        .map_err(|error| format!("Failed to enumerate devices: {error}"))?;
    let mut output = io::stdout().lock();
    serde_json::to_writer(&mut output, &project(&devices))
        .map_err(|error| format!("Failed to write devices JSON: {error}"))?;
    writeln!(output).map_err(|error| format!("Failed to write devices JSON: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio::{DeviceIdNamespace, DeviceInfo, SourceKind};

    #[test]
    fn fake_device_list_preserves_selectors_and_provenance() {
        let id = "device\"\\\n\u{0000}";
        let device = DeviceInfo {
            id: id.into(),
            name: "Input".into(),
            source_kind: SourceKind::Mic,
            sample_rate: 48000,
            channels: 2,
            format_available: true,
            is_loopback: false,
            is_default: true,
        };
        let entries = [NamespacedDevice::new(
            device,
            DeviceIdNamespace::CpalDeviceName,
        )];
        let result: Value = serde_json::from_str(&project(&entries).to_string()).unwrap();
        assert_eq!(
            result,
            json!([{"device": {
            "id":id,"name":"Input","sourceKind":"mic","sampleRate":48000,
            "channels":2,"formatAvailable":true,"isLoopback":false,"isDefault":true
        },"idNamespace":"cpalDeviceName"}])
        );
        assert_eq!(project(&[]), json!([]));
    }
}
