//! Probability probe on a deterministic input. Prints the inference engine's numbers to stdout
//! so they can be cross-checked against external tools.
//!
//! Input: 16000 samples of a 440Hz sine wave, amplitude 0.5 (f32).
//!   x[i] = 0.5 * sin(2*pi*440*i/16000)
//!
//! Output: the raw speech probabilities of the first 10 frames, newline-separated, to stdout.
//!
//! Run: `cargo run -p flexaudio-vad --example probe`

use flexaudio_vad::{Vad, VadConfig};

fn main() {
    // Deterministic input: 440Hz sine, amplitude 0.5.
    let n = 16000usize;
    let mut samples = Vec::with_capacity(n);
    for i in 0..n {
        let v = 0.5f32 * (2.0f32 * std::f32::consts::PI * 440.0 * (i as f32) / 16000.0).sin();
        samples.push(v);
    }

    let mut vad = Vad::new(VadConfig::default()).expect("model load");
    // Feed everything at once. Internally it is grouped into 512 windows for inference, and
    // the raw probabilities are taken out.
    vad.process(&samples);
    let probs = vad.last_frame_probabilities();

    for p in probs.iter().take(10) {
        println!("{p:.6}");
    }
}
