use super::*;
use flexaudio_vad::{SpeechSegment, VadEventKind as Kind};

#[test]
fn params_defaults_literal_zero_and_invalid_domain() {
    let mut p = flexaudio_vad_default_options();
    assert_eq!(params_from_c(&p).unwrap(), VadOptions::default());
    p.threshold = 0.0;
    p.min_speech_ms = 0;
    p.speech_pad_ms = 0;
    let converted = params_from_c(&p).unwrap();
    assert_eq!(converted.threshold, 0.0);
    assert_eq!(converted.min_speech_ms, 0);
    p.min_speech_ms = -1;
    assert!(params_from_c(&p).is_err());
    p.min_speech_ms = 134218;
    assert!(params_from_c(&p).is_err());
    p.min_speech_ms = 0;
    p.threshold = f32::NAN;
    assert!(params_from_c(&p).is_err());
    unsafe {
        assert!(
            flexaudio_vad_new(ptr::null(), &FlexVadRuntimeOptions { provisional: 2 }).is_null()
        );
    }
}

#[test]
fn event_tags_payloads_reasons_and_identity() {
    let kinds = [
        Kind::Segment(SpeechSegment {
            start_ms: 10,
            end_ms: 350,
        }),
        Kind::ProvisionalSpeechStart { at_ms: 32 },
        Kind::ProvisionalSpeechEnd {
            at_ms: 96,
            reason: PreviewCloseReason::Hysteresis,
        },
        Kind::ProvisionalCut {
            start_ms: 32,
            end_ms: 96,
            reason: PreviewCutReason::Limit,
        },
        Kind::EpochEnd {
            reason: EpochEndReason::Error,
        },
    ];
    for (index, kind) in kinds.into_iter().enumerate() {
        let seq = u64::try_from(index).unwrap();
        let out = event_to_c(VadEvent {
            epoch: 7,
            seq,
            kind,
        });
        assert_eq!(out.r#type, u32::try_from(index + 1).unwrap());
        assert_eq!((out.epoch, out.seq), (7, seq));
        unsafe {
            match out.r#type {
                FLEX_VAD_SEGMENT => assert_eq!(
                    out.data.segment,
                    FlexSpeechSegment {
                        start_ms: 10,
                        end_ms: 350
                    }
                ),
                FLEX_VAD_SPEECH_START => assert_eq!(out.data.speech_start.at_ms, 32),
                FLEX_VAD_SPEECH_END => assert_eq!(
                    (out.data.speech_end.at_ms, out.data.speech_end.reason),
                    (96, FLEX_VAD_HYSTERESIS)
                ),
                FLEX_VAD_CUT => assert_eq!(
                    (
                        out.data.cut.start_ms,
                        out.data.cut.end_ms,
                        out.data.cut.reason
                    ),
                    (32, 96, FLEX_VAD_LIMIT)
                ),
                FLEX_VAD_EPOCH_END => assert_eq!(out.data.epoch_end.reason, FLEX_VAD_ERROR),
                _ => unreachable!(),
            }
        }
    }
    for (r, expected) in [
        (PreviewCloseReason::Finish, 2),
        (PreviewCloseReason::Reset, 3),
        (PreviewCloseReason::Error, 4),
    ] {
        assert_eq!(close_reason(r), expected);
    }
}

#[test]
fn output_initialization_and_slice_preflight() {
    unsafe {
        let mut out = ptr::dangling_mut::<FlexVadEvent>();
        let mut len = 99;
        assert_eq!(
            flexaudio_vad_process(
                ptr::null_mut(),
                ptr::null(),
                0,
                16_000,
                1,
                &mut out,
                &mut len
            ),
            code::FLEX_INVALID_ARG
        );
        assert!(out.is_null());
        assert_eq!(len, 0);
        assert!(input::<f32>(ptr::dangling(), usize::MAX).is_err());
        assert!(input::<f32>(ptr::null(), 1).is_err());
        assert!(input::<f32>(ptr::null(), 0).unwrap().is_empty());
        assert_eq!(
            flexaudio_vad_finish(ptr::null_mut(), &mut out, ptr::null_mut()),
            code::FLEX_INVALID_ARG
        );
        assert!(out.is_null());
    }
}

