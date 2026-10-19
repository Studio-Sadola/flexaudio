//! Bounded off-callback notices and the single terminal publication gate.
use super::*;
use std::collections::VecDeque;

#[derive(Default)]
pub(super) struct Notices {
    state: Mutex<State>,
    failed: AtomicBool,
}
#[derive(Default)]
struct State {
    primary: Option<Error>,
    terminal_pending: bool,
    cleanup: Vec<Error>,
    events: VecDeque<Event>,
    // At most twelve fixed lane/reason combinations from child capture and FIFOs.
    losses: Vec<AudioLoss>,
    clipped: bool,
    discontinuity: bool,
}
impl State {
    // Reconciliation can observe the same child error at startup and shutdown.
    // Retain each distinct cause once in observation order through one path.
    fn retain_terminal(&mut self, error: Error) -> bool {
        let Some(previous) = self.primary.take() else {
            self.primary = Some(error);
            return true;
        };
        if previous == error {
            self.primary = Some(previous);
            return false;
        }
        let (primary, mut related) = match previous {
            Error::Multiple(group) => (
                group.primary().clone(),
                group.secondary().cloned().collect::<Vec<_>>(),
            ),
            primary => (primary, Vec::new()),
        };
        if error != primary && !related.contains(&error) {
            related.push(error);
        }
        self.primary = Some(if related.is_empty() {
            primary
        } else {
            let first = related.remove(0);
            Error::Multiple(flexaudio_core::ErrorGroup::new(primary, first, related))
        });
        false
    }
}

impl Notices {
    pub(super) fn poll_lifecycle(&self) -> Option<Event> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let index = state
            .events
            .iter()
            .position(crate::stream::is_helper_lifecycle)?;
        state.events.remove(index)
    }

    pub(super) fn take_lifecycle(&self) -> Vec<Event> {
        let mut events = Vec::new();
        while let Some(event) = self.poll_lifecycle() {
            events.push(event);
        }
        events
    }
    pub(super) fn failed(&self) -> bool {
        self.failed.load(Ordering::SeqCst)
    }
    pub(super) fn retain_primary(&self, error: Error) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.retain_terminal(error);
        self.failed.store(true, Ordering::SeqCst);
    }
    pub(super) fn fail(&self, error: Error) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.retain_terminal(error) {
            state.terminal_pending = true;
        }
        self.failed.store(true, Ordering::SeqCst);
    }
    pub(super) fn take_terminal(&self) -> Option<Event> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !std::mem::take(&mut state.terminal_pending) {
            return None;
        }
        state
            .primary
            .clone()
            .map(|error| Event::TerminalError { error })
    }
    pub(super) fn cleanup(&self, error: Error) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.cleanup.push(error.clone());
        state.events.push_back(Event::ShutdownError { error });
    }
    pub(super) fn queue_final(&self, event: Event) {
        match event {
            Event::AudioLoss { loss } => self.loss(loss),
            Event::Clipped => self.clipped(),
            event => self
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .events
                .push_back(event),
        }
    }
    pub(super) fn clipped(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clipped = true;
    }
    pub(super) fn loss(&self, loss: AudioLoss) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(previous) = state.losses.iter_mut().find(|previous| {
            previous.path() == loss.path()
                && previous.reason() == loss.reason()
                && previous.sample_rate() == loss.sample_rate()
                && previous.channels() == loss.channels()
        }) {
            let samples = previous
                .samples()
                .zip(loss.samples())
                .and_then(|(a, b)| a.get().checked_add(b.get()))
                .and_then(NonZeroU64::new);
            *previous = loss.with_sample_count(samples);
        } else {
            state.losses.push(loss);
        }
    }
    pub(super) fn poll(&self) -> Option<Event> {
        if let Some(event) = self.take_terminal() {
            return Some(event);
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(event) = state.events.pop_front() {
            return Some(event);
        }
        if !state.losses.is_empty() {
            return Some(Event::AudioLoss {
                loss: state.losses.remove(0),
            });
        }
        if std::mem::take(&mut state.clipped) {
            return Some(Event::Clipped);
        }
        None
    }
    pub(super) fn report(&self) -> ShutdownReport {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ShutdownReport::new(state.primary.clone(), state.cleanup.clone())
    }
    pub(super) fn discontinuity(&self) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .discontinuity = true;
    }
    pub(super) fn publish(&self, sink: &mut RawSink, mixed: &[f32]) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.primary.is_none() && !mixed.is_empty() {
            if std::mem::take(&mut state.discontinuity) {
                sink.diagnostics().mark_discontinuity();
            }
            sink.push(mixed, monotonic_now_ns());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn later_terminal_causes_are_retained_in_the_error_group() {
        let notices = Notices::default();
        notices.fail(Error::DeviceLost);
        notices.fail(Error::Backend("second lane failed".into()));
        notices.retain_primary(Error::InvalidState("third lane failed".into()));
        notices.retain_primary(Error::DeviceLost);
        let report = notices.report();
        match report.primary().unwrap() {
            Error::Multiple(group) => {
                assert_eq!(group.primary().root(), &Error::DeviceLost);
                assert_eq!(group.secondary().count(), 2);
            }
            _ => panic!("later terminal causes must be retained"),
        }
        assert!(report.result().is_err());
        assert!(notices.take_terminal().is_some());
        assert!(notices.take_terminal().is_none());
    }
}
