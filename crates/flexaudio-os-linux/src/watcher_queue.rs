// Bounded watcher deltas with a separate sticky inventory invalidation.
use flexaudio_core::{DefaultDeviceKind, DeviceEvent, Error, ErrorGroup};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Default)]
pub(crate) struct WatchEvents {
    pub(crate) deltas: VecDeque<DeviceEvent>,
    dropped_events: u64,
    rescan_pending: bool,
    terminal: Option<Error>,
    terminal_pending: bool,
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
    pub(crate) fn terminal_error(&self) -> Option<Error> {
        self.terminal.clone()
    }

    pub(crate) fn fail(&mut self, error: Error) {
        self.terminal = Some(match self.terminal.take() {
            None => error,
            Some(primary) => Error::Multiple(ErrorGroup::new(primary, error, Vec::new())),
        });
        self.terminal_pending = true;
    }

    pub(crate) fn push(&mut self, event: DeviceEvent, capacity: usize) {
        if self.terminal.is_some() {
            return;
        }
        if self.deltas.len() >= capacity {
            self.deltas.pop_front();
            self.dropped_events = self.dropped_events.saturating_add(1);
            self.rescan_pending = true;
        }
        self.deltas.push_back(event);
    }

    pub(crate) fn poll(&mut self) -> Option<DeviceEvent> {
        if std::mem::take(&mut self.terminal_pending) {
            return self
                .terminal
                .clone()
                .map(|error| DeviceEvent::TerminalError { error });
        }
        if std::mem::take(&mut self.rescan_pending) {
            Some(DeviceEvent::RescanRequired {
                dropped_events: self.dropped_events,
            })
        } else {
            self.deltas.pop_front()
        }
    }
}

#[cfg(test)]
mod failure_tests {
    use super::*;
    #[test]
    fn terminal_failure_survives_overflow_and_remains_structured() {
        let mut queue = WatchEvents::default();
        queue.push(
            DeviceEvent::Removed {
                id: "fixture".into(),
            },
            1,
        );
        queue.push(
            DeviceEvent::Removed {
                id: "fixture2".into(),
            },
            1,
        );
        let error = crate::discovery::core_error(9, -5, "synthetic daemon detail");
        queue.fail(error.clone());
        for _ in 0..3 {
            queue.push(DeviceEvent::Removed { id: "later".into() }, 1);
        }
        assert_eq!(
            queue.poll(),
            Some(DeviceEvent::TerminalError {
                error: error.clone()
            })
        );
        assert_eq!(queue.terminal_error(), Some(error));
        assert!(matches!(
            queue.poll(),
            Some(DeviceEvent::RescanRequired { .. })
        ));
        assert!(matches!(queue.poll(), Some(DeviceEvent::Removed { id }) if id == "fixture2"));
        assert_eq!(queue.poll(), None);
    }

    #[test]
    fn later_join_failure_is_retained_with_original_terminal_cause() {
        let mut queue = WatchEvents::default();
        queue.fail(Error::DeviceLost);
        let _ = queue.poll();
        let cleanup = Error::Backend("worker failed".into());
        queue.fail(cleanup.clone());
        assert!(
            matches!(queue.poll(), Some(DeviceEvent::TerminalError { error: Error::Multiple(group) })
            if group.primary() == &Error::DeviceLost && group.secondary().collect::<Vec<_>>() == [&cleanup])
        );
        assert_eq!(queue.poll(), None);
    }
}
