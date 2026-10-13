use super::*;
use crate::{
    EpochEndReason, InferenceBackend, PreviewCloseReason, VadError, WhisperVadEventKind as Kind,
};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Control {
    frames: Vec<Vec<f32>>,
    fail_at: Option<usize>,
    fail_reset: bool,
    resets: usize,
    reset_attempts: usize,
}

struct Backend(Arc<Mutex<Control>>);
impl InferenceBackend for Backend {
    fn infer(&mut self, frame: &[f32]) -> Result<f32, VadError> {
        let mut control = self.0.lock().unwrap();
        if control.fail_at == Some(control.frames.len()) {
            return Err(VadError::Inference("injected".into()));
        }
        control.frames.push(frame.to_vec());
        let energy: f32 = frame.iter().map(|sample| sample * sample).sum();
        Ok(if energy > 0.01 { 0.9 } else { 0.0 })
    }
    fn reset(&mut self) -> Result<(), VadError> {
        let mut control = self.0.lock().unwrap();
        control.reset_attempts += 1;
        if control.fail_reset {
            return Err(VadError::Reset("injected".into()));
        }
        control.resets += 1;
        Ok(())
    }
}

fn tap() -> (WhisperVadTap, Arc<Mutex<Control>>) {
    let control = Arc::new(Mutex::new(Control::default()));
    let vad = WhisperVad::with_backend(
        Default::default(),
        WhisperVadOptions { provisional: true },
        Box::new(Backend(control.clone())),
    )
    .unwrap();
    (
        WhisperVadTap::with_vad(vad, CaptureConverter::new().unwrap()),
        control,
    )
}

fn assert_order(events: &[Attached]) {
    let mut epoch = None;
    let mut next_seq = 0;
    let mut open = false;
    let mut ended = true;
    for event in events {
        if let Attached::EpochStart { .. } = event {
            assert!(ended);
            assert_ne!(epoch, Some(event.epoch()));
            epoch = Some(event.epoch());
            next_seq = 0;
            ended = false;
        }
        assert!(!ended);
        assert_eq!(Some(event.epoch()), epoch);
        assert_eq!(event.seq(), next_seq);
        next_seq += 1;
        if let Attached::Vad(event) = event {
            match event.kind {
                Kind::ProvisionalSpeechStart { .. } => {
                    assert!(!open);
                    open = true;
                }
                Kind::ProvisionalSpeechEnd { .. } => {
                    assert!(open);
                    open = false;
                }
                Kind::EpochEnd { .. } => {
                    assert!(!open);
                    ended = true;
                }
                _ => {}
            }
        }
    }
    assert!(!open);
    assert!(ended);
}

/// Published capture origins as `(epoch, seq, capture_sample, pts_ns)`.
fn origins(events: &[Attached]) -> Vec<(u32, u64, u64, i64)> {
    events
        .iter()
        .filter_map(|event| match event {
            Attached::EpochStart {
                epoch,
                seq,
                capture_sample,
                pts_ns,
            } => Some((*epoch, *seq, *capture_sample, *pts_ns)),
            _ => None,
        })
        .collect()
}

/// One epoch terminal per published epoch, in publication order.
fn terminals(events: &[Attached]) -> Vec<EpochEndReason> {
    events
        .iter()
        .filter_map(|event| match event {
            Attached::Vad(WhisperVadEvent {
                kind: Kind::EpochEnd { reason },
                ..
            }) => Some(*reason),
            _ => None,
        })
        .collect()
}

fn run(stereo: &[f32], chunk_frames: usize) -> Vec<Attached> {
    let (mut tap, _) = tap();
    let mut events = Vec::new();
    let base = (1_u64 << 54) + 17;
    for (i, chunk) in stereo.chunks(chunk_frames * 2).enumerate() {
        let offset = u64::try_from(i * chunk_frames).unwrap();
        events.extend(
            tap.process(
                chunk,
                base + offset,
                project_pts(987654321, offset).unwrap(),
                false,
            )
            .unwrap(),
        );
    }
    events.extend(tap.stop().unwrap());
    assert_order(&events);
    events
}

