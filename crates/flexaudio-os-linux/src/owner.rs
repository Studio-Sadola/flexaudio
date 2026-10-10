// Shared checked worker cleanup and control-thread failure reporting.
use flexaudio_core::{Error, ErrorContext, ErrorGroup, Event, Operation, Result};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

pub(crate) type BackendEvents = Arc<Mutex<VecDeque<Event>>>;

pub(crate) fn startup_error(events: &BackendEvents, message: String) -> Error {
    let queue = events.lock().unwrap_or_else(|poison| poison.into_inner());
    queue
        .iter()
        .find_map(|event| match event {
            Event::TerminalError { error } => Some(error.clone()),
            _ => None,
        })
        .unwrap_or_else(|| {
            Error::Backend(message).with_context(ErrorContext::new(Operation::Start))
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

pub(crate) fn finish_worker(
    handle: &mut Option<JoinHandle<()>>,
    shutdown: &mut Option<Result<()>>,
    events: &BackendEvents,
) -> Result<()> {
    if let Some(result) = shutdown {
        return result.clone();
    }
    let result = handle.take().map_or(Ok(()), join_worker);
    if let Err(error) = &result {
        push_backend_event(
            events,
            Event::ShutdownError {
                error: error.clone(),
            },
        );
    }
    *shutdown = Some(result.clone());
    result
}

pub(crate) fn push_backend_event(events: &BackendEvents, event: Event) {
    let mut queue = events.lock().unwrap_or_else(|poison| poison.into_inner());
    // At most one terminal failure and one cleanup failure per capture generation.
    if matches!(event, Event::TerminalError { .. })
        && queue
            .iter()
            .any(|event| matches!(event, Event::TerminalError { .. }))
    {
        return;
    }
    queue.push_back(event);
}

pub(crate) fn poll_backend_event(events: &BackendEvents) -> Option<Event> {
    events
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .pop_front()
}
