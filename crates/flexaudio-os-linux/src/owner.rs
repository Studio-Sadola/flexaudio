// Shared checked worker cleanup and control-thread failure reporting.
use flexaudio_core::terminal_causes::retain_terminal_cause;
use flexaudio_core::{Error, ErrorContext, ErrorGroup, Event, Operation, Result, ShutdownReport};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

#[derive(Default)]
pub(crate) struct BackendEventQueue {
    pending: VecDeque<Event>,
    primary: Option<Error>,
}

pub(crate) type BackendEvents = Arc<Mutex<BackendEventQueue>>;

pub(crate) fn request_worker_stop(
    sender: &mut Option<crate::pw::channel::Sender<crate::Terminate>>,
) -> Option<Error> {
    sender.take().and_then(|sender| {
        sender.send(crate::Terminate).err().map(|_| {
            Error::Backend("PipeWire stop request could not be delivered".into())
                .with_context(ErrorContext::new(Operation::Stop))
        })
    })
}

pub(crate) fn join_worker(handle: JoinHandle<()>) -> Result<()> {
    handle.join().map_err(|_| {
        Error::Backend("pipewire worker panicked".into())
            .with_context(ErrorContext::new(Operation::Join))
    })
}

pub(crate) fn rollback_worker(primary: Error, handle: JoinHandle<()>) -> Error {
    match join_worker(handle) {
        Ok(()) => primary,
        Err(cleanup) => Error::Multiple(ErrorGroup::new(
            primary,
            cleanup.with_context(ErrorContext::new(Operation::Rollback)),
            Vec::new(),
        )),
    }
}

pub(crate) fn rollback_capture(
    primary: Error,
    handle: JoinHandle<()>,
    events: BackendEvents,
    retained_events: &mut BackendEvents,
    shutdown: &mut Option<ShutdownReport>,
) -> Error {
    let cleanup = join_worker(handle)
        .err()
        .map(|error| error.with_context(ErrorContext::new(Operation::Rollback)));
    let mut retained_cleanup = shutdown
        .take()
        .map(|previous| previous.cleanup().to_vec())
        .unwrap_or_default();
    retained_cleanup.extend(cleanup);
    let report = ShutdownReport::new(Some(primary), retained_cleanup);
    let error = report
        .result()
        .expect_err("startup failure remains primary");
    *retained_events = events;
    *shutdown = Some(report);
    error
}

pub(crate) fn finish_worker(
    handle: &mut Option<JoinHandle<()>>,
    shutdown: &mut Option<ShutdownReport>,
    events: &BackendEvents,
    stop_error: Option<Error>,
) {
    if shutdown.is_some() {
        return;
    }
    let result = handle.take().map_or(Ok(()), join_worker);
    let cleanup: Vec<_> = stop_error.into_iter().chain(result.err()).collect();
    for error in &cleanup {
        push_backend_event(
            events,
            Event::ShutdownError {
                error: error.clone(),
            },
        );
    }
    let primary = events
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .primary
        .clone();
    *shutdown = Some(ShutdownReport::new(primary, cleanup));
}

pub(crate) fn push_backend_event(events: &BackendEvents, event: Event) {
    let mut queue = events.lock().unwrap_or_else(|poison| poison.into_inner());
    // The first capture failure is the one terminal event; later distinct causes
    // stay related to it in the report, independently of queued cleanup failures.
    if let Event::TerminalError { error } = &event {
        if !retain_terminal_cause(&mut queue.primary, error.clone()) {
            return;
        }
    }
    queue.pending.push_back(event);
}

pub(crate) fn poll_backend_event(events: &BackendEvents) -> Option<Event> {
    events
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .pending
        .pop_front()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terminal(error: Error) -> Event {
        Event::TerminalError { error }
    }

    #[test]
    fn later_distinct_terminal_cause_is_retained_and_equal_cause_collapses() {
        let events = BackendEvents::default();
        let second = Error::Backend("b".into());
        push_backend_event(&events, terminal(Error::DeviceLost));
        push_backend_event(&events, terminal(Error::DeviceLost));
        push_backend_event(&events, terminal(second.clone()));
        push_backend_event(&events, terminal(second.clone()));
        let mut shutdown = None;
        finish_worker(&mut None, &mut shutdown, &events, None);
        let expected = Error::Multiple(ErrorGroup::new(Error::DeviceLost, second, Vec::new()));
        assert_eq!(
            shutdown,
            Some(ShutdownReport::new(Some(expected), Vec::new()))
        );
        assert_eq!(
            poll_backend_event(&events),
            Some(terminal(Error::DeviceLost))
        );
        assert_eq!(poll_backend_event(&events), None);
    }

    #[test]
    fn equal_terminal_cause_is_kept_once() {
        let events = BackendEvents::default();
        push_backend_event(&events, terminal(Error::DeviceLost));
        push_backend_event(&events, terminal(Error::DeviceLost));
        let mut shutdown = None;
        finish_worker(&mut None, &mut shutdown, &events, None);
        assert_eq!(
            shutdown,
            Some(ShutdownReport::new(Some(Error::DeviceLost), Vec::new()))
        );
        assert_eq!(
            poll_backend_event(&events),
            Some(terminal(Error::DeviceLost))
        );
        assert_eq!(poll_backend_event(&events), None);
    }
}