#[test]
fn ten_twenty_thirty_seven_ms_chunks_have_identical_events_and_epoch_pts() {
    let n = 48000 * 2 + 17;
    let stereo: Vec<_> = (0..n)
        .flat_map(|i| {
            let sample = if (4800..28000).contains(&i) || (50000..80000).contains(&i) {
                (i as f32 * 0.1).sin() * 0.3
            } else {
                0.0
            };
            [sample, sample]
        })
        .collect();
    let whole = run(&stereo, n);
    assert!(whole.iter().any(|e| matches!(
        e,
        Attached::Vad(WhisperVadEvent {
            kind: Kind::Segment(_),
            ..
        })
    )));
    for frames in [480, 960, 1776, 1, 37, 959, 1536] {
        assert_eq!(run(&stereo, frames), whole);
    }
}

#[test]
fn epoch_start_maps_recorded_click_energy_without_carrier_or_delay_offset() {
    let (mut tap, control) = tap();
    let base = (1_u64 << 54) + 23;
    let mut stereo = vec![0.0; 6000 * 2];
    stereo[4800 * 2..4800 * 2 + 2].fill(1.0);
    let mut events = tap.process(&stereo, base, 10_000_000_000, false).unwrap();
    events.extend(tap.stop().unwrap());
    let Attached::EpochStart {
        capture_sample,
        pts_ns,
        ..
    } = events[0]
    else {
        panic!("missing origin")
    };
    assert_eq!((capture_sample, pts_ns), (base, 10_000_000_000));
    let samples: Vec<_> = control
        .lock()
        .unwrap()
        .frames
        .iter()
        .flatten()
        .copied()
        .collect();
    let peak = samples
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.abs().total_cmp(&b.abs()))
        .unwrap()
        .0;
    assert!((capture_sample + u64::try_from(peak * 3).unwrap()).abs_diff(base + 4800) <= 3);
    assert_eq!(peak, 1600);
    assert_eq!(capture_sample + 100 * 48, base + 4800);
    assert_eq!(pts_ns + 100 * 1_000_000, 10_100_000_000);
    assert_order(&events);
}

#[test]
fn tiny_tail_starts_epoch_at_drain_and_only_pads_one_model_frame() {
    for n in [0_usize, 1, 15, 16, 959, 960, 961, 1536, 1537] {
        let (mut tap, control) = tap();
        let mut events = tap
            .process(&vec![0.2; n * 2], 480002, 123456789, false)
            .unwrap();
        if n < 960 {
            assert!(events.is_empty());
        }
        events.extend(tap.stop().unwrap());
        assert_eq!(
            control.lock().unwrap().frames.len(),
            n.div_ceil(3).div_ceil(512)
        );
        if n == 0 {
            assert!(events.is_empty());
        } else {
            assert!(matches!(
                events[0],
                Attached::EpochStart {
                    epoch: 0,
                    seq: 0,
                    capture_sample: 480002,
                    pts_ns: 123456789
                }
            ));
            assert_order(&events);
            let control = control.lock().unwrap();
            let valid = n.div_ceil(3) % 512;
            if valid != 0 {
                assert!(control.frames.last().unwrap()[valid..]
                    .iter()
                    .all(|&s| s == 0.0));
            }
        }
        assert!(tap.stop().unwrap().is_empty());
    }
}

#[test]
fn discontinuity_boundary_reports_the_flag_capture_gaps_and_pts_reanchors() {
    let (mut tap, _) = tap();
    // Before any accepted chunk only the explicit flag is a boundary.
    assert!(tap.is_discontinuity_boundary(960, 960, 0, true));
    assert!(!tap.is_discontinuity_boundary(960, 960, 0, false));
    tap.process(&[0.2; 1920], 1000, 0, false).unwrap();
    // 960 accepted frames from sample 1000: the next frame must be 1960.
    assert!(!tap.is_discontinuity_boundary(1960, 960, 20_000_000, false));
    assert!(tap.is_discontinuity_boundary(9999, 960, 20_000_000, false));
    assert!(tap.is_discontinuity_boundary(1960, 960, 20_000_000, true));
    assert!(tap.is_discontinuity_boundary(1960, 960, 30_000_000, false));
    // An empty feed carries no frame and is never a boundary by index.
    assert!(!tap.is_discontinuity_boundary(9999, 0, 30_000_000, false));
}

