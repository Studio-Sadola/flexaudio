//! flexaudio-denoise — RNNoise (nnnoiseless) によるオフラインのノイズ抑制アドオン。
//!
//! `flexaudio-core` には依存せず、±1.0 正規化・48kHz interleaved の `&[f32]` だけを
//! 受け取る。モデル重みは nnnoiseless クレート (BSD-3-Clause) に埋め込まれているので、
//! 実行時のモデルファイルもネットワークも要らない。マイク録音の定常ノイズ
//! (ファン・空調・キーボード打鍵など) の低減を想定している。
//!
//! # 遅延と持ち越しのセマンティクス
//!
//! RNNoise は 480 サンプル (48kHz で 10ms) 固定のフレームでしか処理できないため、
//! [`Denoiser::process`] は内部でフレームに切り、端数を次回呼び出しへ持ち越す。
//! 呼び出し粒度に依存しない固定遅延の設計で、出力は常に「入力をちょうど
//! [`FRAME_SIZE`] サンプル/ch 遅らせた列」になる:
//!
//! - `process` は渡されたバッファと同じ長さをインプレースで返す。ストリーム先頭の
//!   [`FRAME_SIZE`] サンプル/ch は遅延の詰め物 (無音 0.0)。
//! - [`Denoiser::flush`] が末尾の [`FRAME_SIZE`] サンプル/ch を返してストリームを
//!   閉じる。つまり総出力 = 総入力 + [`FRAME_SIZE`] サンプル/ch。
//!
//! チャンクの切り方を変えても出力列はビット単位で一致する。
//!
//! # 例
//! ```
//! use flexaudio_denoise::{Denoiser, FRAME_SIZE};
//!
//! let mut dn = Denoiser::new(1).unwrap();
//! let mut chunk = vec![0.0f32; 1000]; // ±1.0 正規化 mono 48kHz
//! dn.process(&mut chunk).unwrap();    // インプレース (先頭 480 サンプルは遅延の無音)
//! let tail = dn.flush();              // 残りの 480 サンプル/ch
//! assert_eq!(tail.len(), FRAME_SIZE);
//! ```

#![warn(missing_docs)]

use std::collections::VecDeque;

use nnnoiseless::DenoiseState;

/// RNNoise の 1 フレームのサンプル数/ch (48kHz で 10ms)。処理遅延もこの固定値。
pub const FRAME_SIZE: usize = DenoiseState::FRAME_SIZE;

/// nnnoiseless は i16 レンジ (±32768) スケールの f32 を想定するので、flexaudio の
/// ±1.0 正規化とはこの係数で相互変換する。
const I16_SCALE: f32 = 32768.0;

/// ノイズ抑制のエラー型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenoiseError {
    /// チャンネル数が範囲外 (1..=2 のみ対応)。
    InvalidChannels(u16),
    /// interleaved 長がチャンネル数の倍数でない。
    InvalidLength {
        /// 渡されたスライス長。
        len: usize,
        /// 構築時のチャンネル数。
        channels: u16,
    },
}

impl std::fmt::Display for DenoiseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DenoiseError::InvalidChannels(c) => {
                write!(f, "invalid channel count {c} (expected 1 or 2)")
            }
            DenoiseError::InvalidLength { len, channels } => {
                write!(
                    f,
                    "interleaved length {len} is not a multiple of channel count {channels}"
                )
            }
        }
    }
}

impl std::error::Error for DenoiseError {}

/// ストリーミングのノイズ抑制器。ステレオはチャンネル独立に 2 インスタンスの
/// RNNoise 状態で処理する (チャンネル間のクロストークなし)。
///
/// 遅延と持ち越しの詳細は [crate レベルのドキュメント](crate) を参照。
pub struct Denoiser {
    channels: usize,
    /// チャンネルごとの RNNoise 状態。nnnoiseless に reset が無いので、リセットは
    /// インスタンスの作り直しで行う (モデルはデフォルト埋め込み・決定論)。
    states: Vec<Box<DenoiseState<'static>>>,
    /// チャンネル別の未処理入力 (±1.0 スケールのまま保持)。構築時に FRAME_SIZE 分の
    /// 無音を先詰めしておくことで、遅延線が常に入力と同数を排出できる。
    in_buf: Vec<Vec<f32>>,
    /// 処理済み・未排出の interleaved 出力 (±1.0 スケール・クランプ済み)。
    out_buf: VecDeque<f32>,
    /// フレーム処理の入力スクラッチ (i16 スケール・480 サンプル)。
    frame_in: Vec<f32>,
    /// フレーム処理の出力スクラッチ (チャンネル別・480 サンプル)。
    frame_out: Vec<Vec<f32>>,
}

