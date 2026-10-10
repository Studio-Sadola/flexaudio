//! Canonical DSP construction, shared by every capture generation.
use super::*;
/// Wraps [`flexaudio_denoise::Denoiser`] as a core [`InnerProcessor`] so the
/// core stays independent of the concrete noise-suppression implementation.
///
/// The processor runs on the internal normalized form (48kHz / stereo), so the
/// denoiser is always constructed for two channels regardless of the primary
/// output channel count. `process`/`flush` forward to the denoiser (any error
/// is swallowed: the normalized form always has an even length, so
/// [`Denoiser::process`](flexaudio_denoise::Denoiser::process) never rejects it).
struct DenoiseInnerProcessor {
    denoiser: flexaudio_denoise::Denoiser,
}

impl DenoiseInnerProcessor {
    /// Build a fresh stereo denoiser. Called once per intake generation so a
    /// source switch / reopen starts with clean RNNoise state (no bleed across
    /// the discontinuity).
    fn new() -> Self {
        // Denoiser::new(2) only fails for an out-of-range channel count; 2 is
        // always valid, so unwrap is safe here.
        Self {
            denoiser: flexaudio_denoise::Denoiser::new(2)
                .expect("stereo denoiser construction is infallible"),
        }
    }
}

impl InnerProcessor for DenoiseInnerProcessor {
    fn process(&mut self, samples: &mut [f32]) {
        let _ = self.denoiser.process(samples);
    }
    fn flush(&mut self) -> Vec<f32> {
        self.denoiser.flush()
    }
}

/// Build the optional inner processor for a Normalizer, honoring the current
/// `denoise_enabled` flag. Returns `None` when denoise is off.
fn build_inner_processor(denoise_enabled: bool) -> Option<Box<dyn InnerProcessor>> {
    if denoise_enabled {
        Some(Box::new(DenoiseInnerProcessor::new()))
    } else {
        None
    }
}

/// Build the [`Normalizer`] for the current generation: primary output, the
/// optional secondary tap, and the optional denoise inner processor. Kept in one
/// place so `start` and the intake generation-change path stay in sync.
pub(super) fn build_normalizer(
    shared: &SharedState,
    rate: u32,
    channels: u16,
    output: OutputFormat,
    secondary_output: Option<OutputFormat>,
    denoise_enabled: bool,
) -> Result<Normalizer> {
    let mut n = Normalizer::new(rate, channels, output)?;
    if shared.capture_enabled.load(Ordering::SeqCst) {
        n = n.with_capture_tap()?;
    }
    if let Some(sec) = secondary_output {
        n = n.with_secondary(sec)?;
    }
    let processor = build_inner_processor(denoise_enabled);
    if let Some(processor) = processor {
        n = n.with_inner_processor(processor);
    }
    Ok(n)
}
