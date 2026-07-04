//! [`crate::Vad::process_pcm`] の前段。任意フォーマットの interleaved PCM を mono に
//! 落とし、VAD の動作レート（16000 か 8000）へリサンプルして、既存の 16k/mono 経路へ
//! 渡せる形にする。
//!
//! 流儀は flexaudio-core の正規化器に合わせてある。mono 化は各チャンネルの単純平均
//! （stereo なら L/R 平均）、リサンプルはアンチエイリアス込みの rubato sinc。リサンプラは
//! 呼び出しをまたいで内部遅延と端数を持ち越すので、細切れに渡しても継ぎ目は出ない。

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Async, FixedAsync, Indexing, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};

/// [`crate::Vad::process_pcm`] が受け取る入力 PCM のフォーマット記述子。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmFormat {
    /// 入力サンプルレート (Hz)。
    pub sample_rate: u32,
    /// 入力チャンネル数（interleaved のチャンネル数）。
    pub channels: u16,
}

/// interleaved の任意 ch を mono へ落とし、VAD レートへリサンプルする前段変換器。
///
/// 入力フォーマット（[`PcmFormat`]）と目標レートは生成時に固定する。入力レートが目標と
/// 同じ場合は mono 化だけ行い、リサンプラは持たない。
pub(crate) struct PcmConverter {
    format: PcmFormat,
    channels: usize,
    /// 目標レートへの SR 変換器。入力レートが目標と一致するなら `None`（mono 化のみ）。
    resampler: Option<MonoResampler>,
    /// 1 フレームに満たない端数 interleaved サンプル。次回入力の前に連結して持ち越す。
    remainder: Vec<f32>,
    /// mono 化した結果を溜めるスクラッチ（アロケーションの使い回し）。
    mono: Vec<f32>,
}

impl PcmConverter {
    /// 入力フォーマットと目標レート（VAD の動作レート）から変換器を作る。
    ///
    /// rubato の構築は極端なレート比などで失敗し得るので、その場合はエラー文字列を返す
    /// （呼び出し側で panic させずに扱う）。
    pub(crate) fn new(format: PcmFormat, target_rate: u32) -> Result<Self, String> {
        let channels = usize::from(format.channels.max(1));
        let resampler = if format.sample_rate == target_rate {
            None
        } else {
            Some(MonoResampler::new(format.sample_rate, target_rate)?)
        };
        Ok(PcmConverter {
            format,
            channels,
            resampler,
            remainder: Vec::new(),
            mono: Vec::new(),
        })
    }

    /// この変換器が対象とする入力フォーマットか。
    pub(crate) fn matches(&self, format: PcmFormat) -> bool {
        self.format == format
    }

    /// interleaved 入力を mono 化し、必要ならリサンプルして、目標レートの mono サンプルを
    /// `out` へ追記する。
    ///
    /// フレーム境界（channels の倍数）に満たない端数は内部に持ち越すので、任意の位置で
    /// 分割して渡しても一括で渡したときと同じ結果になる。
    pub(crate) fn convert(
        &mut self,
        interleaved: &[f32],
        out: &mut Vec<f32>,
    ) -> Result<(), String> {
        // 前回の端数に今回分を連結し、揃ったフレームだけ mono 化する。
        self.remainder.extend_from_slice(interleaved);
        let frames = self.remainder.len() / self.channels;
        let used = frames * self.channels;

        self.mono.clear();
        downmix_to_mono(&self.remainder[..used], self.channels, &mut self.mono);
        self.remainder.drain(..used);

        match &mut self.resampler {
            None => out.extend_from_slice(&self.mono),
            Some(rs) => rs.push(&self.mono, out)?,
        }
        Ok(())
    }
}

/// interleaved の `channels` ch を各フレームのチャンネル平均で mono 化し `out` へ push する。
///
/// stereo は L/R 平均、それ以上は全チャンネルの平均。`channels <= 1` はそのままコピー。
/// `src` の長さは `channels` の倍数であること（端数フレームは呼び出し側で除いておく）。
fn downmix_to_mono(src: &[f32], channels: usize, out: &mut Vec<f32>) {
    if channels <= 1 {
        out.extend_from_slice(src);
        return;
    }
    let inv = 1.0 / channels as f32;
    for frame in src.chunks_exact(channels) {
        let sum: f32 = frame.iter().sum();
        out.push(sum * inv);
    }
}

