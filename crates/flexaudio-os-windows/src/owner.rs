//! One retained join outcome for both WASAPI backends.
use std::collections::VecDeque;
use std::thread::JoinHandle;

use flexaudio_core::{Error, ErrorContext, Event, Operation, Result, ShutdownReport};

pub(crate) fn join_owner(handle: JoinHandle<Result<()>>) -> Result<()> {
    match handle.join() {
        Ok(result) => {
            result.map_err(|error| error.with_context(ErrorContext::new(Operation::Stop)))
        }
        Err(_) => Err(Error::Backend("capture owner thread panicked".into())
            .with_context(ErrorContext::new(Operation::Join))),
    }
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

    pub(crate) fn finish(&mut self, handle: Option<JoinHandle<Result<()>>>) -> Result<()> {
        if self.report.is_none() {
            let cleanup: Vec<_> = handle
                .and_then(|handle| join_owner(handle).err())
                .into_iter()
                .collect();
            self.events.extend(
                cleanup
                    .iter()
                    .cloned()
                    .map(|error| Event::ShutdownError { error }),
            );
            self.report = Some(ShutdownReport::new(None, cleanup));
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
