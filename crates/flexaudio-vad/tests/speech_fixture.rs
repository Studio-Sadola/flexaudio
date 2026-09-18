//! 実音声フィクスチャによる推論の守り。
//!
//! 16 kHz の期待値は、推論器を `ort` から `tract-onnx` に替える直前の
//! `flexaudio-vad`（git `593a047dcca29708653199c554ff5576a0979a6c`、
//! `ort` 2.0.0-rc.12、当時の `assets/silero_vad.onnx`＝上流 Silero VAD v6.2.1）
//! が同じ WAV に対して出したフレームごとの発話確率。差 1e-4 以内、しきい値 0.5 の
//! 判定が全フレーム一致すること。

use flexaudio_vad::{get_speech_timestamps, Vad, VadConfig};
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

/// PCM s16le mono WAV（canonical RIFF）を f32 [-1,1] にする。
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
        .chunks_exact(2)
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

/// 切り出し手順: 16 kHz mono PCM16 の `jp_2spk_FF.wav` 先頭 4 秒。
/// `ffmpeg -t 4 -ac 1 -ar 16000 -c:a pcm_s16le`。
#[test]
fn tract_matches_ort_frame_probs_on_speech_fixture() {
    let wav = fixture_path(FIXTURE_WAV);
    let (rate, samples) = load_pcm16_mono_wav(&wav);
    assert_eq!(rate, 16_000);
    assert_eq!(samples.len(), 64_000, "4 s × 16 kHz");

    let expected = load_expected_probs(&fixture_path(FIXTURE_PROBS));
    assert_eq!(expected.len(), 64_000 / 512);

    let mut vad = Vad::new(VadConfig::default()).expect("model load");
    let _events = vad.process(&samples);
    let got = vad.last_frame_probabilities();
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

/// 8 kHz 入力は落ちずに、入力レート基準のフレーム数と発話イベントを出す。
#[test]
fn eight_khz_input_emits_events_in_input_rate_units() {
    let (rate, samples16) = load_pcm16_mono_wav(&fixture_path(FIXTURE_WAV));
    assert_eq!(rate, 16_000);
    // 整数 1/2: 偶数番サンプルだけ取る。4 s × 8 kHz = 32000。フレーム 256 → 125 個。
    let samples8: Vec<f32> = samples16.iter().step_by(2).copied().collect();
    assert_eq!(samples8.len(), 32_000);

    let cfg = VadConfig {
        sample_rate: 8_000,
        ..VadConfig::default()
    };
    let mut vad = Vad::new(cfg.clone()).expect("8 kHz model load");
    let during = vad.process(&samples8);
    let probs = vad.last_frame_probabilities();
    assert_eq!(
        probs.len(),
        32_000 / 256,
        "8 kHz のフレーム数は入力レート基準（256 サンプル/フレーム）"
    );
    for &p in probs {
        assert!((0.0..=1.0).contains(&p), "prob {p} out of [0,1]");
    }

    let flushed = vad.flush();
    let mut events = during;
    events.extend(flushed);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, flexaudio_vad::VadEvent::SpeechStart { .. })),
        "8 kHz 実音声で発話イベントが出る: {events:?}"
    );

    let segs = get_speech_timestamps(&samples8, &cfg).expect("batch 8 kHz");
    assert!(!segs.is_empty(), "8 kHz batch でもセグメントが出る");
    let input_len = samples8.len() as u64;
    let pad = cfg.sample_rate as u64 * u64::from(cfg.speech_pad_ms) / 1000;
    for s in &segs {
        assert!(
            s.start_sample < input_len + pad,
            "start {} is not in 8 kHz sample units (len={input_len})",
            s.start_sample
        );
        assert!(
            s.end_sample <= input_len + pad,
            "end {} is not in 8 kHz sample units (len={input_len} pad={pad})",
            s.end_sample
        );
        assert!(s.end_sample > s.start_sample);
    }
}
