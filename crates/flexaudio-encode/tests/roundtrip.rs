//! Tests for FlacWriter round trips, error cases, and compression effectiveness.
//!
//! All tests are deterministic: signals use fixed formulas and do not depend on time or scheduling.
//! Decode with the pure-Rust claxon crate and compare every written value.

use std::path::PathBuf;

use flexaudio_encode::{EncodeError, FlacWriter};

/// Create a unique output path for each test under CARGO_TARGET_TMPDIR.
fn tmp_path(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// Independent test-side reference implementation of the library's quantization formula.
fn quantize(x: f32) -> i32 {
    (x * 32768.0).round().clamp(-32768.0, 32767.0) as i32
}

/// One second of a 440 Hz sine wave at amplitude 0.5, in mono.
fn sine_440(rate: u32) -> Vec<f32> {
    (0..rate as usize)
        .map(|t| 0.5 * (2.0 * std::f32::consts::PI * 440.0 * t as f32 / rate as f32).sin())
        .collect()
}

/// One second of stereo (interleaved): L is a 440 Hz sine wave; R is constant at 0.25.
/// The constant channel also exercises flacenc's Constant subframe path.
fn stereo_test_signal(rate: u32) -> Vec<f32> {
    let sine = sine_440(rate);
    let mut out = Vec::with_capacity(sine.len() * 2);
    for s in sine {
        out.push(s);
        out.push(0.25);
    }
    out
}

/// Shared helper that decodes the written signal with claxon and checks every value.
///
/// - Sample counts match.
/// - Quantized values match exactly (deterministic).
/// - Quantization error relative to the original f32 is at most ±1/32768 + ε.
fn assert_roundtrip(path: &PathBuf, signal: &[f32], rate: u32, channels: u32) {
    let mut reader = claxon::FlacReader::open(path).unwrap();
    let info = reader.streaminfo();
    assert_eq!(info.sample_rate, rate);
    assert_eq!(info.channels, channels);
    assert_eq!(info.bits_per_sample, 16);
    assert_eq!(
        info.samples,
        Some((signal.len() / channels as usize) as u64),
        "STREAMINFO total sample count"
    );

    let decoded: Vec<i32> = reader.samples().map(|s| s.unwrap()).collect();
    assert_eq!(decoded.len(), signal.len(), "decoded total sample count");

    let tol = 1.0 / 32768.0 + 1e-6;
    for (i, (&orig, &dec)) in signal.iter().zip(decoded.iter()).enumerate() {
        assert_eq!(
            dec,
            quantize(orig),
            "sample {i}: quantized values match exactly"
        );
        let restored = dec as f32 / 32768.0;
        assert!(
            (restored - orig).abs() <= tol,
            "sample {i}: orig={orig} restored={restored}"
        );
    }
}

#[test]
fn roundtrip_stereo_sine_and_constant() {
    let rate = 48_000u32;
    let path = tmp_path("roundtrip_stereo.flac");
    let signal = stereo_test_signal(rate);

    let mut writer = FlacWriter::create(&path, rate, 2).unwrap();
    // Feed small pieces like flexaudio's 20 ms chunks (960 frames × 2ch = 1920 elements).
    // 1920 does not align with the block boundary (4096×2=8192), exercising the remainder path.
    for chunk in signal.chunks(960 * 2) {
        writer.write_chunk(chunk).unwrap();
    }
    writer.finalize().unwrap();

    // 48000 frames = 4096×11 + 2944, leaving a partial final frame.
    assert_roundtrip(&path, &signal, rate, 2);
}

#[test]
fn roundtrip_mono_sine() {
    let rate = 44_100u32;
    let path = tmp_path("roundtrip_mono.flac");
    let signal = sine_440(rate);

    let mut writer = FlacWriter::create(&path, rate, 1).unwrap();
    // A chunk length that aligns with neither a block nor 20 ms.
    for chunk in signal.chunks(1000) {
        writer.write_chunk(chunk).unwrap();
    }
    writer.finalize().unwrap();

    assert_roundtrip(&path, &signal, rate, 1);
}

#[test]
fn roundtrip_exact_block_multiple() {
    // Exactly two blocks, exercising finalize with no remainder.
    let rate = 16_000u32;
    let path = tmp_path("roundtrip_exact.flac");
    let signal: Vec<f32> = (0..4096 * 2)
        .map(|t| if t % 2 == 0 { 0.125 } else { -0.125 })
        .collect();

    let mut writer = FlacWriter::create(&path, rate, 1).unwrap();
    writer.write_chunk(&signal).unwrap();
    writer.finalize().unwrap();

    assert_roundtrip(&path, &signal, rate, 1);
}

#[test]
fn roundtrip_tail_shorter_than_16_samples() {
    // The header must remain valid when the final partial frame has fewer than 16 samples
    // (claxon rejects a remainder counted toward STREAMINFO's minimum block size).
    let rate = 16_000u32;
    let path = tmp_path("roundtrip_tiny_tail.flac");
    let signal: Vec<f32> = (0..4096 + 7)
        .map(|t| ((t % 100) as f32 - 50.0) / 128.0)
        .collect();

    let mut writer = FlacWriter::create(&path, rate, 1).unwrap();
    writer.write_chunk(&signal).unwrap();
    writer.finalize().unwrap();

    assert_roundtrip(&path, &signal, rate, 1);
}

#[test]
fn empty_stream_is_valid_flac() {
    let path = tmp_path("empty.flac");
    let writer = FlacWriter::create(&path, 48_000, 2).unwrap();
    writer.finalize().unwrap();

    let mut reader = claxon::FlacReader::open(&path).unwrap();
    let info = reader.streaminfo();
    assert_eq!(info.sample_rate, 48_000);
    assert_eq!(info.channels, 2);
    // In FLAC, a total sample count of 0 means “unknown,” so claxon returns None.
    assert_eq!(info.samples, None);
    assert_eq!(reader.samples().count(), 0);
}

#[test]
fn drop_without_finalize_closes_best_effort() {
    let rate = 16_000u32;
    let path = tmp_path("drop_finalize.flac");
    // 4096 + 904 = 2 frames, with a remainder.
    let signal: Vec<f32> = vec![0.1; 5000];
    {
        let mut writer = FlacWriter::create(&path, rate, 1).unwrap();
        writer.write_chunk(&signal).unwrap();
        // Drop without finalizing.
    }
    assert_roundtrip(&path, &signal, rate, 1);
}

#[test]
fn compresses_below_90_percent_of_raw_pcm() {
    // One second of a sine wave. A loose sanity check that compression brings it below
    // 90% of raw 16-bit PCM (sample count × 2 bytes); sine waves compress much better in practice.
    let rate = 48_000u32;
    let path = tmp_path("compression.flac");
    let signal = stereo_test_signal(rate);

    let mut writer = FlacWriter::create(&path, rate, 2).unwrap();
    writer.write_chunk(&signal).unwrap();
    writer.finalize().unwrap();

    let raw_bytes = signal.len() * 2;
    let flac_bytes = std::fs::metadata(&path).unwrap().len() as usize;
    assert!(
        flac_bytes * 10 < raw_bytes * 9,
        "flac={flac_bytes} bytes, raw 16bit PCM={raw_bytes} bytes"
    );
}

#[test]
fn rejects_zero_channels() {
    // Validation happens before file creation, so a missing directory does not produce an Io error.
    let err = FlacWriter::create("/nonexistent-dir/never.flac", 48_000, 0).unwrap_err();
    assert!(matches!(err, EncodeError::Unsupported(_)), "{err:?}");
}

#[test]
fn rejects_three_channels() {
    let err = FlacWriter::create("/nonexistent-dir/never.flac", 48_000, 3).unwrap_err();
    assert!(matches!(err, EncodeError::Unsupported(_)), "{err:?}");
}

#[test]
fn rejects_unsupported_sample_rate() {
    let err = FlacWriter::create("/nonexistent-dir/never.flac", 0, 2).unwrap_err();
    assert!(matches!(err, EncodeError::Unsupported(_)), "{err:?}");
    let err = FlacWriter::create("/nonexistent-dir/never.flac", 192_000, 2).unwrap_err();
    assert!(matches!(err, EncodeError::Unsupported(_)), "{err:?}");
}

#[test]
fn rejects_chunk_length_not_multiple_of_channels() {
    let path = tmp_path("bad_chunk_len.flac");
    let mut writer = FlacWriter::create(&path, 48_000, 2).unwrap();
    let err = writer.write_chunk(&[0.0; 3]).unwrap_err();
    assert!(matches!(err, EncodeError::Unsupported(_)), "{err:?}");
    // An error writes nothing, and a valid length can still be written afterward.
    writer.write_chunk(&[0.0; 4]).unwrap();
    writer.finalize().unwrap();

    let mut reader = claxon::FlacReader::open(&path).unwrap();
    assert_eq!(reader.samples().count(), 4);
}
