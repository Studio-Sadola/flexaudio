//! VAD configuration. Defaults match the original silero-VAD `get_speech_timestamps` defaults.

/// Settings that control VAD behavior.
///
/// Defaults match silero-VAD `get_speech_timestamps`.
#[derive(Debug, Clone, PartialEq)]
pub struct VadConfig {
    /// Probability threshold for speech onset (>=). Default: 0.5.
    pub threshold: f32,
    /// Lower (silence-side) threshold for silence onset (<). If `None`, use
    /// `max(threshold - 0.15, 0.01)` (silero-compatible).
    pub neg_threshold: Option<f32>,
    /// Minimum accepted speech segment length (ms); shorter segments are discarded. Default: 250.
    pub min_speech_ms: u32,
    /// Silence duration needed to finalize speech end (ms). Default: 100 (silero).
    pub min_silence_ms: u32,
    /// Padding added around segment boundaries (ms). Default: 30 (silero).
    pub speech_pad_ms: u32,
    /// Maximum length of one segment (ms). 0 = unlimited; longer segments are forcibly split. Default: 0.
    pub max_speech_ms: u32,
    /// Sample rate. Only 8000 or 16000 are supported. Default: 16000.
    pub sample_rate: u32,
}

impl Default for VadConfig {
    /// silero defaults (same as [`VadConfig::balanced`]).
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
    /// More sensitive preset that reduces dropped speech.
    ///
    /// Lowers the threshold so speech is more likely to continue through short silences.
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

    /// Balanced preset. Same as silero default ([`VadConfig::default`]).
    pub fn balanced() -> Self {
        VadConfig::default()
    }

    /// Conservative preset that reduces false positives.
    ///
    /// Raises the threshold and requires longer silence to end speech.
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

    /// Return the effective lower (silence-side) threshold.
    ///
    /// Use the explicit value if set; otherwise `max(threshold - 0.15, 0.01)` (silero-compatible).
    pub fn resolved_neg_threshold(&self) -> f32 {
        match self.neg_threshold {
            Some(v) => v,
            None => (self.threshold - 0.15).max(0.01),
        }
    }

    /// 512 at 16k, 256 at 8k. silero frame length.
    pub(crate) fn frame_size(&self) -> usize {
        if self.sample_rate == 8000 {
            256
        } else {
            512
        }
    }

    /// 64 at 16k, 32 at 8k. silero pre-context length.
    pub(crate) fn context_size(&self) -> usize {
        if self.sample_rate == 8000 {
            32
        } else {
            64
        }
    }

    /// Convert ms to samples at the current `sample_rate`.
    pub(crate) fn ms_to_samples(&self, ms: u32) -> u64 {
        (u64::from(ms) * u64::from(self.sample_rate)) / 1000
    }

    /// Validate this configuration.
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
