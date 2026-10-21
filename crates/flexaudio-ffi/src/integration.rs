//! Deliver facade PCM unchanged. VAD uses the canonical capture tap.
use crate::{
    convert::{self, resolve_output},
    types::{FlexChunk, FlexConfig, FlexStream},
};
pub(crate) fn validate_denoise_channels(config: &FlexConfig) -> Result<(), ()> {
    if config.denoise == 0 {
        return Ok(());
    }
    let output = resolve_output(config);
    if !(1..=2).contains(&output.channels) {
        crate::error::set_audio_error(flexaudio::Error::InvalidArg(
            flexaudio_denoise::DenoiseError::InvalidChannels(output.channels).to_string(),
        ));
        return Err(());
    }
    Ok(())
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
        let Some(chunk) = self.inner.poll_chunk() else {
            return Ok(None);
        };
        self.vad_origin.1 = self.vad_origin.1.max(chunk.pts_ns);
        Ok(Some(convert::chunk_to_c(chunk)))
    }
}
#[cfg(test)]
#[path = "../tests/ffi/startup_order.rs"]
mod startup_order_tests;
