//! Offline conversion regressions.
use super::*;

#[test]
fn mono_48k_to_stereo_duplicates_channels() {
    let mut n = Normalizer::new(48_000, 1, default_out()).expect("normalizer");
    assert!(n.is_passthrough());
    assert!(n.is_output_passthrough());
    // 960 frames of mono input (exactly one chunk because of passthrough).
    let mono: Vec<f32> = (0..CHUNK_FRAMES).map(|i| (i as f32) * 0.001).collect();
    n.push(&mono, 0).expect("push");
    let (chunk, _pts) = n.pop_chunk().expect("one chunk");
    assert_eq!(chunk.len(), CHUNK_FRAMES * 2);
    // L == R holds for every frame.
    for f in 0..CHUNK_FRAMES {
        assert_eq!(chunk[f * 2], chunk[f * 2 + 1], "L==R at frame {f}");
        assert_eq!(chunk[f * 2], mono[f]);
    }
}

#[test]
fn passthrough_preserves_frame_count() {
    let mut n = Normalizer::new(48_000, 2, default_out()).expect("normalizer");
    assert!(n.is_passthrough());
    assert!(n.is_output_passthrough());
    // Two chunks plus a remainder.
    let frames = CHUNK_FRAMES * 2 + 100;
    let stereo: Vec<f32> = (0..frames * 2).map(|i| (i as f32) * 1e-4).collect();
    n.push(&stereo, 0).expect("push");

    let mut got_frames = 0usize;
    while let Some((c, _)) = n.pop_chunk() {
        assert_eq!(c.len(), CHUNK_FRAMES * 2);
        got_frames += CHUNK_FRAMES;
    }
    // Exactly two chunks can be retrieved; 100 remainder frames are left.
    assert_eq!(got_frames, CHUNK_FRAMES * 2);
    assert_eq!(n.buffered_out_frames(), 100);
}

#[test]
fn stereo_44100_to_48000_yields_about_50_chunks_per_second() {
    let mut n = Normalizer::new(44_100, 2, default_out()).expect("normalizer");
    assert!(!n.is_passthrough());

    // One second of 44100Hz stereo sine wave.
    let in_frames = 44_100;
    let freq = 440.0_f32;
    let mut interleaved = Vec::with_capacity(in_frames * 2);
    for i in 0..in_frames {
        let s = (2.0 * PI * freq * (i as f32) / 44_100.0).sin() * 0.5;
        interleaved.push(s); // L
        interleaved.push(s); // R
    }

    // Must not panic even with small, separate pushes (simulating small buffers from real hardware).
    let mut pts = 0i64;
    for block in interleaved.chunks(441 * 2) {
        n.push(block, pts).expect("push");
        pts += (block.len() as i64 / 2) * 1_000_000_000 / 44_100;
    }

    let mut chunks = 0usize;
    while let Some((c, _pts)) = n.pop_chunk() {
        assert_eq!(c.len(), CHUNK_FRAMES * 2);
        chunks += 1;
    }
    assert!(
        (47..=50).contains(&chunks),
        "expected ~50 chunks, got {chunks}"
    );
}

// --- Stage 2 (output) tests ---

/// 48k/stereo input + {16000, 1} output → 320-frame mono chunks.
#[test]
fn output_16k_mono_yields_320_frame_mono_chunks() {
    let out = OutputFormat {
        sample_rate: 16_000,
        channels: 1,
    };
    let mut n = Normalizer::new(48_000, 2, out).expect("normalizer");
    assert!(n.is_passthrough()); // Stage 1 sample-rate passthrough (48k input).
    assert!(!n.is_output_passthrough()); // Stage 2 is active.

    // One second of 48k stereo sine wave (pushed in small blocks).
    let in_frames = 48_000;
    let freq = 440.0_f32;
    let mut pts = 0i64;
    for blk in 0..(in_frames / 480) {
        let mut block = Vec::with_capacity(480 * 2);
        for j in 0..480 {
            let i = blk * 480 + j;
            let s = (2.0 * PI * freq * (i as f32) / 48_000.0).sin() * 0.5;
            block.push(s);
            block.push(s);
        }
        n.push(&block, pts).expect("push");
        pts += 480 * 1_000_000_000 / 48_000;
    }

    let mut chunks = 0usize;
    while let Some((c, _)) = n.pop_chunk() {
        assert_eq!(c.len(), 320, "16k mono 20ms = 320 sample (mono)");
        chunks += 1;
    }
    // 16000/320 = 50 chunks/second; approximately 50 after resampler latency.
    assert!(
        (47..=50).contains(&chunks),
        "expected ~50 chunks, got {chunks}"
    );
}