impl Denoiser {
    /// チャンネル数 (1 = mono, 2 = stereo interleaved) を指定して構築する。
    pub fn new(channels: u16) -> Result<Denoiser, DenoiseError> {
        if !(1..=2).contains(&channels) {
            return Err(DenoiseError::InvalidChannels(channels));
        }
        let ch = channels as usize;
        let mut dn = Denoiser {
            channels: ch,
            states: (0..ch).map(|_| DenoiseState::new()).collect(),
            in_buf: vec![Vec::new(); ch],
            out_buf: VecDeque::new(),
            frame_in: vec![0.0; FRAME_SIZE],
            frame_out: vec![vec![0.0; FRAME_SIZE]; ch],
        };
        dn.prime_delay();
        Ok(dn)
    }

    /// 任意長の interleaved (±1.0 正規化・48kHz) サンプルをインプレースで
    /// ノイズ抑制する。長さはチャンネル数の倍数であること (空スライスは no-op)。
    ///
    /// 内部で [`FRAME_SIZE`] サンプル/ch のフレームに切って処理し、端数は次回へ
    /// 持ち越す。出力は入力を [`FRAME_SIZE`] サンプル/ch 遅らせた列で、ストリーム
    /// 先頭の遅延分は無音 (0.0)。呼び出しの切り方を変えても出力列は変わらない。
    /// 出力サンプルは ±1.0 にクランプされる。
    pub fn process(&mut self, interleaved: &mut [f32]) -> Result<(), DenoiseError> {
        if interleaved.len() % self.channels != 0 {
            return Err(DenoiseError::InvalidLength {
                len: interleaved.len(),
                channels: self.channels as u16,
            });
        }

        // deinterleave して持ち越しバッファへ積む。
        for frame in interleaved.chunks_exact(self.channels) {
            for (ch, &s) in frame.iter().enumerate() {
                self.in_buf[ch].push(s);
            }
        }

        self.process_ready_frames();

        // 遅延線から入力と同数を排出する。先詰めした FRAME_SIZE 分の無音のおかげで
        // 「処理済み ≥ 排出済み + 今回分」が常に成り立つ (先頭無音が遅延になる)。
        for s in interleaved.iter_mut() {
            *s = self
                .out_buf
                .pop_front()
                .expect("delay line must hold enough processed samples");
        }
        Ok(())
    }

    /// 持ち越し中の端数をゼロ詰めで 1 フレームに整えて処理し、遅延分の末尾
    /// [`FRAME_SIZE`] サンプル/ch を interleaved で返してストリームを閉じる。
    ///
    /// これで総出力 = 総入力 + [`FRAME_SIZE`] サンプル/ch (先頭の無音詰め物) になる。
    /// 呼び出し後は [`Denoiser::reset`] と同じ初期状態に戻るので、続けて新しい
    /// ストリームを処理できる。
    pub fn flush(&mut self) -> Vec<f32> {
        if !self.in_buf[0].is_empty() {
            for buf in &mut self.in_buf {
                buf.resize(FRAME_SIZE, 0.0);
            }
            self.process_ready_frames();
        }
        let take = FRAME_SIZE * self.channels;
        let mut out = Vec::with_capacity(take);
        for _ in 0..take {
            // 上記のゼロ詰め処理後、遅延線には必ず take 以上が残っている。
            out.push(self.out_buf.pop_front().unwrap_or(0.0));
        }
        self.reset();
        out
    }

    /// RNN 状態・持ち越しバッファ・遅延線をすべて初期化する。
    ///
    /// reset 後は生成直後と同じ状態で、同一入力からは同一出力が得られる。
    pub fn reset(&mut self) {
        for st in &mut self.states {
            *st = DenoiseState::new();
        }
        for buf in &mut self.in_buf {
            buf.clear();
        }
        self.out_buf.clear();
        self.prime_delay();
    }

    /// 構築時に指定したチャンネル数。
    pub fn channels(&self) -> u16 {
        self.channels as u16
    }

