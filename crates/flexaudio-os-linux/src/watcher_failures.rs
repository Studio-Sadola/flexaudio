// One terminal reporting path for watcher callbacks and explicit owner teardown.
use crate::watcher_queue::{lock_events, WatchEventQueue};
use flexaudio_core::{Error, ErrorContext, Operation};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::thread::JoinHandle;

pub(crate) fn fail_watcher(events: &WatchEventQueue, error: Error) {
    lock_events(events).fail(error);
}

pub(crate) fn run_watcher_callback(
    events: &WatchEventQueue,
    quit: impl FnOnce(),
    body: impl FnOnce(),
) {
    if catch_unwind(AssertUnwindSafe(body)).is_err() {
        fail_watcher(
            events,
            Error::Backend("PipeWire watcher callback failed; restart device watching.".into())
                .with_context(ErrorContext::new(Operation::Enumerate)),
        );
        quit();
    }
}

pub(crate) fn finish_watch_worker(handle: &mut Option<JoinHandle<()>>, events: &WatchEventQueue) {
    if let Some(handle) = handle.take() {
        if let Err(error) = crate::owner::join_worker(handle) {
            fail_watcher(events, error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watcher_queue::WatchEvents;
    use std::cell::Cell;
    use std::sync::{Arc, Mutex};
    #[test]
    fn callback_panic_ends_watcher_with_typed_failure() {
        let events = Arc::new(Mutex::new(WatchEvents::default()));
        let quit = Cell::new(false);
        run_watcher_callback(&events, || quit.set(true), || panic!("synthetic failure"));
        assert!(quit.get());
        assert!(
            matches!(lock_events(&events).poll(), Some(flexaudio_core::DeviceEvent::TerminalError { error })
            if error.kind() == flexaudio_core::ErrorKind::Backend && !error.to_string().contains("synthetic failure"))
        );
    }
}
