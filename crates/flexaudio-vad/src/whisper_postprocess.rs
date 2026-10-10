//! Incremental port of whisper_vad_segments_from_probs, whisper.cpp lines 5211–5444,
//! pin 85a69493a601d4ff5a834064f7b7bac250bd8739. The full MIT notice is below.
//! The 98 ms split candidate follows the Silero lineage cited in the pinned source.
//!
//! Copyright (c) 2023-2026 The ggml authors
//!
//! Permission is hereby granted, free of charge, to any person obtaining a copy
//! of this software and associated documentation files (the "Software"), to deal
//! in the Software without restriction, including without limitation the rights
//! to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
//! copies of the Software, and to permit persons to whom the Software is
//! furnished to do so, subject to the following conditions:
//!
//! The above copyright notice and this permission notice shall be included in all
//! copies or substantial portions of the Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
//! IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
//! FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
//! AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
//! LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
//! OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
//! SOFTWARE.

use crate::whisper_params::{DerivedParams, FRAME, MAX_FRAMES};
use crate::{WhisperSpeechSegment, WhisperVadError, WhisperVadOperation, WhisperVadParams};

const MERGE_GAP: u64 = 3200;
const SPLIT_SILENCE_SAMPLES: u64 = 1568; // 98 ms at 16 kHz.

// loop_frame calls raw at most twice. Each raw can emit once from seal and
// once from resolve_neighbor; feed's final seal can emit once more. Each
// resolve_neighbor takes waiting before emitting, so it emits at most once.
pub(crate) const MAX_SEGMENTS_PER_FRAME: u64 = 2 * 2 + 1;
// EOF calls raw once (<=2), seal once (<=1), then emits waiting (<=1).
pub(crate) const MAX_FINISH_SEGMENTS: usize = 4;

#[derive(Debug, Clone, Copy)]
struct Group {
    start: u64,
    end: u64,
    padded_start: Option<u64>,
}

/// Probability-only processor for the pinned 16 kHz / 512-sample geometry.
///
/// Retains a candidate group and a waiting survivor, never the full probability history.
/// Finalization waits for the nearest surviving successor or EOF and has no latency bound.
/// Equivalence excludes the pin's signed-int overflow/undefined behavior near INT_MAX
/// logical samples (about 37 hours), including padded-end arithmetic. This port
/// uses u64 sample arithmetic and clamps padded EOF ends to logical length.
#[derive(Debug, Clone)]
pub struct WhisperVadPostProcessor {
    params: DerivedParams,
    frames: u64,
    finished: bool,
    active: bool,
    has_current: bool,
    start: u64,
    temp_end: u64,
    prev_end: u64,
    next_start: u64,
    candidate: Option<Group>,
    waiting: Option<Group>,
}

impl WhisperVadPostProcessor {
    /// Snapshot and validate the five upstream parameters.
    pub fn new(params: WhisperVadParams) -> Result<Self, WhisperVadError> {
        Ok(Self {
            params: params.derive()?,
            frames: 0,
            finished: false,
            active: false,
            has_current: false,
            start: 0,
            temp_end: 0,
            prev_end: 0,
            next_start: 0,
            candidate: None,
            waiting: None,
        })
    }

    /// Consume a complete validated feed and return immutable final segments.
    /// Invalid input leaves every counter and pending group unchanged.
    pub fn process(
        &mut self,
        probabilities: &[f32],
    ) -> Result<Vec<WhisperSpeechSegment>, WhisperVadError> {
        if self.finished {
            return Err(WhisperVadError::SessionFinished);
        }
        let count = u64::try_from(probabilities.len()).map_err(|_| overflow())?;
        if self
            .frames
            .checked_add(count)
            .is_none_or(|n| n > MAX_FRAMES)
        {
            return Err(overflow());
        }
        for (i, &p) in probabilities.iter().enumerate() {
            if !p.is_finite() || !(0.0..=1.0).contains(&p) {
                return Err(WhisperVadError::InvalidProbability {
                    frame: self.frames + u64::try_from(i).expect("checked feed length"),
                });
            }
        }
        let mut out = Vec::new();
        let budget = count
            .checked_mul(MAX_SEGMENTS_PER_FRAME)
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(capacity)?;
        out.try_reserve(budget).map_err(|_| capacity())?;
        for &p in probabilities {
            self.feed_frame(p, &mut |segment| out.push(segment));
        }
        Ok(out)
    }