    /// 遅延線の先詰め。各チャンネルの入力バッファに FRAME_SIZE 分の無音を積む。
    /// この無音フレームの処理結果は厳密に 0.0 なので、出力ストリームの先頭
    /// FRAME_SIZE サンプル/ch が「無音の詰め物」になる。
    fn prime_delay(&mut self) {
        for buf in &mut self.in_buf {
            buf.resize(FRAME_SIZE, 0.0);
        }
    }

    /// 揃っている分のフレームをすべて処理して遅延線 (out_buf) に積む。
    fn process_ready_frames(&mut self) {
        // 全チャンネル同数積まれているので先頭チャンネルの長さだけ見ればよい。
        while self.in_buf[0].len() >= FRAME_SIZE {
            for ch in 0..self.channels {
                // ±1.0 → i16 レンジへ拡大して 1 フレーム処理。
                for (dst, &src) in self.frame_in.iter_mut().zip(&self.in_buf[ch][..FRAME_SIZE]) {
                    *dst = src * I16_SCALE;
                }
                self.states[ch].process_frame(&mut self.frame_out[ch], &self.frame_in);
                self.in_buf[ch].drain(..FRAME_SIZE);
            }
            // i16 レンジ → ±1.0 に戻し、interleave して排出待ちに積む。
            for i in 0..FRAME_SIZE {
                for out_ch in &self.frame_out {
                    self.out_buf
                        .push_back((out_ch[i] / I16_SCALE).clamp(-1.0, 1.0));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 決定論の擬似乱数 (LCG・Knuth の MMIX 定数)。rand 依存を避ける。
    struct Lcg(u64);

    impl Lcg {
        /// [0, 1) の一様乱数。上位 24bit を使う。
        fn next_unit(&mut self) -> f32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 40) as f32) / (1u32 << 24) as f32
        }
    }

    /// 振幅 ±amp の一様ホワイトノイズ (シード固定・決定論)。
    fn white_noise(n: usize, amp: f32) -> Vec<f32> {
        let mut lcg = Lcg(0x5EED_1234_5678_9ABC);
        (0..n)
            .map(|_| (lcg.next_unit() * 2.0 - 1.0) * amp)
            .collect()
    }

    /// 一次 IIR ローパス (y += a * (x - y))。ファン・空調のような低域寄りの定常ノイズを
    /// 模すために使う (a=0.1 でカットオフ ~800Hz 相当・決定論)。
    fn lowpass(xs: &[f32], a: f32) -> Vec<f32> {
        let mut y = 0.0f32;
        xs.iter()
            .map(|&x| {
                y += a * (x - y);
                y
            })
            .collect()
    }

    fn sine(n: usize, freq: f32, amp: f32) -> Vec<f32> {
        (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / 48_000.0).sin() * amp)
            .collect()
    }

    fn rms(xs: &[f32]) -> f64 {
        let sum: f64 = xs.iter().map(|&x| (x as f64) * (x as f64)).sum();
        (sum / xs.len() as f64).sqrt()
    }

    /// 一括処理して「遅延分を除いて入力に整列した」出力列 (入力と同じ長さ) を返す。
    fn run_aligned(channels: u16, input: &[f32]) -> Vec<f32> {
        let mut dn = Denoiser::new(channels).unwrap();
        let mut buf = input.to_vec();
        dn.process(&mut buf).unwrap();
        buf.extend_from_slice(&dn.flush());
        // 先頭 FRAME_SIZE サンプル/ch (interleaved で FRAME_SIZE*ch) が遅延の無音。
        buf.split_off(FRAME_SIZE * channels as usize)
    }

    /// 閾値決めの実測用 (通常は走らせない): 各テスト信号の RMS 比を出力する。
    /// `cargo test -p flexaudio-denoise -- --ignored --nocapture measure` で実行。
    #[test]
    #[ignore]
    fn measure_rms_ratios() {
        let white = white_noise(96_000, 0.3);
        let fan = lowpass(&white, 0.1);
        let tone = sine(96_000, 440.0, 0.5);
        for (name, input) in [
            ("white noise amp=0.3", &white),
            ("lowpass(a=0.1) noise", &fan),
            ("440Hz sine amp=0.5", &tone),
        ] {
            let output = run_aligned(1, input);
            println!(
                "{name}: in_rms={:.6} out_rms={:.6} ratio={:.6}",
                rms(input),
                rms(&output),
                rms(&output) / rms(input)
            );
        }
    }

