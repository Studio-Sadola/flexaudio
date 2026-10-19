//! Device selector provenance, captured by the enumerating provider.

use crate::DeviceInfo;

/// Provider selector domain; these tags do not promise persistent native IDs.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Ord, PartialOrd, Hash)]
pub enum DeviceIdNamespace {
    /// CPAL microphone name selector.
    CpalDeviceName,
    /// PipeWire node name.
    PipeWireNodeName,
    /// WASAPI output name or native endpoint fallback selector.
    WasapiOutputSelector,
    /// Core Audio output name or native UID fallback selector.
    CoreAudioOutputSelector,
    /// No provider provenance is available.
    Unknown,
}
impl DeviceIdNamespace {
    /// Stable identifier shared by language bindings.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CpalDeviceName => "cpalDeviceName",
            Self::PipeWireNodeName => "pipeWireNodeName",
            Self::WasapiOutputSelector => "wasapiOutputSelector",
            Self::CoreAudioOutputSelector => "coreAudioOutputSelector",
            Self::Unknown => "unknown",
        }
    }
}

/// Existing selectable device plus the provider that produced its ID.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespacedDevice {
    /// Unchanged selectable ID and metadata.
    pub device: DeviceInfo,
    /// Provenance assigned during provider enumeration.
    pub id_namespace: DeviceIdNamespace,
}
impl NamespacedDevice {
    /// Attach known enumeration provenance; never infer it from a device literal.
    pub fn new(device: DeviceInfo, id_namespace: DeviceIdNamespace) -> Self {
        Self {
            device,
            id_namespace,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{inventory, Error, SourceKind};

    #[test]
    fn shared_tags_do_not_infer_identity_or_promise_persistence() {
        let namespaces = [
            DeviceIdNamespace::CpalDeviceName,
            DeviceIdNamespace::PipeWireNodeName,
            DeviceIdNamespace::WasapiOutputSelector,
            DeviceIdNamespace::CoreAudioOutputSelector,
            DeviceIdNamespace::Unknown,
        ];
        assert_eq!(
            namespaces.map(DeviceIdNamespace::as_str),
            [
                "cpalDeviceName",
                "pipeWireNodeName",
                "wasapiOutputSelector",
                "coreAudioOutputSelector",
                "unknown"
            ]
        );
        for namespace in namespaces {
            for readable in [true, false] {
                let name = if readable {
                    Ok("duplicate name".into())
                } else {
                    Err(Error::DeviceNotFound)
                };
                let (id, name) =
                    inventory::device_identity(name, || Ok("native selector".into())).unwrap();
                let device = DeviceInfo {
                    id,
                    name,
                    source_kind: SourceKind::SystemLoopback,
                    sample_rate: 48_000,
                    channels: 2,
                    format_available: true,
                    is_loopback: true,
                    is_default: false,
                };
                let entries = [
                    NamespacedDevice::new(device.clone(), namespace),
                    NamespacedDevice::new(device.clone(), namespace),
                ];
                for entry in entries {
                    assert_eq!(entry.device, device);
                    assert_eq!(entry.id_namespace, namespace);
                    assert_eq!(
                        entry.device.id,
                        if readable {
                            "duplicate name"
                        } else {
                            "native selector"
                        }
                    );
                }
            }
        }
    }
}
