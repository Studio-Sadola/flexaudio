//! Offline audit reproductions. No devices are opened.
use super::*;

#[test]
fn legacy_pts_arithmetic_is_pinned_across_rebuild() {
    let secondary = OutputFormat {
        sample_rate: 16_000,
        channels: 1,
    };
    let mut n = Normalizer::new(48_000, 2, output())
        .unwrap()
        .with_secondary(secondary)
        .unwrap();
    n.push(&vec![0.25; 480 * 2], 0).unwrap();
    n.push(&vec![0.25; 2400 * 2], 10_000_000).unwrap();
    let mut primary: Vec<_> = std::iter::from_fn(|| n.pop_chunk())
        .map(|(_, pts)| pts)
        .collect();
    let mut second: Vec<_> = std::iter::from_fn(|| n.pop_secondary())
        .map(|(_, pts)| pts.max(0))
        .collect();
    n = Normalizer::new(48_000, 2, output())
        .unwrap()
        .with_secondary(secondary)
        .unwrap();
    n.push(&vec![0.25; 2880 * 2], 100_000_000).unwrap();
    primary.extend(std::iter::from_fn(|| n.pop_chunk()).map(|(_, pts)| pts));
    second.extend(std::iter::from_fn(|| n.pop_secondary()).map(|(_, pts)| pts));
    // aecdc40: frame_delta * (1_000_000_000 / rate), with each push's
    // projected output anchor. Stream clock normalization subtracts the
    // first primary timestamp (160 ns), shared by both output taps.
    let epoch = primary[0];
    primary.iter_mut().for_each(|pts| *pts -= epoch);
    second
        .iter_mut()
        .for_each(|pts| *pts = (*pts - epoch).max(0));
    assert_eq!(
        primary,
        [
            0,
            19_999_680,
            39_999_360,
            99_999_840,
            119_999_520,
            139_999_200
        ]
    );
    assert_eq!(second, [0, 19_999_840, 99_999_840, 119_999_840]);
}

#[test]
fn capture_failure_restores_scratch_and_still_feeds_secondary() {
    let mut n = Normalizer::new(48_000, 2, output())
        .unwrap()
        .with_secondary(output())
        .unwrap()
        .with_capture_tap()
        .unwrap();
    let mut failing = OutputTap::new(OutputFormat {
        sample_rate: 16_000,
        channels: 1,
    })
    .unwrap();
    failing
        .stage2
        .as_mut()
        .unwrap()
        .resampler
        .as_mut()
        .unwrap()
        .out_scratch
        .clear();
    n.capture = Some(failing);
    n.inner_scratch = vec![0.25; 1920];
    let capacity = n.inner_scratch.capacity();
    assert!(n.distribute_inner().is_err());
    assert_eq!(n.inner_scratch.capacity(), capacity);
    assert_eq!(n.pop_secondary().unwrap().0, vec![0.25; 1920]);
}

#[test]
fn flush_drains_capture_converter_without_transport_padding() {
    let mut n = Normalizer::new(48_000, 2, output())
        .unwrap()
        .with_capture_tap()
        .unwrap();
    n.capture = Some(
        OutputTap::new(OutputFormat {
            sample_rate: 16_000,
            channels: 2,
        })
        .unwrap(),
    );
    n.push(&[0.25; 200], 0).unwrap();
    n.flush().unwrap();
    assert!(n.capture.as_ref().unwrap().buffered_out_frames() > 0);
}

#[test]
fn canonical_anchor_overflow_returns_a_typed_error() {
    let mut tap = OutputTap::new(OutputFormat {
        sample_rate: 192_000,
        channels: 2,
    })
    .unwrap();
    tap.canonical_clock = true;
    assert!(matches!(
        tap.update_pts_anchor(u64::MAX, 0),
        Err(Error::InvalidState(_))
    ));
    assert!(tap.pts_anchor.is_none());
}

fn output() -> OutputFormat {
    OutputFormat {
        sample_rate: 48_000,
        channels: 2,
    }
}
fn short_tail(rate: u32) {
    let mut n = Normalizer::new(rate, 2, output()).unwrap();
    n.push(&vec![0.25; 200], 0).unwrap();
    n.flush().unwrap();
    let buffered = n.buffered_out_frames();
    assert!(buffered > 0, "F13: 100 real input frames vanished after flush: output_frames={buffered}, stage1_pending={}", n.stage1_resampler.as_ref().map_or(0, |r| r.in_accum.len() / 2));
}
#[test]
fn repro_p1_short_44100_tail() {
    short_tail(44_100);
}
#[test]
fn repro_p1_short_44100_tail_control() {
    short_tail(48_000);
}