    /// Close using the logical padded frame length. Repeated EOF returns no events.
    pub fn finish(&mut self) -> Result<Vec<WhisperSpeechSegment>, WhisperVadError> {
        if self.finished {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        out.try_reserve(MAX_FINISH_SEGMENTS)
            .map_err(|_| capacity())?;
        let length = self.frames * FRAME;
        if self.has_current && length - self.start > self.params.min_speech {
            self.raw(self.start, length, &mut |segment| out.push(segment));
        }
        self.seal(&mut |segment| out.push(segment));
        if let Some(group) = self.waiting.take() {
            out.push(segment(
                group.padded_start.expect("survivor padding"),
                (group.end + self.params.pad).min(length),
            ));
        }
        self.finished = true;
        Ok(out)
    }

    /// Discard pending groups and restart probability time at zero.
    pub fn reset(&mut self) {
        self.frames = 0;
        self.finished = false;
        self.active = false;
        self.has_current = false;
        self.start = 0;
        self.clear_silence();
        self.candidate = None;
        self.waiting = None;
    }

    fn clear_silence(&mut self) {
        self.temp_end = 0;
        self.prev_end = 0;
        self.next_start = 0;
    }

    /// Allocation-free shared frame path. Callers preflight the complete frame
    /// count and validate probabilities before invoking this internal method.
    pub(crate) fn feed_frame(&mut self, p: f32, out: &mut impl FnMut(WhisperSpeechSegment)) {
        let sample = self.frames * FRAME;
        self.frames += 1;
        self.loop_frame(p, sample, out);
        // A future raw interval cannot start before the active interval's start,
        // or before the next frame when inactive. Remembered split starts only move right.
        let earliest = if self.has_current {
            self.start
        } else {
            self.frames * FRAME
        };
        if self
            .candidate
            .is_some_and(|g| earliest >= g.end + MERGE_GAP)
        {
            self.seal(out);
        }
    }

    fn loop_frame(&mut self, p: f32, sample: u64, out: &mut impl FnMut(WhisperSpeechSegment)) {
        if p >= self.params.threshold && self.temp_end != 0 {
            self.temp_end = 0;
            if self.next_start < self.prev_end {
                self.next_start = sample;
            }
        }
        if p >= self.params.threshold && !self.active {
            self.active = true;
            self.has_current = true;
            self.start = sample;
            return;
        }
        if self.active && sample - self.start > self.params.max_speech {
            if self.prev_end != 0 {
                self.raw(self.start, self.prev_end, out);
                self.has_current = true;
                if self.next_start < self.prev_end {
                    self.active = false;
                    self.has_current = false;
                } else {
                    self.start = self.next_start;
                }
                self.clear_silence();
            } else {
                self.raw(self.start, sample, out);
                self.clear_silence();
                self.active = false;
                self.has_current = false;
                return;
            }
        }
        if p < self.params.negative && self.active {
            if self.temp_end == 0 {
                self.temp_end = sample;
            }
            if sample - self.temp_end > SPLIT_SILENCE_SAMPLES {
                self.prev_end = self.temp_end;
            }
            if sample - self.temp_end < self.params.min_silence {
                return;
            }
            if self.temp_end - self.start > self.params.min_speech {
                self.raw(self.start, self.temp_end, out);
            }
            self.clear_silence();
            self.active = false;
            self.has_current = false;
        }
    }

    fn raw(&mut self, start: u64, end: u64, out: &mut impl FnMut(WhisperSpeechSegment)) {
        if let Some(g) = &mut self.candidate {
            if start - g.end < MERGE_GAP {
                g.end = end;
                self.resolve_neighbor(out);
                return;
            }
        }
        self.seal(out);
        self.candidate = Some(Group {
            start,
            end,
            padded_start: None,
        });
        self.resolve_neighbor(out);
    }

    fn resolve_neighbor(&mut self, out: &mut impl FnMut(WhisperSpeechSegment)) {
        let Some(next) = &mut self.candidate else {
            return;
        };
        if next.end - next.start < self.params.min_speech {
            return;
        }
        if let Some(previous) = self.waiting.take() {
            let gap = next.start - previous.end;
            let pad = if gap < 2 * self.params.pad {
                gap / 2
            } else {
                self.params.pad
            };
            // Full padding must also wait for sufficient known logical audio length.
            // The successor's end is already at or beyond previous.end + pad,
            // except for enormous pads, where the half-gap branch applies.
            out(segment(
                previous.padded_start.expect("survivor padding"),
                previous.end + pad,
            ));
            next.padded_start = Some(next.start.saturating_sub(pad));
        } else if next.padded_start.is_none() {
            next.padded_start = Some(next.start.saturating_sub(self.params.pad));
        }
    }

    fn seal(&mut self, out: &mut impl FnMut(WhisperSpeechSegment)) {
        self.resolve_neighbor(out);
        if let Some(g) = self.candidate.take() {
            if g.end - g.start >= self.params.min_speech {
                debug_assert!(self.waiting.is_none());
                self.waiting = Some(g);
            }
        }
    }
}

// Preserve the pin's double expression and truncation, then return exactly cs * 10.
// This is not exact half-up: floating-point evaluation makes samples=2320 yield
// 140 ms rather than 150 ms. Do not "fix" it with integer or half-up rounding.
fn rounded_ms(samples: u64) -> u64 {
    (((samples as f64 / 16000.0) * 100.0 + 0.5).trunc() as u64) * 10
}
fn segment(start: u64, end: u64) -> WhisperSpeechSegment {
    WhisperSpeechSegment {
        start_ms: rounded_ms(start),
        end_ms: rounded_ms(end),
    }
}
fn overflow() -> WhisperVadError {
    WhisperVadError::Overflow {
        operation: WhisperVadOperation::LogicalSamples,
    }
}
fn capacity() -> WhisperVadError {
    WhisperVadError::Overflow {
        operation: WhisperVadOperation::Capacity,
    }
}

#[cfg(test)]
#[path = "whisper_postprocess_tests.rs"]
mod tests;
