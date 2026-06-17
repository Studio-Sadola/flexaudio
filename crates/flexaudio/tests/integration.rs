//! flexaudio facade の end-to-end 検証（MockBackend 駆動・ハードウェア不要）。
//!
//! `MockBackend`（合成サイン波）で「backend → RawRing → 加工スレッド →
//! Normalizer → ChunkRing → poll」の全配線が動くことを確認する。

use std::time::{Duration, Instant};

use flexaudio::core::types::{AudioChunk, ChunkFlags, OutputFormat, StreamConfig};
use flexaudio::{MockBackend, Stream};

/// 指定時間 poll してチャンクを集めるヘルパ。
fn collect_chunks(stream: &mut Stream, dur: Duration) -> Vec<AudioChunk> {
    let mut chunks = Vec::new();
    let start = Instant::now();
    while start.elapsed() < dur {
        while let Some(c) = stream.poll_chunk() {
            chunks.push(c);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    chunks
}

/// mono 44100 入力 → stereo 48000 / 960frame チャンクへ正規化される。
#[test]
fn mock_mono_44100_to_stereo_960_chunks() {
    let backend = Box::new(MockBackend::new(44100, 1, 440.0));
    let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
    stream.start().expect("start");

    let chunks = collect_chunks(&mut stream, Duration::from_millis(500));
    stream.stop();

    assert!(!chunks.is_empty(), "チャンクが1つも来ていない");
    for c in &chunks {
        assert_eq!(c.frames, 960, "20ms@48k = 960 frame でない");
        assert_eq!(c.data.len(), 960 * 2, "stereo interleaved (960*2) でない");
    }
    // seq は単調増加（DROP_OLDEST が起きても増加は保たれる）
    for w in chunks.windows(2) {
        assert!(
            w[1].seq > w[0].seq,
            "seq が単調増加していない: {} -> {}",
            w[0].seq,
            w[1].seq
        );
    }
}

/// 48000 stereo はパススルー経路で 960frame チャンクになる。
#[test]
fn mock_passthrough_48000_stereo() {
    let backend = Box::new(MockBackend::new(48000, 2, 440.0));
    let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
    stream.start().expect("start");

    let chunks = collect_chunks(&mut stream, Duration::from_millis(400));
    stream.stop();

    assert!(!chunks.is_empty(), "チャンクが来ていない");
    for c in &chunks {
        assert_eq!(c.frames, 960);
        assert_eq!(c.data.len(), 1920);
    }
}

/// 出力 {16000, 1}: 48k/stereo 入力 → 320 frame・320 sample（mono）チャンク。
/// peak/rms が妥当（合成サイン波で 0 でない・1.0 を大きく超えない）。
#[test]
fn mock_output_16k_mono() {
    let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
    let config = StreamConfig {
        output: OutputFormat {
            sample_rate: 16_000,
            channels: 1,
        },
        ..Default::default()
    };
    let mut stream = Stream::open(config, backend).expect("open");
    stream.start().expect("start");

    let chunks = collect_chunks(&mut stream, Duration::from_millis(500));
    stream.stop();

    assert!(!chunks.is_empty(), "16k/mono チャンクが来ていない");
    for c in &chunks {
        assert_eq!(c.frames, 320, "16k 20ms = 320 frame でない");
        assert_eq!(c.data.len(), 320, "mono interleaved (320*1) でない");
        // peak/rms 妥当性（合成サイン波 amplitude 0.5）。
        assert!(
            c.peak > 0.0 && c.peak <= 1.5,
            "peak が妥当でない: {}",
            c.peak
        );
        assert!(c.rms > 0.0 && c.rms <= 1.0, "rms が妥当でない: {}", c.rms);
        assert!(
            c.peak >= c.rms,
            "peak >= rms のはず: peak={} rms={}",
            c.peak,
            c.rms
        );
    }
}

/// 出力 {16000, 2}: → 320 frame・640 sample（stereo）チャンク。
#[test]
fn mock_output_16k_stereo() {
    let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
    let config = StreamConfig {
        output: OutputFormat {
            sample_rate: 16_000,
            channels: 2,
        },
        ..Default::default()
    };
    let mut stream = Stream::open(config, backend).expect("open");
    stream.start().expect("start");

    let chunks = collect_chunks(&mut stream, Duration::from_millis(500));
    stream.stop();

    assert!(!chunks.is_empty(), "16k/stereo チャンクが来ていない");
    for c in &chunks {
        assert_eq!(c.frames, 320, "16k 20ms = 320 frame でない");
        assert_eq!(c.data.len(), 640, "stereo interleaved (320*2) でない");
        assert!(
            c.peak > 0.0 && c.peak <= 1.5,
            "peak が妥当でない: {}",
            c.peak
        );
        assert!(c.rms > 0.0 && c.rms <= 1.0, "rms が妥当でない: {}", c.rms);
    }
}

/// 既定出力 {48000, 2} の回帰: frames==960 / data.len()==1920 / peak/rms 妥当。
#[test]
fn mock_default_output_regression_with_peak_rms() {
    let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
    let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
    stream.start().expect("start");

    let chunks = collect_chunks(&mut stream, Duration::from_millis(400));
    stream.stop();

    assert!(!chunks.is_empty(), "チャンクが来ていない");
    for c in &chunks {
        assert_eq!(c.frames, 960);
        assert_eq!(c.data.len(), 1920);
        assert!(c.peak > 0.0 && c.peak <= 1.5, "peak: {}", c.peak);
        assert!(c.rms > 0.0 && c.rms <= 1.0, "rms: {}", c.rms);
    }
}

/// `devices()` 統合列挙が panic せず `Ok(Vec)` を返し、各 DeviceInfo の不変条件
/// （id 非空 / loopback と source_kind の整合 / 正の rate・ch）を満たす。
/// ヘッドレス/CI 環境ではデバイスが無く空 Vec になり得るが、それも妥当（panic しないことが要点）。
#[test]
fn devices_enumeration_never_panics_and_is_consistent() {
    use flexaudio::core::types::SourceKind;

    let devices = flexaudio::devices().expect("devices() は Err を返さない設計");
    for d in &devices {
        assert!(!d.id.is_empty(), "id（安定キー）は空でない");
        assert!(d.sample_rate > 0, "sample_rate は正");
        assert!(d.channels > 0, "channels は正");
        match d.source_kind {
            SourceKind::Mic => assert!(!d.is_loopback, "Mic はループバックでない"),
            SourceKind::SystemLoopback => assert!(d.is_loopback, "SystemLoopback はループバック"),
            other => panic!("devices() が返さないはずの source_kind: {other:?}"),
        }
    }
}

/// open → start → stop がハング/panic なく完了する（スレッド join の健全性）。
#[test]
fn open_start_stop_is_clean() {
    let backend = Box::new(MockBackend::new(48000, 2, 440.0));
    let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
    stream.start().expect("start");
    std::thread::sleep(Duration::from_millis(50));
    stream.stop();
}

/// ソースのホットスワップ e2e（`switch_backend`・MockBackend 駆動）。
///
/// MockBackend(44100/mono/440Hz) で open+start → 数チャンク取得 →
/// `switch_backend(MockBackend(48000/stereo/220Hz))` で差し替え → さらに数チャンク
/// 取得。アサート:
/// 1. 全 chunk の seq が 0,1,2,... と隙間なく連続（切替で seq を触らない）。
/// 2. 切替後最初のチャンクに DISCONTINUITY が立つ（意図的切替）。
/// 3. 切替前後で frames/data.len が output 一定（既定 48k/2 → 960frame・1920sample）。
/// 4. 44100/mono → 48000/stereo の第 1 段再構成後も panic/破綻しない。
/// 5. pts_ns が単調増加（非減少）。
#[test]
fn switch_backend_keeps_seq_continuous_and_flags_discontinuity() {
    // 既定出力 {48000, 2}: 切替前後で frames=960 / data.len=1920 が不変であること。
    let backend = Box::new(MockBackend::new(44_100, 1, 440.0));
    let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
    stream.start().expect("start");

    // 切替前のネイティブフォーマット（mono 44100）。
    assert_eq!(stream.native_format(), (44_100, 1));

    // --- 切替前のチャンクを集める ---
    let before = collect_chunks(&mut stream, Duration::from_millis(300));
    assert!(!before.is_empty(), "切替前にチャンクが来ていない");
    let before_count = before.len();

    // --- ソースを 48000/stereo/220Hz へホットスワップ ---
    let new_backend = Box::new(MockBackend::new(48_000, 2, 220.0));
    stream
        .switch_backend(new_backend)
        .expect("switch_backend should succeed");

    // 切替後のネイティブフォーマットが新ソースの値へ更新されている。
    assert_eq!(stream.native_format(), (48_000, 2));

    // --- 切替後のチャンクを集める ---
    let after = collect_chunks(&mut stream, Duration::from_millis(300));
    stream.stop();
    // stop 後にリング残を取り切る。
    let mut after = after;
    while let Some(c) = stream.poll_chunk() {
        after.push(c);
    }
    assert!(!after.is_empty(), "切替後にチャンクが来ていない");

    // --- 全チャンクを時系列順に連結 ---
    let mut all: Vec<AudioChunk> = Vec::with_capacity(before.len() + after.len());
    all.extend(before);
    all.extend(after);

    // (1) seq が 0,1,2,... と隙間なく連続。
    for (i, c) in all.iter().enumerate() {
        assert_eq!(
            c.seq, i as u64,
            "seq が連続していない: index {i} に seq {} (gap)",
            c.seq
        );
    }

    // (2) 切替によって DISCONTINUITY がちょうど 1 回立ち、それが切替前後の境界
    //     （= before_count）以降の最初のチャンクに付く。
    //     `collect_chunks` の戻りと switch の発効の間に旧ソースのチャンクが 1〜2 個
    //     `after` 側へ紛れ込み得るため、厳密な index 等値ではなく「境界以降の最初の
    //     DISCONTINUITY」を探す（その手前の chunk は通常録音でフラグ無し）。
    let disc_positions: Vec<usize> = all
        .iter()
        .enumerate()
        .filter(|(_, c)| c.flags.contains(ChunkFlags::DISCONTINUITY))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        disc_positions.len(),
        1,
        "DISCONTINUITY はちょうど 1 回（切替）だけ立つはず: 位置={disc_positions:?}"
    );
    let disc_idx = disc_positions[0];
    // DISCONTINUITY は切替境界（before_count）以降に立つ（切替前は通常録音）。
    assert!(
        disc_idx >= before_count,
        "DISCONTINUITY が切替前に立っている: disc_idx={disc_idx} < before_count={before_count}"
    );
    // 意図的切替なので RECOVERED は付かない。
    assert!(
        !all[disc_idx].flags.contains(ChunkFlags::RECOVERED),
        "意図的切替なのに RECOVERED が立っている: flags={:?}",
        all[disc_idx].flags
    );
    // DISCONTINUITY より手前は全て通常録音（フラグ無し）。
    for c in &all[..disc_idx] {
        assert!(
            c.flags.is_empty(),
            "切替前の通常チャンクにフラグが立っている: seq={} flags={:?}",
            c.seq,
            c.flags
        );
    }

    // (3)/(4) 全チャンクで frames/data.len が output 一定（48k/2 → 960frame・1920sample）。
    //         切替で第 1 段（44100/mono → 48000/stereo）が再構成されても不変。
    for c in &all {
        assert_eq!(c.frames, 960, "frames が 960 でない: seq={}", c.seq);
        assert_eq!(
            c.data.len(),
            1920,
            "data.len が 1920 (960*2) でない: seq={}",
            c.seq
        );
    }

    // (5) pts_ns が単調増加（非減少）。
    for w in all.windows(2) {
        assert!(
            w[1].pts_ns >= w[0].pts_ns,
            "pts_ns が後退した: {} -> {}",
            w[0].pts_ns,
            w[1].pts_ns
        );
    }
}

/// `switch_source` は出力フォーマット変更要求を InvalidArg で弾く（連続性保護）。
#[test]
fn switch_source_rejects_output_change() {
    use flexaudio::core::types::{Error, SourceKind};

    let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
    let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
    stream.start().expect("start");

    // output だけ変える new_config。
    let new_config = StreamConfig {
        kind: SourceKind::Mic,
        output: OutputFormat {
            sample_rate: 16_000,
            channels: 1,
        },
        ..Default::default()
    };
    let err = stream
        .switch_source(new_config)
        .expect_err("output 変更は弾かれるべき");
    assert!(
        matches!(err, Error::InvalidArg(_)),
        "InvalidArg であるべき: {err:?}"
    );

    stream.stop();
}

/// 未 start で `switch_backend` を呼ぶと InvalidState（backend を起動しない分岐）。
#[test]
fn switch_backend_on_unstarted_is_invalid_state() {
    use flexaudio::core::types::Error;

    let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
    // open するが start しない。
    let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");

    let new_backend = Box::new(MockBackend::new(48_000, 2, 220.0));
    let err = stream
        .switch_backend(new_backend)
        .expect_err("未 start では InvalidState のはず");
    assert!(
        matches!(err, Error::InvalidState(_)),
        "InvalidState であるべき: {err:?}"
    );
    // 起動していないので stop は no-op（ハングしない）。
    stream.stop();
}

/// 未 start で `switch_source` を呼ぶと InvalidState。
#[test]
fn switch_source_on_unstarted_is_invalid_state() {
    use flexaudio::core::types::{Error, SourceKind};

    let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
    let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");

    let new_config = StreamConfig {
        kind: SourceKind::Mic,
        ..Default::default()
    };
    let err = stream
        .switch_source(new_config)
        .expect_err("未 start では InvalidState のはず");
    assert!(
        matches!(err, Error::InvalidState(_)),
        "InvalidState であるべき: {err:?}"
    );
    stream.stop();
}
