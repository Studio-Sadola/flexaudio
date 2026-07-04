//! 独立したノイズ抑制クラス [`Denoiser`]。
//!
//! RNNoise（nnnoiseless）によるオフラインのノイズ抑制アドオン（[`flexaudio_denoise`]）の
//! Python 露出。

use pyo3::prelude::*;

use flexaudio_denoise::Denoiser as CoreDenoiser;

use crate::denoise_err_to_py;

/// ストリーミングのノイズ抑制器。
///
/// **48kHz 前提**: 入力は ±1.0 正規化・48kHz・interleaved の f32 でなければならない
/// （RNNoise が 48kHz 固定フレームでしか動かないため）。ステレオはチャンネル独立に処理する。
///
/// 遅延: 出力は入力を 480 サンプル/ch（48kHz で 10ms）遅らせた列になる。ストリーム先頭の
/// 480 サンプル/ch は遅延の詰め物（無音）で、末尾の残りは [`flush`](Denoiser::flush) が返す。
#[pyclass(module = "flexaudio", name = "Denoiser")]
pub struct Denoiser {
    inner: CoreDenoiser,
}

#[pymethods]
impl Denoiser {
    /// チャンネル数（1 = mono / 2 = stereo interleaved）を指定して構築する。1..=2 以外は
    /// `ValueError`。
    #[new]
    fn new(channels: u16) -> PyResult<Self> {
        let inner = CoreDenoiser::new(channels).map_err(denoise_err_to_py)?;
        Ok(Denoiser { inner })
    }

    /// 任意長の interleaved（±1.0 正規化・48kHz）サンプルをノイズ抑制して返す。
    ///
    /// 長さはチャンネル数の倍数であること（そうでなければ `ValueError`）。端数は次回へ持ち
    /// 越すので、任意の位置で分割して連続で渡してよい。`samples` は list / array.array /
    /// numpy 配列いずれも渡せる。返すのは同じ長さのノイズ抑制後サンプル（先頭は遅延の無音）。
    fn process(&mut self, mut samples: Vec<f32>) -> PyResult<Vec<f32>> {
        self.inner
            .process(&mut samples)
            .map_err(denoise_err_to_py)?;
        Ok(samples)
    }

    /// 持ち越し中の端数を処理し、遅延分の末尾 480 サンプル/ch を返してストリームを閉じる。
    /// 呼び出し後は [`reset`](Denoiser::reset) と同じ初期状態に戻る（続けて再利用できる）。
    fn flush(&mut self) -> Vec<f32> {
        self.inner.flush()
    }

    /// 状態・持ち越しバッファ・遅延線をすべて初期化する（同一入力から同一出力になる）。
    fn reset(&mut self) {
        self.inner.reset();
    }

    /// 構築時に指定したチャンネル数。
    fn channels(&self) -> u16 {
        self.inner.channels()
    }
}
