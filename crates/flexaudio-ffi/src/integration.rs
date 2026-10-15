//! Optional noise suppression on delivered PCM. VAD uses the canonical capture tap.
use crate::{
    convert::{self, resolve_output},
    error::set_last_error,
    types::{FlexChunk, FlexConfig, FlexStream},
};
use flexaudio_denoise::Denoiser;
pub(crate) fn build_addons(config: &FlexConfig) -> Result<Option<Denoiser>, ()> {
    let output = resolve_output(config);
    if config.denoise == 0 {
        return Ok(None);
    }
    if output.sample_rate != 48_000 {
        set_last_error("denoise requires output_rate 48000");
        return Err(());
    }
    Denoiser::new(output.channels).map(Some).map_err(|error| {
        crate::error::set_audio_error(flexaudio::Error::InvalidArg(error.to_string()));
    })
}
impl FlexStream {
    pub(crate) fn poll_processed(
        &mut self,
    ) -> Result<Option<FlexChunk>, flexaudio_vad::InferenceError> {
        if self.vad_tap.is_none() {
            if let Some(chunk) = self.ready_chunks.pop_front() {
                return Ok(Some(chunk.chunk));
            }
        }
        let Some(mut chunk) = self.inner.poll_chunk() else {
            return Ok(None);
        };
        self.vad_origin.1 = self.vad_origin.1.max(chunk.pts_ns);
        if let Some(denoiser) = self.denoiser.as_mut() {
            if chunk.flags.contains(flexaudio::ChunkFlags::DISCONTINUITY) {
                denoiser.reset();
            }
            denoiser
                .process(&mut chunk.data)
                .map_err(|error| flexaudio_vad::InferenceError::Inference(error.to_string()))?;
        }
        (chunk.peak, chunk.rms) = peak_rms(&chunk.data);
        let output = self.inner.config().output;
        self.last_output = Some((
            chunk
                .frame_index
                .saturating_add(chunk.frames as u64 * 48_000 / u64::from(output.sample_rate)),
            chunk.pts_ns.saturating_add(
                chunk.frames as i64 * 1_000_000_000 / i64::from(output.sample_rate),
            ),
            chunk.seq.saturating_add(1),
        ));
        Ok(Some(convert::chunk_to_c(chunk)))
    }
}
pub(crate) fn peak_rms(data: &[f32]) -> (f32, f32) {
    if data.is_empty() {
        return (0.0, 0.0);
    }
    let peak = data.iter().fold(0.0_f32, |p, x| p.max(x.abs()));
    let rms =
        (data.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>() / data.len() as f64).sqrt() as f32;
    (peak, rms)
}

#[cfg(test)]
#[path = "../tests/ffi/startup_order.rs"]
mod startup_order_tests;
