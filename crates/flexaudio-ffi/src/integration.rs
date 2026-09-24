//! Integration of addons into the stream (option B).
//!
//! Depending on `denoise` / `has_vad` in `FlexConfig`, a Denoiser / VAD lives inside
//! [`FlexStream`], and each chunk is passed through **denoise → VAD** in that order right
//! before `poll_chunk` returns it. This is a thin integration layer holding only their
//! construction ([`build_addons`]) and processing path ([`FlexStream::poll_processed`]); the
//! logic of each addon lives in its own crate (no god class).

use flexaudio_denoise::Denoiser;
use flexaudio_vad::Vad;

use crate::convert::{self, resolve_output, vad_config_from_c, vad_events_to_c};
use crate::error::set_last_error;
use crate::types::{FlexChunk, FlexConfig, FlexStream};

/// Builds the denoise / VAD addons from a `FlexConfig` (used by `flexaudio_open`).
///
/// - With `denoise` enabled, returns `Err` unless the output rate is 48000 (RNNoise is fixed
///   at 48kHz). The Denoiser is built with the output channel count (resolved including
///   sentinels).
/// - With `has_vad` enabled, maps `vad` to a [`VadConfig`](flexaudio_vad::VadConfig) and
///   builds the VAD.
///
/// On any failure, sets last_error and returns `Err(())` (the caller can simply return NULL).
/// A disabled addon is `None`.
pub(crate) fn build_addons(config: &FlexConfig) -> Result<(Option<Denoiser>, Option<Vad>), ()> {
    let output = resolve_output(config);

    let denoiser = if config.denoise {
        // RNNoise assumes 48kHz. If the output rate differs, do not allow opening (rejected
        // at open).
        if output.sample_rate != 48_000 {
            set_last_error(format!(
                "denoise requires output_rate 48000 (0=default), got {}",
                output.sample_rate
            ));
            return Err(());
        }
        match Denoiser::new(output.channels) {
            Ok(d) => Some(d),
            Err(e) => {
                set_last_error(e.to_string());
                return Err(());
            }
        }
    } else {
        None
    };

    let vad = if config.has_vad {
        let vad_config = vad_config_from_c(&config.vad);
        match Vad::new(vad_config) {
            Ok(v) => Some(v),
            Err(e) => {
                set_last_error(e.to_string());
                return Err(());
            }
        }
    } else {
        None
    };

    Ok((denoiser, vad))
}

impl FlexStream {
    /// Polls one chunk, passes it through the enabled addons in **denoise → VAD** order, then
    /// maps it to a `FlexChunk` and returns it. `None` if there is none.
    ///
    /// - denoise: processes the interleaved data in place (the 48kHz assumption is already
    ///   guaranteed at open).
    /// - VAD: passes the (post-denoise) data through `process_pcm` in the output format as is,
    ///   and packs the finalized events into `FlexChunk::vad_events`.
    pub(crate) fn poll_processed(&mut self) -> Option<FlexChunk> {
        let mut chunk = self.inner.poll_chunk()?;

        // 1) denoise (in place). The length is frames×channels, a multiple of the channel
        //    count, so it does not error, but if it ever does, the original data passes
        //    through unchanged.
        if let Some(dn) = self.denoiser.as_mut() {
            let _ = dn.process(&mut chunk.data);
        }

        // 2) VAD. Pass the output format (unchanged since open) to process_pcm.
        //    output is Copy, so save it before the mutable borrow.
        let output = self.inner.config().output;
        let vad_events = match self.vad.as_mut() {
            Some(vad) => vad.process_pcm(&chunk.data, output.sample_rate, output.channels),
            None => Vec::new(),
        };

        let mut fc = convert::chunk_to_c(chunk);
        let (ev_ptr, ev_len) = vad_events_to_c(vad_events);
        fc.vad_events = ev_ptr;
        fc.vad_events_len = ev_len;
        Some(fc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FlexVadConfig;
    use std::ptr;

    fn zero_vad() -> FlexVadConfig {
        FlexVadConfig {
            threshold: 0.0,
            neg_threshold: 0.0,
            min_speech_ms: 0,
            min_silence_ms: 0,
            speech_pad_ms: 0,
            max_speech_ms: 0,
            sample_rate: 0,
        }
    }

    fn base_config() -> FlexConfig {
        FlexConfig {
            kind: crate::types::FlexSourceKind::Mic,
            device_id: ptr::null(),
            process_id: 0,
            mode: crate::types::FlexProcessMode::Include,
            exclude_self: false,
            output_rate: 0,
            output_channels: 0,
            chunk_ms: 0,
            gain: 0.0,
            mix_mic_device_id: ptr::null(),
            mix_system_device_id: ptr::null(),
            mix_mic_gain: 0.0,
            mix_system_gain: 0.0,
            denoise: false,
            has_vad: false,
            vad: zero_vad(),
        }
    }

    #[test]
    fn no_addons_yields_none() {
        let c = base_config();
        let (dn, vad) = build_addons(&c).expect("disabled addons are always Ok");
        assert!(dn.is_none());
        assert!(vad.is_none());
    }

    #[test]
    fn denoise_requires_48k_output() {
        // denoise enabled + non-48k → Err.
        let mut c = base_config();
        c.denoise = true;
        c.output_rate = 16_000;
        assert!(build_addons(&c).is_err());

        // denoise enabled + 48k (explicit) → Ok, and a Denoiser is built.
        let mut c48 = base_config();
        c48.denoise = true;
        c48.output_rate = 48_000;
        let (dn, _) = build_addons(&c48).expect("48k passes");
        assert!(dn.is_some());

        // denoise enabled + default (output_rate=0 → 48000) → Ok.
        let mut cdef = base_config();
        cdef.denoise = true;
        let (dn2, _) = build_addons(&cdef).expect("the default 48k passes");
        assert!(dn2.is_some());
    }
}
