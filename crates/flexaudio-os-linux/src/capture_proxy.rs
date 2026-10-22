// Own external capture-source proxies and coordinate their asynchronous release.
use crate::capture_failures::CaptureCallbacks;
use crate::link_error::{decide, CoreFailure, Decision, GlobalRef, Globals, LinkAttempt};
use crate::proxy_release::{ProxyReleases, RequestWindow};
use flexaudio_core::{Error, ErrorContext, Operation, Result};
use pipewire::{
    core::CoreRc,
    proxy::{ProxyListener, ProxyT},
};
use std::{
    any::Any,
    cell::{Cell, RefCell},
    collections::HashMap,
    ops::Deref,
    rc::{Rc, Weak},
};

pub(crate) struct CoreOutcome {
    pub(crate) failure: CoreFailure,
    pub(crate) decision: Decision,
    pub(crate) attempt: Option<LinkAttempt>,
}

pub(crate) struct ProxyLifecycle {
    checkpoint: Box<dyn Fn() -> Result<i32>>,
    callbacks: CaptureCallbacks,
    pending: RefCell<ProxyReleases<Box<dyn Any>>>,
    globals: RefCell<Globals>,
    dormant_sources: RefCell<HashMap<u32, GlobalRef>>,
    attempts: RefCell<Vec<LinkAttempt>>,
    retired: RefCell<Vec<(i32, u32, Option<RequestWindow>)>>,
    errors: RefCell<Vec<(i32, CoreFailure, LinkAttempt)>>,
}

