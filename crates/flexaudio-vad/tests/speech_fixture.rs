//! Regression coverage for inference using real-audio fixtures.
//!
//! The 16 kHz expected values are per-frame speech probabilities produced for the same WAV
//! by `flexaudio-vad` immediately before switching inference from `ort` to `tract-onnx`
//! (git `593a047dcca29708653199c554ff5576a0979a6c`, `ort` 2.0.0-rc.12, and the then-current
//! `assets/silero_vad.onnx` = upstream Silero VAD v6.2.1). Probabilities must differ by no
//! more than 1e-4, and threshold-0.5 decisions must match on every frame.

use flexaudio_vad::{Vad, VadEventKind, VadOptions, VadRuntimeOptions};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

const FIXTURE_WAV: &str = "tests/fixtures/jp_2spk_FF_4s_16k.wav";
const FIXTURE_PROBS: &str = "tests/fixtures/jp_2spk_FF_4s_16k.probs.txt";
const ABS_TOL: f32 = 1e-4;
const SPEECH_THRESHOLD: f32 = 0.5;

fn fixture_path(rel: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)
}

/// Convert a PCM s16le mono WAV (canonical RIFF) to f32 [-1,1].
fn load_pcm16_mono_wav(path: &Path) -> (u32, Vec<f32>) {
    let mut f = File::open(path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    let mut hdr = [0u8; 12];
    f.read_exact(&mut hdr).expect("riff header");
    assert_eq!(&hdr[0..4], b"RIFF", "not RIFF: {}", path.display());
    assert_eq!(&hdr[8..12], b"WAVE", "not WAVE: {}", path.display());
    let mut fmt_channels = 0u16;
    let mut fmt_rate = 0u32;
    let mut fmt_bits = 0u16;
    let mut data = Vec::new();
    loop {
        let mut chunk_hdr = [0u8; 8];
        if f.read_exact(&mut chunk_hdr).is_err() {
            break;
        }
        let id = &chunk_hdr[0..4];
        let size = u32::from_le_bytes(chunk_hdr[4..8].try_into().unwrap()) as u64;
        if id == b"fmt " {
            let mut buf = vec![0u8; size as usize];
            f.read_exact(&mut buf).expect("fmt chunk");
            fmt_channels = u16::from_le_bytes(buf[2..4].try_into().unwrap());
            fmt_rate = u32::from_le_bytes(buf[4..8].try_into().unwrap());
            fmt_bits = u16::from_le_bytes(buf[14..16].try_into().unwrap());
            if size % 2 == 1 {
                let _ = f.seek(SeekFrom::Current(1));
            }
        } else if id == b"data" {
            data.resize(size as usize, 0);
            f.read_exact(&mut data).expect("data chunk");
            break;
        } else {
            let pad = i64::from(size % 2 == 1);
            f.seek(SeekFrom::Current(size as i64 + pad))
                .expect("skip chunk");
        }
    }
    assert_eq!(fmt_channels, 1, "fixture must be mono");
    assert_eq!(fmt_bits, 16, "fixture must be pcm16");
    let samples = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
        .collect();
    (fmt_rate, samples)
}

fn load_expected_probs(path: &Path) -> Vec<f32> {
    let f = File::open(path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    BufReader::new(f)
        .lines()
        .map(|l| {
            let s = l.expect("read probs line");
            s.parse::<f32>()
                .unwrap_or_else(|e| panic!("parse prob {s:?}: {e}"))
        })
        .collect()
}

/// Fixture source: the first 4 seconds of 16 kHz mono PCM16 `jp_2spk_FF.wav`.
/// `ffmpeg -t 4 -ac 1 -ar 16000 -c:a pcm_s16le`.
#[test]
fn tract_matches_ort_frame_probs_on_speech_fixture() {
    let wav = fixture_path(FIXTURE_WAV);
    let (rate, samples) = load_pcm16_mono_wav(&wav);
    assert_eq!(rate, 16_000);
    assert_eq!(samples.len(), 64_000, "4 s × 16 kHz");

    let expected = load_expected_probs(&fixture_path(FIXTURE_PROBS));
    assert_eq!(expected.len(), 64_000 / 512);

    let mut vad =
        Vad::new(VadOptions::default(), VadRuntimeOptions::default()).expect("model load");
    let _events = vad.process(&samples).unwrap();
    let got = vad.last_frame_probabilities().values;
    assert_eq!(
        got.len(),
        expected.len(),
        "frame count must match input rate"
    );

    let mut max_abs = 0.0f32;
    let mut disagree = 0usize;
    for (i, (g, e)) in got.iter().zip(expected.iter()).enumerate() {
        let d = (g - e).abs();
        if d > max_abs {
            max_abs = d;
        }
        assert!(
            d <= ABS_TOL,
            "frame {i}: abs diff {d} exceeds {ABS_TOL} (got={g} expected={e})"
        );
        let g_speech = *g >= SPEECH_THRESHOLD;
        let e_speech = *e >= SPEECH_THRESHOLD;
        if g_speech != e_speech {
            disagree += 1;
        }
    }
    assert_eq!(
        disagree, 0,
        "threshold {SPEECH_THRESHOLD} decisions must match every frame (max_abs={max_abs})"
    );
}

/// Arbitrary source rates retain millisecond times on the source timeline.
#[test]
fn eight_khz_input_produces_millisecond_events() {
    let (_, samples16) = load_pcm16_mono_wav(&fixture_path(FIXTURE_WAV));
    let samples8: Vec<f32> = samples16.iter().step_by(2).copied().collect();
    let mut vad = Vad::new(VadOptions::default(), VadRuntimeOptions::default()).unwrap();
    let mut events = vad.process_pcm(&samples8, 8_000, 1).unwrap();
    events.extend(vad.finish().unwrap());
    let segments: Vec<_> = events
        .into_iter()
        .filter_map(|event| match event.kind {
            VadEventKind::Segment(segment) => Some(segment),
            _ => None,
        })
        .collect();
    assert!(!segments.is_empty());
    for segment in segments {
        assert!(segment.start_ms < 4_050);
        assert!(segment.end_ms <= 4_050);
        assert!(segment.end_ms > segment.start_ms);
    }
}

#[test]
fn converted_stereo_speech_onset_matches_mono_within_one_frame() {
    let (_, samples16) = load_pcm16_mono_wav(&fixture_path(FIXTURE_WAV));
    let stereo48: Vec<f32> = samples16.iter().flat_map(|sample| [*sample; 6]).collect();
    let mut direct = Vad::new(VadOptions::default(), VadRuntimeOptions::default()).unwrap();
    let mut converted = Vad::new(VadOptions::default(), VadRuntimeOptions::default()).unwrap();
    let mut mono_events = direct.process(&samples16).unwrap();
    mono_events.extend(direct.finish().unwrap());
    let mut stereo_events = converted.process_pcm(&stereo48, 48_000, 2).unwrap();
    stereo_events.extend(converted.finish().unwrap());
    let start = |events: Vec<flexaudio_vad::VadEvent>| {
        events
            .into_iter()
            .find_map(|event| match event.kind {
                VadEventKind::Segment(segment) => Some(segment.start_ms),
                _ => None,
            })
            .expect("speech fixture must produce a segment")
    };
    assert!(start(mono_events).abs_diff(start(stereo_events)) <= 32);
}

#[test]
fn converted_full_scale_constant_and_step_pcm_remain_valid_through_eof() {
    for (rate, channels) in [(8_000, 1), (44_100, 1), (48_000, 2)] {
        for step in [false, true] {
            let frames = usize::try_from(rate / 5).unwrap();
            let samples: Vec<f32> = (0..frames)
                .flat_map(|frame| {
                    let value = if step && frame < frames / 2 {
                        -1.0
                    } else {
                        1.0
                    };
                    std::iter::repeat_n(value, usize::from(channels))
                })
                .collect();
            let mut vad = Vad::new(VadOptions::default(), VadRuntimeOptions::default()).unwrap();
            let midpoint = samples.len() / 2;
            vad.process_pcm(&samples[..midpoint], rate, channels)
                .unwrap();
            vad.process_pcm(&samples[midpoint..], rate, channels)
                .unwrap();
            let events = vad.finish().unwrap();
            assert!(matches!(
                events.last().unwrap().kind,
                VadEventKind::EpochEnd {
                    reason: flexaudio_vad::EpochEndReason::Finish
                }
            ));
            assert!(vad.finish().unwrap().is_empty());
        }
    }
}