#[test]
fn discontinuity_drains_old_epoch_before_new_origin_and_never_bridges_gap() {
    let (mut tap, control) = tap();
    let mut events = tap.process(&[0.2; 2000], 100, 0, false).unwrap();
    events.extend(tap.process(&[0.0; 1920], 1100, 200_000_000, true).unwrap());
    events.extend(tap.stop().unwrap());
    assert_order(&events);
    let boundary = events
        .iter()
        .position(|e| matches!(e, Attached::EpochStart { epoch: 1, .. }))
        .unwrap();
    assert!(matches!(
        events[boundary - 1],
        Attached::Vad(WhisperVadEvent {
            epoch: 0,
            kind: Kind::EpochEnd {
                reason: EpochEndReason::Finish
            },
            ..
        })
    ));
    assert!(matches!(
        events[boundary],
        Attached::EpochStart {
            capture_sample: 1100,
            pts_ns: 200_000_000,
            seq: 0,
            ..
        }
    ));
    let c = control.lock().unwrap();
    assert_eq!(c.frames.len(), 2);
    assert_eq!(c.resets, 1);
    assert!(c.frames[1].iter().all(|&s| s == 0.0));
}

#[test]
fn pts_reanchors_use_same_drain_finish_reset_path() {
    for pts in [0, 999999] {
        let (mut tap, control) = tap();
        tap.process(&[0.2; 120], 1000, 0, false).unwrap();
        let mut events = tap.process(&[0.2; 120], 1060, pts, false).unwrap();
        events.extend(tap.stop().unwrap());
        assert_order(&events);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, Attached::EpochStart { .. }))
                .count(),
            2
        );
        assert_eq!(control.lock().unwrap().resets, 1);
    }
}

#[test]
fn capture_index_gap_finishes_the_prefix_and_reanchors_before_new_audio() {
    for discontinuity in [false, true] {
        let (mut tap, control) = tap();
        let mut events = tap.process(&[0.2; 4000], 100, 0, false).unwrap();
        assert!(matches!(
            events[0],
            Attached::EpochStart {
                epoch: 0,
                seq: 0,
                capture_sample: 100,
                pts_ns: 0,
            }
        ));
        // The next accepted frame skips 19000 capture frames (a real drop), with and
        // without an explicit discontinuity flag.
        events.extend(
            tap.process(&[0.2; 1920], 21100, 500_000_000, discontinuity)
                .unwrap(),
        );
        // The old epoch closes as an intact prefix; missing audio never becomes padding.
        assert_eq!(terminals(&events), [EpochEndReason::Finish]);
        assert_eq!(
            origins(&events),
            [(0, 0, 100, 0), (1, 0, 21100, 500_000_000)]
        );
        assert_eq!(control.lock().unwrap().resets, 1);
        events.extend(tap.stop().unwrap());
        assert_order(&events);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Attached::EpochStart { epoch: 1, .. }))
                .count(),
            1
        );
    }
}

#[test]
fn discontinuity_before_the_first_origin_anchors_that_epoch_without_failing() {
    let (mut tap, control) = tap();
    let mut events = tap.process(&[0.2; 1920], 960, 20_000_000, true).unwrap();
    // Nothing precedes the first accepted feed, so there is nothing to close: the
    // first input simply establishes its anchor.
    assert!(matches!(
        events[0],
        Attached::EpochStart {
            epoch: 0,
            seq: 0,
            capture_sample: 960,
            pts_ns: 20_000_000,
        }
    ));
    events.extend(tap.stop().unwrap());
    assert_order(&events);
    assert!(matches!(
        events.last().unwrap(),
        Attached::Vad(WhisperVadEvent {
            kind: Kind::EpochEnd {
                reason: EpochEndReason::Finish
            },
            ..
        })
    ));
    assert_eq!(control.lock().unwrap().resets, 0);
}