/// Output {16000, 2} → 320 frames / 640 samples (stereo).
#[test]
fn output_16k_stereo_yields_320_frame_640_sample_chunks() {
    let out = OutputFormat {
        sample_rate: 16_000,
        channels: 2,
    };
    let mut n = Normalizer::new(48_000, 2, out).expect("normalizer");
    let in_frames = 48_000;
    let stereo: Vec<f32> = (0..in_frames * 2)
        .map(|i| ((i / 2) as f32 * 0.0001).sin() * 0.3)
        .collect();
    for block in stereo.chunks(480 * 2) {
        n.push(block, 0).expect("push");
    }
    let mut chunks = 0usize;
    while let Some((c, _)) = n.pop_chunk() {
        assert_eq!(c.len(), 640, "16k stereo 20ms = 320 frame * 2 = 640 sample");
        chunks += 1;
    }
    assert!(
        (47..=50).contains(&chunks),
        "expected ~50 chunks, got {chunks}"
    );
}

/// Output {8000, 2} → 160 frames / 320 samples.
#[test]
fn output_8k_stereo_yields_160_frame_chunks() {
    let out = OutputFormat {
        sample_rate: 8_000,
        channels: 2,
    };
    let mut n = Normalizer::new(48_000, 2, out).expect("normalizer");
    let stereo: Vec<f32> = (0..48_000 * 2)
        .map(|i| (i as f32 * 1e-5).sin() * 0.2)
        .collect();
    for block in stereo.chunks(480 * 2) {
        n.push(block, 0).expect("push");
    }
    let mut chunks = 0usize;
    while let Some((c, _)) = n.pop_chunk() {
        assert_eq!(c.len(), 320, "8k stereo 20ms = 160 frame * 2 = 320 sample");
        chunks += 1;
    }
    assert!(
        (47..=50).contains(&chunks),
        "expected ~50 chunks, got {chunks}"
    );
}

/// stereo→mono averages L/R (opposite phase, L=+a and R=-a, approaches 0).
#[test]
fn stereo_to_mono_is_lr_average() {
    // Use 48000/mono output to test sample-rate passthrough and channel conversion only.
    let out = OutputFormat {
        sample_rate: 48_000,
        channels: 1,
    };
    let mut n = Normalizer::new(48_000, 2, out).expect("normalizer");
    // Perfectly opposite phase (L=+0.5, R=-0.5) → average 0.
    let mut stereo = Vec::with_capacity(CHUNK_FRAMES * 2);
    for _ in 0..CHUNK_FRAMES {
        stereo.push(0.5);
        stereo.push(-0.5);
    }
    n.push(&stereo, 0).expect("push");
    let (chunk, _) = n.pop_chunk().expect("one mono chunk");
    assert_eq!(chunk.len(), CHUNK_FRAMES); // 960 mono samples.
    for &s in &chunk {
        assert!(
            s.abs() < 1e-6,
            "opposite-phase average should be near 0: {s}"
        );
    }
}

