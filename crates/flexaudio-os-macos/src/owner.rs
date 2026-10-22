//! Checked control-thread shutdown shared by the process and system adapters.
use std::collections::VecDeque;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;

use flexaudio_core::{Error, ErrorContext, Event, Operation, Result, ShutdownReport};

use crate::cleanup::CleanupLedger;
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
    pub(crate) cleanup: Arc<CleanupLedger>,
    events: VecDeque<Event>,
    lifecycle: VecDeque<Event>,
}

impl OwnerShutdown {
    /// Keep completed helper pairs when a new generation replaces its mailbox.
    pub(crate) fn retain_lifecycle(&mut self, events: Option<&mpsc::Receiver<Event>>) {
        if let Some(events) = events {
            self.lifecycle.extend(events.try_iter().filter(|event| {
                matches!(
                    event,
                    Event::SilenceProbeStarted | Event::SilenceProbeStopped { .. }
                )
            }));
        }
    }

    pub(crate) fn poll_lifecycle(&mut self) -> Option<Event> {
        self.lifecycle.pop_front()
    }
    pub(crate) fn reset(&mut self) {
        self.report = None;
        self.events.clear();
    }

    pub(crate) fn report(&self) -> Option<&ShutdownReport> {
        self.report.as_ref()
    }

    /// Return this startup attempt's outcome while retaining earlier generation diagnostics.
    pub(crate) fn retain_startup_report(&mut self, report: ShutdownReport) -> Result<()> {
        let report = self.with_generation_cleanup(report);
        let result = report.result();
        self.events.extend(
            report
                .primary()
                .cloned()
                .map(|error| Event::TerminalError { error }),
        );
        self.retain_report(report);
        result
    }

    fn with_generation_cleanup(&self, report: ShutdownReport) -> ShutdownReport {
        let mut cleanup = report.cleanup().to_vec();
        cleanup.extend(self.cleanup.take());
        ShutdownReport::new(report.primary().cloned(), cleanup)
    }

    fn retain_report(&mut self, report: ShutdownReport) {
        self.events.extend(
            report
                .cleanup()
                .iter()
                .cloned()
                .map(|error| Event::ShutdownError { error }),
        );
        self.report = Some(match self.report.take() {
            Some(previous) => {
                let mut cleanup = previous.cleanup().to_vec();
                cleanup.extend(report.cleanup().iter().cloned());
                ShutdownReport::new(previous.primary().cloned(), cleanup)
            }
            None => report,
        });
    }

    pub(crate) fn finish(
        &mut self,
        publication: &PublicationGate,
        stop: &AtomicBool,
        terminal: &TerminalFailure,
        handle: Option<JoinHandle<Result<()>>>,
    ) {
        if self.report.is_none() {
            let cleanup = stop_owner(publication, stop, handle);
            let report =
                self.with_generation_cleanup(ShutdownReport::new(terminal.error(), cleanup));
            self.retain_report(report);
        }
    }

    pub(crate) fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio_core::HelperStopReason;

    #[test]
    fn failed_restart_keeps_previous_report_and_returns_current_attempt() {
        for previous_primary in [None, Some(Error::DeviceLost)] {
            let mut shutdown = OwnerShutdown::default();
            let publication = PublicationGate::default();
            let stop = AtomicBool::new(false);
            let terminal = TerminalFailure::default();
            if let Some(primary) = previous_primary.clone() {
                terminal.record(primary);
            }
            shutdown.finish(
                &publication,
                &stop,
                &terminal,
                Some(std::thread::spawn(|| {
                    Err(Error::Backend("previous owner cleanup failed".into()))
                })),
            );
            let previous = shutdown.report().unwrap().clone();
            let cleanup = stop_owner(
                &publication,
                &stop,
                Some(std::thread::spawn(|| {
                    Err(Error::Backend("restart owner cleanup failed".into()))
                })),
            );
            let attempt = ShutdownReport::new(Some(Error::DeviceNotFound), cleanup.clone());
            assert_eq!(
                shutdown.retain_startup_report(attempt.clone()),
                attempt.result()
            );
            let retained = shutdown.report().unwrap().clone();
            assert_eq!(retained.primary(), previous_primary.as_ref());
            let expected_cleanup = [previous.cleanup(), cleanup.as_slice()].concat();
            assert_eq!(retained.cleanup(), expected_cleanup.as_slice());
            assert_eq!(retained.cleanup().len(), 2);
            shutdown.finish(&publication, &stop, &terminal, None);
            assert_eq!(shutdown.report(), Some(&retained));
            assert_eq!(
                std::iter::from_fn(|| shutdown.poll_event()).collect::<Vec<_>>(),
                vec![
                    Event::ShutdownError {
                        error: previous.cleanup()[0].clone()
                    },
                    Event::TerminalError {
                        error: Error::DeviceNotFound
                    },
                    Event::ShutdownError {
                        error: cleanup[0].clone()
                    },
                ],
            );
            shutdown.reset();
            shutdown.finish(&publication, &stop, &TerminalFailure::default(), None);
            assert!(shutdown.report().unwrap().cleanup().is_empty());
            assert!(shutdown.report().unwrap().primary().is_none());
        }
    }

    #[test]
    fn resetting_owner_preserves_unread_lifecycle_in_order() {
        let mut shutdown = OwnerShutdown::default();
        let (tx, rx) = mpsc::channel();
        let stopped = Event::SilenceProbeStopped {
            reason: HelperStopReason::StreamStopped,
        };
        tx.send(Event::SilenceProbeStarted).unwrap();
        tx.send(stopped.clone()).unwrap();
        assert_eq!(rx.try_recv(), Ok(Event::SilenceProbeStarted));
        shutdown.retain_lifecycle(Some(&rx));
        shutdown.reset();
        assert_eq!(shutdown.poll_lifecycle(), Some(stopped));
        assert!(shutdown.poll_lifecycle().is_none());
    }
}
