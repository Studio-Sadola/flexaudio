// Execute fan-in error decisions, readiness updates, and terminal effects in one path.
use crate::capture_failures::CaptureCallbacks;
use crate::capture_proxy::{CoreOutcome, ProxyLifecycle};
use crate::link_error::{CoreFailure, LinkAttempt};
use crate::owner::BackendEvents;
use crate::readiness::Readiness;
use crate::{capture_error, capture_failures, link_error, owner::push_backend_event};
use flexaudio_core::{Event, Result};
use std::rc::Rc;

#[derive(Clone)]
pub(crate) struct CaptureControl {
    lifecycle: Rc<ProxyLifecycle>,
    callbacks: CaptureCallbacks,
}

impl CaptureControl {
    pub(crate) fn new(lifecycle: Rc<ProxyLifecycle>, callbacks: CaptureCallbacks) -> Self {
        Self {
            lifecycle,
            callbacks,
        }
    }
    pub(crate) fn error(&self, failure: CoreFailure) {
        self.callbacks.run(|| {
            let outcome = self.lifecycle.handle_error(failure);
            self.callbacks
                .readiness
                .set_control_pending(self.lifecycle.has_pending_errors());
            self.apply(outcome);
        });
    }
    // Returns true if capture stopped. The caller's registry snapshot remains independent
    // of native proxy types, allowing the same executable path to run against fake effects.
    pub(crate) fn barrier(
        &self,
        sequence: i32,
        mut retire: impl FnMut(&LinkAttempt) -> Result<()>,
        reconcile: impl FnOnce() -> Result<()>,
    ) -> bool {
        let released = self.lifecycle.acknowledge(sequence);
        let outcomes = self.lifecycle.resolve_errors(sequence);
        let resolved = !outcomes.is_empty();
        for outcome in outcomes {
            let attempt = outcome.attempt.clone();
            if self.apply(outcome) {
                return true;
            }
            if let Some(attempt) = attempt {
                if let Err(error) = retire(&attempt) {
                    self.callbacks.fail(error);
                    return true;
                }
            }
        }
        if released || resolved {
            // Reconciliation already reports its typed failure before stopping capture.
            if reconcile().is_err() {
                return true;
            }
        }
        self.callbacks
            .readiness
            .set_control_pending(self.lifecycle.has_pending_errors());
        false
    }
    fn apply(&self, outcome: CoreOutcome) -> bool {
        apply_core_outcome(
            outcome,
            &self.callbacks.readiness,
            &self.callbacks.events,
            || self.callbacks.quit(),
        )
    }
}

