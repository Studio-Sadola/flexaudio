//! Offline audit reproductions. No devices are opened.
use super::*;

fn output() -> OutputFormat {
    OutputFormat {
        sample_rate: 48_000,
        channels: 2,
    }
}
fn short_tail(rate: u32) {
    let mut n = Normalizer::new(rate, 2, output()).unwrap();
    n.push(&vec![0.25; 200], 0).unwrap();
    n.flush();
    let buffered = n.buffered_out_frames();
    assert!(buffered > 0, "F13: 100 real input frames vanished after flush: output_frames={buffered}, stage1_pending={}", n.stage1_resampler.as_ref().map_or(0, |r| r.in_accum.len() / 2));
}
#[test]
#[ignore = "repro: F13"]
fn repro_p1_short_44100_tail() {
    short_tail(44_100);
}
#[test]
fn repro_p1_short_44100_tail_control() {
    short_tail(48_000);
}

fn multichannel(channel: usize) {
    let mut n = Normalizer::new(48_000, 6, output()).unwrap();
    let mut input = vec![0.0; 960 * 6];
    for frame in input.chunks_exact_mut(6) {
        frame[channel] = 0.5;
    }
    n.push(&input, 0).unwrap();
    let (data, _) = n.pop_chunk().unwrap();
    let nonzero = data.iter().filter(|&&s| s != 0.0).count();
    assert!(nonzero > 0, "F15: accepted 6-channel center-only input became silence: nonzero_output_samples={nonzero}");
}
#[test]
#[ignore = "repro: F15"]
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
    n.flush();
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
    n.push(&vec![0.25; 200], 0).unwrap();
    let resampler = n
        .primary
        .stage2
        .as_mut()
        .unwrap()
        .resampler
        .as_mut()
        .unwrap();
    if inject {
        resampler.out_scratch.clear();
        let error = resampler.flush_into(&mut Vec::new()).unwrap_err();
        assert!(
            error.to_string().contains("output adapter failed"),
            "unexpected injected failure: {error}"
        );
        eprintln!("injected error: {error}");
    }
    n.flush();
    assert!(n.pop_chunk().is_some(), "F14/M24: flush returned normally but lost tail after injected output-adapter failure: output_frames={}", n.buffered_out_frames());
}
#[test]
#[ignore = "repro: F14 / D M24"]
fn repro_p1_flush_error() {
    flush_error(true);
}
#[test]
fn repro_p1_flush_error_control() {
    flush_error(false);
}