fn multichannel(channel: usize) {
    let mut input = vec![0.0; 960 * 6];
    for frame in input.chunks_exact_mut(6) {
        frame[channel] = 0.5;
    }
    let result = Normalizer::new(48_000, 6, output());
    assert!(
        matches!(result, Err(Error::UnsupportedFormat(_))),
        "six-channel input must be rejected before consuming either center or front PCM"
    );
}
#[test]
fn repro_p1_center_channel() {
    multichannel(2);
}
#[test]
fn repro_p1_center_channel_control() {
    multichannel(0);
}

fn partial_frame(split: bool) {
    let mut n = Normalizer::new(48_000, 2, output()).unwrap();
    // A valid complete stereo frame enters the public raw ring. Its sample-based
    // consumer can split that frame; Normalizer must retain the remainder if
    // the transport/normalizer combination is to preserve the real samples.
    let (mut producer, mut consumer) = crate::raw_ring::raw_ring(2);
    assert_eq!(producer.push_slice(&[0.25, 0.75]), 2);
    let mut scratch = vec![0.0; if split { 1 } else { 2 }];
    loop {
        let got = consumer.pop_slice(&mut scratch);
        if got == 0 {
            break;
        }
        n.push(&scratch[..got], 0).unwrap();
    }
    n.flush().unwrap();
    assert!(
        n.pop_chunk().is_some(),
        "H8: partial input frames discarded rather than retained: output_frames={}",
        n.buffered_out_frames()
    );
}
#[test]
fn repro_p1_partial_frames() {
    partial_frame(true);
}
#[test]
fn repro_p1_partial_frames_control() {
    partial_frame(false);
}

// Fault injection changes only rubato's output storage, leaving its valid format
// and buffered real samples intact. Verify the precise returned adapter error
// before exercising the public best-effort flush path with the same fault.
fn flush_error(inject: bool) {
    let format = OutputFormat {
        sample_rate: 16_000,
        channels: 1,
    };
    let mut n = Normalizer::new(48_000, 2, format).unwrap();
    // Retain a completed valid chunk before injecting the original adapter failure.
    n.push(&vec![0.25; 5760], 0).unwrap();
    n.push(&vec![0.25; 200], 60_000_000).unwrap();
    if inject {
        n.primary
            .stage2
            .as_mut()
            .unwrap()
            .resampler
            .as_mut()
            .unwrap()
            .out_scratch
            .clear();
    }
    let result = n.flush();
    assert_eq!(
        result.is_err(),
        inject,
        "injected adapter failure must be returned by flush"
    );
    if let Err(error) = &result {
        assert!(
            matches!(error, Error::Context { context, .. } if context.operation() == Operation::Flush)
        );
        assert_eq!(error.kind(), crate::ErrorKind::Backend);
    }
    assert!(
        n.pop_chunk().is_some(),
        "valid output must remain drainable after a cleanup-only failure"
    );
    assert_eq!(n.flush(), result, "repeated flush retains its result");
}
#[test]
fn repro_p1_flush_error() {
    flush_error(true);
}
#[test]
fn repro_p1_flush_error_control() {
    flush_error(false);
}

#[test]
fn supported_stereo_control() {
    let mut n = Normalizer::new(48_000, 2, output()).unwrap();
    n.push(&vec![0.5; 1920], 0).unwrap();
    assert_eq!(n.pop_chunk().unwrap().0, vec![0.5; 1920]);
}
#[test]
fn flush_is_idempotent_and_padding_is_not_a_gap() {
    let mut n = Normalizer::new(48_000, 2, output()).unwrap();
    n.push(&[0.25; 200], 0).unwrap();
    n.flush().unwrap();
    let chunk = n.pop_chunk_with_metadata().unwrap();
    assert!(chunk.flags.contains(ChunkFlags::PADDED));
    assert!(!chunk
        .flags
        .intersects(ChunkFlags::SILENCE | ChunkFlags::DISCONTINUITY));
    n.flush().unwrap();
    assert!(n.pop_chunk_with_metadata().is_none());
}
#[test]
fn zero_native_format_is_invalid() {
    for (rate, channels) in [(0, 2), (48_000, 0)] {
        assert!(matches!(
            Normalizer::new(rate, channels, output()),
            Err(Error::InvalidArg(_))
        ));
    }
}