/// mono 1ch 専用の rubato sinc リサンプラ。固定入力チャンク（`FixedAsync::Input`）で
/// 動き、端数と内部遅延は呼び出しをまたいで持ち越す。
///
/// パラメータは flexaudio-core の正規化器と同じ（sinc_len=128 / BlackmanHarris2 など）。
struct MonoResampler {
    inner: Async<f32>,
    /// rubato が 1 回の `process` で要求する入力フレーム数（固定）。
    chunk_in_frames: usize,
    /// 1 回の `process` が生成しうる最大出力フレーム数。
    max_out_frames: usize,
    /// 未処理の入力 mono サンプル。
    in_accum: Vec<f32>,
    /// rubato への出力スクラッチ（使い回してアロケートを避ける）。
    out_scratch: Vec<f32>,
}

impl MonoResampler {
    fn new(in_sr: u32, out_sr: u32) -> Result<Self, String> {
        let ratio = out_sr as f64 / in_sr as f64;
        // 固定入力チャンクは 20ms 相当の入力フレーム（端数は rubato が内部に保持する）。
        let chunk_in_frames = (in_sr as usize / 50).max(64);

        let params = SincInterpolationParameters {
            sinc_len: 128,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 128,
            window: WindowFunction::BlackmanHarris2,
        };

        let inner = Async::<f32>::new_sinc(
            ratio,
            1.0, // 比は固定
            &params,
            chunk_in_frames,
            1, // mono
            FixedAsync::Input,
        )
        .map_err(|e| format!("rubato sinc resampler construction failed: {e}"))?;

        let max_out_frames = inner.output_frames_max();

        Ok(MonoResampler {
            inner,
            chunk_in_frames,
            max_out_frames,
            in_accum: Vec::with_capacity(chunk_in_frames * 4),
            out_scratch: vec![0.0; max_out_frames],
        })
    }

