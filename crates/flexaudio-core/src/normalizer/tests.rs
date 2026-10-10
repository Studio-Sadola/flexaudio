use super::*;
use std::f32::consts::PI;

/// Helper for the default output ({48000, 2}).
fn default_out() -> OutputFormat {
    OutputFormat::default()
}

// --- Value-check helpers (verify amplitude and frequency preservation) ---

/// RMS of a sample sequence (linear). For a sine wave, this is A/√2 for amplitude A.
fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = samples.iter().map(|&x| (x as f64) * (x as f64)).sum();
    (sum_sq / samples.len() as f64).sqrt() as f32
}

/// Count zero crossings from positive to negative or negative to positive. There are two
/// crossings per cycle, so estimated frequency = (crossings / 2) / seconds. Pass the middle
/// section to avoid transients at the start and end.
fn zero_crossings(samples: &[f32]) -> usize {
    let mut crossings = 0;
    for w in samples.windows(2) {
        // Count strict sign changes only (ignore exact zero).
        if (w[0] < 0.0 && w[1] >= 0.0) || (w[0] >= 0.0 && w[1] < 0.0) {
            crossings += 1;
        }
    }
    crossings
}

// --- InnerProcessor (denoise hook equivalent) and stop flush ---

/// Test processor: doubles every sample (has no trailing tail).
struct DoubleProcessor;
impl InnerProcessor for DoubleProcessor {
    fn process(&mut self, s: &mut [f32]) {
        for x in s.iter_mut() {
            *x *= 2.0;
        }
    }
    fn flush(&mut self) -> Vec<f32> {
        Vec::new()
    }
}

/// Test processor: fixed delay line holding `hold` samples (simulates a denoise delay line).
/// Output is delayed by `hold` samples (the first `hold` are silent). Returns the final
/// `hold` samples from `flush`.
struct DelayProcessor {
    held: Vec<f32>,
}
impl DelayProcessor {
    fn new(hold: usize) -> Self {
        Self {
            held: vec![0.0; hold],
        }
    }
}
impl InnerProcessor for DelayProcessor {
    fn process(&mut self, s: &mut [f32]) {
        self.held.extend_from_slice(s);
        let n = s.len();
        s.copy_from_slice(&self.held[..n]);
        self.held.drain(..n);
    }
    fn flush(&mut self) -> Vec<f32> {
        std::mem::take(&mut self.held)
    }
}

#[path = "tests/conversion.rs"]
mod conversion;

#[path = "tests/clock.rs"]
mod clock;

#[path = "tests/processor.rs"]
mod processor;
