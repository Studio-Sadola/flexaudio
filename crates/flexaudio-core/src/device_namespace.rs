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
    /// Provider crates use this constructor at enumeration time. The caller is
    /// responsible for choosing the namespace of the selector it actually exposes.
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

    #[test]
    fn shared_namespace_tags_are_stable() {
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
    }
}
