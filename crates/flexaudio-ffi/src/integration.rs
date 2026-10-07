//! Stream-integrated noise suppression and VAD.
//!
//! Based on `denoise` / `has_vad` in `FlexConfig`, keep Denoiser / VAD inside `FlexStream` and
//! process chunks as **denoise → VAD** just before `poll_chunk` returns them. This thin layer handles
//! construction ([`build_addons`]) and processing ([`FlexStream::poll_processed`]); add-on logic
//! remains in each crate (avoiding a god class).

use flexaudio_denoise::Denoiser;
use flexaudio_vad::Vad;

use crate::convert::{self, resolve_output, vad_config_from_c, vad_events_to_c};
use crate::error::set_last_error;
use crate::types::{FlexChunk, FlexConfig, FlexStream};

/// Build denoise / VAD add-ons from `FlexConfig` (used by `flexaudio_open`).
///
/// - If `denoise` is enabled, output rate must be 48000 (RNNoise is fixed at 48 kHz); otherwise return `Err`.
///   Create Denoiser using the resolved output channel count (including the sentinel value).
/// - If `has_vad` is enabled, map `vad` to [`VadConfig`](flexaudio_vad::VadConfig) and create VAD.
///
/// On failure, both set last_error and return `Err(())` (the caller can return NULL directly).
/// Disabled add-ons are `None`.
pub(crate) fn build_addons(config: &FlexConfig) -> Result<(Option<Denoiser>, Option<Vad>), ()> {
    let output = resolve_output(config);

    let denoiser = if config.denoise {
        // RNNoise requires 48 kHz. Reject other output rates during open.
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
    /// Poll one chunk, run enabled add-ons in **denoise → VAD** order, then convert it to
    /// `FlexChunk`. Return `None` if there is no chunk.
    ///
    /// - denoise: process interleaved data in place (48 kHz is guaranteed by open).
    /// - VAD: pass data in the output format (after denoise) to `process_pcm` and append finalized
    ///   events to `FlexChunk::vad_events`.
    pub(crate) fn poll_processed(&mut self) -> Result<Option<FlexChunk>, flexaudio_vad::VadError> {
        let Some(mut chunk) = self.inner.poll_chunk() else {
            return Ok(None);
        };

        // 1) denoise (in place). Length is frames×channels, hence divisible by channel count,
        //    so this should not fail; if it does, pass through the original data.
        if let Some(dn) = self.denoiser.as_mut() {
            let _ = dn.process(&mut chunk.data);
        }

        // 2) VAD. Pass the unchanged output format (guaranteed after open) to process_pcm.
        //    output is Copy, so save it before borrowing mutably.
        let output = self.inner.config().output;
        let vad_events = match self.vad.as_mut() {
            Some(vad) => vad.process_pcm(&chunk.data, output.sample_rate, output.channels)?,
            None => Vec::new(),
        };

        let mut fc = convert::chunk_to_c(chunk);
        let (ev_ptr, ev_len) = vad_events_to_c(vad_events);
        fc.vad_events = ev_ptr;
        fc.vad_events_len = ev_len;
        Ok(Some(fc))
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
        let (dn, vad) = build_addons(&c).expect("Disabled add-ons always succeed");
        assert!(dn.is_none());
        assert!(vad.is_none());
    }

    #[test]
    fn denoise_requires_48k_output() {
        // Denoise enabled + non-48k → Err.
        let mut c = base_config();
        c.denoise = true;
        c.output_rate = 16_000;
        assert!(build_addons(&c).is_err());

        // Denoise enabled + explicit 48k → Ok and creates Denoiser.
        let mut c48 = base_config();
        c48.denoise = true;
        c48.output_rate = 48_000;
        let (dn, _) = build_addons(&c48).expect("48k should succeed");
        assert!(dn.is_some());

        // Denoise enabled + default (output_rate=0 → 48000) → Ok.
        let mut cdef = base_config();
        cdef.denoise = true;
        let (dn2, _) = build_addons(&cdef).expect("Default 48k should succeed");
        assert!(dn2.is_some());
    }
}
