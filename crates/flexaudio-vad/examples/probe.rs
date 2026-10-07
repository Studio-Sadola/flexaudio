//! Probability probe for deterministic input. Prints inference values to stdout for comparison
//! with external tools.
//!
//! Input: 16,000 samples of a 440 Hz sine wave at amplitude 0.5 (f32).
//!   x[i] = 0.5 * sin(2*pi*440*i/16000)
//!
//! Output: raw speech probabilities for the first 10 frames, one per line.
//!
//! Run: `cargo run -p flexaudio-vad --example probe`

use flexaudio_vad::{Vad, VadConfig};

fn main() {
    // Deterministic input: 440 Hz sine at amplitude 0.5.
    let n = 16000usize;
    let mut samples = Vec::with_capacity(n);
    for i in 0..n {
        let v = 0.5f32 * (2.0f32 * std::f32::consts::PI * 440.0 * (i as f32) / 16000.0).sin();
        samples.push(v);
    }

    let mut vad = Vad::new(VadConfig::default()).expect("model load");
    // Submit all input at once. Internally, it is batched into 512-sample windows for inference; read raw probabilities.
    vad.process(&samples).unwrap();
    let probs = vad.last_frame_probabilities();

    for p in probs.iter().take(10) {
        println!("{p:.6}");
    }
}