#[test]
fn synthetic_pcm_tail_lifecycle_probabilities_and_owned_arrays() {
    unsafe {
        let mut p = flexaudio_vad_default_options();
        p.threshold = 0.0;
        p.min_speech_ms = 0;
        p.speech_pad_ms = 0;
        let v = flexaudio_vad_new(&p, &FlexVadRuntimeOptions { provisional: 1 });
        assert!(!v.is_null());
        let mut out = ptr::null_mut();
        let mut len = 0;
        let pcm = [0.0; 513];
        assert_eq!(
            flexaudio_vad_process(v, pcm.as_ptr(), pcm.len(), 16_000, 1, &mut out, &mut len),
            0
        );
        assert_eq!(len, 1);
        assert_eq!((*out).r#type, FLEX_VAD_SPEECH_START);
        flexaudio_vad_events_free(out, len);
        let mut first = 0;
        let mut probabilities = ptr::null();
        let mut n = 0;
        assert_eq!(
            flexaudio_vad_probabilities(v, &mut first, &mut probabilities, &mut n),
            0
        );
        assert_eq!((first, n), (0, 1));
        assert!(!probabilities.is_null());
        assert_eq!(flexaudio_vad_finish(v, &mut out, &mut len), 0);
        assert!(len >= 3);
        let events = slice::from_raw_parts(out, len);
        assert_eq!(events.last().unwrap().r#type, FLEX_VAD_EPOCH_END);
        assert_eq!(
            events.last().unwrap().data.epoch_end.reason,
            FLEX_VAD_FINISH
        );
        assert!(events
            .iter()
            .any(|e| e.r#type == FLEX_VAD_SEGMENT && e.data.segment.end_ms == 60));
        flexaudio_vad_events_free(out, len);
        assert_eq!(
            flexaudio_vad_probabilities(v, &mut first, &mut probabilities, &mut n),
            0
        );
        assert_eq!((first, n), (1, 1));
        assert_eq!(flexaudio_vad_finish(v, &mut out, &mut len), 0);
        assert!(out.is_null());
        assert_eq!(len, 0);
        assert_eq!(
            flexaudio_vad_process(v, ptr::null(), 0, 16_000, 1, &mut out, &mut len),
            code::FLEX_INVALID_STATE
        );
        assert_eq!(flexaudio_vad_reset(v, &mut out, &mut len), 0);
        assert_eq!(len, 0);
        assert_eq!(
            flexaudio_vad_process(v, [f32::NAN].as_ptr(), 1, 16_000, 1, &mut out, &mut len),
            code::FLEX_INVALID_ARG
        );
        assert_eq!(len, 0);
        flexaudio_vad_free(v);
        flexaudio_vad_free(ptr::null_mut());
        flexaudio_vad_events_free(ptr::null_mut(), 0);
    }
}

#[test]
fn processor_segments_validation_and_matching_free() {
    unsafe {
        let v = flexaudio_vad_postprocessor_new(ptr::null());
        assert!(!v.is_null());
        let mut out = ptr::null_mut();
        let mut len = 0;
        assert_eq!(
            flexaudio_vad_postprocessor_process(v, [1.1].as_ptr(), 1, &mut out, &mut len),
            code::FLEX_INVALID_ARG
        );
        let probabilities: Vec<f32> = (0..15).map(|i| if i < 10 { 0.9 } else { 0.0 }).collect();
        assert_eq!(
            flexaudio_vad_postprocessor_process(v, probabilities.as_ptr(), 15, &mut out, &mut len),
            0
        );
        flexaudio_vad_segments_free(out, len);
        assert_eq!(flexaudio_vad_postprocessor_finish(v, &mut out, &mut len), 0);
        assert_eq!(
            slice::from_raw_parts(out, len),
            &[FlexSpeechSegment {
                start_ms: 0,
                end_ms: 350
            }]
        );
        flexaudio_vad_segments_free(out, len);
        assert_eq!(flexaudio_vad_postprocessor_reset(v), 0);
        flexaudio_vad_postprocessor_free(v);
        flexaudio_vad_postprocessor_free(ptr::null_mut());
        flexaudio_vad_segments_free(ptr::null_mut(), 0);
    }
}

#[test]
fn failure_returns_owned_terminal_batch_with_negative_status() {
    unsafe {
        let terminal = VadEvent {
            epoch: 0,
            seq: 1,
            kind: Kind::EpochEnd {
                reason: EpochEndReason::Error,
            },
        };
        let mut out = ptr::null_mut();
        let mut len = 0;
        assert_eq!(
            stream_result(
                Err(VadFailure {
                    error: VadError::Inference,
                    terminal_events: vec![terminal]
                }),
                &mut out,
                &mut len
            ),
            code::FLEX_FAILURE
        );
        assert_eq!(len, 1);
        assert_eq!((*out).r#type, FLEX_VAD_EPOCH_END);
        assert_eq!((*out).data.epoch_end.reason, FLEX_VAD_ERROR);
        flexaudio_vad_events_free(out, len);
    }
}

#[test]
fn standalone_free_rejects_structurally_invalid_arrays_without_access() {
    unsafe {
        for (events, len) in [
            (ptr::dangling_mut::<u8>().cast::<FlexVadEvent>(), 1),
            (ptr::null_mut(), 1),
            (ptr::dangling_mut(), usize::MAX),
        ] {
            flexaudio_vad_events_free(events, len);
            assert_eq!(
                crate::error::last_audio_error().unwrap().kind(),
                flexaudio::ErrorKind::InvalidArg
            );
        }
        for (segments, len) in [
            (ptr::dangling_mut::<u8>().cast::<FlexSpeechSegment>(), 1),
            (ptr::null_mut(), 1),
            (ptr::dangling_mut(), usize::MAX),
        ] {
            flexaudio_vad_segments_free(segments, len);
            assert_eq!(
                crate::error::last_audio_error().unwrap().kind(),
                flexaudio::ErrorKind::InvalidArg
            );
        }
        flexaudio_vad_events_free(ptr::null_mut(), 0);
        flexaudio_vad_segments_free(ptr::null_mut(), 0);
    }
}