/// Resampling a 44.1kHz/mono 440Hz sine wave to 48kHz/stereo preserves its amplitude (RMS)
/// and frequency (estimated by zero crossings). Process one second and measure only the
/// middle chunks to avoid resampler ringing.
#[test]
fn resample_44100_to_48000_preserves_amplitude_and_frequency() {
    let mut n = Normalizer::new(44_100, 1, default_out()).expect("normalizer");
    let freq = 440.0_f32;
    let amp = 0.5_f32;
    let in_rate = 44_100usize;
    // Process two seconds to get enough chunks and leave room to discard transients.
    let total_frames = in_rate * 2;
    let mut pts = 0i64;
    for blk in 0..(total_frames / 441) {
        let mut block = Vec::with_capacity(441);
        for j in 0..441 {
            let i = blk * 441 + j;
            block.push((2.0 * PI * freq * (i as f32) / in_rate as f32).sin() * amp);
        }
        n.push(&block, pts).expect("push");
        pts += 441 * 1_000_000_000 / in_rate as i64;
    }

    // Concatenate all chunks (48k/stereo/960 frames output).
    let mut left: Vec<f32> = Vec::new();
    while let Some((c, _)) = n.pop_chunk() {
        assert_eq!(c.len(), CHUNK_FRAMES * 2);
        // Keep only the L channel (mono→stereo duplication means L==R).
        for f in 0..CHUNK_FRAMES {
            assert_eq!(c[f * 2], c[f * 2 + 1], "L==R for mono input");
            left.push(c[f * 2]);
        }
    }
    assert!(
        left.len() >= 48_000,
        "need at least 1 second of output: {}",
        left.len()
    );

    // Discard 0.25 seconds (12000 samples) of transients at each end and measure the middle second.
    let start = 12_000;
    let mid = &left[start..start + 48_000];

    // Amplitude: sine-wave RMS is amp/√2 ≈ 0.3536. Allow ±5% through the resampler.
    let got_rms = rms(mid);
    let expect_rms = amp / std::f32::consts::SQRT_2;
    let rms_err = ((got_rms - expect_rms) / expect_rms).abs();
    assert!(
        rms_err < 0.05,
        "RMS preservation error too large: got={got_rms} expect={expect_rms} err={rms_err}"
    );

    // Frequency: expect ≈ 2*440 = 880 crossings in the middle second (48000 samples), within ±2%.
    let crossings = zero_crossings(mid);
    let est_freq = crossings as f32 / 2.0; // One second, so crossings/2 = Hz.
    let freq_err = ((est_freq - freq) / freq).abs();
    assert!(
            freq_err < 0.02,
            "Frequency preservation error too large: crossings={crossings} estimate={est_freq}Hz err={freq_err}"
        );
}

/// Verify the channel count and sample values of 16k/mono output: converting 48k/stereo
/// 440Hz input to 16k/mono preserves amplitude and frequency in 1-channel, 320-sample chunks.
#[test]
fn output_16k_mono_preserves_values() {
    let out = OutputFormat {
        sample_rate: 16_000,
        channels: 1,
    };
    let mut n = Normalizer::new(48_000, 2, out).expect("normalizer");
    let freq = 440.0_f32;
    let amp = 0.5_f32;
    let in_rate = 48_000usize;
    let total_frames = in_rate * 2;
    let mut pts = 0i64;
    for blk in 0..(total_frames / 480) {
        let mut block = Vec::with_capacity(480 * 2);
        for j in 0..480 {
            let i = blk * 480 + j;
            let s = (2.0 * PI * freq * (i as f32) / in_rate as f32).sin() * amp;
            block.push(s); // L
            block.push(s); // R
        }
        n.push(&block, pts).expect("push");
        pts += 480 * 1_000_000_000 / in_rate as i64;
    }

    let mut mono: Vec<f32> = Vec::new();
    while let Some((c, _)) = n.pop_chunk() {
        assert_eq!(c.len(), 320, "16k/mono 20ms = 320 samples (1ch)");
        mono.extend_from_slice(&c);
    }
    assert!(
        mono.len() >= 16_000,
        "need at least 1 second: {}",
        mono.len()
    );

    // Discard transients and measure the middle second (16000 samples).
    let start = 4_000;
    let mid = &mono[start..start + 16_000];

    // Averaging in-phase L==R leaves the level unchanged → RMS ≈ amp/√2.
    let got_rms = rms(mid);
    let expect_rms = amp / std::f32::consts::SQRT_2;
    let rms_err = ((got_rms - expect_rms) / expect_rms).abs();
    assert!(
        rms_err < 0.05,
        "16k/mono RMS preservation error: got={got_rms} expect={expect_rms} err={rms_err}"
    );

    // Frequency: expect ≈ 880 crossings in the middle second (16000 samples), within ±2%.
    let est_freq = zero_crossings(mid) as f32 / 2.0;
    let freq_err = ((est_freq - freq) / freq).abs();
    assert!(
        freq_err < 0.02,
        "16k/mono frequency preservation error: estimate={est_freq}Hz err={freq_err}"
    );
}

