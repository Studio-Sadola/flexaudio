//! Offline clock regressions.
use super::*;

#[test]
fn pts_increases_monotonically_across_chunks() {
    let mut n = Normalizer::new(48_000, 2, default_out()).expect("normalizer");
    let frames = CHUNK_FRAMES * 3;
    let stereo = vec![0.0f32; frames * 2];
    n.push(&stereo, 100_000_000).expect("push");

    let mut last = i64::MIN;
    let mut count = 0;
    while let Some((_, pts)) = n.pop_chunk() {
        assert!(pts >= last, "pts must be non-decreasing");
        last = pts;
        count += 1;
    }
    assert_eq!(count, 3);
}

/// PTS increases monotonically, and the delta between adjacent chunks is about 20ms
/// (1e7 ns, within tolerance). Check measured values to verify PTS anchor extrapolation on
/// the 48k passthrough path.
#[test]
fn pts_delta_is_about_20ms_between_chunks() {
    let mut n = Normalizer::new(48_000, 2, default_out()).expect("normalizer");
    // Push 480 frames (10ms) at a time with PTS (simulating small buffers from real hardware).
    let mut device_pts = 1_000_000_000i64; // Arbitrary origin.
    let block_frames = 480usize;
    for _ in 0..20 {
        let stereo = vec![0.1f32; block_frames * 2];
        n.push(&stereo, device_pts).expect("push");
        device_pts += block_frames as i64 * 1_000_000_000 / 48_000;
    }

    let mut pts_list = Vec::new();
    while let Some((_, pts)) = n.pop_chunk() {
        pts_list.push(pts);
    }
    assert!(
        pts_list.len() >= 5,
        "need enough chunks: {}",
        pts_list.len()
    );

    // 20ms = 20_000_000 ns. Allow ±5% (1e6 ns).
    for w in pts_list.windows(2) {
        let delta = w[1] - w[0];
        assert!(
            delta > 0,
            "PTS must increase strictly: {} -> {}",
            w[0],
            w[1]
        );
        assert!(
            (delta - 20_000_000).abs() <= 1_000_000,
            "adjacent PTS delta is not about 20ms: {delta} ns"
        );
    }
}

/// Both taps share the PTS axis from the same pushes and increase monotonically with 20ms
/// between adjacent chunks (each tap has an independent PTS anchor).
#[test]
fn dual_output_taps_share_pts_axis() {
    let secondary = OutputFormat {
        sample_rate: 16_000,
        channels: 1,
    };
    let mut n = Normalizer::new(48_000, 2, default_out())
        .expect("normalizer")
        .with_secondary(secondary)
        .expect("secondary");

    let mut device_pts = 1_000_000_000i64;
    for _ in 0..40 {
        let block = vec![0.1f32; 480 * 2];
        n.push(&block, device_pts).expect("push");
        device_pts += 480 * 1_000_000_000 / 48_000;
    }

    let mut primary_pts = Vec::new();
    while let Some((_, p)) = n.pop_chunk() {
        primary_pts.push(p);
    }
    let mut secondary_pts = Vec::new();
    while let Some((_, p)) = n.pop_secondary() {
        secondary_pts.push(p);
    }
    assert!(primary_pts.len() >= 5 && secondary_pts.len() >= 5);
    for w in primary_pts.windows(2) {
        assert!((w[1] - w[0] - 20_000_000).abs() <= 1_000_000);
    }
    for w in secondary_pts.windows(2) {
        assert!((w[1] - w[0] - 20_000_000).abs() <= 1_000_000);
    }
    // The first PTS for both taps starts near the same push origin (within tens of ms).
    assert!(
        (primary_pts[0] - secondary_pts[0]).abs() < 100_000_000,
        "primary and secondary start PTS should be close: {} vs {}",
        primary_pts[0],
        secondary_pts[0]
    );
}
