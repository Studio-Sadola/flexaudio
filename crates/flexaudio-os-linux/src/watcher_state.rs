//! Registry identities and default transitions for device watching.
use crate::watcher_queue::transition_default;
use flexaudio_core::{DefaultDeviceKind, DeviceEvent, DeviceInfo, SourceKind};
use std::collections::HashMap;

#[derive(Default)]
pub(crate) struct WatchState {
    // The registry removes globals by numeric ID; keep their selectable names here.
    by_global_id: HashMap<u32, DeviceInfo>,
    pub(crate) initial_scan_done: bool,
    pub(crate) default_sink: Option<String>,
}

impl WatchState {
    pub(crate) fn add_device(&mut self, id: u32, info: DeviceInfo) -> Option<DeviceEvent> {
        // Only outputs share the facade inventory's selectable PipeWire ID space.
        // Filter before insertion so removals use exactly the same inventory.
        if info.source_kind != SourceKind::SystemLoopback {
            return None;
        }
        self.by_global_id.insert(id, info.clone());
        self.initial_scan_done.then_some(DeviceEvent::Added(info))
    }

    pub(crate) fn remove_device(&mut self, id: u32) -> Option<DeviceEvent> {
        self.by_global_id
            .remove(&id)
            .map(|info| DeviceEvent::Removed { id: info.id })
    }

    pub(crate) fn update_default(
        &mut self,
        next: Option<String>,
        kind: DefaultDeviceKind,
    ) -> Option<DeviceEvent> {
        if kind != DefaultDeviceKind::SystemAudio {
            return None;
        }
        let event = transition_default(&mut self.default_sink, next, kind);
        self.initial_scan_done.then_some(event).flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio_core::SourceKind;

    fn device(id: &str, kind: SourceKind) -> DeviceInfo {
        DeviceInfo {
            id: id.into(),
            name: id.into(),
            source_kind: kind,
            sample_rate: 48_000,
            channels: 2,
            format_available: true,
            is_loopback: kind == SourceKind::SystemLoopback,
            is_default: false,
        }
    }

    #[test]
    fn watcher_omits_pipewire_microphone_additions_and_removals() {
        for initial_scan_done in [false, true] {
            let mut state = WatchState {
                initial_scan_done,
                ..Default::default()
            };
            assert_eq!(state.add_device(7, device("source", SourceKind::Mic)), None);
            assert_eq!(state.remove_device(7), None);
        }
    }

    #[test]
    fn watcher_keeps_output_ids_for_initial_and_later_devices() {
        let mut state = WatchState::default();
        let output = device("sink", SourceKind::SystemLoopback);
        assert_eq!(state.add_device(1, output.clone()), None);
        state.initial_scan_done = true;
        assert_eq!(
            state.remove_device(1),
            Some(DeviceEvent::Removed { id: "sink".into() })
        );
        assert_eq!(
            state.add_device(2, output.clone()),
            Some(DeviceEvent::Added(output))
        );
        assert_eq!(
            state.remove_device(2),
            Some(DeviceEvent::Removed { id: "sink".into() })
        );
        assert_eq!(state.remove_device(2), None);
    }

    #[test]
    fn watcher_omits_pipewire_microphone_defaults_but_keeps_output_defaults() {
        let mut state = WatchState {
            initial_scan_done: true,
            ..Default::default()
        };
        for next in [Some("source".into()), None] {
            assert_eq!(
                state.update_default(next, DefaultDeviceKind::Microphone),
                None
            );
        }
        assert_eq!(
            state.update_default(Some("sink".into()), DefaultDeviceKind::SystemAudio),
            Some(DeviceEvent::DefaultChanged {
                kind: DefaultDeviceKind::SystemAudio,
                id: "sink".into(),
            })
        );
        assert_eq!(
            state.update_default(None, DefaultDeviceKind::SystemAudio),
            Some(DeviceEvent::DefaultCleared {
                kind: DefaultDeviceKind::SystemAudio
            })
        );
    }
}
