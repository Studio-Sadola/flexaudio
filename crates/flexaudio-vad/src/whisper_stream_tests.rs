use super::*;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Control {
    fail_at: Option<usize>,
    fail_reset: bool,
    frames: Vec<Vec<f32>>,
    calls: usize,
    resets: usize,
}
struct Backend {
    control: Arc<Mutex<Control>>,
    probabilities: Vec<f32>,
    index: usize,
}
impl InferenceBackend for Backend {
    fn infer(&mut self, frame: &[f32]) -> Result<f32, VadError> {
        let mut control = self.control.lock().unwrap();
        let call = control.calls;
        control.calls += 1;
        if control.fail_at == Some(call) {
            return Err(VadError::Inference("injected".into()));
        }
        control.frames.push(frame.to_vec());
        let p = self
            .probabilities
            .get(self.index)
            .copied()
            .unwrap_or(frame[0]);
        self.index += 1;
        Ok(p)
    }
    fn reset(&mut self) -> Result<(), VadError> {
        let mut control = self.control.lock().unwrap();
        control.resets += 1;
        if control.fail_reset {
            Err(VadError::Reset("injected".into()))
        } else {
            self.index = 0;
            Ok(())
        }
    }
}
fn session(
    params: WhisperVadParams,
    preview: bool,
    probs: Vec<f32>,
) -> (WhisperVad, Arc<Mutex<Control>>) {
    let control = Arc::new(Mutex::new(Control::default()));
    let vad = WhisperVad::with_backend(
        params,
        WhisperVadOptions {
            provisional: preview,
        },
        Box::new(Backend {
            control: control.clone(),
            probabilities: probs,
            index: 0,
        }),
    )
    .unwrap();
    (vad, control)
}
fn runs(runs: &[(f32, usize)]) -> Vec<f32> {
    runs.iter()
        .flat_map(|&(p, n)| std::iter::repeat_n(p, n))
        .collect()
}
fn kinds(events: &[WhisperVadEvent]) -> Vec<Kind> {
    events.iter().map(|e| e.kind.clone()).collect()
}
fn finals(events: &[WhisperVadEvent]) -> Vec<WhisperSpeechSegment> {
    events
        .iter()
        .filter_map(|e| {
            if let Kind::Segment(s) = e.kind {
                Some(s)
            } else {
                None
            }
        })
        .collect()
}
fn cuts(events: &[WhisperVadEvent]) -> Vec<(u64, u64, crate::PreviewCutReason)> {
    events
        .iter()
        .filter_map(|e| {
            if let Kind::ProvisionalCut {
                start_ms,
                end_ms,
                reason,
            } = e.kind
            {
                Some((start_ms, end_ms, reason))
            } else {
                None
            }
        })
        .collect()
}
fn assert_invariants(events: &[WhisperVadEvent]) {
    let mut open = None;
    let mut cut_count = 0;
    let mut last_end = 0;
    for (i, event) in events.iter().enumerate() {
        assert_eq!(event.seq, u64::try_from(i).expect("event index fits u64"));
        match event.kind {
            Kind::ProvisionalSpeechStart { at_ms } => {
                assert!(open.is_none());
                assert!(at_ms >= last_end);
                open = Some(at_ms);
                cut_count = 0;
            }
            Kind::ProvisionalCut {
                start_ms, end_ms, ..
            } => {
                assert!(open.is_some());
                assert!(start_ms >= open.unwrap());
                assert!(start_ms >= last_end);
                assert!(end_ms > start_ms);
                assert!(end_ms - start_ms <= 30000);
                last_end = end_ms;
                cut_count += 1;
            }
            Kind::ProvisionalSpeechEnd { at_ms, .. } => {
                let start = open.take().expect("strict alternating start/end");
                assert!(at_ms >= start);
                if at_ms > start {
                    assert!(cut_count >= 1);
                    assert_eq!(last_end, at_ms);
                }
                last_end = at_ms;
            }
            Kind::Segment(s) => {
                assert_eq!(s.start_ms % 10, 0);
                assert_eq!(s.end_ms % 10, 0);
            }
            Kind::EpochEnd { .. } => {
                assert!(open.is_none());
                assert_eq!(i + 1, events.len());
            }
        }
    }
    assert!(
        open.is_none(),
        "every published start closes by the terminal"
    );
}
#[test]
fn pcm_tail_lengths_and_latest_frame_indices() {
    for n in [0, 1, 15, 16, 511, 512, 513, 1024] {
        let (mut vad, control) = session(
            WhisperVadParams {
                threshold: 0.0,
                min_speech_duration_ms: 0,
                ..Default::default()
            },
            true,
            vec![0.9; 4],
        );
        let input = vec![0.5; n];
        let mut events = vad.process(&input).unwrap();
        assert_eq!(vad.last_frame_probabilities().first_frame_index, 0);
        assert_eq!(vad.last_frame_probabilities().values.len(), n / 512);
        events.extend(vad.finish().unwrap());
        assert_eq!(
            vad.last_frame_probabilities().first_frame_index,
            u64::try_from(n / 512).unwrap()
        );
        assert_eq!(
            vad.last_frame_probabilities().values.len(),
            usize::from(n % 512 != 0)
        );
        let control = control.lock().unwrap();
        assert_eq!(control.calls, n.div_ceil(512));
        if n % 512 != 0 {
            let frame = control.frames.last().unwrap();
            assert!(frame[..n % 512].iter().all(|&s| s == 0.5));
            assert!(frame[n % 512..].iter().all(|&s| s == 0.0));
        }
        assert_invariants(&events);
        if n > 0 {
            assert!(kinds(&events).contains(&Kind::ProvisionalSpeechEnd {
                at_ms: u64::try_from(n / 16).unwrap(),
                reason: PreviewCloseReason::Finish
            }));
            assert_eq!(
                finals(&events).last().unwrap().end_ms,
                ((u64::try_from(n.div_ceil(512)).unwrap() * 32 + 5) / 10) * 10
            );
        }
    }
}
#[test]
fn buffer_remainders_never_infer_until_full_or_true_eof() {
    let (mut vad, c) = session(Default::default(), false, vec![0.9; 10]);
    vad.process(&[0.1; 300]).unwrap();
    assert_eq!(c.lock().unwrap().calls, 0);
    vad.process(&[0.2; 300]).unwrap();
    assert_eq!(c.lock().unwrap().calls, 1);
    assert_eq!(vad.pending_len, 88);
    vad.process(&[]).unwrap();
    assert!(vad.last_frame_probabilities().values.is_empty());
    assert_eq!(vad.first_frame, 1);
    vad.finish().unwrap();
    assert_eq!(c.lock().unwrap().calls, 2);
    assert!(vad.finish().unwrap().is_empty());
    assert!(vad.last_frame_probabilities().values.is_empty());
    assert_eq!(vad.last_frame_probabilities().first_frame_index, 2);
    assert_eq!(c.lock().unwrap().calls, 2);
}
#[test]
fn preview_is_separate_from_final_policy_and_is_opt_in() {
    let probs = runs(&[(0.0, 10), (0.9, 10), (0.0, 5), (0.9, 10), (0.0, 5)]);
    let pcm = vec![0.0; probs.len() * 512];
    let (mut enabled, _) = session(Default::default(), true, probs.clone());
    let mut on = enabled.process(&pcm).unwrap();
    let on_probs = enabled.last_probs.clone();
    on.extend(enabled.finish().unwrap());
    assert_eq!(
        cuts(&on)
            .iter()
            .map(|&(s, e, _)| (s, e))
            .collect::<Vec<_>>(),
        [(320, 640), (800, 1120)]
    );
    assert_eq!(
        finals(&on),
        [WhisperSpeechSegment {
            start_ms: 290,
            end_ms: 1150
        }]
    );
    assert_invariants(&on);
    let (mut disabled, _) = session(Default::default(), false, probs);
    let mut off = disabled.process(&pcm).unwrap();
    assert_eq!(disabled.last_probs, on_probs);
    off.extend(disabled.finish().unwrap());
    assert_eq!(finals(&off), finals(&on));
    assert!(cuts(&off).is_empty());
    let (mut short, _) = session(Default::default(), true, runs(&[(0.9, 3), (0.0, 5)]));
    let mut e = short.process(&[0.0; 8 * 512]).unwrap();
    e.extend(short.finish().unwrap());
    assert_eq!(
        cuts(&e).iter().map(|&(s, e, _)| (s, e)).collect::<Vec<_>>(),
        [(0, 96)]
    );
    assert!(finals(&e).is_empty());
}
#[test]
fn preview_gray_hold_immediate_low_and_high_precedence() {
    for threshold in [0.5, 0.005, 0.0] {
        let high = threshold;
        let gray = if threshold == 0.5 { 0.4 } else { high };
        let (mut vad, _) = session(
            WhisperVadParams {
                threshold,
                ..Default::default()
            },
            true,
            vec![high, gray, high],
        );
        let mut e = vad.process(&[0.0; 1536]).unwrap();
        e.extend(vad.finish().unwrap());
        assert_invariants(&e);
        assert_eq!(
            cuts(&e).iter().map(|&(s, e, _)| (s, e)).collect::<Vec<_>>(),
            [(0, 96)]
        );
    }
    let (mut vad, _) = session(Default::default(), true, vec![0.9, 0.4, 0.0, 0.9]);
    let mut e = vad.process(&[0.0; 2048]).unwrap();
    e.extend(vad.finish().unwrap());
    assert_invariants(&e);
    assert_eq!(
        cuts(&e).iter().map(|&(s, e, _)| (s, e)).collect::<Vec<_>>(),
        [(0, 64), (96, 128)]
    );
}
#[test]
fn continuous_65_seconds_has_exact_in_frame_limits_and_positive_residual() {
    let (mut vad, _) = session(
        WhisperVadParams {
            max_speech_duration_s: 30.0,
            ..Default::default()
        },
        true,
        vec![0.9; 2032],
    );
    let mut e = vad.process(&vec![0.0; 65 * 16000]).unwrap();
    e.extend(vad.finish().unwrap());
    assert_invariants(&e);
    assert_eq!(
        cuts(&e),
        [
            (0, 30000, crate::PreviewCutReason::Limit),
            (30000, 60000, crate::PreviewCutReason::Limit),
            (60000, 65000, crate::PreviewCutReason::Finish)
        ]
    );
    assert_eq!(finals(&e).len(), 1);
}
#[test]
fn exact_deadline_finish_has_no_zero_residual() {
    let (mut vad, _) = session(Default::default(), true, vec![0.9; 938]);
    let mut e = vad.process(&vec![0.0; 30000 * 16]).unwrap();
    e.extend(vad.finish().unwrap());
    assert_invariants(&e);
    assert_eq!(cuts(&e), [(0, 30000, crate::PreviewCutReason::Limit)]);
}
#[test]
fn reset_closes_published_hint_discards_tail_finals_and_starts_new_epoch() {
    let (mut vad, c) = session(Default::default(), true, vec![0.9; 10]);
    let mut e = vad.process(&[0.0; 513]).unwrap();
    let old_seq = vad.seq;
    let closing = vad.reset().unwrap();
    assert_eq!(closing[0].seq, old_seq);
    e.extend(closing);
    assert_invariants(&e);
    assert_eq!(cuts(&e), [(0, 32, crate::PreviewCutReason::Reset)]);
    assert!(finals(&e).is_empty());
    assert_eq!(c.lock().unwrap().calls, 1);
    assert_eq!(c.lock().unwrap().resets, 1);
    assert_eq!(vad.epoch, 1);
    assert_eq!(
        vad.process(&[0.0; 512]).unwrap()[0],
        WhisperVadEvent {
            epoch: 1,
            seq: 0,
            kind: Kind::ProvisionalSpeechStart { at_ms: 0 }
        }
    );
}
#[test]
fn idle_finish_and_reset_and_finished_reset_have_one_terminal() {
    let (mut vad, _) = session(Default::default(), true, vec![]);
    assert_eq!(
        kinds(&vad.finish().unwrap()),
        [Kind::EpochEnd {
            reason: EpochEndReason::Finish
        }]
    );
    assert!(vad.finish().unwrap().is_empty());
    assert_eq!(
        vad.process(&[]).unwrap_err().error,
        WhisperVadError::SessionFinished
    );
    assert!(vad.reset().unwrap().is_empty());
    assert_eq!(vad.epoch, 1);
    assert_eq!(
        kinds(&vad.reset().unwrap()),
        [Kind::EpochEnd {
            reason: EpochEndReason::Reset
        }]
    );
    assert_eq!(vad.epoch, 2);
}
#[test]
fn invalid_complete_pcm_feed_preserves_every_published_and_buffered_state() {
    let (mut vad, c) = session(Default::default(), true, vec![0.9; 10]);
    vad.process(&[0.0; 600]).unwrap();
    let preview = format!("{:?}", vad.preview);
    let probs = vad.last_probs.clone();
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -1.01, 1.01] {
        let failure = vad.process(&[0.0, bad]).unwrap_err();
        assert_eq!(failure.error, WhisperVadError::InvalidPcm { sample: 601 });
        assert!(failure.terminal_events.is_empty());
        assert_eq!(vad.pending_len, 88);
        assert_eq!(vad.actual_samples, 600);
        assert_eq!(vad.seq, 1);
        assert_eq!(format!("{:?}", vad.preview), preview);
        assert_eq!(vad.last_probs, probs);
        assert_eq!(c.lock().unwrap().calls, 1);
    }
}
#[test]
fn error_rolls_back_staged_onsets_cuts_and_finals_and_closes_published_watermark() {
    let (mut vad, c) = session(Default::default(), true, vec![0.9; 10]);
    let mut e = vad.process(&[0.0; 512]).unwrap();
    c.lock().unwrap().fail_at = Some(3);
    let failure = vad.process(&[0.0; 4 * 512]).unwrap_err();
    assert_eq!(failure.error, WhisperVadError::Inference);
    e.extend(failure.terminal_events);
    assert_invariants(&e);
    assert_eq!(cuts(&e), [(0, 32, crate::PreviewCutReason::Error)]);
    assert_eq!(
        kinds(&e).last().unwrap(),
        &Kind::EpochEnd {
            reason: EpochEndReason::Error
        }
    );
    assert!(vad.finish().unwrap_err().terminal_events.is_empty());
    assert!(vad.process(&[]).unwrap_err().terminal_events.is_empty());
    c.lock().unwrap().fail_at = None;
    assert!(vad.reset().unwrap().is_empty());
    assert_eq!(vad.epoch, 1);
    let (mut vad, c) = session(Default::default(), true, vec![0.9; 10]);
    c.lock().unwrap().fail_at = Some(1);
    let failure = vad.process(&[0.0; 3 * 512]).unwrap_err();
    assert_eq!(
        failure.terminal_events,
        [WhisperVadEvent {
            epoch: 0,
            seq: 0,
            kind: Kind::EpochEnd {
                reason: EpochEndReason::Error
            }
        }]
    );
    // This call stages both a new onset and an immutable predecessor final before failing.
    let probs = runs(&[(0.9, 10), (0.0, 10), (0.9, 10), (0.0, 10)]);
    let (mut vad, c) = session(Default::default(), true, probs);
    c.lock().unwrap().fail_at = Some(39);
    let failure = vad.process(&vec![0.0; 40 * 512]).unwrap_err();
    assert_eq!(
        kinds(&failure.terminal_events),
        [Kind::EpochEnd {
            reason: EpochEndReason::Error
        }]
    );
}
#[test]
fn reset_failure_emits_only_error_closure_and_no_new_epoch() {
    let (mut vad, c) = session(Default::default(), true, vec![0.9; 10]);
    let mut e = vad.process(&[0.0; 512]).unwrap();
    c.lock().unwrap().fail_reset = true;
    let failure = vad.reset().unwrap_err();
    e.extend(failure.terminal_events);
    assert_invariants(&e);
    assert_eq!(cuts(&e), [(0, 32, crate::PreviewCutReason::Error)]);
    assert_eq!(vad.epoch, 0);
    assert!(vad.reset().unwrap_err().terminal_events.is_empty());
    assert_eq!(vad.epoch, 0);
    c.lock().unwrap().fail_reset = false;
    assert!(vad.reset().unwrap().is_empty());
    assert_eq!(vad.epoch, 1);
}
#[test]
fn failing_reset_after_finished_never_duplicates_terminal() {
    let (mut vad, c) = session(Default::default(), true, vec![]);
    vad.finish().unwrap();
    c.lock().unwrap().fail_reset = true;
    assert!(vad.reset().unwrap_err().terminal_events.is_empty());
    assert_eq!(vad.epoch, 0);
}
#[test]
fn invalid_model_probability_is_fatal_not_a_validation_rejection() {
    for p in [f32::NAN, f32::INFINITY, -0.1, 1.1] {
        let (mut vad, _) = session(Default::default(), true, vec![p]);
        let f = vad.process(&[0.0; 512]).unwrap_err();
        assert_eq!(f.error, WhisperVadError::Inference);
        assert_eq!(
            kinds(&f.terminal_events),
            [Kind::EpochEnd {
                reason: EpochEndReason::Error
            }]
        );
    }
}
#[test]
fn tail_failure_closes_only_published_speech_and_no_eof_finals() {
    let (mut vad, c) = session(Default::default(), true, vec![0.9; 20]);
    let mut e = vad.process(&[0.0; 10 * 512 + 1]).unwrap();
    c.lock().unwrap().fail_at = Some(10);
    let f = vad.finish().unwrap_err();
    e.extend(f.terminal_events);
    assert_invariants(&e);
    assert_eq!(cuts(&e), [(0, 320, crate::PreviewCutReason::Error)]);
    assert!(finals(&e).is_empty());
}
#[test]
fn preflight_logical_eof_and_event_epoch_counters() {
    let (mut vad, c) = session(Default::default(), true, vec![]);
    vad.actual_samples = MAX_FRAMES * 512;
    assert!(matches!(
        vad.process(&[0.0]).unwrap_err().error,
        WhisperVadError::Overflow {
            operation: Operation::LogicalSamples
        }
    ));
    assert_eq!(c.lock().unwrap().calls, 0);
    vad.actual_samples = 0;
    vad.seq = u64::MAX - 1;
    assert!(matches!(
        vad.process(&[]).unwrap_err().error,
        WhisperVadError::Overflow {
            operation: Operation::EventSequence
        }
    ));
    vad.seq = 0;
    vad.epoch = u32::MAX;
    assert!(matches!(
        vad.reset().unwrap_err().error,
        WhisperVadError::Overflow {
            operation: Operation::Epoch
        }
    ));
    assert_eq!(c.lock().unwrap().resets, 0);
}
#[test]
fn randomized_pcm_partitions_match_whole_events_and_probabilities() {
    let mut seed = 0x91ef0123_u64;
    let mut next = || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        seed >> 32
    };
    for _ in 0..20 {
        let probs: Vec<_> = (0..300)
            .map(|_| [0.0, 0.4, 0.9][usize::try_from(next() % 3).unwrap()])
            .collect();
        let n = probs.len() * 512 - usize::try_from(next() % 512).unwrap();
        let pcm = vec![0.0; n];
        let p = WhisperVadParams {
            min_speech_duration_ms: u32::try_from(next() % 600).unwrap(),
            max_speech_duration_s: 1.0,
            ..Default::default()
        };
        let (mut vad, _) = session(p.clone(), true, probs.clone());
        let mut whole = vad.process(&pcm).unwrap();
        let mut whole_probs = vad.last_probs.clone();
        whole.extend(vad.finish().unwrap());
        whole_probs.extend(&vad.last_probs);
        assert_invariants(&whole);
        for partition in 0..5 {
            let (mut vad, _) = session(p.clone(), true, probs.clone());
            let mut split = Vec::new();
            let mut split_probs: Vec<f32> = Vec::new();
            let mut offset = 0;
            while offset < n {
                let size = match partition {
                    0 => 1,
                    1 => 320,
                    2 => 512,
                    _ => 1 + usize::try_from(next() % 2048).unwrap(),
                };
                let end = (offset + size).min(n);
                split.extend(vad.process(&pcm[offset..end]).unwrap());
                split_probs.extend(&vad.last_probs);
                offset = end;
            }
            split.extend(vad.finish().unwrap());
            split_probs.extend(&vad.last_probs);
            assert_eq!(split, whole);
            assert_eq!(split_probs, whole_probs);
            assert_invariants(&split);
        }
    }
}
#[test]
fn embedded_v6_fixture_random_partitions_and_reset_are_bit_identical() {
    let wav = include_bytes!("../tests/fixtures/jp_2spk_FF_4s_16k.wav");
    let pcm: Vec<_> = wav[44..]
        .chunks_exact(2)
        .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0)
        .collect();
    let mut vad =
        WhisperVad::new(Default::default(), WhisperVadOptions { provisional: true }).unwrap();
    let mut whole = vad.process(&pcm).unwrap();
    let mut whole_probs = vad.last_probs.clone();
    whole.extend(vad.finish().unwrap());
    whole_probs.extend(&vad.last_probs);
    assert_invariants(&whole);
    for seed in [2_u64, 71] {
        assert!(vad.reset().unwrap().is_empty());
        let mut seed = seed;
        let mut split = Vec::new();
        let mut split_probs: Vec<f32> = Vec::new();
        let mut offset = 0;
        while offset < pcm.len() {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let end = (offset + 1 + usize::try_from((seed >> 32) % 2048).unwrap()).min(pcm.len());
            split.extend(vad.process(&pcm[offset..end]).unwrap());
            split_probs.extend(&vad.last_probs);
            offset = end;
        }
        split.extend(vad.finish().unwrap());
        split_probs.extend(&vad.last_probs);
        for event in &mut split {
            event.epoch = 0;
        }
        assert_eq!(split, whole);
        assert_eq!(split_probs, whole_probs);
        assert_invariants(&split);
    }
}

#[test]
fn failed_call_limit_cut_is_discarded_and_reset_after_limit_has_positive_residual() {
    let (mut vad, c) = session(Default::default(), true, vec![0.9; 1000]);
    let mut events = vad.process(&vec![0.0; 937 * 512]).unwrap();
    c.lock().unwrap().fail_at = Some(938);
    let failure = vad.process(&[0.0; 2 * 512]).unwrap_err();
    events.extend(failure.terminal_events);
    assert_invariants(&events);
    assert_eq!(cuts(&events), [(0, 29984, crate::PreviewCutReason::Error)]);
    let (mut vad, _) = session(Default::default(), true, vec![0.9; 1000]);
    let mut events = vad.process(&vec![0.0; 938 * 512]).unwrap();
    events.extend(vad.reset().unwrap());
    assert_invariants(&events);
    assert_eq!(
        cuts(&events),
        [
            (0, 30000, crate::PreviewCutReason::Limit),
            (30000, 30016, crate::PreviewCutReason::Reset)
        ]
    );
}
