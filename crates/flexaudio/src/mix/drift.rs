//! System clock drift correction.
use super::*;
/// Fine resampler for the system lane (linear interpolation stitcher).
///
/// Use mic as the reference clock (it is natural to align the recording timeline with the
/// person's voice on the mic side). Read only the system FIFO at ratio r using linear
/// interpolation to compensate for the rate difference between child clocks. Keep only
/// the fractional read position as state.
pub(super) struct LinearStitcher {
    /// Fractional read position from the FIFO's first frame (in frames, [0, 1)).
    pub(super) frac: f64,
}

impl LinearStitcher {
    pub(super) fn new() -> Self {
        Self { frac: 0.0 }
    }

    /// Number of output frames that can be interpolated at `ratio` when the FIFO has
    /// `fifo_frames` frames. The zero-based k-th output uses the two frames around
    /// position `frac + k×ratio`, so output is possible only while that position does
    /// not exceed the final frame F-1 (the exact final frame is valid because the right-hand
    /// interpolation term has zero weight).
    pub(super) fn producible(&self, fifo_frames: usize, ratio: f64) -> usize {
        if fifo_frames == 0 {
            return 0;
        }
        let span = (fifo_frames - 1) as f64 - self.frac;
        if span < 0.0 {
            return 0;
        }
        (span / ratio) as usize + 1
    }

    /// Read `out_frames` from the system FIFO using linear interpolation at `ratio`,
    /// appending to `out` in interleaved form. Discard consumed frames from the FIFO and
    /// carry the fractional position forward so phase remains continuous across block
    /// boundaries. The caller must ensure `out_frames <= producible(...)`.
    pub(super) fn pull(
        &mut self,
        fifo: &mut Vec<f32>,
        ratio: f64,
        out_frames: usize,
        out: &mut Vec<f32>,
    ) {
        let ch = CHANNELS as usize;
        let frames = fifo.len() / ch;
        debug_assert!(out_frames <= self.producible(frames, ratio));
        for k in 0..out_frames {
            let pos = self.frac + k as f64 * ratio;
            let left = pos as usize;
            // Clamp the right edge only when the position is exactly on the final frame
            // (the weight is then zero, so the interpolation result is unchanged).
            let right = (left + 1).min(frames - 1);
            let w = (pos - left as f64) as f32;
            for c in 0..ch {
                let a = fifo[left * ch + c];
                let b = fifo[right * ch + c];
                out.push(a + w * (b - a));
            }
        }
        // Compute the next read position. Discard consumed whole frames and keep only
        // the fractional part.
        let end = self.frac + out_frames as f64 * ratio;
        let consumed = (end as usize).min(frames);
        self.frac = end - consumed as f64;
        fifo.drain(..consumed * ch);
    }

    /// Reset phase when the starvation path flushes the entire system FIFO.
    pub(super) fn reset(&mut self) {
        self.frac = 0.0;
    }
}

/// Feedback controller that sets read ratio r from the FIFO level difference (EMA + P
/// control + slew).
///
/// Every [`DRIFT_UPDATE_INTERVAL_SAMPLES`] of mixed output, take an exponential moving
/// average of the post-consumption FIFO level difference (mic - system). If the
/// difference is negative (system is accumulating), increase r to consume it faster; if
/// positive, decrease r. P control is sufficient because the level difference itself
/// integrates the rate difference, so proportional correction balances it at a finite
/// level.
pub(super) struct DriftController {
    /// Current read ratio r, clamped around 1.0 by ±[`DRIFT_RATIO_LIMIT`].
    pub(super) ratio: f64,
    /// Exponential moving average of the level difference (mic_len - system_len), in
    /// f32 samples.
    pub(super) ema_diff: f64,
    /// Mixed output samples since the last update.
    pub(super) pending_samples: usize,
}

impl DriftController {
    pub(super) fn new() -> Self {
        Self {
            ratio: 1.0,
            ema_diff: 0.0,
            pending_samples: 0,
        }
    }

    /// Record mixed output progress and update the ratio once it reaches
    /// [`DRIFT_UPDATE_INTERVAL_SAMPLES`]. Pass post-consumption levels; pre-consumption
    /// levels would include the samples consumed in this iteration in the difference.
    pub(super) fn on_output(&mut self, samples: usize, mic_len: usize, system_len: usize) {
        self.pending_samples += samples;
        if self.pending_samples >= DRIFT_UPDATE_INTERVAL_SAMPLES {
            self.pending_samples = 0;
            self.update(mic_len, system_len);
        }
    }

    /// Update the EMA of the level difference and adjust the ratio by one step using P
    /// control + slew.
    pub(super) fn update(&mut self, mic_len: usize, system_len: usize) {
        let diff = mic_len as f64 - system_len as f64;
        self.ema_diff += DRIFT_EMA_ALPHA * (diff - self.ema_diff);
        let target = (1.0 - DRIFT_GAIN * self.ema_diff)
            .clamp(1.0 - DRIFT_RATIO_LIMIT, 1.0 + DRIFT_RATIO_LIMIT);
        let step = (target - self.ratio).clamp(-DRIFT_SLEW_PER_UPDATE, DRIFT_SLEW_PER_UPDATE);
        self.ratio += step;
    }
}

/// Drift correction state (stitcher + ratio controller), held once per mixer thread.
pub(super) struct DriftCorrection {
    pub(super) stitcher: LinearStitcher,
    pub(super) controller: DriftController,
}

impl DriftCorrection {
    pub(super) fn new() -> Self {
        Self {
            stitcher: LinearStitcher::new(),
            controller: DriftController::new(),
        }
    }
}
