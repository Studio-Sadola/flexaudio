//! Offline gain regressions.
use super::*;

// --- Input gain (config.gain / set_gain) ---

/// Verify that the gain from config affects completed chunk data and peak/rms meters. The
/// MockBackend sine wave has amplitude 0.5, so gain 2.0 gives a chunk peak of about 1.0 and gain
/// 0.5 gives about 0.25. Also verify that peak is computed from data after gain (the meter shows
/// the actual post-gain level).
#[test]
fn gain_scales_samples_and_meters() {
    // (gain, expected peak range). Sine amplitude 0.5 × gain.
    for (gain, lo, hi) in [(2.0f32, 0.95f32, 1.0f32), (0.5, 0.2, 0.3)] {
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        let config = StreamConfig {
            gain,
            ..Default::default()
        };
        let mut stream = Stream::open(config, backend).expect("open");
        stream.start().expect("start");
        let chunks = collect_for(&mut stream, Duration::from_millis(300));
        stream.stop();
        assert!(!chunks.is_empty(), "expected chunks with gain={gain}");

        // peak matches the data after gain is applied (the meter reports the actual post-gain level).
        let mut max_peak = 0.0f32;
        for c in &chunks {
            let recomputed = c.data.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
            assert_eq!(
                c.peak, recomputed,
                "peak should be computed from data after gain is applied"
            );
            max_peak = max_peak.max(c.peak);
        }
        assert!(
            (lo..=hi).contains(&max_peak),
            "expected peak for gain={gain} in {lo}..={hi}: {max_peak}"
        );
    }
}

/// set_gain takes effect on the next chunk during recording. Start at 1.0, receive a chunk, then
/// call set_gain(0.0) and verify that subsequent chunks have all-zero samples and peak 0.
#[test]
fn set_gain_takes_effect_mid_stream() {
    let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
    let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
    stream.start().expect("start");
    assert_eq!(stream.gain(), 1.0, "default gain is 1.0");

    // First wait for a normal chunk.
    let got_before = wait_until(|| stream.poll_chunk().is_some(), Duration::from_secs(2));
    assert!(got_before, "expected a chunk before set_gain");

    // Set gain to 0.0 (silence); it takes effect from the next completed chunk (20 ms granularity).
    stream.set_gain(0.0).expect("set_gain(0.0)");
    assert_eq!(stream.gain(), 0.0);

    // Chunks completed before the setting may still arrive, so wait for a silent chunk.
    let got_silent = wait_until(
        || matches!(stream.poll_chunk(), Some(c) if c.peak == 0.0),
        Duration::from_secs(2),
    );
    assert!(got_silent, "expected a silent chunk after set_gain(0.0)");

    // Subsequent chunks should retain all-zero samples, peak 0, and rms 0.
    let after = collect_for(&mut stream, Duration::from_millis(300));
    stream.stop();
    assert!(
        !after.is_empty(),
        "chunks should continue to flow during silence"
    );
    for c in &after {
        assert!(
            c.data.iter().all(|&x| x == 0.0),
            "all samples should be 0 at gain 0.0"
        );
        assert_eq!(c.peak, 0.0);
        assert_eq!(c.rms, 0.0);
    }
}

/// Samples are clamped to ±1.0 even at high gain. With sine amplitude 0.5 × gain 100, samples
/// would reach 50 without clamping, but all stay within ±1.0 and the peak is exactly 1.0.
#[test]
fn gain_clamps_to_unit_range() {
    let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
    let config = StreamConfig {
        gain: 100.0,
        ..Default::default()
    };
    let mut stream = Stream::open(config, backend).expect("open");
    stream.start().expect("start");
    let chunks = collect_for(&mut stream, Duration::from_millis(300));
    stream.stop();
    assert!(!chunks.is_empty(), "expected chunks to arrive");

    let mut max_peak = 0.0f32;
    for c in &chunks {
        assert!(
            c.data.iter().all(|&x| (-1.0..=1.0).contains(&x)),
            "samples should not exceed ±1.0"
        );
        max_peak = max_peak.max(c.peak);
    }
    assert_eq!(max_peak, 1.0, "clamping should make the peak exactly 1.0");
}
