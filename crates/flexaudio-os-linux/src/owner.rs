// Shared checked worker cleanup and control-thread failure reporting.
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
) {
    if shutdown.is_some() {
        return;
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
    let primary = events
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .primary
        .clone();
    *shutdown = Some(ShutdownReport::new(
        primary,
        result.err().into_iter().collect(),
    ));
}

pub(crate) fn push_backend_event(events: &BackendEvents, event: Event) {
    let mut queue = events.lock().unwrap_or_else(|poison| poison.into_inner());
    // At most one terminal failure and one cleanup failure per capture generation.
    if let Event::TerminalError { error } = &event {
        if queue.primary.is_some() {
            return;
        }
        queue.primary = Some(error.clone());
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