impl ProxyLifecycle {
    pub(crate) fn new(core: &CoreRc, callbacks: CaptureCallbacks) -> Rc<Self> {
        let core = core.downgrade();
        Self::with_checkpoint(callbacks, move || {
            core.upgrade().and_then(|core| core.sync(0).ok()).map(|seq| seq.seq())
                .ok_or_else(|| Error::Backend(
                    "PipeWire could not synchronize capture proxies; restart the audio session and retry capture.".into()
                ).with_context(ErrorContext::new(Operation::Link)))
        })
    }
    pub(crate) fn with_checkpoint(
        callbacks: CaptureCallbacks,
        checkpoint: impl Fn() -> Result<i32> + 'static,
    ) -> Rc<Self> {
        Rc::new(Self {
            checkpoint: Box::new(checkpoint),
            callbacks,
            pending: RefCell::new(ProxyReleases::default()),
            globals: RefCell::new(Globals::default()),
            dormant_sources: RefCell::new(HashMap::new()),
            attempts: RefCell::new(Vec::new()),
            retired: RefCell::new(Vec::new()),
            errors: RefCell::new(Vec::new()),
        })
    }
    pub(crate) fn checkpoint(&self) -> Result<i32> {
        (self.checkpoint)()
    }
    pub(crate) fn observe(&self, id: u32) {
        self.dormant_sources.borrow_mut().remove(&id);
        self.globals.borrow_mut().observe(id);
    }
    pub(crate) fn remove(&self, id: u32) {
        self.dormant_sources.borrow_mut().remove(&id);
        self.globals.borrow_mut().remove(id);
    }
    pub(crate) fn source_port_available(&self, node_id: u32) {
        self.dormant_sources.borrow_mut().remove(&node_id);
    }
    pub(crate) fn source_requires_negotiation(&self, node_id: u32, has_output_ports: bool) -> bool {
        if has_output_ports {
            self.source_port_available(node_id);
            return true;
        }
        !self
            .dormant_sources
            .borrow()
            .get(&node_id)
            .is_some_and(|node| self.globals.borrow().is_current(node))
    }
    pub(crate) fn link_attempt(
        &self,
        proxy_id: u32,
        requests: RequestWindow,
        output_node: u32,
        output_port: u32,
        input_node: u32,
        input_port: u32,
    ) -> Result<LinkAttempt> {
        let globals = self.globals.borrow();
        let missing = || {
            Error::Backend("PipeWire capture route is no longer available; retry capture.".into())
                .with_context(ErrorContext::new(Operation::Link))
        };
        let attempt = LinkAttempt {
            proxy_id,
            requests,
            output_node: globals.get(output_node).ok_or_else(missing)?,
            output_port: globals.get(output_port).ok_or_else(missing)?,
            input_node: globals.get(input_node).ok_or_else(missing)?,
            input_port: globals.get(input_port).ok_or_else(missing)?,
        };
        self.attempts.borrow_mut().push(attempt.clone());
        Ok(attempt)
    }
    pub(crate) fn defer(&self, resource: impl Any, routes: Vec<u32>) -> Result<()> {
        let checkpoint = self.checkpoint()?;
        self.pending
            .borrow_mut()
            .defer(checkpoint, routes, Box::new(resource));
        Ok(())
    }
    pub(crate) fn acknowledge(&self, checkpoint: i32) -> bool {
        let retired: Vec<_> = self
            .retired
            .borrow()
            .iter()
            .filter(|(seq, _, _)| *seq == checkpoint)
            .copied()
            .collect();
        self.retired
            .borrow_mut()
            .retain(|(seq, _, _)| *seq != checkpoint);
        // The release barrier follows destruction. Ordered core replies guarantee that
        // synchronous link-factory failures arrived before it; queued verifications keep
        // their own snapshot even after the proxy ownership record expires.
        self.attempts.borrow_mut().retain(|attempt| {
            !retired
                .iter()
                .any(|(_, id, window)| *id == attempt.proxy_id && *window == Some(attempt.requests))
        });
        // Drop outside the borrow: each proxy's destructor records its own release.
        let resources = self.pending.borrow_mut().acknowledge(checkpoint);
        let released = !resources.is_empty();
        drop(resources);
        released
    }
    pub(crate) fn is_deferred(&self, route: u32) -> bool {
        self.pending.borrow().is_deferred(route)
    }
    pub(crate) fn has_pending_errors(&self) -> bool {
        !self.errors.borrow().is_empty()
    }
    pub(crate) fn handle_error(&self, failure: CoreFailure) -> CoreOutcome {
        let released = crate::capture_error::destroy_target(&failure.message)
            .is_some_and(|id| self.pending.borrow().was_released(id, failure.sequence));
        let attempt = self
            .attempts
            .borrow()
            .iter()
            .find(|attempt| {
                attempt.proxy_id == failure.id && attempt.requests.contains(failure.sequence)
            })
            .cloned();
        let mut decision = decide(&failure, released, attempt.as_ref(), false);
        if decision == Decision::Continue {
            match (self.checkpoint(), attempt.as_ref()) {
                (Ok(checkpoint), Some(attempt)) => {
                    self.errors
                        .borrow_mut()
                        .push((checkpoint, failure.clone(), attempt.clone()))
                }
                _ => decision = Decision::Terminate,
            }
        }
        CoreOutcome {
            failure,
            decision,
            attempt,
        }
    }
    pub(crate) fn resolve_errors(&self, checkpoint: i32) -> Vec<CoreOutcome> {
        let mut pending = self.errors.borrow_mut();
        let mut ready = Vec::new();
        let mut index = 0;
        while index < pending.len() {
            if pending[index].0 == checkpoint {
                let (_, failure, attempt) = pending.swap_remove(index);
                let decision = decide(&failure, false, Some(&attempt), true);
                // A verified source with no output ports may remain registered. Do
                // not require it to negotiate until a new output port announces activity.
                if decision
                    == Decision::Recoverable(
                        crate::capture_error::CoreErrorDisposition::VanishedLink,
                    )
                    && self.globals.borrow().is_current(&attempt.output_node)
                {
                    self.dormant_sources
                        .borrow_mut()
                        .insert(attempt.output_node.id, attempt.output_node.clone());
                }
                ready.push(CoreOutcome {
                    failure,
                    decision,
                    attempt: Some(attempt),
                });
            } else {
                index += 1;
            }
        }
        ready
    }
    fn released(&self, proxy_id: u32, before: Option<i32>, creation: Option<RequestWindow>) {
        if let Ok(after) = self.checkpoint() {
            self.retired.borrow_mut().push((after, proxy_id, creation));
            if let Some(before) = before {
                self.pending
                    .borrow_mut()
                    .expect_destroy(proxy_id, RequestWindow { before, after });
            }
        }
        // A failed sync supplies no destroy evidence, so errors remain fatal.
    }
}

