// Bounded watcher deltas with a separate sticky inventory invalidation.
use flexaudio_core::{
    terminal_causes::retain_terminal_cause, DefaultDeviceKind, DeviceEvent, Error,
};
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
        retain_terminal_cause(&mut self.terminal, error);
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
    fn distinct_failures_and_equal_snapshots_keep_one_ordered_flat_group() {
        flexaudio_core::test_support::assert_distinct_failures_retained(
            Error::DeviceLost,
            Error::Backend("later watcher join failure".into()),
            |first, second| {
                let mut queue = WatchEvents::default();
                queue.fail(first.clone());
                assert_eq!(
                    queue.poll(),
                    Some(DeviceEvent::TerminalError {
                        error: first.clone(),
                    })
                );
                queue.fail(first.clone());
                queue.fail(second.clone());
                queue.fail(second.clone());
                let expected =
                    Error::Multiple(flexaudio_core::ErrorGroup::new(first, second, Vec::new()));
                queue.fail(expected.clone());
                let retained = queue.terminal_error().unwrap();
                assert_eq!(retained, expected);
                assert_eq!(
                    queue.poll(),
                    Some(DeviceEvent::TerminalError {
                        error: retained.clone(),
                    })
                );
                assert_eq!(queue.poll(), None);
                retained
            },
        );
    }

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
        assert_eq!(
            queue.poll(),
            Some(DeviceEvent::TerminalError {
                error: Error::DeviceLost
            })
        );
        let cleanup = Error::Backend("worker failed".into());
        queue.fail(cleanup.clone());
        assert!(
            matches!(queue.poll(), Some(DeviceEvent::TerminalError { error: Error::Multiple(group) })
            if group.primary() == &Error::DeviceLost && group.secondary().collect::<Vec<_>>() == [&cleanup])
        );
        assert_eq!(queue.poll(), None);
    }
}
