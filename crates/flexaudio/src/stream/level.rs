//! Latest-value input monitoring, shared with language adapters without a command round-trip.
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::{Error, Result};

/// The level of what the primary stream would deliver, including while paused.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Level {
    /// Maximum absolute sample after denoise, output conversion, gain and clipping.
    pub peak: f32,
    /// Root mean square of those same float samples.
    pub rms: f32,
    /// Monotonic nanoseconds since measurement, not hardware capture latency.
    pub age_ns: u64,
}

#[derive(Default)]
struct State {
    active: bool,
    generation: u64,
    value: Option<(f32, f32, Instant)>,
    terminal: Option<Error>,
}

/// Shared adapter access to the stream's single level cell; no bridge command is needed.
///
/// This adapter interface is internal and has no stability guarantee.
#[doc(hidden)]
#[derive(Clone, Default)]
pub struct LevelReader(Arc<Mutex<State>>);

impl LevelReader {
    /// Copy the latest value without consuming it; retain typed terminal failures.
    pub fn poll_level(&self) -> Result<Option<Level>> {
        let state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(error) = &state.terminal {
            return Err(error.clone());
        }
        Ok(state.value.map(|(peak, rms, measured)| Level {
            peak,
            rms,
            age_ns: u64::try_from(measured.elapsed().as_nanos()).unwrap_or(u64::MAX),
        }))
    }

    /// Clear the level synchronously when an adapter requests pause.
    pub fn clear(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).value = None;
    }

    /// Prevent late publication as soon as an adapter requests stop.
    pub fn close(&self) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.active = false;
        state.value = None;
    }

    pub(super) fn start(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).active = true;
    }

    pub(super) fn replace_generation(&self, generation: u64) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.generation = generation;
        state.value = None;
    }

    pub(super) fn fail(&self, error: Error) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.active = false;
        state.value = None;
        state.terminal = Some(error);
    }

    pub(super) fn publish(&self, generation: u64, peak: f32, rms: f32) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if state.active && state.generation == generation {
            state.value = Some((peak, rms, Instant::now()));
        }
    }
}

impl super::Stream {
    /// Latest primary level, without consuming it. Also updates while paused.
    ///
    /// Measures the float samples the stream would deliver after denoise/conversion/gain.
    /// Gain zero meters zero. Pause clears the cell, then any completed block (including
    /// queued input) can refresh it. Repeated reads retain the value with increasing age.
    /// Returns None before the first measurement, after stop, or on terminal failure;
    /// inspect [`Self::terminal_error`] for the retained failure.
    pub fn poll_level(&self) -> Option<Level> {
        self.shared.level.poll_level().unwrap_or(None)
    }

    /// Share the same level cell with a language adapter before moving the stream.
    #[doc(hidden)]
    pub fn level_reader(&self) -> LevelReader {
        self.shared.level.clone()
    }
}