#[test]
fn transport_gap_after_a_short_prefix_closes_it_at_drain_then_anchors_the_next_gap() {
    let (mut tap, control) = tap();
    assert!(tap.process(&[0.2; 120], 1000, 0, false).unwrap().is_empty());
    let mut events = tap.process(&[0.2; 120], 2000, 100_000_000, false).unwrap();
    // The 60-frame prefix retains no converter output until the split drains it, so
    // sample zero is established then and closed with a real finish terminal. The gap
    // itself is not a failure, and the post-gap frame re-anchors a fresh epoch, which
    // publishes its origin when its own converter first retains output.
    assert_eq!(origins(&events), [(0, 0, 1000, 0)]);
    assert_eq!(terminals(&events), [EpochEndReason::Finish]);
    assert_eq!(control.lock().unwrap().resets, 1);
    events.extend(tap.stop().unwrap());
    assert_order(&events);
    assert_eq!(
        origins(&events),
        [(0, 0, 1000, 0), (1, 0, 2000, 100_000_000)]
    );
}

#[test]
fn failed_session_reinitializes_at_the_next_discontinuity() {
    let (mut tap, control) = tap();
    let mut events = tap.process(&[0.2; 4000], 0, 0, false).unwrap();
    control.lock().unwrap().fail_at = Some(1);
    let failure = tap
        .process(&[0.2; 4000], 2000, project_pts(0, 2000).unwrap(), false)
        .unwrap_err();
    assert_eq!(failure.error.code(), flexaudio_core::WhisperCode::Inference);
    events.extend(failure.terminal_events);
    assert_order(&events);
    // Contiguous audio carries no reinitialization point: intake stays failed.
    assert_eq!(
        tap.process(&[0.2; 2], 4000, project_pts(0, 4000).unwrap(), false)
            .unwrap_err()
            .error,
        Error::FailedSession
    );
    control.lock().unwrap().fail_at = None;
    let restart_pts = project_pts(0, 4000).unwrap();
    events.extend(tap.process(&[0.2; 4000], 4000, restart_pts, true).unwrap());
    // The discontinuity reinitializes converter and model, so the first post-gap
    // frame starts a new anchored epoch instead of staying failed.
    assert_eq!(origins(&events), [(0, 0, 0, 0), (1, 0, 4000, restart_pts)]);
    assert_eq!(control.lock().unwrap().resets, 1);
    events.extend(tap.stop().unwrap());
    assert_order(&events);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Attached::EpochStart { epoch: 1, .. }))
            .count(),
        1
    );
}

#[test]
fn failed_reinitialization_stays_failed_until_a_successful_reset() {
    let (mut tap, control) = tap();
    let mut events = tap.process(&[0.2; 4000], 0, 0, false).unwrap();
    control.lock().unwrap().fail_at = Some(1);
    let failure = tap
        .process(&[0.2; 4000], 2000, project_pts(0, 2000).unwrap(), false)
        .unwrap_err();
    events.extend(failure.terminal_events);
    assert_order(&events);
    control.lock().unwrap().fail_at = None;
    control.lock().unwrap().fail_reset = true;
    let failure = tap
        .process(&[0.2; 4000], 4000, project_pts(0, 4000).unwrap(), true)
        .unwrap_err();
    // The reset failed, so the new epoch is never published and the old closure is
    // not repeated (no event storm).
    assert_eq!(failure.error.code(), flexaudio_core::WhisperCode::Inference);
    assert!(failure.terminal_events.is_empty());
    assert_eq!(
        tap.process(
            &[0.2; 2],
            6000,
            project_pts(project_pts(0, 4000).unwrap(), 2000).unwrap(),
            false,
        )
        .unwrap_err()
        .error,
        Error::FailedSession
    );
    assert_eq!(control.lock().unwrap().resets, 0);
    control.lock().unwrap().fail_reset = false;
    assert!(tap.flush().unwrap().is_empty());
    assert!(tap
        .process(&[0.0; 2], 8000, project_pts(0, 8000).unwrap(), false)
        .unwrap()
        .is_empty());
    let next = tap.stop().unwrap();
    assert!(matches!(
        next[0],
        Attached::EpochStart {
            epoch: 1,
            capture_sample: 8000,
            ..
        }
    ));
    assert_order(&next);
    assert_eq!(control.lock().unwrap().resets, 1);
}

