//! flexaudio facade の end-to-end 検証（MockBackend 駆動・ハードウェア不要）。
//!
//! `MockBackend`（合成サイン波）で「backend → RawRing → 加工スレッド →
//! Normalizer → ChunkRing → poll」の全配線が動くことを確認する。

use std::time::{Duration, Instant};

use flexaudio::core::types::{AudioChunk, ChunkFlags, OutputFormat, StreamConfig};
use flexaudio::{MockBackend, Stream};

/// 条件を満たすまで poll してチャンクを集めるヘルパ。
///
/// `done` は集まった全チャンクを受け取り、true を返したら収集を終える。壁時計の
/// 固定窓（「500ms 集めて N 個来るはず」）は、負荷でスレッド群がデスケジュール
/// されると窓内の生産量を保証できず原理的にフレークするため、「条件到達まで待つ」
/// 方式にする。`max_wait` は極端な負荷でも走り続けないためのハング保険で、超過時は
/// 集まったぶんを返す（不足は呼び出し側のアサーションが検出する）。
fn collect_until(
    stream: &mut Stream,
    max_wait: Duration,
    mut done: impl FnMut(&[AudioChunk]) -> bool,
) -> Vec<AudioChunk> {
    let mut chunks = Vec::new();
    let start = Instant::now();
    loop {
        while let Some(c) = stream.poll_chunk() {
            chunks.push(c);
        }
        if done(&chunks) || start.elapsed() >= max_wait {
            return chunks;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// [`collect_until`] の待ち上限。通常環境では条件到達で即抜けるので、これは
/// 「極端な負荷でスレッドがほとんど走れない」場合のハング防止でしかない。
const COLLECT_MAX_WAIT: Duration = Duration::from_secs(30);

/// 「パイプラインが実際に流れた」と認める最低チャンク数（20ms × 10 = 約 200ms 分）。
const MIN_CHUNKS: usize = 10;

/// mono 44100 入力 → stereo 48000 / 960frame チャンクへ正規化される。
#[test]
fn mock_mono_44100_to_stereo_960_chunks() {
    let backend = Box::new(MockBackend::new(44100, 1, 440.0));
    let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
    stream.start().expect("start");

    let chunks = collect_until(&mut stream, COLLECT_MAX_WAIT, |c| c.len() >= MIN_CHUNKS);
    stream.stop();

    assert!(
        chunks.len() >= MIN_CHUNKS,
        "チャンクが相応に来ていない: {}",
        chunks.len()
    );
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

    let chunks = collect_until(&mut stream, COLLECT_MAX_WAIT, |c| c.len() >= MIN_CHUNKS);
    stream.stop();

    assert!(
        chunks.len() >= MIN_CHUNKS,
        "チャンクが相応に来ていない: {}",
        chunks.len()
    );
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

    let chunks = collect_until(&mut stream, COLLECT_MAX_WAIT, |c| c.len() >= MIN_CHUNKS);
    stream.stop();

    assert!(
        chunks.len() >= MIN_CHUNKS,
        "16k/mono チャンクが相応に来ていない: {}",
        chunks.len()
    );
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

    let chunks = collect_until(&mut stream, COLLECT_MAX_WAIT, |c| c.len() >= MIN_CHUNKS);
    stream.stop();

    assert!(
        chunks.len() >= MIN_CHUNKS,
        "16k/stereo チャンクが相応に来ていない: {}",
        chunks.len()
    );
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

    let chunks = collect_until(&mut stream, COLLECT_MAX_WAIT, |c| c.len() >= MIN_CHUNKS);
    stream.stop();

    assert!(
        chunks.len() >= MIN_CHUNKS,
        "チャンクが相応に来ていない: {}",
        chunks.len()
    );
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
/// 2. フラグは許容集合（空 / 切替の DISCONTINUITY 単独 / 自動復帰の
///    RECOVERED|DISCONTINUITY）のみ。切替マーカー（DISCONTINUITY 単独）は高々 1 回・
///    切替境界以降で、境界以降に不連続の通知が必ず 1 つ以上ある。
/// 3. 切替前後で frames/data.len が output 一定（既定 48k/2 → 960frame・1920sample）。
/// 4. 44100/mono → 48000/stereo の第 1 段再構成後も panic/破綻しない。
/// 5. pts_ns の後退が構造的上界（バースト到着の再アンカー幅）を超えない。
#[test]
fn switch_backend_keeps_seq_continuous_and_flags_discontinuity() {
    // 切替マーカーの述語。意図的切替は DISCONTINUITY のみで、自動復帰の RECOVERED は
    // 付かない設計なので「RECOVERED を伴わない DISCONTINUITY」で識別する（切替が誤って
    // RECOVERED を立てるバグはこの述語に合致せず検出される）。
    fn is_switch_marker(c: &AudioChunk) -> bool {
        c.flags.contains(ChunkFlags::DISCONTINUITY) && !c.flags.contains(ChunkFlags::RECOVERED)
    }

    // 既定出力 {48000, 2}: 切替前後で frames=960 / data.len=1920 が不変であること。
    //
    // チャンクリングは既定の 50（=1 秒分）だと、負荷で poll 側だけが長く止まったとき
    // DROP_OLDEST が起きて本丸の「seq 連続」検証がスケジューラ依存になる。MockBackend の
    // 生産は実時間ペース（≤50 チャンク/秒）で、このテストの総所要はハング保険込みでも
    // 収集 2 回 × 30 秒 ≒ 3,000 チャンク強が上限なので、それを丸ごと収容できる容量に
    // してドロップを構造的に不可能にする。
    let config = StreamConfig {
        ring_capacity_chunks: 4096,
        ..Default::default()
    };
    let backend = Box::new(MockBackend::new(44_100, 1, 440.0));
    let mut stream = Stream::open(config, backend).expect("open");
    stream.start().expect("start");

    // 切替前のネイティブフォーマット（mono 44100）。
    assert_eq!(stream.native_format(), (44_100, 1));

    // --- 切替前のチャンクを集める（相応の数が揃うまで待つ） ---
    let before = collect_until(&mut stream, COLLECT_MAX_WAIT, |c| c.len() >= MIN_CHUNKS);
    assert!(
        before.len() >= MIN_CHUNKS,
        "切替前にチャンクが相応に来ていない: {}",
        before.len()
    );
    let before_count = before.len();

    // --- ソースを 48000/stereo/220Hz へホットスワップ ---
    let new_backend = Box::new(MockBackend::new(48_000, 2, 220.0));
    stream
        .switch_backend(new_backend)
        .expect("switch_backend should succeed");

    // 切替後のネイティブフォーマットが新ソースの値へ更新されている。
    assert_eq!(stream.native_format(), (48_000, 2));

    // --- 切替後のチャンクを集める ---
    // 不連続の通知（DISCONTINUITY）が観測でき、かつその後もチャンクが相応に流れ続ける
    // まで待つ（通知が出た瞬間で打ち切ると「切替後もストリームが続く」ことを証明でき
    // ない）。切替マーカー単独でなく DISCONTINUITY 全般を待つのは、極端な負荷では
    // 切替の通知が自動復帰チャンクへ合流し得るため（詳細は (2) のコメント参照）。
    let mut after = collect_until(&mut stream, COLLECT_MAX_WAIT, |c| {
        c.iter()
            .position(|chunk| chunk.flags.contains(ChunkFlags::DISCONTINUITY))
            .is_some_and(|pos| c.len() - (pos + 1) >= MIN_CHUNKS)
    });
    stream.stop();
    // stop 後にリング残を取り切る。
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

    // (2) フラグの許容集合検証（mix.rs の「値の許容集合」と同じ発想: スケジューラは
    //     チャンクの量やタイミングを動かせても、集合の外のフラグは作れない）。
    //     現れてよいのは:
    //       - 空 … 通常録音
    //       - DISCONTINUITY 単独 … 意図的切替のマーカー（RECOVERED は付かない設計）
    //       - RECOVERED|DISCONTINUITY … ウォッチドッグの自動復帰。極端な負荷で取り込みが
    //         2 秒超止まると正当に発生する（このテストの検証対象外だが混ざり得る）
    let recovery_flags = ChunkFlags::RECOVERED | ChunkFlags::DISCONTINUITY;
    for c in &all {
        assert!(
            c.flags.is_empty() || c.flags == ChunkFlags::DISCONTINUITY || c.flags == recovery_flags,
            "許容集合外のフラグ（切替/復帰のフラグ付けが壊れている）: seq={} flags={:?}",
            c.seq,
            c.flags
        );
    }
    //     切替マーカーは本来「ちょうど 1 回・境界以降」だが、切替直後に自動復帰が重なる
    //     と、切替の DISCONTINUITY が復帰チャンク（RECOVERED|DISCONTINUITY）へ合流して
    //     単独マーカーが現れないことがある（両 pending フラグは同じ次チャンクで OR 消費
    //     される）。そこで決定論に検証できる 3 つへ分ける:
    //       (2a) 単独マーカーは高々 1 回（2 回以上あれば切替の実装が壊れている）
    //       (2b) 単独マーカーは切替境界（= before_count）より前には現れない
    //       (2c) 境界以降に不連続の通知（単独 or 合流）が必ず 1 つ以上ある
    //     自動復帰が 1 つも無い実行（通常環境は常にこれ）では (2a)+(2c) と許容集合から
    //     「ちょうど 1 回・境界以降・RECOVERED なし」まで完全に確定する。
    let marker_positions: Vec<usize> = all
        .iter()
        .enumerate()
        .filter(|(_, c)| is_switch_marker(c))
        .map(|(i, _)| i)
        .collect();
    assert!(
        marker_positions.len() <= 1,
        "切替の DISCONTINUITY が複数回立っている: 位置={marker_positions:?}"
    );
    if let Some(&idx) = marker_positions.first() {
        assert!(
            idx >= before_count,
            "DISCONTINUITY が切替前に立っている: idx={idx} < before_count={before_count}"
        );
    }
    assert!(
        all[before_count..]
            .iter()
            .any(|c| c.flags.contains(ChunkFlags::DISCONTINUITY)),
        "切替境界以降に DISCONTINUITY が 1 つも無い（切替が不連続を通知していない）"
    );

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

    // (5) pts_ns の連続性。pts は「到着時刻を音声位置へ張り直すアンカー」からの外挿
    //     （normalizer の update_pts_anchor）なので、バースト到着（負荷で取り込みが
    //     止まり、溜まった生サンプルを一括 pop）では直後の再アンカーで小さく後退し得る
    //     ＝厳密な単調（非減少）は壁時計依存でアサートできない。ただし後退幅は構造的に
    //     有界: 一括 pop は RawRing／スクラッチの 48,000 サンプルが上限で、音声時間に
    //     して最長 48000 / 44100(mono) ≒ 1.09 秒（切替後の 48k/stereo なら 0.5 秒）＋
    //     normalizer 内部の保持ぶん（1〜2 チャンク）。この上界を超える後退（切替で
    //     クロック原点が巻き戻る類のバグ）だけを検出する。
    const MAX_PTS_BACKWARD_NS: i64 = 1_200_000_000;
    for w in all.windows(2) {
        assert!(
            w[1].pts_ns >= w[0].pts_ns - MAX_PTS_BACKWARD_NS,
            "pts_ns が再アンカーの上界を超えて後退した: {} -> {}",
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
