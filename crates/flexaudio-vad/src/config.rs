//! VAD configuration. The defaults match those of the original silero-VAD
//! (`get_speech_timestamps`).

/// Settings that control VAD behavior.
///
/// The default values match silero-VAD's `get_speech_timestamps`.
#[derive(Debug, Clone, PartialEq)]
pub struct VadConfig {
    /// Probability threshold (>=) for treating as speech start. Default 0.5.
    pub threshold: f32,
    /// Negative-side threshold (<) for treating as silence start. `None` means
    /// `max(threshold - 0.15, 0.01)` (following silero).
    pub neg_threshold: Option<f32>,
    /// Minimum length (ms) of accepted speech. Shorter segments are discarded. Default 250.
    pub min_speech_ms: u32,
    /// Silence length (ms) required to finalize speech end. Default 100 (silero).
    pub min_silence_ms: u32,
    /// Padding (ms) that widens segment boundaries on both sides. Default 30 (silero).
    pub speech_pad_ms: u32,
    /// Maximum length (ms) of one segment. 0 = unlimited. Exceeding it forces a split.
    /// Default 0.
    pub max_speech_ms: u32,
    /// Sample rate. Only 8000 or 16000. Default 16000.
    pub sample_rate: u32,
}

impl Default for VadConfig {
    /// The silero defaults (same as [`VadConfig::balanced`]).
    fn default() -> Self {
        VadConfig {
            threshold: 0.5,
            neg_threshold: None,
            min_speech_ms: 250,
            min_silence_ms: 100,
            speech_pad_ms: 30,
            max_speech_ms: 0,
            sample_rate: 16000,
        }
    }
}

impl VadConfig {
    /// Higher-sensitivity preset that reduces misses.
    ///
    /// Lowers the threshold and makes speech more likely to continue through short silences.
    pub fn aggressive() -> Self {
        VadConfig {
            threshold: 0.35,
            neg_threshold: None,
            min_speech_ms: 200,
            min_silence_ms: 150,
            speech_pad_ms: 50,
            max_speech_ms: 0,
            sample_rate: 16000,
        }
    }

    /// Balanced preset. Same as the silero defaults ([`VadConfig::default`]).
    pub fn balanced() -> Self {
        VadConfig::default()
    }

    /// Conservative preset that reduces false positives.
    ///
    /// Raises the threshold and ends speech after longer silences.
    pub fn conservative() -> Self {
        VadConfig {
            threshold: 0.6,
            neg_threshold: None,
            min_speech_ms: 300,
            min_silence_ms: 300,
            speech_pad_ms: 30,
            max_speech_ms: 0,
            sample_rate: 16000,
        }
    }

    /// Returns the effective negative-side threshold.
    ///
    /// The explicit value if given, otherwise `max(threshold - 0.15, 0.01)` (following silero).
    pub fn resolved_neg_threshold(&self) -> f32 {
        match self.neg_threshold {
            Some(v) => v,
            None => (self.threshold - 0.15).max(0.01),
        }
    }

    /// 512 for 16k, 256 for 8k. The silero frame length.
    pub(crate) fn frame_size(&self) -> usize {
        if self.sample_rate == 8000 {
            256
        } else {
            512
        }
    }

    /// 64 for 16k, 32 for 8k. The silero leading context length.
    pub(crate) fn context_size(&self) -> usize {
        if self.sample_rate == 8000 {
            32
        } else {
            64
        }
    }

    /// Converts ms to a sample count (based on the current `sample_rate`).
    pub(crate) fn ms_to_samples(&self, ms: u32) -> u64 {
        (u64::from(ms) * u64::from(self.sample_rate)) / 1000
    }

    /// Validates the settings.
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.sample_rate != 8000 && self.sample_rate != 16000 {
            return Err(format!(
                "sample_rate must be 8000 or 16000, got {}",
                self.sample_rate
            ));
        }
        if !(0.0..=1.0).contains(&self.threshold) {
            return Err(format!(
                "threshold must be within [0.0, 1.0], got {}",
                self.threshold
            ));
        }
        if let Some(nt) = self.neg_threshold {
            if !(0.0..=1.0).contains(&nt) {
                return Err(format!("neg_threshold must be within [0.0, 1.0], got {nt}"));
            }
        }
        Ok(())
    }
}