#[test]
fn failed_session_observes_skipped_capture_until_a_pts_reanchor() {
    let (mut tap, control) = tap();
    let mut events = tap.process(&[0.2; 4000], 0, 0, false).unwrap();
    control.lock().unwrap().fail_at = Some(1);
    let failure = tap
        .process(&[0.2; 4000], 2000, project_pts(0, 2000).unwrap(), false)
        .unwrap_err();
    events.extend(failure.terminal_events);
    control.lock().unwrap().fail_at = None;
    for sample in [4000, 6000, 8000] {
        assert_eq!(
            tap.process(&[0.2; 4000], sample, project_pts(0, sample).unwrap(), false)
                .unwrap_err()
                .error,
            Error::FailedSession,
            "contiguous failed capture must not retry model reset"
        );
    }
    assert_eq!(control.lock().unwrap().reset_attempts, 0);
    // Capture indices still continue, but a changed PTS anchor is a recovery boundary.
    let restart_pts = 1_000_000_000;
    events.extend(
        tap.process(&[0.2; 4000], 10000, restart_pts, false)
            .unwrap(),
    );
    assert_eq!(origins(&events), [(0, 0, 0, 0), (1, 0, 10000, restart_pts)]);
    assert_eq!(control.lock().unwrap().reset_attempts, 1);
    events.extend(tap.stop().unwrap());
    assert_order(&events);
}

#[test]
fn failed_reset_observes_its_chunk_and_does_not_retry_on_contiguous_capture() {
    let (mut tap, control) = tap();
    let mut events = tap.process(&[0.2; 4000], 0, 0, false).unwrap();
    control.lock().unwrap().fail_at = Some(1);
    events.extend(
        tap.process(&[0.2; 4000], 2000, project_pts(0, 2000).unwrap(), false)
            .unwrap_err()
            .terminal_events,
    );
    {
        let mut control = control.lock().unwrap();
        control.fail_at = None;
        control.fail_reset = true;
    }
    assert!(tap
        .process(&[0.2; 4000], 4000, 1_000_000_000, true)
        .unwrap_err()
        .terminal_events
        .is_empty());
    // Allow reset again to prove it is not attempted just because failed audio was skipped.
    control.lock().unwrap().fail_reset = false;
    for offset in [2000, 4000, 6000] {
        let failure = tap
            .process(
                &[0.2; 4000],
                4000 + offset,
                project_pts(1_000_000_000, offset).unwrap(),
                false,
            )
            .unwrap_err();
        assert_eq!(failure.error, Error::FailedSession);
        assert!(failure.terminal_events.is_empty());
    }
    assert_eq!(control.lock().unwrap().reset_attempts, 1);
    assert_eq!(origins(&events).len(), 1);
    assert_order(&events);
}

#[cfg(feature = "test-faults")]
#[test]
fn injected_failure_before_converter_feed_tracks_capture_and_failed_reset_once() {
    let (mut tap, control) = tap();
    let mut events = tap.process(&[0.2; 4000], 0, 0, false).unwrap();
    tap.inject_failure();
    events.extend(
        tap.process(&[0.2; 4000], 2000, project_pts(0, 2000).unwrap(), false)
            .unwrap_err()
            .terminal_events,
    );
    assert_eq!(terminals(&events), [EpochEndReason::Error]);
    for sample in [4000, 6000] {
        assert_eq!(
            tap.process(&[0.2; 4000], sample, project_pts(0, sample).unwrap(), false)
                .unwrap_err()
                .error,
            Error::FailedSession
        );
    }
    tap.inject_reset_failure();
    assert!(tap
        .process(&[0.2; 4000], 8000, 1_000_000_000, true)
        .unwrap_err()
        .terminal_events
        .is_empty());
    for offset in [2000, 4000, 6000] {
        assert_eq!(
            tap.process(
                &[0.2; 4000],
                8000 + offset,
                project_pts(1_000_000_000, offset).unwrap(),
                false,
            )
            .unwrap_err()
            .error,
            Error::FailedSession
        );
    }
    assert_eq!(control.lock().unwrap().reset_attempts, 0);
    events.extend(
        tap.process(&[0.2; 4000], 16000, 2_000_000_000, true)
            .unwrap(),
    );
    assert_eq!(
        origins(&events),
        [(0, 0, 0, 0), (1, 0, 16000, 2_000_000_000)]
    );
    events.extend(tap.stop().unwrap());
    assert_order(&events);
}