    /// mono 入力を溜め、`chunk_in_frames` 単位で可能な限りリサンプルして `out` へ追記する。
    /// 満たない端数は `in_accum` に残して次回へ持ち越す。
    fn push(&mut self, mono: &[f32], out: &mut Vec<f32>) -> Result<(), String> {
        self.in_accum.extend_from_slice(mono);
        let step = self.chunk_in_frames; // mono なのでフレーム数 = サンプル数。

        while self.in_accum.len() >= step {
            let in_adapter = InterleavedSlice::new(&self.in_accum[..step], 1, self.chunk_in_frames)
                .map_err(|e| format!("rubato interleaved input adapter failed: {e}"))?;
            let mut out_adapter =
                InterleavedSlice::new_mut(&mut self.out_scratch[..], 1, self.max_out_frames)
                    .map_err(|e| format!("rubato interleaved output adapter failed: {e}"))?;

            let indexing = Indexing {
                input_offset: 0,
                output_offset: 0,
                partial_len: None,
                active_channels_mask: None,
            };

            let (_in_used, out_written) = self
                .inner
                .process_into_buffer(&in_adapter, &mut out_adapter, Some(&indexing))
                .map_err(|e| format!("rubato process_into_buffer failed: {e}"))?;

            out.extend_from_slice(&self.out_scratch[..out_written]); // mono。
            self.in_accum.drain(..step);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    #[test]
    fn downmix_stereo_is_lr_average() {
        // 完全逆相は 0、同相は元の値。
        let src = [0.5, -0.5, 0.3, 0.3, 1.0, 0.0];
        let mut out = Vec::new();
        downmix_to_mono(&src, 2, &mut out);
        assert_eq!(out, vec![0.0, 0.3, 0.5]);
    }

    #[test]
    fn downmix_quad_is_channel_average() {
        let src = [1.0, 2.0, 3.0, 4.0]; // 1 フレーム 4ch → 平均 2.5。
        let mut out = Vec::new();
        downmix_to_mono(&src, 4, &mut out);
        assert_eq!(out, vec![2.5]);
    }

    #[test]
    fn downmix_mono_is_copy() {
        let src = [0.1, -0.2, 0.3];
        let mut out = Vec::new();
        downmix_to_mono(&src, 1, &mut out);
        assert_eq!(out, src.to_vec());
    }

    /// 正弦波を 48k→16k へリサンプルしても周波数（ゼロ交差）と振幅（RMS）が保たれる。
    /// 過渡を避けて中央だけで測る。
    #[test]
    fn resample_48k_to_16k_preserves_tone() {
        let mut conv = PcmConverter::new(
            PcmFormat {
                sample_rate: 48_000,
                channels: 1,
            },
            16_000,
        )
        .unwrap();

        let freq = 440.0_f32;
        let amp = 0.5_f32;
        let mut out = Vec::new();
        // 2 秒ぶん、441 サンプルずつ push（細切れでも継ぎ目が出ないことも兼ねる）。
        let total = 48_000 * 2;
        let mut i = 0usize;
        while i < total {
            let take = 441.min(total - i);
            let block: Vec<f32> = (0..take)
                .map(|k| (2.0 * PI * freq * ((i + k) as f32) / 48_000.0).sin() * amp)
                .collect();
            conv.convert(&block, &mut out).unwrap();
            i += take;
        }
        assert!(out.len() >= 16_000, "1 秒以上の出力が必要: {}", out.len());

        // 過渡（先頭・末尾各 0.25 秒 = 4000 sample）を捨てて中央 1 秒で測る。
        let mid = &out[4_000..4_000 + 16_000];

        // 周波数: 16000 sample 中のゼロ交差 ≈ 2*440 = 880。
        let crossings = zero_crossings(mid);
        assert!(
            (876..=884).contains(&crossings),
            "16k リサンプル後の周波数がずれた: crossings={crossings}"
        );

        // 振幅: 正弦の RMS は amp/√2 ≈ 0.3536。
        let got = rms(mid);
        let expect = amp / std::f32::consts::SQRT_2;
        assert!(
            (got - expect).abs() < 0.02,
            "16k リサンプル後の RMS がずれた: got={got} expect={expect}"
        );
    }

    /// SR が目標と一致するときはリサンプラを持たず（mono 化のみ）、mono 化した値が
    /// そのまま出る。
    #[test]
    fn same_rate_stereo_only_downmixes() {
        let mut conv = PcmConverter::new(
            PcmFormat {
                sample_rate: 16_000,
                channels: 2,
            },
            16_000,
        )
        .unwrap();
        assert!(conv.resampler.is_none());
        let mut out = Vec::new();
        conv.convert(&[0.5, -0.5, 0.2, 0.2], &mut out).unwrap();
        assert_eq!(out, vec![0.0, 0.2]);
    }

    /// フレーム境界（ch の倍数）に満たない端数を挟んで分割しても、一括と同じ mono 列。
    #[test]
    fn split_across_partial_frame_matches_bulk() {
        let fmt = PcmFormat {
            sample_rate: 16_000,
            channels: 2,
        };
        let interleaved: Vec<f32> = (0..2000).map(|i| (i as f32) * 1e-3).collect();

        let mut bulk = Vec::new();
        PcmConverter::new(fmt, 16_000)
            .unwrap()
            .convert(&interleaved, &mut bulk)
            .unwrap();

        // 奇数長（フレーム境界をまたぐ）で分割して流す。
        let mut split = Vec::new();
        let mut conv = PcmConverter::new(fmt, 16_000).unwrap();
        for chunk in interleaved.chunks(777) {
            conv.convert(chunk, &mut split).unwrap();
        }
        assert_eq!(bulk, split);
    }

    fn rms(samples: &[f32]) -> f32 {
        let sum_sq: f64 = samples.iter().map(|&x| (x as f64) * (x as f64)).sum();
        (sum_sq / samples.len() as f64).sqrt() as f32
    }

    fn zero_crossings(samples: &[f32]) -> usize {
        let mut n = 0;
        for w in samples.windows(2) {
            if (w[0] < 0.0 && w[1] >= 0.0) || (w[0] >= 0.0 && w[1] < 0.0) {
                n += 1;
            }
        }
        n
    }
}
