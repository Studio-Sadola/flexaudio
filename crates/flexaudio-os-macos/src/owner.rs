//! Checked control-thread shutdown shared by the process and system adapters.
use std::collections::VecDeque;
use std::sync::atomic::AtomicBool;
use std::thread::JoinHandle;

use flexaudio_core::{Error, ErrorContext, Event, Operation, Result, ShutdownReport};

use crate::probe::PublicationGate;
use crate::terminal::TerminalFailure;

pub(crate) fn join_owner(handle: JoinHandle<Result<()>>) -> Result<()> {
    match handle.join() {
        Ok(result) => result.map_err(|error| match error {
            // Every native cleanup in this group already retains Stop context.
            Error::Multiple(_) => error,
            error => error.with_context(ErrorContext::new(Operation::Stop)),
        }),
        Err(_) => Err(Error::Backend("capture owner thread panicked".into())
            .with_context(ErrorContext::new(Operation::Join))),
    }
}

pub(crate) fn stop_owner(
    publication: &PublicationGate,
    stop: &AtomicBool,
    handle: Option<JoinHandle<Result<()>>>,
) -> Vec<Error> {
    let mut cleanup = Vec::new();
    if publication.cancel(stop).is_err() {
        cleanup.push(
            Error::Backend("capture publication gate poisoned".into())
                .with_context(ErrorContext::new(Operation::Stop)),
        );
    }
    if let Some(error) = handle.and_then(|handle| join_owner(handle).err()) {
        match error {
            // The owner's result contains only native cleanup failures.
            Error::Multiple(group) => {
                cleanup.push(group.primary().clone());
                cleanup.extend(group.secondary().cloned());
            }
            error => cleanup.push(error),
        }
    }
    cleanup
}

#[derive(Default)]
pub(crate) struct OwnerShutdown {
    report: Option<ShutdownReport>,
    events: VecDeque<Event>,
}

impl OwnerShutdown {
    pub(crate) fn reset(&mut self) {
        self.report = None;
        self.events.clear();
    }

    pub(crate) fn finish(
        &mut self,
        publication: &PublicationGate,
        stop: &AtomicBool,
        terminal: &TerminalFailure,
        handle: Option<JoinHandle<Result<()>>>,
    ) -> Result<()> {
        if self.report.is_none() {
            let cleanup = stop_owner(publication, stop, handle);
            self.events.extend(
                cleanup
                    .iter()
                    .cloned()
                    .map(|error| Event::ShutdownError { error }),
            );
            self.report = Some(ShutdownReport::new(terminal.error(), cleanup));
        }
        self.report
            .as_ref()
            .expect("completed owner shutdown")
            .result()
    }

    pub(crate) fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }
}
