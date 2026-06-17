//! 決定的入力に対する確率プローブ。onnxruntime の数値を stdout に出して外部ツールと
//! 突き合わせられる。
//!
//! 入力: 16000 サンプルの 440Hz サイン波・振幅 0.5 (f32)。
//!   x[i] = 0.5 * sin(2*pi*440*i/16000)
//!
//! 出力: 最初の 10 フレームの生発話確率を改行区切りで stdout へ。
//!
//! 実行: `cargo run -p flexaudio-vad --example probe`

use flexaudio_vad::{Vad, VadConfig};

fn main() {
    // 決定的入力: 440Hz サイン振幅 0.5。
    let n = 16000usize;
    let mut samples = Vec::with_capacity(n);
    for i in 0..n {
        let v = 0.5f32 * (2.0f32 * std::f32::consts::PI * 440.0 * (i as f32) / 16000.0).sin();
        samples.push(v);
    }

    let mut vad = Vad::new(VadConfig::default()).expect("model load");
    // 一括投入。内部で 512 窓に束ねて推論し、生確率を取り出す。
    vad.process(&samples);
    let probs = vad.last_frame_probabilities();

    for p in probs.iter().take(10) {
        println!("{p:.6}");
    }
}
