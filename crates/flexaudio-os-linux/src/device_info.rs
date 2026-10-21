// Convert a completed PipeWire inventory without querying native objects.
use super::{NATIVE_CHANNELS, NATIVE_RATE};
use flexaudio_core::{DeviceIdNamespace, DeviceInfo, NamespacedDevice, SourceKind};

/// Raw node properties collected by registry callbacks.
pub(super) struct NodeRecord {
    pub(super) node_name: String,
    pub(super) description: String,
    pub(super) media_class: String,
    pub(super) rate: Option<u32>,
    pub(super) channels: Option<u16>,
}

/// Collector confined to the enumeration loop's thread.
#[derive(Default)]
pub(super) struct EnumState {
    pub(super) nodes: Vec<NodeRecord>,
    pub(super) default_sink: Option<String>,
    pub(super) default_source: Option<String>,
}

pub(super) fn namespaced_devices(state: &EnumState) -> Vec<NamespacedDevice> {
    let mut out = Vec::with_capacity(state.nodes.len());
    for n in &state.nodes {
        let is_loopback = n.media_class == "Audio/Sink";
        let source_kind = if is_loopback {
            SourceKind::SystemLoopback
        } else {
            SourceKind::Mic
        };
        let is_default = if is_loopback {
            state.default_sink.as_deref() == Some(n.node_name.as_str())
        } else {
            state.default_source.as_deref() == Some(n.node_name.as_str())
        };
        let (sample_rate, channels, format_available) =
            flexaudio_core::inventory::device_format(Ok((
                n.rate.unwrap_or(NATIVE_RATE),
                n.channels.unwrap_or(NATIVE_CHANNELS),
            )));
        out.push(NamespacedDevice::new(
            {
                let mut device_value =
                    DeviceInfo::new(n.node_name.clone(), n.description.clone(), source_kind);
                device_value.sample_rate = sample_rate;
                device_value.channels = channels;
                device_value.format_available = format_available;
                device_value.is_loopback = is_loopback;
                device_value.is_default = is_default;
                device_value
            },
            DeviceIdNamespace::PipeWireNodeName,
        ));
    }
    out
}