#[cfg(feature = "test-faults")]
#[test]
fn injected_finish_failure_closes_the_published_epoch_on_flush_and_stop() {
    for stopping in [false, true] {
        let (mut tap, _) = tap();
        let mut events = tap.process(&[0.2; 4000], 0, 0, false).unwrap();
        tap.inject_failure();
        let failure = if stopping { tap.stop() } else { tap.flush() }.unwrap_err();
        assert_eq!(failure.error.code(), flexaudio_core::WhisperCode::Inference);
        events.extend(failure.terminal_events);
        assert_eq!(terminals(&events), [EpochEndReason::Error]);
        assert_order(&events);
        assert!(tap
            .stop()
            .expect("stop after a reported failure must succeed")
            .is_empty());
    }
}

#[test]
fn flush_is_exactly_once_and_stop_never_creates_next_epoch() {
    let (mut tap, control) = tap();
    assert!(tap.flush().unwrap().is_empty());
    let mut events = tap.process(&[0.2; 4000], 123, 999, false).unwrap();
    events.extend(tap.flush().unwrap());
    assert!(tap.flush().unwrap().is_empty());
    assert_eq!(control.lock().unwrap().resets, 1);
    events.extend(tap.process(&[0.2; 2], 50000, 50000000, false).unwrap());
    events.extend(tap.stop().unwrap());
    assert_order(&events);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Attached::EpochStart { .. }))
            .count(),
        2
    );
    assert_eq!(
        tap.process(&[], 0, 0, false).unwrap_err().error,
        Error::Stopped
    );
    assert_eq!(tap.flush().unwrap_err().error, Error::Stopped);
}

#[test]
fn capture_validation_preserves_buffered_audio_and_origin_even_with_discontinuity() {
    let (mut tap, control) = tap();
    tap.process(&[0.2; 1000], 123, 999, false).unwrap();
    for (pcm, sample, pts, error) in [
        (vec![0.0], 623, 10417665, Error::InvalidStereoLength),
        (
            vec![f32::NAN, 0.0],
            623,
            10417665,
            Error::InvalidPcm { sample: 0 },
        ),
        (
            vec![1.1, 0.0],
            623,
            10417665,
            Error::InvalidPcm { sample: 0 },
        ),
        (vec![0.0; 2], u64::MAX, 0, Error::CaptureSampleOverflow),
        (vec![0.0; 2], 623, MAX_SAFE_PTS, Error::PtsOutOfRange),
        (vec![], 623, -MAX_SAFE_PTS - 1, Error::PtsOutOfRange),
        (vec![0.0; 2], 623, -MAX_SAFE_PTS - 1, Error::PtsOutOfRange),
    ] {
        let failure = tap.process(&pcm, sample, pts, true).unwrap_err();
        assert_eq!(failure.error, error);
        assert!(failure.terminal_events.is_empty());
    }
    let events = tap.stop().unwrap();
    assert!(matches!(
        events[0],
        Attached::EpochStart {
            capture_sample: 123,
            pts_ns: 999,
            ..
        }
    ));
    assert_order(&events);
    assert_eq!(control.lock().unwrap().resets, 0);
}

#[test]
fn stop_after_inference_failure_succeeds_without_repeating_closure_or_reset() {
    let (mut tap, control) = tap();
    let mut events = tap
        .process(&[0.2; 4000], 0, 0, false)
        .expect("the first feed must establish an epoch");
    control
        .lock()
        .expect("test backend lock must be available")
        .fail_at = Some(1);
    let failure = tap
        .process(
            &[0.2; 4000],
            2000,
            project_pts(0, 2000).expect("test PTS must be valid"),
            false,
        )
        .expect_err("the second feed must fail model inference");
    assert_eq!(failure.error.code(), flexaudio_core::WhisperCode::Inference);
    events.extend(failure.terminal_events);
    assert_eq!(terminals(&events), [EpochEndReason::Error]);
    assert_order(&events);
    assert!(tap
        .stop()
        .expect("an already reported VAD failure must not reject stop")
        .is_empty());
    assert!(tap.stop().expect("repeated stop must succeed").is_empty());
    assert_eq!(
        tap.flush()
            .expect_err("stopped intake cannot recover")
            .error,
        Error::Stopped
    );
    assert_eq!(
        tap.process(
            &[0.2; 2],
            4000,
            project_pts(0, 4000).expect("test PTS must be valid"),
            false
        )
        .expect_err("stopped intake cannot process")
        .error,
        Error::Stopped
    );
    assert_eq!(
        control
            .lock()
            .expect("test backend lock must be available")
            .reset_attempts,
        0
    );
    assert_eq!(terminals(&events), [EpochEndReason::Error]);
}

