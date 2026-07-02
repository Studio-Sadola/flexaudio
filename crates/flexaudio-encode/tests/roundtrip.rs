//! FlacWriter の往復・異常系・圧縮実効性テスト。
//!
//! すべて決定論（信号は固定式で生成、時刻やスケジューリングに依存しない）。
//! デコードには純 Rust の claxon を使い、書いた値を全数照合する。

use std::path::PathBuf;

use flexaudio_encode::{EncodeError, FlacWriter};

/// テストごとに一意な出力パスを CARGO_TARGET_TMPDIR 配下に作る。
fn tmp_path(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// ライブラリと同じ量子化式（テスト側の独立参照実装）。
fn quantize(x: f32) -> i32 {
    (x * 32768.0).round().clamp(-32768.0, 32767.0) as i32
}

/// 440Hz 正弦 1 秒（振幅 0.5）のモノラル信号。
fn sine_440(rate: u32) -> Vec<f32> {
    (0..rate as usize)
        .map(|t| 0.5 * (2.0 * std::f32::consts::PI * 440.0 * t as f32 / rate as f32).sin())
        .collect()
}

/// L = 440Hz 正弦、R = 定数 0.25 のステレオ 1 秒（interleaved）。
/// 定数チャンネルは flacenc の Constant サブフレーム経路も通す。
fn stereo_test_signal(rate: u32) -> Vec<f32> {
    let sine = sine_440(rate);
    let mut out = Vec::with_capacity(sine.len() * 2);
    for s in sine {
        out.push(s);
        out.push(0.25);
    }
    out
}

/// 書いた信号を claxon でデコードして全数照合する共通ヘルパ。
///
/// - サンプル数の一致
/// - 量子化値の完全一致（決定論）
/// - 元の f32 に対して量子化誤差 ±1/32768 + ε 以内
fn assert_roundtrip(path: &PathBuf, signal: &[f32], rate: u32, channels: u32) {
    let mut reader = claxon::FlacReader::open(path).unwrap();
    let info = reader.streaminfo();
    assert_eq!(info.sample_rate, rate);
    assert_eq!(info.channels, channels);
    assert_eq!(info.bits_per_sample, 16);
    assert_eq!(
        info.samples,
        Some((signal.len() / channels as usize) as u64),
        "STREAMINFO の総サンプル数"
    );

    let decoded: Vec<i32> = reader.samples().map(|s| s.unwrap()).collect();
    assert_eq!(decoded.len(), signal.len(), "デコードしたサンプル総数");

    let tol = 1.0 / 32768.0 + 1e-6;
    for (i, (&orig, &dec)) in signal.iter().zip(decoded.iter()).enumerate() {
        assert_eq!(dec, quantize(orig), "sample {i}: 量子化値の完全一致");
        let restored = dec as f32 / 32768.0;
        assert!(
            (restored - orig).abs() <= tol,
            "sample {i}: orig={orig} restored={restored}"
        );
    }
}

#[test]
fn roundtrip_stereo_sine_and_constant() {
    let rate = 48_000u32;
    let path = tmp_path("roundtrip_stereo.flac");
    let signal = stereo_test_signal(rate);

    let mut writer = FlacWriter::create(&path, rate, 2).unwrap();
    // flexaudio の 20ms チャンク（960 frames × 2ch = 1920 要素）を模して小刻みに渡す。
    // 1920 はブロック境界 (4096×2=8192) と揃わないので、端数持ち越しの経路も通る。
    for chunk in signal.chunks(960 * 2) {
        writer.write_chunk(chunk).unwrap();
    }
    writer.finalize().unwrap();

    // 48000 frames = 4096×11 + 2944 → 最終フレームは端数。
    assert_roundtrip(&path, &signal, rate, 2);
}

#[test]
fn roundtrip_mono_sine() {
    let rate = 44_100u32;
    let path = tmp_path("roundtrip_mono.flac");
    let signal = sine_440(rate);

    let mut writer = FlacWriter::create(&path, rate, 1).unwrap();
    // ブロックとも 20ms とも揃わない中途半端なチャンク長。
    for chunk in signal.chunks(1000) {
        writer.write_chunk(chunk).unwrap();
    }
    writer.finalize().unwrap();

    assert_roundtrip(&path, &signal, rate, 1);
}

#[test]
fn roundtrip_exact_block_multiple() {
    // ちょうど 2 ブロック分（端数なしで finalize する経路）。
    let rate = 16_000u32;
    let path = tmp_path("roundtrip_exact.flac");
    let signal: Vec<f32> = (0..4096 * 2)
        .map(|t| if t % 2 == 0 { 0.125 } else { -0.125 })
        .collect();

    let mut writer = FlacWriter::create(&path, rate, 1).unwrap();
    writer.write_chunk(&signal).unwrap();
    writer.finalize().unwrap();

    assert_roundtrip(&path, &signal, rate, 1);
}

#[test]
fn roundtrip_tail_shorter_than_16_samples() {
    // 端数最終フレームが 16 サンプル未満でもヘッダが仕様違反にならないこと
    // （STREAMINFO の min ブロックサイズに端数を数えると claxon が拒否する）。
    let rate = 16_000u32;
    let path = tmp_path("roundtrip_tiny_tail.flac");
    let signal: Vec<f32> = (0..4096 + 7)
        .map(|t| ((t % 100) as f32 - 50.0) / 128.0)
        .collect();

    let mut writer = FlacWriter::create(&path, rate, 1).unwrap();
    writer.write_chunk(&signal).unwrap();
    writer.finalize().unwrap();

    assert_roundtrip(&path, &signal, rate, 1);
}

#[test]
fn empty_stream_is_valid_flac() {
    let path = tmp_path("empty.flac");
    let writer = FlacWriter::create(&path, 48_000, 2).unwrap();
    writer.finalize().unwrap();

    let mut reader = claxon::FlacReader::open(&path).unwrap();
    let info = reader.streaminfo();
    assert_eq!(info.sample_rate, 48_000);
    assert_eq!(info.channels, 2);
    // FLAC では総サンプル数 0 は「不明」の意味なので claxon は None を返す。
    assert_eq!(info.samples, None);
    assert_eq!(reader.samples().count(), 0);
}

#[test]
fn drop_without_finalize_closes_best_effort() {
    let rate = 16_000u32;
    let path = tmp_path("drop_finalize.flac");
    // 4096 + 904 = 2 フレーム（端数つき）。
    let signal: Vec<f32> = vec![0.1; 5000];
    {
        let mut writer = FlacWriter::create(&path, rate, 1).unwrap();
        writer.write_chunk(&signal).unwrap();
        // finalize せずに drop。
    }
    assert_roundtrip(&path, &signal, rate, 1);
}

#[test]
fn compresses_below_90_percent_of_raw_pcm() {
    // 正弦波 1 秒。16bit 生 PCM（サンプル数 × 2 バイト）の 90% 未満に縮む
    // という緩い存在保証（正弦波は実際にはずっと良く縮む）。
    let rate = 48_000u32;
    let path = tmp_path("compression.flac");
    let signal = stereo_test_signal(rate);

    let mut writer = FlacWriter::create(&path, rate, 2).unwrap();
    writer.write_chunk(&signal).unwrap();
    writer.finalize().unwrap();

    let raw_bytes = signal.len() * 2;
    let flac_bytes = std::fs::metadata(&path).unwrap().len() as usize;
    assert!(
        flac_bytes * 10 < raw_bytes * 9,
        "flac={flac_bytes} bytes, raw 16bit PCM={raw_bytes} bytes"
    );
}

#[test]
fn rejects_zero_channels() {
    // バリデーションはファイル作成より先（存在しないディレクトリでも Io にならない）。
    let err = FlacWriter::create("/nonexistent-dir/never.flac", 48_000, 0).unwrap_err();
    assert!(matches!(err, EncodeError::Unsupported(_)), "{err:?}");
}

#[test]
fn rejects_three_channels() {
    let err = FlacWriter::create("/nonexistent-dir/never.flac", 48_000, 3).unwrap_err();
    assert!(matches!(err, EncodeError::Unsupported(_)), "{err:?}");
}

#[test]
fn rejects_unsupported_sample_rate() {
    let err = FlacWriter::create("/nonexistent-dir/never.flac", 0, 2).unwrap_err();
    assert!(matches!(err, EncodeError::Unsupported(_)), "{err:?}");
    let err = FlacWriter::create("/nonexistent-dir/never.flac", 192_000, 2).unwrap_err();
    assert!(matches!(err, EncodeError::Unsupported(_)), "{err:?}");
}

#[test]
fn rejects_chunk_length_not_multiple_of_channels() {
    let path = tmp_path("bad_chunk_len.flac");
    let mut writer = FlacWriter::create(&path, 48_000, 2).unwrap();
    let err = writer.write_chunk(&[0.0; 3]).unwrap_err();
    assert!(matches!(err, EncodeError::Unsupported(_)), "{err:?}");
    // エラーでは何も書かれず、正しい長さなら引き続き書ける。
    writer.write_chunk(&[0.0; 4]).unwrap();
    writer.finalize().unwrap();

    let mut reader = claxon::FlacReader::open(&path).unwrap();
    assert_eq!(reader.samples().count(), 4);
}
