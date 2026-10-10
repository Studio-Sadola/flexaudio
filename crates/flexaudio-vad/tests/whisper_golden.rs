//! Independent pinned-oracle fixtures provided by the coordinator.
use flexaudio_vad::{WhisperVadParams, WhisperVadPostProcessor};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    pin: String,
    cases: Vec<Case>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    params: Params,
    probs: Vec<String>,
    segments_cs: Vec<[u64; 2]>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Params {
    threshold: f32,
    min_speech_duration_ms: u32,
    min_silence_duration_ms: u32,
    max_speech_duration_s: f32,
    speech_pad_ms: u32,
}

#[test]
fn pinned_whisper_cpp_golden_in_random_partitions() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/whisper_vad_golden.json");
    let data=std::fs::read_to_string(&path).unwrap_or_else(|error|panic!("Required pinned whisper.cpp golden fixture is missing or unreadable: {} ({error}). The coordinator must provide it; this conformance test must not be skipped.",path.display()));
    let fixture: Fixture = serde_json::from_str(&data).expect("valid pinned golden fixture schema");
    assert!(
        fixture.pin == "85a69493" || fixture.pin == "85a69493a601d4ff5a834064f7b7bac250bd8739",
        "unexpected oracle pin: {}",
        fixture.pin
    );
    assert!(
        !fixture.cases.is_empty(),
        "golden fixture must contain oracle cases"
    );
    let mut seed = 0x5831_u64;
    for case in fixture.cases {
        let probabilities: Vec<f32> = case
            .probs
            .iter()
            .enumerate()
            .map(|(frame, probability)| {
                probability.parse::<f32>().unwrap_or_else(|error| {
                    panic!(
                        "oracle case {}, probability {}: invalid f32 string ({error})",
                        case.name, frame
                    )
                })
            })
            .collect();
        let params = WhisperVadParams {
            threshold: case.params.threshold,
            min_speech_duration_ms: case.params.min_speech_duration_ms,
            min_silence_duration_ms: case.params.min_silence_duration_ms,
            max_speech_duration_s: case.params.max_speech_duration_s,
            speech_pad_ms: case.params.speech_pad_ms,
        };
        for partition in 0..20 {
            let mut processor =
                WhisperVadPostProcessor::new(params.clone()).expect("valid oracle parameters");
            let mut segments = Vec::new();
            let mut offset = 0;
            while offset < probabilities.len() {
                segments.extend(processor.process(&[]).unwrap());
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                let size = match partition {
                    0 => probabilities.len(),
                    1 => 1,
                    _ => 1 + usize::try_from((seed >> 32) % 97).unwrap(),
                };
                let end = offset.saturating_add(size).min(probabilities.len());
                segments.extend(processor.process(&probabilities[offset..end]).unwrap());
                offset = end;
            }
            segments.extend(processor.finish().unwrap());
            let actual: Vec<_> = segments
                .iter()
                .map(|s| {
                    assert_eq!(s.start_ms % 10, 0);
                    assert_eq!(s.end_ms % 10, 0);
                    [s.start_ms, s.end_ms]
                })
                .collect();
            let expected: Vec<_> = case
                .segments_cs
                .iter()
                .map(|[a, b]| [a * 10, b * 10])
                .collect();
            assert_eq!(
                actual, expected,
                "oracle case {}, partition {}",
                case.name, partition
            );
        }
    }
}