    #[test]
    fn stationary_noise_rms_strongly_reduced() {
        // 2 秒 @48k mono の低域寄り定常ノイズ (LCG ホワイトノイズの一次ローパス)。
        // ファン・空調に近い、このクレートの本来の対象。measure_rms_ratios の実測で
        // ratio = 0.0556 (in_rms 0.0399 → out_rms 0.0022、後半 1 秒に限れば 0.006)。
        // 実測の約 4.5 倍のマージンを取って閾値 25% とする。
        let input = lowpass(&white_noise(96_000, 0.3), 0.1);
        let output = run_aligned(1, &input);
        let (in_rms, out_rms) = (rms(&input), rms(&output));
        assert!(
            out_rms < in_rms * 0.25,
            "stationary noise must be strongly attenuated: \
             in_rms={in_rms:.4} out_rms={out_rms:.4}"
        );
    }

    #[test]
    fn white_noise_rms_reduced() {
        // 2 秒 @48k mono のフルバンドホワイトノイズ (振幅 ±0.3)。学習分布から遠い
        // 合成ノイズなので RNNoise の抑制は弱く、実測 ratio = 0.7883 に留まる
        // (定常ノイズへの実効性は stationary_noise_rms_strongly_reduced が担う)。
        // ここでは「増幅せず一定の低減はある」ことだけを実測 + 余裕の 90% で確認する。
        let input = white_noise(96_000, 0.3);
        let output = run_aligned(1, &input);
        let (in_rms, out_rms) = (rms(&input), rms(&output));
        assert!(
            out_rms < in_rms * 0.90,
            "white noise must not be amplified: in_rms={in_rms:.4} out_rms={out_rms:.4}"
        );
    }

    #[test]
    fn sine_output_sane() {
        // 440Hz 正弦 (振幅 0.5) 2 秒。実測では ratio = 0.9999 とほぼ素通し
        // (RNNoise は周期信号を有声とみなす)。とはいえ純音の扱いはモデル依存なので、
        // アサーションは健全性 (NaN なし・±1.0 内) と「消えないこと」= 実測の半分の
        // 50% 床に留める。
        let input = sine(96_000, 440.0, 0.5);
        let output = run_aligned(1, &input);
        assert!(
            output.iter().all(|x| x.is_finite()),
            "output must not contain NaN/inf"
        );
        assert!(
            output.iter().all(|&x| (-1.0..=1.0).contains(&x)),
            "output must stay within +/-1.0"
        );
        let (in_rms, out_rms) = (rms(&input), rms(&output));
        assert!(
            out_rms > in_rms * 0.50,
            "sine must pass through mostly intact: in_rms={in_rms:.4} out_rms={out_rms:.4}"
        );
    }

    #[test]
    fn first_delay_block_is_silence() {
        // 出力ストリームの先頭 FRAME_SIZE サンプル/ch は遅延の詰め物で厳密に 0.0。
        let mut dn = Denoiser::new(1).unwrap();
        let mut buf = white_noise(FRAME_SIZE * 2, 0.3);
        dn.process(&mut buf).unwrap();
        assert!(
            buf[..FRAME_SIZE].iter().all(|&x| x == 0.0),
            "first FRAME_SIZE output samples must be exactly zero"
        );
    }

    #[test]
    fn chunked_equals_oneshot() {
        // 480 の倍数でない 1000 サンプル刻みで流しても、一括処理と出力がビット単位で
        // 一致する (呼び出し粒度非依存の検証)。総サンプル数の整合も同時に確認。
        let input = white_noise(96_000, 0.3);

        let mut oneshot = input.clone();
        let mut dn1 = Denoiser::new(1).unwrap();
        dn1.process(&mut oneshot).unwrap();
        let tail1 = dn1.flush();

        let mut chunked = Vec::with_capacity(input.len());
        let mut dn2 = Denoiser::new(1).unwrap();
        for chunk in input.chunks(1000) {
            let mut buf = chunk.to_vec();
            dn2.process(&mut buf).unwrap();
            assert_eq!(
                buf.len(),
                chunk.len(),
                "process must emit in place, same length"
            );
            chunked.extend_from_slice(&buf);
        }
        let tail2 = dn2.flush();

        assert_eq!(
            chunked.len(),
            input.len(),
            "total process output == total input"
        );
        assert_eq!(
            tail1.len(),
            FRAME_SIZE,
            "flush must emit exactly FRAME_SIZE per channel"
        );
        assert_eq!(
            oneshot, chunked,
            "chunk granularity must not change the output"
        );
        assert_eq!(tail1, tail2, "flush residue must also match");
    }

