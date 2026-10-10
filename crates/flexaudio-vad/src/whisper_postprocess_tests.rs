use super::*;

fn params() -> WhisperVadParams {
    WhisperVadParams {
        min_speech_duration_ms: 0,
        min_silence_duration_ms: 0,
        speech_pad_ms: 0,
        ..Default::default()
    }
}
fn runs(runs: &[(f32, usize)]) -> Vec<f32> {
    runs.iter()
        .flat_map(|&(p, n)| std::iter::repeat_n(p, n))
        .collect()
}
fn run(p: WhisperVadParams, probs: &[f32]) -> Vec<WhisperSpeechSegment> {
    let mut processor = WhisperVadPostProcessor::new(p).unwrap();
    let mut out = processor.process(probs).unwrap();
    out.extend(processor.finish().unwrap());
    out
}
fn pairs(out: &[WhisperSpeechSegment]) -> Vec<(u64, u64)> {
    out.iter().map(|s| (s.start_ms, s.end_ms)).collect()
}

#[test]
fn default_goldens() {
    assert_eq!(
        pairs(&run(Default::default(), &runs(&[(0.9, 10), (0.0, 5)]))),
        [(0, 350)]
    );
    assert_eq!(
        pairs(&run(
            Default::default(),
            &runs(&[(0.9, 10), (0.0, 10), (0.9, 10), (0.0, 5)])
        )),
        [(0, 350), (610, 990)]
    );
}
#[test]
fn empty_silence_short_and_leading_silence() {
    assert!(run(Default::default(), &[]).is_empty());
    assert!(run(Default::default(), &[0.0; 40]).is_empty());
    assert!(run(Default::default(), &[0.9]).is_empty());
    assert_eq!(
        pairs(&run(Default::default(), &runs(&[(0.0, 10), (0.9, 10)]))),
        [(290, 640)]
    );
}
#[test]
fn eof_uses_logical_length_and_unconfirmed_silence_tail() {
    assert_eq!(
        pairs(&run(Default::default(), &runs(&[(0.9, 10), (0.0, 3)]))),
        [(0, 420)]
    );
}
#[test]
fn strict_min_speech_at_normal_close_and_eof() {
    for n in [7, 8, 9] {
        let p = WhisperVadParams {
            min_speech_duration_ms: 256,
            ..params()
        };
        assert_eq!(
            run(p.clone(), &runs(&[(0.9, n), (0.0, 10)])).is_empty(),
            n <= 8
        );
        assert_eq!(run(p, &vec![0.9; n]).is_empty(), n <= 8);
    }
}
#[test]
fn second_filter_accepts_forced_equality_and_merges_short_first() {
    let mut processor = WhisperVadPostProcessor::new(WhisperVadParams {
        min_speech_duration_ms: 256,
        ..params()
    })
    .unwrap();
    processor.frames = 20;
    let mut out = Vec::new();
    processor.raw(0, 4096, &mut |segment| out.push(segment));
    processor.seal(&mut |segment| out.push(segment));
    out.extend(processor.finish().unwrap());
    assert_eq!(pairs(&out), [(0, 260)]);
    let mut processor = WhisperVadPostProcessor::new(WhisperVadParams {
        min_speech_duration_ms: 256,
        ..params()
    })
    .unwrap();
    processor.frames = 20;
    let mut out = Vec::new();
    processor.raw(0, 1000, &mut |segment| out.push(segment));
    processor.raw(1500, 4500, &mut |segment| out.push(segment));
    out.extend(processor.finish().unwrap());
    assert_eq!(pairs(&out), [(0, 280)]);
}
#[test]
fn onset_negative_threshold_edges_and_high_precedence() {
    let negative = (0.5 - 0.15_f32).max(0.01);
    assert!(run(params(), &[f32::from_bits(0.5_f32.to_bits() - 1); 10]).is_empty());
    assert_eq!(pairs(&run(params(), &[0.5; 10])), [(0, 320)]);
    assert_eq!(pairs(&run(params(), &[0.5, negative, 0.4])), [(0, 100)]);
    assert_eq!(
        pairs(&run(
            params(),
            &[0.5, f32::from_bits(negative.to_bits() - 1), 0.4]
        )),
        [(0, 30)]
    );
    assert_eq!(
        pairs(&run(
            WhisperVadParams {
                threshold: 1.0,
                ..params()
            },
            &[1.0; 10]
        )),
        [(0, 320)]
    );
    // The pin starts then continues at frame zero, but subsequent zero probabilities
    // also pass its silence condition because the derived lower threshold is 0.01.
    assert_eq!(
        pairs(&run(
            WhisperVadParams {
                threshold: 0.0,
                ..params()
            },
            &[0.0; 3]
        )),
        [(0, 100)]
    );
}
#[test]
fn gray_zone_preserves_silence_clock_and_high_cancels_it() {
    let p = WhisperVadParams {
        min_silence_duration_ms: 128,
        ..params()
    };
    let mut processor = WhisperVadPostProcessor::new(p.clone()).unwrap();
    processor
        .process(&runs(&[(0.9, 10), (0.0, 1), (0.4, 3)]))
        .unwrap();
    assert!(processor.active);
    assert_eq!(processor.temp_end, 5120);
    processor.process(&[0.0]).unwrap();
    assert!(!processor.active);
    let mut processor = WhisperVadPostProcessor::new(p).unwrap();
    processor
        .process(&runs(&[(0.9, 10), (0.0, 4), (0.9, 1)]))
        .unwrap();
    assert!(processor.active);
    assert_eq!(processor.temp_end, 0);
}
#[test]
fn min_silence_equality_and_first_silence_start() {
    for ms in [0, 100, 128] {
        let mut processor = WhisperVadPostProcessor::new(WhisperVadParams {
            min_silence_duration_ms: ms,
            ..params()
        })
        .unwrap();
        processor.process(&[0.9; 10]).unwrap();
        let before = (u64::from(ms) * 16).div_ceil(512);
        for _ in 0..before {
            processor.process(&[0.0]).unwrap();
            assert!(processor.active);
        }
        processor.process(&[0.0]).unwrap();
        assert!(!processor.active);
    }
}
#[test]
fn max_candidate_requires_more_than_98_ms_and_high_retains_it() {
    let mut processor = WhisperVadPostProcessor::new(WhisperVadParams {
        min_silence_duration_ms: 1000,
        ..params()
    })
    .unwrap();
    processor.process(&runs(&[(0.9, 10), (0.0, 4)])).unwrap();
    assert_eq!(processor.prev_end, 0); // 96 ms elapsed.
    processor.process(&[0.0]).unwrap();
    assert_eq!(processor.prev_end, 5120);
    processor.process(&[0.9]).unwrap();
    assert_eq!(processor.temp_end, 0);
    assert_eq!(processor.prev_end, 5120);
    assert_eq!(processor.next_start, 15 * 512);
}
#[test]
fn max_split_without_candidate_skips_current_frame_and_gray_also_splits() {
    let p = WhisperVadParams {
        max_speech_duration_s: 1.0,
        ..params()
    };
    let mut processor = WhisperVadPostProcessor::new(p).unwrap();
    processor.process(&[0.9; 32]).unwrap();
    assert!(!processor.active);
    assert_eq!(processor.candidate.unwrap().end, 31 * 512);
    processor.process(&[0.9]).unwrap();
    assert_eq!(processor.start, 32 * 512);
    let mut processor = WhisperVadPostProcessor::new(WhisperVadParams {
        max_speech_duration_s: 1.0,
        ..params()
    })
    .unwrap();
    processor.process(&[0.9]).unwrap();
    processor.process(&[0.4; 31]).unwrap();
    assert!(!processor.active);
}
#[test]
fn max_with_candidate_resumed_and_still_silent_and_ordering() {
    let p = WhisperVadParams {
        min_silence_duration_ms: 2000,
        max_speech_duration_s: 1.0,
        ..params()
    };
    let mut processor = WhisperVadPostProcessor::new(p.clone()).unwrap();
    processor.process(&runs(&[(0.9, 10), (0.0, 22)])).unwrap();
    assert!(!processor.active);
    assert_eq!(processor.waiting.unwrap().end, 5120);
    let mut processor = WhisperVadPostProcessor::new(p.clone()).unwrap();
    processor
        .process(&runs(&[(0.9, 10), (0.0, 5), (0.9, 17)]))
        .unwrap();
    assert!(processor.active);
    assert_eq!(processor.start, 15 * 512);
    assert_eq!(processor.candidate.unwrap().end, 5120);
    let mut processor = WhisperVadPostProcessor::new(p).unwrap();
    processor.process(&runs(&[(0.9, 27), (0.0, 4)])).unwrap();
    // At index31 the max check precedes formation of the 128 ms candidate.
    processor.process(&[0.0]).unwrap();
    assert_eq!(processor.candidate.unwrap().end, 31 * 512);
}
#[test]
fn all_high_max_splits_are_remerged_and_fractional_seconds_match() {
    let probs = vec![0.9; 125];
    assert_eq!(
        pairs(&run(
            WhisperVadParams {
                max_speech_duration_s: 1.0,
                ..params()
            },
            &probs
        )),
        [(0, 4000)]
    );
    let probs = vec![0.9; 2000];
    assert_eq!(
        run(
            WhisperVadParams {
                max_speech_duration_s: 30.9,
                ..params()
            },
            &probs
        ),
        run(
            WhisperVadParams {
                max_speech_duration_s: 30.0,
                ..params()
            },
            &probs
        )
    );
}
#[test]
fn raw_merge_gap_strict_boundary_and_chain() {
    for gap in [3199, 3200, 3201] {
        let mut processor = WhisperVadPostProcessor::new(params()).unwrap();
        processor.frames = 100;
        let mut out = Vec::new();
        processor.raw(0, 1000, &mut |segment| out.push(segment));
        processor.raw(1000 + gap, 5000 + gap, &mut |segment| out.push(segment));
        out.extend(processor.finish().unwrap());
        assert_eq!(out.len(), if gap < 3200 { 1 } else { 2 });
    }
    let mut processor = WhisperVadPostProcessor::new(params()).unwrap();
    processor.frames = 100;
    let mut out = Vec::new();
    for (s, e) in [(0, 1000), (2000, 3000), (4000, 5000)] {
        processor.raw(s, e, &mut |segment| out.push(segment));
    }
    out.extend(processor.finish().unwrap());
    assert_eq!(pairs(&out), [(0, 310)]);
}
#[test]
fn padding_half_gap_odd_equality_full_and_zero() {
    for (gap, pad) in [(3201, 2000), (3200, 1600), (3201, 1600), (3200, 0)] {
        let mut processor = WhisperVadPostProcessor::new(params()).unwrap();
        processor.params.pad = pad;
        processor.frames = 100;
        let mut out = Vec::new();
        processor.raw(5000, 6000, &mut |segment| out.push(segment));
        processor.raw(6000 + gap, 12000, &mut |segment| out.push(segment));
        out.extend(processor.finish().unwrap());
        let applied = if gap < 2 * pad { gap / 2 } else { pad };
        assert_eq!(
            out,
            [
                segment(5000 - pad, 6000 + applied),
                segment(6000 + gap - applied, 12000 + pad)
            ]
        );
    }
}
#[test]
fn rounding_preserves_reference_double_expression() {
    for (sample, ms) in [
        (79, 0),
        (80, 10),
        (81, 10),
        (159, 10),
        (160, 10),
        (161, 10),
        (240, 20),
        (2320, 140),
        (i32::MAX as u64, 134217730),
    ] {
        assert_eq!(rounded_ms(sample), ms);
    }
}
#[test]
fn nearest_surviving_successor_skips_arbitrarily_many_dropped_groups() {
    for repetitions in [0, 3, 100] {
        let p = WhisperVadParams {
            min_speech_duration_ms: 5000,
            max_speech_duration_s: 3.0,
            speech_pad_ms: 1000,
            ..Default::default()
        };
        let mut processor = WhisperVadPostProcessor::new(p).unwrap();
        assert!(processor
            .process(&runs(&[(0.9, 190), (0.0, 20)]))
            .unwrap()
            .is_empty());
        for _ in 0..repetitions {
            assert!(processor
                .process(&runs(&[(0.9, 40), (0.0, 20)]))
                .unwrap()
                .is_empty());
        }
        let mut out = processor.process(&runs(&[(0.9, 190), (0.0, 20)])).unwrap();
        out.extend(processor.finish().unwrap());
        if repetitions == 3 {
            assert_eq!(pairs(&out), [(0, 7110), (11480, 19200)]);
        }
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].end_ms, if repetitions == 0 { 6420 } else { 7110 });
    }
}
#[test]
fn incremental_emits_on_surviving_successor_and_eof_without_one() {
    let mut processor = WhisperVadPostProcessor::new(Default::default()).unwrap();
    assert!(processor
        .process(&runs(&[(0.9, 10), (0.0, 10)]))
        .unwrap()
        .is_empty());
    assert_eq!(
        pairs(&processor.process(&runs(&[(0.9, 10), (0.0, 5)])).unwrap()),
        [(0, 350)]
    );
    assert_eq!(pairs(&processor.finish().unwrap()), [(610, 990)]);
    let p = WhisperVadParams {
        min_speech_duration_ms: 5000,
        max_speech_duration_s: 3.0,
        speech_pad_ms: 1000,
        ..Default::default()
    };
    assert_eq!(
        pairs(&run(
            p,
            &runs(&[(0.9, 190), (0.0, 20), (0.9, 40), (0.0, 20)])
        )),
        [(0, 7110)]
    );
}
#[test]
fn invalid_whole_feed_is_atomic_lifecycle_and_overflow() {
    let mut p = WhisperVadPostProcessor::new(params()).unwrap();
    p.process(&[0.9; 10]).unwrap();
    let control = p.clone();
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.1, 1.1] {
        assert!(matches!(
            p.process(&[0.9, bad]),
            Err(WhisperVadError::InvalidProbability { frame: 11 })
        ));
        assert_eq!(format!("{p:?}"), format!("{control:?}"));
    }
    p.frames = MAX_FRAMES;
    assert!(matches!(
        p.process(&[0.9]),
        Err(WhisperVadError::Overflow { .. })
    ));
    p.reset();
    assert!(p.finish().unwrap().is_empty());
    assert!(p.finish().unwrap().is_empty());
    assert_eq!(
        p.process(&[]).unwrap_err(),
        WhisperVadError::SessionFinished
    );
    p.reset();
    assert_eq!(pairs(&p.process(&[0.9; 10]).unwrap()), []);
    assert_eq!(pairs(&p.finish().unwrap()), [(0, 320)]);
}
#[test]
fn randomized_probability_partition_invariance() {
    let mut seed = 0x73a64d09_u64;
    let mut next = || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        seed >> 32
    };
    for _case in 0..100 {
        let p = WhisperVadParams {
            threshold: [0.0, 0.005, 0.5, 1.0][usize::try_from(next() % 4).unwrap()],
            min_speech_duration_ms: u32::try_from(next() % 1000).unwrap(),
            min_silence_duration_ms: u32::try_from(next() % 500).unwrap(),
            max_speech_duration_s: [0.0, 0.9, 1.0, 3.9, f32::MAX]
                [usize::try_from(next() % 5).unwrap()],
            speech_pad_ms: u32::try_from(next() % 2000).unwrap(),
        };
        let neg = (p.threshold - 0.15).max(0.01);
        let probs: Vec<_> = (0..2000)
            .map(|_| [0.0, 0.9, p.threshold, neg, 0.4][usize::try_from(next() % 5).unwrap()])
            .collect();
        let whole = run(p.clone(), &probs);
        for _ in 0..5 {
            let mut processor = WhisperVadPostProcessor::new(p.clone()).unwrap();
            let mut out = Vec::new();
            let mut offset = 0;
            while offset < probs.len() {
                out.extend(processor.process(&[]).unwrap());
                let end = (offset + 1 + usize::try_from(next() % 97).unwrap()).min(probs.len());
                out.extend(processor.process(&probs[offset..end]).unwrap());
                offset = end;
            }
            out.extend(processor.finish().unwrap());
            assert_eq!(out, whole);
        }
    }
}
