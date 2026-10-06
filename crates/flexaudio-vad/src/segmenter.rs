//! Segmentation state machine (independent of ONNX).
//!
//! Reproduces the decision logic in the original silero-VAD `utils_vad.py::get_speech_timestamps`.
//! Feed speech probabilities one frame at a time to finalize segments (start/end sample positions)
//! in order, applying min_speech / min_silence / max_speech / pad / neg_threshold.
//!
//! Streaming ([`crate::Vad::process`]) and batch ([`crate::get_speech_timestamps`]) use this same logic.
//! Sample positions are absolute cumulative positions, not frame indices.

use crate::config::VadConfig;

/// A finalized speech segment (after padding, using absolute sample positions).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    /// Speech start sample position (after padding).
    pub start_sample: u64,
    /// Speech end sample position (after padding, exclusive; ends just before this position).
    pub end_sample: u64,
}

impl Segment {
    /// Convert the start position to ms at the given sample rate.
    pub fn start_ms(&self, sample_rate: u32) -> u64 {
        (self.start_sample * 1000) / u64::from(sample_rate.max(1))
    }

    /// Convert the end position to ms at the given sample rate.
    pub fn end_ms(&self, sample_rate: u32) -> u64 {
        (self.end_sample * 1000) / u64::from(sample_rate.max(1))
    }

    /// Segment length in samples.
    pub fn len_samples(&self) -> u64 {
        self.end_sample.saturating_sub(self.start_sample)
    }
}

/// Segmentation state machine.
///
/// Advances by the absolute sample position at each frame end, not by frame index. Each frame has
/// `frame_size` samples (16k=512), and each `feed` advances the position by `frame_size`.
#[derive(Debug, Clone)]
pub struct Segmenter {
    threshold: f32,
    neg_threshold: f32,
    min_speech_samples: u64,
    min_silence_samples: u64,
    speech_pad_samples: u64,
    max_speech_samples: u64, // 0 = unlimited
    frame_size: u64,

    /// Whether speech is active.
    triggered: bool,
    /// Start sample position of the current (unpadded) speech segment.
    current_start: u64,
    /// Position where silence began. 0 means no silence interval (same sentinel as silero).
    temp_end: u64,
    /// Absolute end position of the next frame to feed (total samples fed so far).
    next_pos: u64,
    /// End of the last finalized segment (after padding), used to clamp overlapping padding. 0 = none.
    prev_end: u64,
}

impl Segmenter {
    /// Create a segmenter from configuration.
    pub fn new(config: &VadConfig) -> Self {
        Segmenter {
            threshold: config.threshold,
            neg_threshold: config.resolved_neg_threshold(),
            min_speech_samples: config.ms_to_samples(config.min_speech_ms),
            min_silence_samples: config.ms_to_samples(config.min_silence_ms),
            speech_pad_samples: config.ms_to_samples(config.speech_pad_ms),
            max_speech_samples: config.ms_to_samples(config.max_speech_ms),
            frame_size: config.frame_size() as u64,
            triggered: false,
            current_start: 0,
            temp_end: 0,
            next_pos: 0,
            prev_end: 0,
        }
    }

    /// Reset the state.
    pub fn reset(&mut self) {
        self.triggered = false;
        self.current_start = 0;
        self.temp_end = 0;
        self.next_pos = 0;
        self.prev_end = 0;
    }

    /// Feed one frame of speech probability and return any finalized segment.
    ///
    /// `prob` is the speech probability for `[next_pos, next_pos + frame_size)`. The internal position
    /// advances by `frame_size`. Corresponds to the loop body of silero `get_speech_timestamps`.
    pub fn feed(&mut self, prob: f32, out: &mut Vec<Segment>) {
        // Interval covered by this frame: [frame_start, frame_end).
        let frame_start = self.next_pos;
        let frame_end = frame_start + self.frame_size;
        self.next_pos = frame_end;

        // Speech present (prob >= threshold).
        if prob >= self.threshold {
            // Reset silence counter (silero: temp_end = 0).
            if self.temp_end != 0 {
                self.temp_end = 0;
            }
            if !self.triggered {
                self.triggered = true;
                // silero uses frame index * window. Here use the absolute sample position at frame start.
                self.current_start = frame_start;
            }
            // Like silero, check max_speech on every frame while triggered.
            self.check_max_speech(frame_end, out);
            return;
        }

        // Silence (prob < neg_threshold) while speech is active.
        if prob < self.neg_threshold && self.triggered {
            if self.temp_end == 0 {
                self.temp_end = frame_start;
            }
            // Finalize speech when silence reaches min_silence. End at the silence start (temp_end).
            if frame_start.saturating_sub(self.temp_end) >= self.min_silence_samples {
                self.finalize_segment(self.current_start, self.temp_end, out);
                self.triggered = false;
                self.temp_end = 0;
                return;
            }
            // Still below min_silence; continue speech and evaluate max_speech.
        }
        // In the gray zone (threshold > prob >= neg_threshold), keep the state unchanged, as silero does;
        // remain triggered without starting a silence count.

        // Enforce max_speech during silence/gray frames while triggered.
        self.check_max_speech(frame_end, out);
    }

    /// Force a split if speech length exceeds max_speech while triggered.
    ///
    /// silero: split at the latest silence (temp_end), if any; otherwise split at the current frame end.
    fn check_max_speech(&mut self, frame_end: u64, out: &mut Vec<Segment>) {
        if !self.triggered || self.max_speech_samples == 0 {
            return;
        }
        let speech_len = frame_end.saturating_sub(self.current_start);
        if speech_len <= self.max_speech_samples {
            return;
        }
        if self.temp_end != 0 {
            // Split at the latest silence and continue the next segment from that position.
            let split = self.temp_end;
            self.finalize_segment(self.current_start, split, out);
            self.current_start = split;
            self.temp_end = 0;
        } else {
            // If there was no silence for too long, split at the current frame end and continue.
            self.finalize_segment(self.current_start, frame_end, out);
            self.current_start = frame_end;
        }
    }

    /// Call at end of input. If speech is active, finalize through the current position.
    /// silero includes an unfinished trailing segment through `audio_length`.
    pub fn flush(&mut self, out: &mut Vec<Segment>) {
        if self.triggered {
            self.finalize_segment(self.current_start, self.next_pos, out);
            self.triggered = false;
            self.temp_end = 0;
        }
    }

    /// Filter the raw (unpadded) speech interval by min_speech, apply padding, and append to `out`.
    fn finalize_segment(&mut self, raw_start: u64, raw_end: u64, out: &mut Vec<Segment>) {
        // Drop segments shorter than min_speech (silero: (end - start) < min_speech_samples).
        if raw_end.saturating_sub(raw_start) < self.min_speech_samples {
            return;
        }

        // Extend the start backward and end forward by speech_pad.
        let mut start = raw_start.saturating_sub(self.speech_pad_samples);
        let end = raw_end + self.speech_pad_samples;

        // Clamp the start so padding does not overlap the previous segment. silero compares the gap
        // between adjacent segments to pad*2 and splits it at the midpoint; here we clamp the start
        // to avoid crossing prev_end (equivalent and conservative).
        if self.prev_end != 0 && start < self.prev_end {
            start = self.prev_end;
        }

        self.prev_end = end;
        out.push(Segment {
            start_sample: start,
            end_sample: end,
        });
    }
}
