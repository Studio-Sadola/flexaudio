//! ストリームへのアドオン統合（案 B）。
//!
//! `FlexConfig` の `denoise` / `has_vad` に応じて、[`FlexStream`] の中に Denoiser / VAD を
//! 同居させ、`poll_chunk` が返す直前にチャンクを **denoise → VAD** の順で通す。ここは
//! その組み立て（[`build_addons`]）と処理経路（[`FlexStream::poll_processed`]）だけを持つ薄い
//! 統合層で、個々のアドオンのロジックは各クレート側にある（神クラス化しない）。

use flexaudio_denoise::Denoiser;
use flexaudio_vad::Vad;

use crate::convert::{self, resolve_output, vad_config_from_c, vad_events_to_c};
use crate::error::set_last_error;
use crate::types::{FlexChunk, FlexConfig, FlexStream};

/// `FlexConfig` から denoise / VAD アドオンを組み立てる（`flexaudio_open` が使う）。
///
/// - `denoise` 有効時は出力レートが 48000 でなければ `Err`（RNNoise は 48kHz 固定）。
///   出力チャンネル数（番兵込みで解決）で Denoiser を作る。
/// - `has_vad` 有効時は `vad` を [`VadConfig`](flexaudio_vad::VadConfig) に写して VAD を作る。
///
/// いずれも失敗時は last_error をセットして `Err(())` を返す（呼び出し側はそのまま NULL を
/// 返せばよい）。無効なアドオンは `None`。
pub(crate) fn build_addons(config: &FlexConfig) -> Result<(Option<Denoiser>, Option<Vad>), ()> {
    let output = resolve_output(config);

    let denoiser = if config.denoise {
        // RNNoise は 48kHz 前提。出力レートが違うなら開かせない（open で弾く）。
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
    /// チャンクを 1 つ poll し、有効なアドオンを **denoise → VAD** の順で通してから
    /// `FlexChunk` に写して返す。無ければ `None`。
    ///
    /// - denoise: interleaved data をインプレース処理する（48kHz 前提は open で保証済み）。
    /// - VAD: （denoise 後の）data を出力フォーマットのまま `process_pcm` に通し、確定した
    ///   イベントを `FlexChunk::vad_events` に詰める。
    pub(crate) fn poll_processed(&mut self) -> Option<FlexChunk> {
        let mut chunk = self.inner.poll_chunk()?;

        // 1) denoise（インプレース）。長さは frames×channels でチャンネル数の倍数なので
        //    エラーにはならないが、万一のときは元データのまま素通しさせる。
        if let Some(dn) = self.denoiser.as_mut() {
            let _ = dn.process(&mut chunk.data);
        }

        // 2) VAD。出力フォーマット（open 以降不変）を process_pcm に渡す。
        //    output は Copy なので、可変借用の前に控えておく。
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
        let (dn, vad) = build_addons(&c).expect("アドオン無効は常に Ok");
        assert!(dn.is_none());
        assert!(vad.is_none());
    }

    #[test]
    fn denoise_requires_48k_output() {
        // denoise 有効 + 非 48k → Err。
        let mut c = base_config();
        c.denoise = true;
        c.output_rate = 16_000;
        assert!(build_addons(&c).is_err());

        // denoise 有効 + 48k（明示）→ Ok で Denoiser が作られる。
        let mut c48 = base_config();
        c48.denoise = true;
        c48.output_rate = 48_000;
        let (dn, _) = build_addons(&c48).expect("48k なら通る");
        assert!(dn.is_some());

        // denoise 有効 + 既定（output_rate=0 → 48000）→ Ok。
        let mut cdef = base_config();
        cdef.denoise = true;
        let (dn2, _) = build_addons(&cdef).expect("既定 48k なら通る");
        assert!(dn2.is_some());
    }
}
