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
    // At most ten fixed lane/reason combinations from child capture and FIFOs.
    losses: Vec<AudioLoss>,
    clipped: bool,
}
impl Notices {
    pub(super) fn failed(&self) -> bool {
        self.failed.load(Ordering::SeqCst)
    }
    pub(super) fn retain_primary(&self, error: Error) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.primary.is_none() {
            state.primary = Some(error);
        }
        self.failed.store(true, Ordering::SeqCst);
    }
    pub(super) fn fail(&self, error: Error) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.primary.is_none() {
            state.primary = Some(error);
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
            *previous = combine_loss(loss, samples);
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
    pub(super) fn publish(&self, sink: &mut RawSink, mixed: &[f32]) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.primary.is_none() {
            sink.push(mixed, monotonic_now_ns());
        }
    }
}

fn combine_loss(loss: AudioLoss, samples: Option<NonZeroU64>) -> AudioLoss {
    match (loss.path(), loss.reason()) {
        (AudioPath::MixFifo { lane }, LossReason::MixFifoOverflow) => {
            AudioLoss::mix_fifo_overflow(lane, samples)
        }
        (AudioPath::Capture { lane }, LossReason::RawOverflow) => {
            AudioLoss::raw_overflow(lane, samples, loss.sample_rate(), loss.channels())
                .expect("validated native format")
        }
        (AudioPath::Capture { lane }, reason) => {
            let diagnostics = CaptureDiagnostics::new(loss.sample_rate(), loss.channels());
            match reason {
                LossReason::CorruptBuffer => diagnostics.record_corrupt_buffer(samples),
                LossReason::MalformedBuffer => diagnostics.record_malformed_buffer(samples),
                LossReason::CallbackRejected => diagnostics.record_callback_rejected(samples),
                _ => unreachable!("only capture rejection reports reach the Mix mailbox"),
            }
            let combined = diagnostics.drain().expect("validated native format")[0];
            match lane {
                Some(lane) => combined
                    .with_capture_lane(lane)
                    .expect("unattributed capture report"),
                None => combined,
            }
        }
        _ => unreachable!("only child capture and FIFO reports reach the Mix mailbox"),
    }
}