/// Empty or partial input (less than a multiple of `in_channels`) returns `Ok` without
/// panicking and produces no chunks (boundary defense).
#[test]
fn push_empty_and_subframe_are_noops() {
    let mut n = Normalizer::new(48_000, 2, default_out()).expect("normalizer");
    // Empty input.
    n.push(&[], 0).expect("empty push ok");
    // Only one sample for stereo (2ch) → `in_frames=0`, so return early.
    n.push(&[0.5], 0).expect("subframe push ok");
    assert!(
        n.pop_chunk().is_none(),
        "partial input alone must not produce a chunk"
    );
    assert_eq!(n.buffered_out_frames(), 0);
}

#[test]
fn fragmented_frames_preserve_samples_in_both_taps() {
    let mut normalizer = Normalizer::new(48_000, 2, default_out())
        .expect("normalizer")
        .with_secondary(default_out())
        .expect("secondary");
    let input: Vec<f32> = (0..CHUNK_FRAMES * 2)
        .map(|sample| sample as f32 / 2048.0)
        .collect();
    // Odd sample counts split stereo frames both at the beginning and end
    // of successive pushes. Empty pushes must preserve the retained tail.
    for (index, fragment) in input.chunks(3).enumerate() {
        let pts = (index * 3 / 2) as i64 * 1_000_000_000 / 48_000;
        normalizer.push(fragment, pts).expect("fragment");
        normalizer.push(&[], pts).expect("empty push");
    }
    let (primary, _) = normalizer.pop_chunk().expect("primary chunk");
    let (secondary, _) = normalizer.pop_secondary().expect("secondary chunk");
    assert_eq!(primary, input);
    assert_eq!(secondary, input);
    assert!(normalizer.pop_chunk().is_none());
    assert!(normalizer.pop_secondary().is_none());
}

/// Zero-frequency (silent DC) input produces all-zero output (verifies the peak/RMS-zero path).
#[test]
fn silence_input_yields_zero_output() {
    let mut n = Normalizer::new(48_000, 2, default_out()).expect("normalizer");
    let stereo = vec![0.0f32; CHUNK_FRAMES * 2];
    n.push(&stereo, 0).expect("push");
    let (chunk, _) = n.pop_chunk().expect("one chunk");
    assert!(
        chunk.iter().all(|&s| s == 0.0),
        "silent input must produce silent output"
    );
}

// --- Secondary tap (dual output) tests ---

/// Without a secondary tap, `pop_secondary` always returns `None` and `has_secondary` is false.
#[test]
fn no_secondary_tap_by_default() {
    let mut n = Normalizer::new(48_000, 2, default_out()).expect("normalizer");
    assert!(!n.has_secondary());
    assert_eq!(n.secondary_output(), None);
    let stereo = vec![0.1f32; CHUNK_FRAMES * 2];
    n.push(&stereo, 0).expect("push");
    assert!(n.pop_secondary().is_none(), "no secondary tap means None");
}

/// Generate primary 48k/stereo and secondary 16k/mono from a single stage 1 pass. Primary
/// emits 960-frame stereo chunks and secondary emits 320-frame mono chunks, both at about
/// 50 chunks per second.
#[test]
fn dual_output_primary_and_secondary_shapes() {
    let secondary = OutputFormat {
        sample_rate: 16_000,
        channels: 1,
    };
    let mut n = Normalizer::new(48_000, 2, default_out())
        .expect("normalizer")
        .with_secondary(secondary)
        .expect("secondary");
    assert!(n.has_secondary());
    assert_eq!(n.secondary_output(), Some(secondary));

    // Push one second of 48k/stereo in 480-frame blocks.
    let mut pts = 0i64;
    for _ in 0..100 {
        let block = vec![0.2f32; 480 * 2];
        n.push(&block, pts).expect("push");
        pts += 480 * 1_000_000_000 / 48_000;
    }

    let mut primary_chunks = 0usize;
    while let Some((c, _)) = n.pop_chunk() {
        assert_eq!(
            c.len(),
            CHUNK_FRAMES * 2,
            "primary is 48k/stereo = 1920 samples"
        );
        primary_chunks += 1;
    }
    let mut secondary_chunks = 0usize;
    while let Some((c, _)) = n.pop_secondary() {
        assert_eq!(c.len(), 320, "secondary is 16k/mono = 320 samples");
        secondary_chunks += 1;
    }
    assert!(
        (47..=50).contains(&primary_chunks),
        "primary ~50 chunks: {primary_chunks}"
    );
    assert!(
        (47..=50).contains(&secondary_chunks),
        "secondary ~50 chunks: {secondary_chunks}"
    );
}
