//! Segmentation state machine (independent of ONNX).
//!
//! Reproduces the decision logic of the original silero-VAD
//! `utils_vad.py::get_speech_timestamps`. Feeding it per-frame speech probabilities one by one
//! returns segments (start/end sample positions) with min_speech / min_silence / max_speech /
//! pad / neg_threshold applied, in the order they are finalized.
//!
//! Streaming ([`crate::Vad::process`]) and batch ([`crate::get_speech_timestamps`]) use this
//! same logic. Sample positions are absolute sample positions (cumulative), not frame indices.

use crate::config::VadConfig;

/// A finalized speech segment (after padding, absolute sample positions).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    /// Speech start sample position (after padding).
    pub start_sample: u64,
    /// Speech end sample position (after padding, exclusive: up to just before this position).
    pub end_sample: u64,
}

impl Segment {
    /// Converts the start position to ms (based on the given sample rate).
    pub fn start_ms(&self, sample_rate: u32) -> u64 {
        (self.start_sample * 1000) / u64::from(sample_rate.max(1))
    }

    /// Converts the end position to ms (based on the given sample rate).
    pub fn end_ms(&self, sample_rate: u32) -> u64 {
        (self.end_sample * 1000) / u64::from(sample_rate.max(1))
    }

    /// Segment length (in samples).
    pub fn len_samples(&self) -> u64 {
        self.end_sample.saturating_sub(self.start_sample)
    }
}

/// Segmentation state machine.
///
/// Advances by the absolute sample position of the frame end rather than by frame index. One
/// frame is `frame_size` samples (16k=512), and each `feed` advances the position by
/// `frame_size`.
#[derive(Debug, Clone)]
pub struct Segmenter {
    threshold: f32,
    neg_threshold: f32,
    min_speech_samples: u64,
    min_silence_samples: u64,
    speech_pad_samples: u64,
    max_speech_samples: u64, // 0 = unlimited
    frame_size: u64,

    /// In-speech flag.
    triggered: bool,
    /// Start sample position of the current speech (raw, unpadded).
    current_start: u64,
    /// Position where silence started. 0 = no silent stretch (the same sentinel as silero).
    temp_end: u64,
    /// Absolute sample position of the end of the next frame to feed (the total number of
    /// samples fed so far).
    next_pos: u64,
    /// End of the most recently finalized segment (after pad). Used for the pad-overlap clamp.
    /// 0 = none finalized yet.
    prev_end: u64,
}

impl Segmenter {
    /// Constructs a segmenter from the configuration.
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

    /// Resets the state.
    pub fn reset(&mut self) {
        self.triggered = false;
        self.current_start = 0;
        self.temp_end = 0;
        self.next_pos = 0;
        self.prev_end = 0;
    }

    /// Feeds the speech probability of one frame and returns any finalized segments.
    ///
    /// `prob` is the speech probability of that frame [next_pos, next_pos + frame_size). The
    /// internal position advances by `frame_size`. Corresponds to the loop body of silero's
    /// `get_speech_timestamps`.
    pub fn feed(&mut self, prob: f32, out: &mut Vec<Segment>) {
        // The range this frame occupies [frame_start, frame_end).
        let frame_start = self.next_pos;
        let frame_end = frame_start + self.frame_size;
        self.next_pos = frame_end;

        // Speech present (prob >= threshold).
        if prob >= self.threshold {
            // Reset the silence counter (silero: temp_end = 0).
            if self.temp_end != 0 {
                self.temp_end = 0;
            }
            if !self.triggered {
                self.triggered = true;
                // silero uses frame index * window. Here it is the absolute sample position of
                // the frame start.
                self.current_start = frame_start;
            }
            // While triggered, check max_speech every frame, as silero does.
            self.check_max_speech(frame_end, out);
            return;
        }

        // Silence side (prob < neg_threshold) while in speech.
        if prob < self.neg_threshold && self.triggered {
            if self.temp_end == 0 {
                self.temp_end = frame_start;
            }
            // When the silence reaches min_silence, finalize the speech end. The end is the
            // silence start position (temp_end).
            if frame_start.saturating_sub(self.temp_end) >= self.min_silence_samples {
                self.finalize_segment(self.current_start, self.temp_end, out);
                self.triggered = false;
                self.temp_end = 0;
                return;
            }
            // Still under min_silence. Fall through to the max_speech check with speech
            // continuing.
        }
        // In the gray zone threshold > prob >= neg_threshold, continue without changing state,
        // as silero does (stay triggered and do not start counting silence either).

        // Forced max_speech split on silent/gray frames while still triggered.
        self.check_max_speech(frame_end, out);
    }

    /// Forces a split if the speech length exceeds max_speech while triggered.
    ///
    /// silero: if there is a recent silence (temp_end), split there and continue; otherwise
    /// split at the end of the current frame.
    fn check_max_speech(&mut self, frame_end: u64, out: &mut Vec<Segment>) {
        if !self.triggered || self.max_speech_samples == 0 {
            return;
        }
        let speech_len = frame_end.saturating_sub(self.current_start);
        if speech_len <= self.max_speech_samples {
            return;
        }
        if self.temp_end != 0 {
            // Split at the recent silence position and continue the next speech from there.
            let split = self.temp_end;
            self.finalize_segment(self.current_start, split, out);
            self.current_start = split;
            self.temp_end = 0;
        } else {
            // Too long with no silence → split at the end of the current frame and continue.
            self.finalize_segment(self.current_start, frame_end, out);
            self.current_start = frame_end;
        }
    }

    /// Called when the end of input is reached. If in speech, finalizes up to the current
    /// position as the final segment. silero accepts trailing unfinalized speech up to
    /// `audio_length`.
    pub fn flush(&mut self, out: &mut Vec<Segment>) {
        if self.triggered {
            self.finalize_segment(self.current_start, self.next_pos, out);
            self.triggered = false;
            self.temp_end = 0;
        }
    }

    /// Filters a raw (unpadded) speech segment by min_speech, applies the pad, and pushes it to
    /// `out`.
    fn finalize_segment(&mut self, raw_start: u64, raw_end: u64, out: &mut Vec<Segment>) {
        // Discard anything under min_speech (silero: (end - start) < min_speech_samples).
        if raw_end.saturating_sub(raw_start) < self.min_speech_samples {
            return;
        }

        // speech_pad: widen the start earlier and the end later.
        let mut start = raw_start.saturating_sub(self.speech_pad_samples);
        let end = raw_end + self.speech_pad_samples;

        // Clamp so the pad does not overlap the previous segment. silero compares the gap
        // between adjacent segments with pad*2 and splits it at the midpoint, but here the
        // start is clamped so as not to intrude on prev_end (equivalent and conservative).
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
