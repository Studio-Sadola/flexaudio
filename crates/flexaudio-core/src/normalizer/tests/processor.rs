//! Offline processor regressions.
use super::*;

/// InnerProcessor is applied to the internal canonical form before the stage 2 split (output
/// doubles directly on the primary 48k/stereo passthrough path).
#[test]
fn inner_processor_applies_before_stage2() {
    let mut n = Normalizer::new(48_000, 2, default_out())
        .expect("normalizer")
        .with_inner_processor(Box::new(DoubleProcessor));
    let stereo: Vec<f32> = (0..CHUNK_FRAMES * 2).map(|i| (i as f32) * 1e-4).collect();
    n.push(&stereo, 0).expect("push");
    let (chunk, _) = n.pop_chunk().expect("one chunk");
    for (i, &s) in chunk.iter().enumerate() {
        assert!(
            (s - stereo[i] * 2.0).abs() < 1e-6,
            "sample {i} should be doubled"
        );
    }
}

/// InnerProcessor affects both primary and secondary taps (secondary 16k/mono is also doubled).
#[test]
fn inner_processor_affects_both_taps() {
    let secondary = OutputFormat {
        sample_rate: 48_000,
        channels: 2,
    };
    // Set secondary to 48k/stereo (passthrough) so the doubling can be observed directly.
    let mut n = Normalizer::new(48_000, 2, default_out())
        .expect("normalizer")
        .with_secondary(secondary)
        .expect("secondary")
        .with_inner_processor(Box::new(DoubleProcessor));
    let stereo = vec![0.25f32; CHUNK_FRAMES * 2];
    n.push(&stereo, 0).expect("push");
    let (p, _) = n.pop_chunk().expect("primary chunk");
    let (s, _) = n.pop_secondary().expect("secondary chunk");
    assert!(
        p.iter().all(|&x| (x - 0.5).abs() < 1e-6),
        "primary is doubled"
    );
    assert!(
        s.iter().all(|&x| (x - 0.5).abs() < 1e-6),
        "secondary is doubled"
    );
}

/// Stop flush feeds the processor's trailing tail so it can be retrieved as the final chunk.
/// The held portion of the delay-line processor appears in an additional chunk after flush.
#[test]
fn stop_flush_emits_processor_tail() {
    // Delay line holding 4 samples (2 stereo frames).
    let mut n = Normalizer::new(48_000, 2, default_out())
        .expect("normalizer")
        .with_inner_processor(Box::new(DelayProcessor::new(4)));
    // Push one chunk of identifiable, non-zero input.
    let stereo: Vec<f32> = (0..CHUNK_FRAMES * 2)
        .map(|i| (i as f32 + 1.0) * 1e-4)
        .collect();
    n.push(&stereo, 0).expect("push");

    // Before flush: one chunk (the first 4 samples are silent due to the delay).
    let (c0, _) = n.pop_chunk().expect("first chunk");
    assert_eq!(c0.len(), CHUNK_FRAMES * 2);
    assert!(
        c0[..4].iter().all(|&x| x == 0.0),
        "first 4 samples should be silent due to the delay"
    );
    assert!(
        n.pop_chunk().is_none(),
        "only one chunk should be available before flush"
    );

    // Flush emits the delay line's final 4 samples in an additional chunk (padded with silence).
    n.flush().unwrap();
    let (c1, _) = n.pop_chunk().expect("flushed tail chunk");
    assert_eq!(c1.len(), CHUNK_FRAMES * 2, "final chunk is padded to 20ms");
    // The final 4 samples are the last 4 input samples.
    let last4 = &stereo[stereo.len() - 4..];
    for (i, &x) in c1[..4].iter().enumerate() {
        assert!(
            (x - last4[i]).abs() < 1e-6,
            "flush tail should match the end of the input"
        );
    }
}

/// Stop flush also drains the secondary tap's stage 2 resampler remainder (the tail reaches
/// 16k/mono output too).
#[test]
fn stop_flush_drains_secondary_resampler() {
    let secondary = OutputFormat {
        sample_rate: 16_000,
        channels: 1,
    };
    let mut n = Normalizer::new(48_000, 2, default_out())
        .expect("normalizer")
        .with_secondary(secondary)
        .expect("secondary");
    // Push an amount just under one chunk (leaving a remainder in the resampler).
    let stereo = vec![0.3f32; CHUNK_FRAMES * 2];
    n.push(&stereo, 0).expect("push");

    // Count secondary chunks available before flush.
    let mut before = 0usize;
    while n.pop_secondary().is_some() {
        before += 1;
    }
    n.flush().unwrap();
    // Flush drains the remainder, adding at least one final chunk.
    let mut after = 0usize;
    while let Some((c, _)) = n.pop_secondary() {
        assert_eq!(c.len(), 320, "secondary aligns to the fixed 20ms boundary");
        after += 1;
    }
    assert!(after >= 1, "flush should emit the secondary tap's tail");
    let _ = before;
}