pub(crate) struct CaptureProxy<P: ProxyT> {
    // Field order: remove the callback, drop the proxy, then record the destroy's end barrier.
    _listener: ProxyListener,
    proxy: P,
    release: ReleaseOnDrop,
    pub(crate) attempt: Option<LinkAttempt>,
}

impl<P: ProxyT> CaptureProxy<P> {
    pub(crate) fn new(proxy: P, owner: &Rc<ProxyLifecycle>) -> Self {
        let proxy_id = proxy.upcast_ref().id();
        let removed = Rc::new(Cell::new(false));
        let removed_for_callback = removed.clone();
        let callbacks = owner.callbacks.clone();
        let listener = proxy
            .upcast_ref()
            .add_listener_local()
            .removed(move || callbacks.run(|| removed_for_callback.set(true)))
            .register();
        Self {
            _listener: listener,
            proxy,
            release: ReleaseOnDrop {
                proxy_id,
                removed,
                before: Cell::new(None),
                creation: None,
                owner: Rc::downgrade(owner),
            },
            attempt: None,
        }
    }
    pub(crate) fn with_attempt(mut self, attempt: LinkAttempt) -> Self {
        self.release.creation = Some(attempt.requests);
        self.attempt = Some(attempt);
        self
    }
    pub(crate) fn matches(&self, attempt: &LinkAttempt) -> bool {
        self.attempt
            .as_ref()
            .is_some_and(|own| own.proxy_id == attempt.proxy_id && own.requests == attempt.requests)
    }
}
impl<P: ProxyT> Drop for CaptureProxy<P> {
    fn drop(&mut self) {
        // PipeWire proxy.c sends core.destroy only when removed is false. Once
        // server removal arrived, dropping the proxy is local-only: do not invent
        // destroy evidence for that case. Bracket actual requests against id reuse.
        if !self.release.removed.get() {
            self.release.before.set(
                self.release
                    .owner
                    .upgrade()
                    .and_then(|owner| owner.checkpoint().ok()),
            );
        }
    }
}
impl<P: ProxyT> Deref for CaptureProxy<P> {
    type Target = P;
    fn deref(&self) -> &P {
        &self.proxy
    }
}
struct ReleaseOnDrop {
    proxy_id: u32,
    removed: Rc<Cell<bool>>,
    before: Cell<Option<i32>>,
    creation: Option<RequestWindow>,
    owner: Weak<ProxyLifecycle>,
}
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.upgrade() {
            owner.released(self.proxy_id, self.before.get(), self.creation);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{capture_error::CoreErrorDisposition, readiness::Readiness};
    use std::sync::{mpsc, Arc, Mutex};

    #[test]
    fn queued_link_verification_survives_proxy_release_acknowledgement() {
        let (tx, _rx) = mpsc::channel();
        let callbacks = CaptureCallbacks::new(
            Rc::new(Readiness::new(tx, true)),
            Arc::new(Mutex::new(Default::default())),
            || {},
        );
        let sequence = Cell::new(100);
        let owner = ProxyLifecycle::with_checkpoint(callbacks, move || {
            let next = sequence.get();
            sequence.set(next + 1);
            Ok(next)
        });
        for id in [51, 52, 61, 62] {
            owner.observe(id);
        }
        let requests = RequestWindow {
            before: 20,
            after: 22,
        };
        owner
            .link_attempt(5, requests, 51, 52, 61, 62)
            .expect("owned attempt");
        owner.released(5, None, Some(requests));
        let outcome = owner.handle_error(CoreFailure {
            id: 5,
            sequence: 21,
            status: -22,
            message: "link-factory: unknown output port 52".into(),
        });
        assert_eq!(outcome.decision, Decision::Continue);
        owner.acknowledge(100);
        assert!(owner.attempts.borrow().is_empty());
        owner.remove(52);
        let outcomes = owner.resolve_errors(101);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(
            outcomes[0].decision,
            Decision::Recoverable(CoreErrorDisposition::VanishedLink)
        );
        assert!(!owner.has_pending_errors());
    }
}
