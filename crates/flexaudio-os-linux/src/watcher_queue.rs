// Bounded watcher deltas with a separate sticky inventory invalidation.
use flexaudio_core::{DefaultDeviceKind, DeviceEvent};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Default)]
pub(crate) struct WatchEvents {
    pub(crate) deltas: VecDeque<DeviceEvent>,
    dropped_events: u64,
    rescan_pending: bool,
}
pub(crate) type WatchEventQueue = Arc<Mutex<WatchEvents>>;

pub(crate) fn transition_default(
    previous: &mut Option<String>,
    next: Option<String>,
    kind: DefaultDeviceKind,
) -> Option<DeviceEvent> {
    if *previous == next {
        return None;
    }
    *previous = next.clone();
    Some(match next {
        Some(id) => DeviceEvent::DefaultChanged { kind, id },
        None => DeviceEvent::DefaultCleared { kind },
    })
}

pub(crate) fn lock_events(events: &WatchEventQueue) -> MutexGuard<'_, WatchEvents> {
    match events.lock() {
        Ok(queue) => queue,
        Err(poison) => {
            let mut queue = poison.into_inner();
            queue.rescan_pending = true;
            events.clear_poison();
            queue
        }
    }
}

impl WatchEvents {
    pub(crate) fn invalidate(&mut self) {
        self.rescan_pending = true;
    }
    pub(crate) fn push(&mut self, event: DeviceEvent, capacity: usize) {
        if self.deltas.len() >= capacity {
            self.deltas.pop_front();
            self.dropped_events = self.dropped_events.saturating_add(1);
            self.rescan_pending = true;
        }
        self.deltas.push_back(event);
    }

    pub(crate) fn poll(&mut self) -> Option<DeviceEvent> {
        if std::mem::take(&mut self.rescan_pending) {
            Some(DeviceEvent::RescanRequired {
                dropped_events: self.dropped_events,
            })
        } else {
            self.deltas.pop_front()
        }
    }
}
