//! Startup boundaries for the N-API push adapter; events live in the shared FIFO.
use super::*;

#[derive(Default)]
pub(super) struct StartupOrder {
    enabled: bool,
    pending: VecDeque<StartupNotice>,
}
struct StartupNotice {
    boundaries: [u64; 3],
}
#[derive(Clone, Copy)]
pub(super) enum Tap {
    Primary = 0,
    Secondary = 1,
    Capture = 2,
}
impl StartupOrder {
    pub(super) fn enable(&mut self) {
        self.enabled = true;
    }
    pub(super) fn register(&mut self, shared: &SharedState) {
        if !self.enabled {
            return;
        }
        let canonical = |counter: &AtomicU64, rate: u32| {
            u64::try_from(u128::from(counter.load(Ordering::SeqCst)) * 48_000 / u128::from(rate))
                .unwrap_or(u64::MAX)
        };
        self.pending.push_back(StartupNotice {
            boundaries: [
                canonical(
                    &shared.primary_frame_index,
                    shared.output_format.sample_rate,
                ),
                canonical(
                    &shared.secondary_frame_index,
                    shared.secondary_format.map_or(48_000, |f| f.sample_rate),
                ),
                shared.capture_frame_index.load(Ordering::SeqCst),
            ],
        });
    }
    #[cfg(test)]
    pub(super) fn pending_count(&self) -> usize {
        self.pending.len()
    }
    fn blocks(&self, tap: Tap, index: u64) -> bool {
        self.pending
            .iter()
            .any(|notice| index >= notice.boundaries[tap as usize])
    }
    pub(super) fn claim(&mut self) {
        self.pending.pop_front();
    }
}

/// Cache at most one chunk while the N-API push adapter awaits its startup event.
/// Pull APIs use this same path without waiting for event polling.
pub(super) fn poll<T>(
    cached: &mut Option<T>,
    pop: impl FnOnce() -> Option<T>,
    index: impl FnOnce(&T) -> u64,
    shared: &SharedState,
    tap: Tap,
    ordered: bool,
) -> Option<T> {
    let _delivery = shared.delivery.lock().unwrap_or_else(|e| e.into_inner());
    if shared.terminal.is_failed() {
        return None;
    }
    if cached.is_none() {
        *cached = pop();
    }
    let chunk = cached.as_ref()?;
    if ordered
        && shared
            .startup_order
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .blocks(tap, index(chunk))
    {
        return None;
    }
    cached.take()
}