#[test]
fn inference_and_reset_failures_preserve_ordered_exactly_once_closure() {
    let (mut tap, control) = tap();
    let mut events = tap.process(&[0.2; 4000], 0, 0, false).unwrap();
    control.lock().unwrap().fail_at = Some(1);
    let failure = tap
        .process(&[0.2; 4000], 2000, project_pts(0, 2000).unwrap(), false)
        .unwrap_err();
    assert_eq!(failure.error.code(), flexaudio_core::WhisperCode::Inference);
    events.extend(failure.terminal_events);
    assert_order(&events);
    assert!(matches!(
        events.last().unwrap(),
        Attached::Vad(WhisperVadEvent {
            kind: Kind::EpochEnd {
                reason: EpochEndReason::Error
            },
            ..
        })
    ));
    assert_eq!(terminals(&events), [EpochEndReason::Error]);
    control.lock().unwrap().fail_at = None;
    assert!(tap.flush().unwrap().is_empty());
    let mut next = tap.process(&[0.2; 2], 9999, 999, false).unwrap();
    control.lock().unwrap().fail_reset = true;
    let failure = tap.flush().unwrap_err();
    next.extend(failure.terminal_events);
    assert_order(&next);
    assert!(matches!(
        next.last().unwrap(),
        Attached::Vad(WhisperVadEvent {
            kind: Kind::EpochEnd {
                reason: EpochEndReason::Finish
            },
            ..
        })
    ));
    assert!(tap.flush().unwrap_err().terminal_events.is_empty());
    control.lock().unwrap().fail_reset = false;
    tap.flush().unwrap();
    assert!(tap
        .stop()
        .expect("recovered session must stop cleanly")
        .is_empty());
}

#[test]
fn first_feed_failure_carries_origin_before_terminal_and_suppresses_staged_onset() {
    let (mut tap, control) = tap();
    control.lock().unwrap().fail_at = Some(0);
    let failure = tap.process(&[0.2; 4000], 12, 34, false).unwrap_err();
    assert_eq!(failure.terminal_events.len(), 2);
    assert_order(&failure.terminal_events);
    assert!(matches!(
        failure.terminal_events[0],
        Attached::EpochStart {
            capture_sample: 12,
            pts_ns: 34,
            ..
        }
    ));
    // A re-anchored capture index is a discontinuity, so this call reinitializes the failed
    // session instead of reporting the same failure again: the epoch is never closed twice.
    assert!(tap.process(&[0.2; 2], 12, 34, false).unwrap().is_empty());
    assert_eq!(terminals(&failure.terminal_events), [EpochEndReason::Error]);
    // The reinitialized epoch continues from the re-anchored frame.
    assert!(tap
        .process(&[0.2; 2], 13, project_pts(34, 1).unwrap(), false)
        .unwrap()
        .is_empty());
}

#[test]
fn fractional_ms_preview_eof_and_logical_final_padding_use_valid_drained_length() {
    let (mut tap, _) = tap();
    let mut events = tap
        .process(&vec![0.2; (48000 + 17) * 2], 1000, 0, false)
        .unwrap();
    events.extend(tap.stop().unwrap());
    assert_order(&events);
    assert!(events.iter().any(|e| matches!(
        e,
        Attached::Vad(WhisperVadEvent {
            kind: Kind::ProvisionalSpeechEnd {
                at_ms: 1000,
                reason: PreviewCloseReason::Finish
            },
            ..
        })
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        Attached::Vad(WhisperVadEvent {
            kind: Kind::Segment(crate::WhisperSpeechSegment { end_ms: 1020, .. }),
            ..
        })
    )));
}

