// Pure fan-in error policy and registry-incarnation ownership evidence.
use crate::capture_error::{classify_core_error, CoreErrorDisposition};
use crate::proxy_release::RequestWindow;
use std::{cell::Cell, collections::HashMap, rc::Rc};

#[derive(Clone, Debug)]
pub(crate) struct GlobalRef {
    pub(crate) id: u32,
    alive: Rc<Cell<bool>>,
}

#[derive(Default)]
pub(crate) struct Globals(HashMap<u32, GlobalRef>);

impl Globals {
    pub(crate) fn observe(&mut self, id: u32) {
        self.remove(id);
        self.0.insert(
            id,
            GlobalRef {
                id,
                alive: Rc::new(Cell::new(true)),
            },
        );
    }
    pub(crate) fn remove(&mut self, id: u32) {
        if let Some(global) = self.0.remove(&id) {
            global.alive.set(false);
        }
    }
    pub(crate) fn is_current(&self, global: &GlobalRef) -> bool {
        self.0
            .get(&global.id)
            .is_some_and(|current| Rc::ptr_eq(&current.alive, &global.alive))
    }
    pub(crate) fn get(&self, id: u32) -> Option<GlobalRef> {
        self.0.get(&id).cloned()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct LinkAttempt {
    pub(crate) proxy_id: u32,
    pub(crate) requests: RequestWindow,
    pub(crate) output_node: GlobalRef,
    pub(crate) output_port: GlobalRef,
    pub(crate) input_node: GlobalRef,
    pub(crate) input_port: GlobalRef,
}

#[derive(Clone, Debug)]
pub(crate) struct CoreFailure {
    pub(crate) id: u32,
    pub(crate) sequence: i32,
    pub(crate) status: i32,
    pub(crate) message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    Continue,
    Recoverable(CoreErrorDisposition),
    Terminate,
}

// Both native error and barrier callbacks use this decision. A failed link is
// recoverable only after a subsequent registry barrier proves its source vanished.
pub(crate) fn decide(
    failure: &CoreFailure,
    released: bool,
    attempt: Option<&LinkAttempt>,
    synchronized: bool,
) -> Decision {
    if classify_core_error(failure.id, failure.status, &failure.message, |_| released)
        == CoreErrorDisposition::ReleasedProxy
    {
        return Decision::Recoverable(CoreErrorDisposition::ReleasedProxy);
    }
    let Some(attempt) = attempt else {
        return Decision::Terminate;
    };
    let expected = format!(
        "link-factory: unknown output port {}",
        attempt.output_port.id
    );
    if failure.id != attempt.proxy_id
        || !attempt.requests.contains(failure.sequence)
        || failure.status != -22
        || failure.message != expected
    {
        return Decision::Terminate;
    }
    if !synchronized {
        return Decision::Continue;
    }
    if !attempt.input_node.alive.get() || !attempt.input_port.alive.get() {
        return Decision::Terminate;
    }
    if !attempt.output_node.alive.get() || !attempt.output_port.alive.get() {
        return Decision::Recoverable(CoreErrorDisposition::VanishedLink);
    }
    Decision::Terminate
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Globals, LinkAttempt, CoreFailure) {
        let mut globals = Globals::default();
        for id in [51, 52, 61, 62] {
            globals.observe(id);
        }
        let attempt = LinkAttempt {
            proxy_id: 5,
            requests: RequestWindow {
                before: 20,
                after: 22,
            },
            output_node: globals.get(51).expect("output node"),
            output_port: globals.get(52).expect("output port"),
            input_node: globals.get(61).expect("input node"),
            input_port: globals.get(62).expect("input port"),
        };
        let error = CoreFailure {
            id: 5,
            sequence: 21,
            status: -22,
            message: "link-factory: unknown output port 52".into(),
        };
        (globals, attempt, error)
    }
    #[test]
    fn vanished_link_target_is_verified_then_recovered() {
        let (mut globals, attempt, error) = fixture();
        assert_eq!(
            decide(&error, false, Some(&attempt), false),
            Decision::Continue
        );
        globals.remove(52);
        assert_eq!(
            decide(&error, false, Some(&attempt), true),
            Decision::Recoverable(CoreErrorDisposition::VanishedLink)
        );
    }
    #[test]
    fn surviving_targets_permissions_foreign_proxies_and_capture_loss_are_fatal() {
        let (mut globals, attempt, error) = fixture();
        assert_eq!(
            decide(&error, false, Some(&attempt), true),
            Decision::Terminate
        );
        globals.remove(52);
        for status in [-1, -13, -2, -32] {
            assert_eq!(
                decide(
                    &CoreFailure {
                        status,
                        ..error.clone()
                    },
                    false,
                    Some(&attempt),
                    false
                ),
                Decision::Terminate
            );
        }
        assert_eq!(decide(&error, false, None, true), Decision::Terminate);
        for (id, sequence, message) in [
            (0, 21, error.message.as_str()),
            (6, 21, error.message.as_str()),
            (5, 23, error.message.as_str()),
            (5, 21, "link-factory: unknown output port 53"),
            (5, 21, "link-factory: unknown output port 052"),
            (5, 21, "link-factory: unknown input port 62"),
        ] {
            assert_eq!(
                decide(
                    &CoreFailure {
                        id,
                        sequence,
                        message: message.into(),
                        ..error.clone()
                    },
                    false,
                    Some(&attempt),
                    true
                ),
                Decision::Terminate
            );
        }
        globals.remove(62);
        assert_eq!(
            decide(&error, false, Some(&attempt), true),
            Decision::Terminate
        );
    }
    #[test]
    fn global_id_reuse_does_not_resurrect_the_failed_link_target() {
        let (mut globals, attempt, error) = fixture();
        globals.remove(52);
        globals.observe(52);
        assert_eq!(
            decide(&error, false, Some(&attempt), true),
            Decision::Recoverable(CoreErrorDisposition::VanishedLink)
        );
        assert!(globals.get(52).expect("new port").alive.get());
    }
    #[test]
    fn a_reused_proxy_id_cannot_borrow_an_older_destroy_request() {
        use crate::proxy_release::ProxyReleases;
        let mut releases = ProxyReleases::<()>::default();
        releases.expect_destroy(
            5,
            RequestWindow {
                before: 44,
                after: 46,
            },
        );
        for (sequence, expected) in [
            (
                45,
                Decision::Recoverable(CoreErrorDisposition::ReleasedProxy),
            ),
            (47, Decision::Terminate),
        ] {
            let failure = CoreFailure {
                id: 0,
                sequence,
                status: -2,
                message: "unknown resource 5 op:7".into(),
            };
            assert_eq!(
                decide(&failure, releases.was_released(5, sequence), None, false),
                expected
            );
        }
    }

    #[test]
    fn native_dispatch_preserves_the_policy_decision_and_diagnostic_tuple() {
        use crate::{capture_proxy::CoreOutcome, owner::poll_backend_event, readiness::Readiness};
        use flexaudio_core::Event;
        use std::{
            collections::VecDeque,
            sync::{mpsc, Arc, Mutex},
        };
        for decision in [
            Decision::Continue,
            Decision::Recoverable(CoreErrorDisposition::VanishedLink),
            Decision::Terminate,
        ] {
            let (_, _, failure) = fixture();
            let (tx, rx) = mpsc::channel();
            let readiness = Readiness::new(tx, true);
            let events = Arc::new(Mutex::new(VecDeque::new()));
            let quit = Cell::new(false);
            let stopped = crate::capture_control::apply_core_outcome(
                CoreOutcome {
                    failure,
                    decision,
                    attempt: None,
                },
                &readiness,
                &events,
                || quit.set(true),
            );
            assert_eq!(stopped, decision == Decision::Terminate);
            assert_eq!(quit.get(), stopped);
            match decision {
                Decision::Continue => assert!(poll_backend_event(&events).is_none()),
                Decision::Recoverable(_) => {
                    let Some(Event::RecoverableError { error }) = poll_backend_event(&events)
                    else {
                        panic!("expected recovery event");
                    };
                    assert_eq!(error.diagnostic_details(), ["PipeWire core error: id=5 sequence=21 status=-22; link-factory: unknown output port 52"]);
                }
                Decision::Terminate => {
                    assert!(matches!(
                        poll_backend_event(&events),
                        Some(Event::TerminalError { .. })
                    ));
                    assert!(rx.try_recv().expect("startup failure").is_err());
                }
            }
        }
    }
}