// Returns true only when the pure owner policy terminates capture.
pub(crate) fn apply_core_outcome(
    outcome: CoreOutcome,
    readiness: &Readiness,
    events: &BackendEvents,
    quit: impl FnOnce(),
) -> bool {
    let disposition = match outcome.decision {
        link_error::Decision::Continue => return false,
        link_error::Decision::Recoverable(disposition) => disposition,
        link_error::Decision::Terminate => capture_error::CoreErrorDisposition::Fatal,
    };
    let phase = if readiness.is_reported() {
        capture_error::CapturePhase::Running
    } else {
        capture_error::CapturePhase::Setup
    };
    let failure = outcome.failure;
    let error = capture_error::core_error(
        failure.id,
        failure.sequence,
        failure.status,
        &failure.message,
        phase,
        disposition,
    );
    if outcome.decision == link_error::Decision::Terminate {
        capture_failures::report_control_failure(readiness, events, error);
        quit();
        true
    } else {
        push_backend_event(events, Event::RecoverableError { error });
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{link_error::CoreFailure, owner::poll_backend_event, proxy_release::RequestWindow};
    use std::{
        cell::Cell,
        collections::VecDeque,
        sync::{mpsc, Arc, Mutex},
    };

    struct Fixture {
        control: CaptureControl,
        lifecycle: Rc<ProxyLifecycle>,
        readiness: Rc<Readiness>,
        result: mpsc::Receiver<Result<()>>,
        events: BackendEvents,
        stopped: Rc<Cell<bool>>,
    }
    impl Fixture {
        fn new() -> Self {
            let sequence = Cell::new(100);
            Self::with_checkpoint(move || {
                let next = sequence.get();
                sequence.set(next + 1);
                Ok(next)
            })
        }
        fn with_checkpoint(checkpoint: impl Fn() -> Result<i32> + 'static) -> Self {
            let (tx, result) = mpsc::channel();
            let readiness = Rc::new(Readiness::new(tx, true));
            readiness.set_target_present(true);
            readiness.registry_synchronized();
            let events = Arc::new(Mutex::new(VecDeque::new()));
            let stopped = Rc::new(Cell::new(false));
            let stopped_for_callback = stopped.clone();
            let callbacks = CaptureCallbacks::new(readiness.clone(), events.clone(), move || {
                stopped_for_callback.set(true)
            });
            let lifecycle = ProxyLifecycle::with_checkpoint(callbacks.clone(), checkpoint);
            for id in [51, 52, 53, 61, 62] {
                lifecycle.observe(id);
            }
            lifecycle
                .link_attempt(
                    5,
                    RequestWindow {
                        before: 20,
                        after: 22,
                    },
                    51,
                    52,
                    61,
                    62,
                )
                .expect("owned link attempt");
            let control = CaptureControl::new(lifecycle.clone(), callbacks);
            Self {
                control,
                lifecycle,
                readiness,
                result,
                events,
                stopped,
            }
        }
        fn error(&self, status: i32) {
            self.control.error(CoreFailure {
                id: 5,
                sequence: 21,
                status,
                message: "link-factory: unknown output port 52".into(),
            });
        }
    }

    #[test]
    fn verified_last_source_disappearance_starts_silent_before_deadline() {
        for node_survives in [false, true] {
            let owner = Fixture::new();
            owner.error(-22);
            assert!(owner.lifecycle.has_pending_errors());
            assert_eq!(owner.result.try_recv(), Err(mpsc::TryRecvError::Empty));
            assert!(poll_backend_event(&owner.events).is_none());
            owner.lifecycle.remove(52);
            owner.lifecycle.remove(53);
            if !node_survives {
                owner.lifecycle.remove(51);
            }
            let retired = Cell::new(0);
            assert!(!owner.control.barrier(
                100,
                |_| {
                    retired.set(retired.get() + 1);
                    Ok(())
                },
                || {
                    owner.readiness.set_target_present(
                        node_survives && owner.lifecycle.source_requires_negotiation(51, false),
                    );
                    Ok(())
                }
            ));
            assert!(
                !owner.readiness.deadline_expired(),
                "verified source disappearance must not time out"
            );
            assert_eq!(owner.result.try_recv(), Ok(Ok(())));
            assert!(!owner.stopped.get());
            assert_eq!(retired.get(), 1);
            assert!(!owner.lifecycle.has_pending_errors());
            let Some(Event::RecoverableError { error }) = poll_backend_event(&owner.events) else {
                panic!("expected recovery");
            };
            assert_eq!(error.diagnostic_details(), ["PipeWire core error: id=5 sequence=21 status=-22; link-factory: unknown output port 52"]);
            assert!(poll_backend_event(&owner.events).is_none());
        }
    }
    #[test]
    fn surviving_link_target_terminates_with_original_error_after_barrier() {
        let owner = Fixture::new();
        owner.error(-22);
        assert!(!owner.control.barrier(
            99,
            |_| panic!("unrelated barrier"),
            || panic!("unrelated barrier")
        ));
        assert!(!owner.stopped.get());
        assert!(owner.control.barrier(
            100,
            |_| panic!("fatal route must not retire before termination"),
            || panic!("fatal route must not reconcile")
        ));
        assert!(owner.stopped.get());
        assert!(owner.result.try_recv().expect("startup failure").is_err());
        let Some(Event::TerminalError { error }) = poll_backend_event(&owner.events) else {
            panic!("expected terminal failure");
        };
        assert_eq!(error.diagnostic_details(), ["PipeWire core error: id=5 sequence=21 status=-22; link-factory: unknown output port 52"]);
        assert!(poll_backend_event(&owner.events).is_none());
    }
    #[test]
    fn permission_failure_terminates_without_scheduling_verification() {
        let owner = Fixture::new();
        owner.error(-13);
        assert!(owner.stopped.get());
        assert!(!owner.lifecycle.has_pending_errors());
        assert!(owner.result.try_recv().expect("startup failure").is_err());
        assert!(matches!(
            poll_backend_event(&owner.events),
            Some(Event::TerminalError { .. })
        ));
    }
    #[test]
    fn a_surviving_neighbor_still_requires_negotiation_after_benign_disappearance() {
        let owner = Fixture::new();
        owner.lifecycle.observe(71);
        owner.error(-22);
        owner.lifecycle.remove(52);
        owner.lifecycle.remove(53);
        assert!(!owner.control.barrier(
            100,
            |_| Ok(()),
            || {
                let dormant = owner.lifecycle.source_requires_negotiation(51, false);
                let neighbor = owner.lifecycle.source_requires_negotiation(71, true);
                assert!(!dormant);
                assert!(neighbor);
                owner.readiness.set_target_present(dormant || neighbor);
                Ok(())
            }
        ));
        assert_eq!(owner.result.try_recv(), Err(mpsc::TryRecvError::Empty));
        assert!(matches!(
            poll_backend_event(&owner.events),
            Some(Event::RecoverableError { .. })
        ));
        assert!(owner.readiness.deadline_expired());
        assert!(owner
            .result
            .try_recv()
            .expect("negotiation timeout")
            .is_err());
    }

    #[test]
    fn returning_ports_and_reused_node_ids_clear_verified_dormancy() {
        let owner = Fixture::new();
        owner.error(-22);
        owner.lifecycle.remove(52);
        owner.lifecycle.remove(53);
        owner.control.barrier(100, |_| Ok(()), || Ok(()));
        assert!(!owner.lifecycle.source_requires_negotiation(51, false));
        owner.lifecycle.source_port_available(51);
        assert!(owner.lifecycle.source_requires_negotiation(51, false));

        let owner = Fixture::new();
        owner.error(-22);
        owner.lifecycle.remove(52);
        owner.control.barrier(100, |_| Ok(()), || Ok(()));
        assert!(owner.lifecycle.source_requires_negotiation(51, true));
        assert!(owner.lifecycle.source_requires_negotiation(51, false));

        let owner = Fixture::new();
        owner.error(-22);
        owner.lifecycle.remove(52);
        owner.lifecycle.remove(53);
        owner.control.barrier(100, |_| Ok(()), || Ok(()));
        owner.lifecycle.remove(51);
        owner.lifecycle.observe(51);
        assert!(owner.lifecycle.source_requires_negotiation(51, false));
    }

    #[test]
    fn callback_panic_is_contained_and_terminates_through_the_shared_owner() {
        let owner = Fixture::new();
        owner
            .control
            .callbacks
            .run(|| panic!("synthetic callback panic"));
        assert!(owner.stopped.get());
        assert!(owner.result.try_recv().expect("callback failure").is_err());
        assert!(matches!(
            poll_backend_event(&owner.events),
            Some(Event::TerminalError { .. })
        ));
    }
    #[test]
    fn verification_sync_failure_is_terminal_and_retains_the_original_tuple() {
        let owner = Fixture::with_checkpoint(|| {
            Err(flexaudio_core::Error::Backend(
                "synthetic sync failure".into(),
            ))
        });
        owner.error(-22);
        assert!(owner.stopped.get());
        assert!(!owner.lifecycle.has_pending_errors());
        assert!(owner.result.try_recv().expect("startup failure").is_err());
        let Some(Event::TerminalError { error }) = poll_backend_event(&owner.events) else {
            panic!("expected terminal failure");
        };
        assert_eq!(error.diagnostic_details(), ["PipeWire core error: id=5 sequence=21 status=-22; link-factory: unknown output port 52"]);
    }
}