#[test]
fn conversion_failure_closes_published_epoch_once_and_flush_recovers() {
    for published in [false, true] {
        let (mut tap, _) = tap();
        let mut events = Vec::new();
        let frames = if published { 2000 } else { 1 };
        events.extend(tap.process(&vec![0.2; frames * 2], 0, 0, false).unwrap());
        tap.converter.fail_conversion();
        let failure = tap
            .process(
                &[0.2; 2],
                u64::try_from(frames).unwrap(),
                project_pts(0, u64::try_from(frames).unwrap()).unwrap(),
                false,
            )
            .unwrap_err();
        assert_eq!(
            failure.error.code(),
            flexaudio_core::WhisperCode::Conversion
        );
        events.extend(failure.terminal_events);
        if published {
            assert_order(&events);
            assert!(matches!(
                events.last().unwrap(),
                Attached::Vad(WhisperVadEvent {
                    kind: Kind::EpochEnd {
                        reason: EpochEndReason::Error
                    },
                    ..
                })
            ));
        } else {
            assert!(events.is_empty());
        }
        assert_eq!(terminals(&events).len(), usize::from(published));
        assert!(tap.flush().unwrap().is_empty());
        assert!(tap.process(&[0.0; 2], 9999, 0, false).unwrap().is_empty());
        let next = tap.stop().unwrap();
        assert!(matches!(next[0], Attached::EpochStart { epoch: 1, .. }));
        assert_order(&next);
    }
}

#[test]
fn failing_post_gap_inference_keeps_finished_prefix_before_new_error_epoch() {
    let (mut tap, control) = tap();
    let mut events = tap.process(&[0.2; 2000], 100, 0, false).unwrap();
    control.lock().unwrap().fail_at = Some(1);
    let failure = tap
        .process(&[0.2; 4000], 1100, 200_000_000, true)
        .unwrap_err();
    events.extend(failure.terminal_events);
    assert_order(&events);
    let terminals: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            Attached::Vad(WhisperVadEvent {
                kind: Kind::EpochEnd { reason },
                ..
            }) => Some(*reason),
            _ => None,
        })
        .collect();
    assert_eq!(terminals, [EpochEndReason::Finish, EpochEndReason::Error]);
    assert!(matches!(
        events[events.len() - 2],
        Attached::EpochStart {
            epoch: 1,
            seq: 0,
            capture_sample: 1100,
            pts_ns: 200_000_000,
        }
    ));
}

#[test]
fn embedded_model_capture_events_match_whole_and_10_20_37_ms_partitions() {
    let wav = include_bytes!("../tests/fixtures/jp_2spk_FF_4s_16k.wav");
    // Deterministic test source at capture rate; this is not a resampling policy for callers.
    let stereo: Vec<_> = wav[44..]
        .as_chunks::<2>()
        .0
        .iter()
        .flat_map(|&bytes| {
            let value = f32::from(i16::from_le_bytes(bytes)) / 32768.0;
            [value; 6]
        })
        .collect();
    let mut whole = None;
    for chunk_frames in [stereo.len() / 2, 480, 960, 1776] {
        let mut tap =
            WhisperVadTap::new(Default::default(), WhisperVadOptions { provisional: true })
                .unwrap();
        let mut events = Vec::new();
        for (i, chunk) in stereo.chunks(chunk_frames * 2).enumerate() {
            let offset = u64::try_from(i * chunk_frames).unwrap();
            events.extend(
                tap.process(
                    chunk,
                    480002 + offset,
                    project_pts(10000000000, offset).unwrap(),
                    false,
                )
                .unwrap(),
            );
        }
        events.extend(tap.stop().unwrap());
        assert_order(&events);
        if let Some(expected) = &whole {
            assert_eq!(&events, expected);
        } else {
            assert!(events.iter().any(|event| matches!(
                event,
                Attached::Vad(WhisperVadEvent {
                    kind: Kind::Segment(_),
                    ..
                })
            )));
            whole = Some(events);
        }
    }
}