    #[test]
    fn stereo_keeps_channels_independent_and_interleaved() {
        // L = ノイズ, R = 無音の interleaved ステレオ。R 出力は厳密に 0 のまま、
        // L 出力は同じ信号を mono で処理した結果とビット単位で一致する
        // (interleave の維持とチャンネル独立性の両方を検証)。
        let n = 48_000; // 1 秒/ch。1000 サンプル刻み (=500/ch, 480 の倍数でない) で流す。
        let left = white_noise(n, 0.3);
        let mut stereo = Vec::with_capacity(n * 2);
        for &l in &left {
            stereo.push(l);
            stereo.push(0.0);
        }

        let mut dn = Denoiser::new(2).unwrap();
        let mut stereo_out = Vec::with_capacity(stereo.len());
        for chunk in stereo.chunks(1000) {
            let mut buf = chunk.to_vec();
            dn.process(&mut buf).unwrap();
            stereo_out.extend_from_slice(&buf);
        }
        let tail = dn.flush();
        assert_eq!(
            tail.len(),
            FRAME_SIZE * 2,
            "stereo flush is FRAME_SIZE per channel"
        );
        stereo_out.extend_from_slice(&tail);

        let left_out: Vec<f32> = stereo_out.iter().step_by(2).copied().collect();
        let right_out: Vec<f32> = stereo_out.iter().skip(1).step_by(2).copied().collect();
        assert!(
            right_out.iter().all(|&x| x == 0.0),
            "silent right channel must stay exactly zero (no crosstalk)"
        );

        let mut mono_ref = left.clone();
        let mut dn_mono = Denoiser::new(1).unwrap();
        dn_mono.process(&mut mono_ref).unwrap();
        mono_ref.extend_from_slice(&dn_mono.flush());
        assert_eq!(
            left_out, mono_ref,
            "stereo left must equal the mono reference"
        );
    }

    #[test]
    fn reset_restores_initial_state() {
        // reset 後に同一入力を流すと同一出力 (ビット単位)。flush の自動リセットも同様。
        let input = white_noise(10_000, 0.3);

        let mut dn = Denoiser::new(1).unwrap();
        let mut first = input.clone();
        dn.process(&mut first).unwrap();

        dn.reset();
        let mut second = input.clone();
        dn.process(&mut second).unwrap();
        assert_eq!(first, second, "reset must restore the initial state");

        // flush はストリームを閉じたあと reset と同じ初期状態に戻す。
        dn.flush();
        let mut third = input.clone();
        dn.process(&mut third).unwrap();
        assert_eq!(first, third, "flush must leave the denoiser reusable");
    }

    #[test]
    fn rejects_invalid_channels() {
        assert_eq!(
            Denoiser::new(0).err(),
            Some(DenoiseError::InvalidChannels(0))
        );
        assert_eq!(
            Denoiser::new(3).err(),
            Some(DenoiseError::InvalidChannels(3))
        );
    }

    #[test]
    fn rejects_misaligned_length() {
        let mut dn = Denoiser::new(2).unwrap();
        let mut buf = vec![0.0f32; 999]; // 2ch の倍数でない
        let err = dn.process(&mut buf).unwrap_err();
        assert_eq!(
            err,
            DenoiseError::InvalidLength {
                len: 999,
                channels: 2
            }
        );
        // エラー時はバッファに触れない。
        assert!(buf.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn empty_process_and_bare_flush() {
        let mut dn = Denoiser::new(1).unwrap();
        let mut empty: [f32; 0] = [];
        dn.process(&mut empty).unwrap(); // 空は no-op
        assert_eq!(dn.channels(), 1);

        // 入力ゼロのまま flush しても遅延分 (無音) がちょうど返る。
        let tail = dn.flush();
        assert_eq!(tail.len(), FRAME_SIZE);
        assert!(tail.iter().all(|&x| x == 0.0));
    }
}
