//! flexaudio-cli — 実機テスト用キャプチャ CLI。
//!
//! 既定マイク等から N 秒キャプチャし、出力フォーマット（既定 48000 Hz / stereo 2ch /
//! interleaved f32）のチャンクを集めて 16-bit PCM WAV に書き出す。
//! ピーク / RMS(dBFS) / チャンク数 / ドロップ数などのサマリも表示する。
//!
//! 出力フォーマットは `--output-rate <Hz>`（既定 48000）と
//! `--output-channels <1|2>`（既定 2）で指定する。内部正規形（48k/stereo）から
//! 第 2 段リサンプラ（rubato・アンチエイリアス込み）で再変換され、WAV ヘッダ・
//! stdout の rate/ch もこれに追従する。チャンクは時間ベース 20ms 固定なので、
//! レートに応じて 1 チャンクのフレーム数が変わる（48k=960 / 16k=320）。
//!
//! ```text
//! flexaudio-cli --source mic --seconds 5 --out mic.wav
//! flexaudio-cli --source system --output-rate 16000 --output-channels 1 --out 16k.wav --seconds 3
//! ```
//!
//! `--source mic` では `--device-id <ID>` で入力デバイスを選べる（ID は
//! `--list-devices` の ID 列＝安定キー＝デバイス名。省略で既定入力デバイス）。
//! device_id は **mic 専用**で、system（既定 render 固定）・process（target_pid 固定）
//! では無視される。
//!
//! process / system の 2 概念は別フラグに分離している（合成しない）:
//! - `--mode include|exclude`（①・**process 専用**・既定 include）: include=対象 PID だけ録る /
//!   exclude=対象 PID 以外の全システム音を録る（`--process-id` 必須）。
//! - `--exclude-self`（②・**system 専用**）: システム音から自ホスト（自プロセス）の音を除外する
//!   （フィードバック防止）。
//!   process ソースは `--exclude-self` を、system ソースは `--mode` を無視する。
//!   Linux では `--mode exclude` / `--exclude-self` はいずれも未実装で `Error::Unsupported`。
//!
//! ```text
//! flexaudio-cli --list-devices
//! flexaudio-cli --source mic --device-id "ステレオ ミキサー (Realtek(R) Audio)" --out cap.wav
//! ```
//!
//! また `--out -` を指定すると、WAV ではなく **ヘッダ無し raw PCM** を
//! stdout（バイナリ）へチャンク到着次第ストリーミングする。受け手
//! （例: WhisperApp の `spawn('flexaudio-cli', ...)` + stdout 読み）が
//! リアルタイムに音声を受け取れる。`--encoding f32|s16` で標本形式を選ぶ。
//! このモードでは stdout は PCM バイト専用とし、サマリ等のログは stderr へ出す。
//! `--seconds 0` で無限ストリーミング（パイプ切れ / Ctrl-C で綺麗に停止）。
//! raw PCM のレート/ch も出力フォーマットに追従する（受け手の `-r/-c` を合わせること）。
//!
//! ```text
//! flexaudio-cli --source system --out - --encoding s16 --seconds 0 | aplay -f S16_LE -r 48000 -c 2
//! flexaudio-cli --source system --out - --encoding s16 --output-rate 16000 --output-channels 1 --seconds 0 | aplay -f S16_LE -r 16000 -c 1
//! ```
//!
//! 入力デバイスが無い環境（homelab 等）では実キャプチャはできず、
//! 分かりやすいメッセージを表示して非ゼロ終了する（panic しない）。

use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};

use flexaudio::core::{AudioChunk, Error, OutputFormat, SourceKind, StreamConfig};
use flexaudio::{ProcessMode, Stream};

/// キャプチャするソース種別（CLI 引数用）。
#[derive(Debug, Clone, Copy, ValueEnum)]
enum SourceArg {
    /// 既定マイク入力。
    Mic,
    /// システム出力ループバック（Linux / Windows / macOS）。
    System,
    /// プロセス出力ループバック（Linux / Windows / macOS・`--process-id <PID>` 必須）。
    Process,
}

/// `--source process` の対象 PID の扱い（①・process 専用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ModeArg {
    /// 対象 PID（そのツリー）だけを録る（既定）。
    Include,
    /// 対象 PID（そのツリー）以外の全システム音を録る（`--process-id` 必須）。
    Exclude,
}

impl From<ModeArg> for ProcessMode {
    fn from(m: ModeArg) -> Self {
        match m {
            ModeArg::Include => ProcessMode::Include,
            ModeArg::Exclude => ProcessMode::Exclude,
        }
    }
}

/// stdout ストリーミング時の標本形式（`--out -` 専用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum EncodingArg {
    /// interleaved f32 little-endian（固定契約そのまま）。TS の Float32Array 直読み用。
    F32,
    /// interleaved i16 little-endian。`aplay -f S16_LE` 等の外部ツール互換。
    S16,
}

/// flexaudio キャプチャ CLI。
#[derive(Debug, Parser)]
#[command(
    name = "flexaudio-cli",
    about = "flexaudio キャプチャ CLI（実機テスト用）"
)]
struct Cli {
    /// 録音せず、利用可能なオーディオデバイスを一覧表示して終了する
    /// （`devices()` の統合列挙。`--source` 等とは独立に動く）。
    #[arg(long)]
    list_devices: bool,

    /// 録音せず、デバイスの着脱（ホットプラグ）を監視して stderr に表示し続ける
    /// （`watch_devices()`。Ctrl-C で停止。`--source` 等とは独立に動く）。
    #[arg(long)]
    watch_devices: bool,

    /// キャプチャするソース（mic / system\[Linux\] / process\[Linux\]）。
    #[arg(long, value_enum, default_value_t = SourceArg::Mic)]
    source: SourceArg,

    /// 録音中にソースをシームレスにホットスワップするスケジュール。
    /// `<src>:<secs>` をカンマ区切りで並べる（例: `mic:2,system:2,process:2`）。
    /// 各セグメントを指定秒だけ録ってから次ソースへ `switch_source` で切り替える。
    /// **出力先（ファイル/パイプ）は 1 本のまま**＝単一の連続チャンクストリーム。
    /// 指定すると `--source` / `--seconds` を上書きする（先頭セグメントが初期ソース、
    /// 各 secs の総和が総録音時間）。`process` を含む場合は `--process-id` が必須。
    /// 切替境界では `[switch] -> <kind>` を stderr に出す。切替失敗時は警告を出して
    /// 録音は継続する（旧ソースのまま）。WAV 出力では各 secs は 1 以上であること。
    /// `--mode`（①）/ `--exclude-self`（②）は全セグメントに一様に渡る
    /// （process セグメントは `--mode`、system セグメントは `--exclude-self` のみ honor）。
    #[arg(long)]
    sources: Option<String>,

    /// `--source process` の対象プロセス PID（Linux / Windows / macOS・process では必須）。
    /// 対象 PID のアプリ出力ノードへ fan-out リンクして複製で録る（非侵襲：
    /// ユーザーのスピーカーは鳴ったまま）。対象が後から鳴り始めるのは正常系。
    #[arg(long)]
    process_id: Option<u32>,

    /// `--source mic` で選ぶ入力デバイスの安定 ID（= デバイス名）。
    /// `--list-devices` の ID 列からコピーする。省略すると既定入力デバイス。
    /// **mic 専用**: system は既定 render、process は `--process-id` 固定で、
    /// この値は無視される（system の render 選択は将来対応）。
    /// 一致するデバイスが無ければクラッシュせず DeviceNotFound で終了する。
    #[arg(long)]
    device_id: Option<String>,

    /// `--source process` の対象 PID の扱い（①・process 専用・既定 include）。
    /// `include`=対象 PID だけ録る / `exclude`=対象 PID 以外の全システム音を録る
    /// （`--process-id` 必須）。mic / system では無視される。
    /// 旧来の「対象除外」は本フラグ `--mode exclude` へ移行した（旧 `--exclude-self` の
    /// 対象除外用途ではない）。Linux では `exclude` は未実装で `Error::Unsupported`。
    #[arg(long, value_enum, default_value_t = ModeArg::Include)]
    mode: ModeArg,

    /// システム音から**自ホスト（自プロセス）の再生音を除外**する（②・system 専用・
    /// フィードバックループ防止）。`--source system` でのみ効く。mic / process では無視。
    /// **意味更新**: 旧「対象除外」の用途は `--mode exclude`（①）へ移行済み。本フラグは
    /// 自ホスト除外（自プロセスの音をシステム録音から外す）専用。Linux では未実装で
    /// `Error::Unsupported`（Windows/macOS は OS プリミティブで対応）。
    #[arg(long, default_value_t = false)]
    exclude_self: bool,

    /// キャプチャ秒数。`0` で無限ストリーミング（`--out -` 想定、Ctrl-C / パイプ切れで停止）。
    #[arg(long, default_value_t = 5)]
    seconds: u64,

    /// 出力先。ファイルパスなら WAV 書き出し、`-` なら stdout へ raw PCM ストリーミング。
    #[arg(long, default_value = "capture.wav")]
    out: PathBuf,

    /// stdout ストリーミング時の標本形式（`--out -` 専用。WAV 出力では無視）。
    #[arg(long, value_enum, default_value_t = EncodingArg::F32)]
    encoding: EncodingArg,

    /// 出力サンプルレート（Hz）。既定 48000。例: `--output-rate 16000` で 16kHz へ
    /// ダウンサンプル。WAV ヘッダ・stdout の標本レートもこれに追従する。
    #[arg(long, default_value_t = 48_000)]
    output_rate: u32,

    /// 出力チャンネル数（1 = mono / 2 = stereo）。既定 2。stereo→mono は L/R 平均。
    #[arg(long, default_value_t = 2)]
    output_channels: u16,
}

impl Cli {
    /// `--out -`（ハイフン 1 文字）かどうか。true なら stdout へ raw PCM ストリーミング。
    fn is_stdout_stream(&self) -> bool {
        self.out == Path::new("-")
    }

    /// CLI 引数から [`OutputFormat`] を組み立てる。
    fn output_format(&self) -> OutputFormat {
        OutputFormat {
            sample_rate: self.output_rate,
            channels: self.output_channels,
        }
    }
}

/// `--sources` の 1 セグメント: 切り替え先ソースとその継続秒数。
#[derive(Debug, Clone, Copy)]
struct Segment {
    kind: SourceKind,
    secs: u32,
}

/// `--sources "mic:2,system:2,process:2"` を `Vec<Segment>` へパースする。
///
/// 各要素は `<src>:<secs>`。`<src>` は `mic|system|process`、`<secs>` は正の整数
/// （秒）。空・不正な要素・非対応 OS のソースはエラー（人間向け `String`）。
/// 非 Linux で system/process を含む場合もここで弾く。
fn parse_sources(spec: &str) -> std::result::Result<Vec<Segment>, String> {
    let mut segments = Vec::new();
    for (idx, raw) in spec.split(',').enumerate() {
        let item = raw.trim();
        if item.is_empty() {
            return Err(format!(
                "--sources の {} 番目が空です（形式: <src>:<secs>、例 mic:2）",
                idx + 1
            ));
        }
        let (src, secs_str) = item.split_once(':').ok_or_else(|| {
            format!(
                "--sources の要素 {item:?} は <src>:<secs> 形式である必要があります（例 mic:2）"
            )
        })?;
        let kind = match src.trim() {
            "mic" => SourceKind::Mic,
            "system" => {
                #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
                {
                    SourceKind::SystemLoopback
                }
                #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
                {
                    return Err(
                        "--sources の system（システム出力ループバック）は現在 Linux / Windows / macOS のみ対応です。"
                            .into(),
                    );
                }
            }
            "process" => {
                #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
                {
                    SourceKind::ProcessLoopback
                }
                #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
                {
                    return Err(
                        "--sources の process（プロセス出力ループバック）は現在 Linux / Windows / macOS のみ対応です。"
                            .into(),
                    );
                }
            }
            other => {
                return Err(format!(
                    "--sources の未知のソース {other:?}（mic|system|process のいずれか）"
                ))
            }
        };
        let secs: u32 = secs_str.trim().parse().map_err(|_| {
            format!("--sources の秒数 {secs_str:?} は正の整数である必要があります（例 mic:2）")
        })?;
        if secs == 0 {
            return Err(format!(
                "--sources の秒数は 1 以上である必要があります（要素 {item:?}）"
            ));
        }
        segments.push(Segment { kind, secs });
    }
    if segments.is_empty() {
        return Err("--sources が空です（例: mic:2,system:2）".into());
    }
    Ok(segments)
}

/// 指定 [`SourceKind`] と CLI の共有設定（output / pid / exclude_self）から
/// [`StreamConfig`] を組み立てる。`--sources` の各セグメント config 生成に使う。
fn config_for_kind(cli: &Cli, kind: SourceKind) -> StreamConfig {
    StreamConfig {
        kind,
        output: cli.output_format(),
        target_pid: cli.process_id,
        // mode（①）は process セグメントでのみ honor される（mic/system では facade が無視）。
        mode: cli.mode.into(),
        // exclude_self（②）は system セグメントでのみ honor される（mic/process では無視）。
        exclude_self: cli.exclude_self,
        // device_id は mic のみで honor される（system/process では facade が無視）。
        // セグメント config でも一様に載せておけば mic セグメントだけ効く。
        device_id: cli.device_id.clone(),
        ..Default::default()
    }
}

/// `--sources` のホットスワップスケジューラ。
///
/// 先頭セグメントは初期ソース（既に open/start 済み）。以降のセグメント境界
/// （秒の累積）に達したら `stream.switch_source()` で次ソースへ差し替える。
/// 出力先（ファイル/パイプ）は呼び出し側が 1 本に保つので、切替は単一の連続
/// チャンクストリームへ透過的に反映される（seq/PTS 連続は Stream 層が保証）。
///
/// 収集ループから毎周回 [`tick`](Self::tick) を呼ぶと、境界時刻を過ぎた切替を
/// まとめて実行する。切替失敗は `eprintln!` で警告し、録音は継続する（旧ソース
/// のまま）。境界で `[switch] -> <kind>` を stderr に出す。
struct SwitchScheduler {
    /// 次に切り替えるセグメント索引（1 始まり。先頭は初期ソースで切替対象外）。
    next: usize,
    /// 各境界の絶対時刻（`deadlines[i]` = セグメント `i+1` へ切り替える時刻）。
    deadlines: Vec<Instant>,
    /// 切替先 config（`configs[i]` = `deadlines[i]` で切り替える config）。
    configs: Vec<StreamConfig>,
    /// 表示ラベル（`labels[i]` = `configs[i]` の kind ラベル）。
    labels: Vec<&'static str>,
}

impl SwitchScheduler {
    /// セグメント計画から、`start` を基準にスケジューラを構築する。
    /// 先頭セグメントは初期ソースなので切替対象に含めない。
    fn new(cli: &Cli, segments: &[Segment], start: Instant) -> Self {
        let mut deadlines = Vec::new();
        let mut configs = Vec::new();
        let mut labels = Vec::new();
        let mut cumulative = 0u64;
        for (i, seg) in segments.iter().enumerate() {
            cumulative += seg.secs as u64;
            // 最後のセグメントの終端は「総録音時間」であり切替境界ではない。
            // セグメント i の終端 = セグメント i+1 への切替時刻（i+1 が存在する場合のみ）。
            if i + 1 < segments.len() {
                deadlines.push(start + Duration::from_secs(cumulative));
                let next_seg = segments[i + 1];
                configs.push(config_for_kind(cli, next_seg.kind));
                labels.push(source_kind_label(next_seg.kind));
            }
        }
        Self {
            next: 0,
            deadlines,
            configs,
            labels,
        }
    }

    /// 総録音時間（全セグメント秒の総和）。収集ループの deadline に使う。
    fn total_duration(segments: &[Segment]) -> Duration {
        let total: u64 = segments.iter().map(|s| s.secs as u64).sum();
        Duration::from_secs(total)
    }

    /// `now` までに到達した境界の切替を全て実行する。失敗は警告し継続。
    fn tick(&mut self, stream: &mut Stream, now: Instant) {
        while self.next < self.deadlines.len() && now >= self.deadlines[self.next] {
            let label = self.labels[self.next];
            let config = self.configs[self.next].clone();
            match stream.switch_source(config) {
                Ok(()) => {
                    eprintln!("[switch] -> {label}");
                }
                Err(e) => {
                    eprintln!("[switch] 警告: {label} への切替に失敗しました（録音は継続）: {e}");
                }
            }
            self.next += 1;
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // stdout ストリーミング時はエラーも stdout を汚さないよう stderr へ。
    // （run 内のログクロージャと同じ方針。main からのエラーは常に stderr で問題ない。）
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("エラー: {msg}");
            ExitCode::FAILURE
        }
    }
}

/// 実処理本体。失敗は人間向けメッセージ（`String`）として返す。
fn run(cli: &Cli) -> std::result::Result<(), String> {
    // --- デバイス一覧モード（録音せず列挙して終了） ---
    // `--source` 等とは独立。最優先で処理する。
    if cli.list_devices {
        return list_devices();
    }

    // --- デバイス着脱監視モード（録音せず監視し続ける） ---
    // list_devices の直後。`--source` 等とは独立に最優先で処理する。
    if cli.watch_devices {
        return watch_devices_loop();
    }

    let stdout_stream = cli.is_stdout_stream();

    // ログ出力先の切り替え:
    //   - stdout ストリーミング時 → 全ログを stderr へ（stdout は PCM 専用）。
    //   - ファイル出力時 → 従来通り stdout サマリでよい。
    // クロージャ 1 つで集約し、以降の println!/eprintln! はこれを通す。
    macro_rules! log {
        ($($arg:tt)*) => {
            if stdout_stream {
                eprintln!($($arg)*);
            } else {
                println!($($arg)*);
            }
        };
    }

    // --- --sources（ホットスワップスケジュール）の解決 ---
    // 指定時は --source / --seconds を上書きし、先頭セグメントを初期ソースにする。
    // process を含むなら --process-id 必須（switch_source 経由の build_backend が
    // PID 欠落で失敗するため、人間向け文言で先に弾く）。
    let segments: Option<Vec<Segment>> = match &cli.sources {
        None => None,
        Some(spec) => {
            let segs = parse_sources(spec)?;
            let needs_pid = segs.iter().any(|s| s.kind == SourceKind::ProcessLoopback);
            if needs_pid && cli.process_id.is_none() {
                return Err(
                    "--sources に process を含む場合は --process-id <PID> が必要です。".into(),
                );
            }
            Some(segs)
        }
    };

    // --- ソース種別から SourceKind / 表示ラベルを解決 ---
    // バックエンドの構築・選択は facade `flexaudio::open` に一元化されている
    // （DRY）。CLI 側では SourceKind・表示ラベルの決定と、人間向けの事前チェック
    // （process の PID 必須・非 Linux の system/process 拒否）だけを担う。
    // open は Box<dyn CaptureBackend> を内部で選んで Stream を返す。
    // --sources 指定時は先頭セグメントの kind を初期ソースに使う。
    let (kind, source_label): (SourceKind, &str) = if let Some(segs) = &segments {
        let first = segs[0].kind;
        let label = match first {
            SourceKind::Mic => "mic（既定入力デバイス）",
            SourceKind::SystemLoopback => "system（既定出力の monitor / PipeWire）",
            SourceKind::ProcessLoopback => "process（特定 PID 出力の fan-out / PipeWire）",
        };
        (first, label)
    } else {
        match cli.source {
            SourceArg::Mic => (SourceKind::Mic, "mic（既定入力デバイス）"),
            SourceArg::System => {
                #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
                {
                    (
                        SourceKind::SystemLoopback,
                        "system（既定出力の monitor / PipeWire）",
                    )
                }
                #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
                {
                    return Err(
                    "--source system（システム出力ループバック）は現在 Linux / Windows / macOS のみ対応です。"
                        .into(),
                );
                }
            }
            SourceArg::Process => {
                // process では PID 必須。無ければ分かりやすいエラーで止める
                // （facade も InvalidArg を返すが、CLI では人間向け文言で先に弾く）。
                // PID 必須チェックは OS 非依存（system/process 対応 OS なら常に要求）。
                if cli.process_id.is_none() {
                    return Err("--source process には --process-id <PID> が必要です。\
                     （対象プロセスの PID を指定してください。例: \
                     speaker-test を鳴らして得た PID）"
                        .into());
                }
                #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
                {
                    (
                        SourceKind::ProcessLoopback,
                        "process（特定 PID 出力の fan-out / PipeWire）",
                    )
                }
                #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
                {
                    return Err(
                    "--source process（プロセス出力ループバック）は現在 Linux / Windows / macOS のみ対応です。"
                        .into(),
                );
                }
            }
        }
    };

    // --- 出力フォーマット解決・検証 ---
    let output = cli.output_format();
    output.validate().map_err(|e| {
        format!(
            "出力フォーマット {}Hz/{}ch は非対応です: {e}",
            output.sample_rate, output.channels
        )
    })?;
    let out_rate = output.sample_rate;
    let out_ch = output.channels;

    // --- ストリームを開く（backend 選択は facade flexaudio::open に一元化） ---
    // open は config.kind に応じて Box<dyn CaptureBackend> を内部で選んで返す。
    // まだ start しない（二段方式）。native_format は開いた Stream から取る。
    let config = StreamConfig {
        kind,
        output,
        target_pid: cli.process_id,
        // mode（①・process 専用）。include 既定。
        mode: cli.mode.into(),
        // exclude_self（②・system 専用）。自ホスト除外。
        exclude_self: cli.exclude_self,
        // device_id は mic 選択用（system/process では facade が無視する）。
        device_id: cli.device_id.clone(),
        ..Default::default()
    };
    let mut stream = flexaudio::open(config).map_err(describe_error)?;

    // --- ネイティブフォーマット表示 ---
    let (native_rate, native_ch) = stream.native_format();
    log!("ソース            : {source_label}");
    // device_id 指定時は選択デバイスを明示（mic のみ有効。system/process では無視）。
    if let Some(id) = &cli.device_id {
        if kind == SourceKind::Mic {
            log!("デバイス ID        : {id}");
        } else {
            log!("デバイス ID        : {id}（注: mic 以外では無視されます）");
        }
    }
    log!("ネイティブフォーマット: {native_rate} Hz / {native_ch} ch");
    if stdout_stream {
        let enc = match cli.encoding {
            EncodingArg::F32 => "f32 LE",
            EncodingArg::S16 => "s16 LE",
        };
        log!("出力フォーマット   : {out_rate} Hz / {out_ch} ch / {enc} raw PCM（stdout）");
    } else {
        log!("出力フォーマット   : {out_rate} Hz / {out_ch} ch / 16-bit PCM WAV");
    }
    if let Some(segs) = &segments {
        let plan: Vec<String> = segs
            .iter()
            .map(|s| format!("{}:{}s", source_kind_label(s.kind), s.secs))
            .collect();
        let total: u32 = segs.iter().map(|s| s.secs).sum();
        log!(
            "スケジュール       : {}（計 {total} 秒・1 本の連続ストリーム）",
            plan.join(" -> ")
        );
    } else if cli.seconds == 0 {
        log!("キャプチャ秒数     : 無限（Ctrl-C / パイプ切れで停止）");
    } else {
        log!("キャプチャ秒数     : {} 秒", cli.seconds);
    }
    if stdout_stream {
        log!("出力先             : stdout（raw PCM ストリーミング）");
    } else {
        log!("出力パス           : {}", cli.out.display());
    }
    log!("");

    // --- キャプチャ開始（open 済みの Stream を start する二段方式） ---
    stream.start().map_err(describe_error)?;

    log!("キャプチャ中 ...");

    if stdout_stream {
        run_stdout_stream(cli, &mut stream, segments.as_deref())
    } else {
        run_wav(cli, &mut stream, output, segments.as_deref())
    }
}

/// `--list-devices`: 利用可能なオーディオデバイスを `devices()` で取得して表形式で表示する。
///
/// 列: SOURCE（mic/system/process）/ LOOPBACK / DEFAULT / RATE / CH / NAME / ID。
/// id は安定 ID（cpal=デバイス名 / PipeWire=node.name）。デバイスが 1 つも無い環境では
/// その旨を表示する（エラーにはしない）。
fn list_devices() -> std::result::Result<(), String> {
    let devices = flexaudio::devices().map_err(|e| format!("デバイス列挙に失敗しました: {e}"))?;

    if devices.is_empty() {
        println!("利用可能なオーディオデバイスが見つかりませんでした。");
        println!(
            "（PipeWire/オーディオデバイスのある実機で実行してください。\
             Linux は system 列挙に PipeWire セッションが必要です。）"
        );
        return Ok(());
    }

    println!("利用可能なオーディオデバイス: {} 件", devices.len());
    println!();
    // ヘッダ。固定幅で揃える（id/name は可変長なので末尾）。
    println!(
        "{:<7} {:<8} {:<7} {:>6} {:>3}  {:<28} ID",
        "SOURCE", "LOOPBACK", "DEFAULT", "RATE", "CH", "NAME"
    );
    println!("{}", "-".repeat(88));
    for d in &devices {
        println!(
            "{:<7} {:<8} {:<7} {:>6} {:>3}  {:<28} {}",
            source_kind_label(d.source_kind),
            if d.is_loopback { "yes" } else { "no" },
            if d.is_default { "*" } else { "" },
            d.sample_rate,
            d.channels,
            truncate(&d.name, 28),
            d.id,
        );
    }
    println!();
    println!(
        "（DEFAULT の * は OS 既定デバイス。ID は `--source mic --device-id <ID>` で\
         入力デバイスを選ぶのに使える安定キー。system/process では device_id は無視。）"
    );
    Ok(())
}

/// `--watch-devices`: デバイスの着脱（ホットプラグ）を `watch_devices()` で監視し、
/// 着脱イベントを **stderr** へ表示し続ける（Ctrl-C で停止）。
///
/// stdout は将来の機械可読出力用に空けておく既存思想に従い、ログ・イベントは全て
/// stderr へ出す。起動時に `devices()` で既存デバイスを数えて件数を表示する。
///
/// 表示形式（いずれも stderr）:
/// - `[+] ADDED   <source> <name> (<id>)` — デバイス追加
/// - `[-] REMOVED <id>` — デバイス取り外し（id = node.name のみ）
/// - `[*] DEFAULT <source> -> <id>` — 既定デバイス切替
fn watch_devices_loop() -> std::result::Result<(), String> {
    use flexaudio::core::DeviceEvent;

    // 起動時に既存デバイス数を数えて案内（stderr）。列挙失敗は致命的にしない。
    let existing = flexaudio::devices().map(|d| d.len()).unwrap_or(0);
    eprintln!("デバイス着脱監視を開始しました（既存 {existing} 件）。Ctrl-C で停止します。");
    eprintln!();

    // Ctrl-C(SIGINT) で停止フラグを倒す。
    let running = Arc::new(AtomicBool::new(true));
    {
        let r = running.clone();
        ctrlc::set_handler(move || {
            r.store(false, Ordering::SeqCst);
        })
        .map_err(|e| format!("Ctrl-C ハンドラの登録に失敗しました: {e}"))?;
    }

    // 監視開始。PipeWire 不在等でも縮退して Ok（着脱が来ないだけ）。
    let mut watcher =
        flexaudio::watch_devices().map_err(|e| format!("デバイス監視の開始に失敗しました: {e}"))?;

    while running.load(Ordering::SeqCst) {
        while let Some(ev) = watcher.poll_event() {
            match ev {
                DeviceEvent::Added(info) => {
                    eprintln!(
                        "[+] ADDED   {:<7} {} ({})",
                        source_kind_label(info.source_kind),
                        info.name,
                        info.id,
                    );
                }
                DeviceEvent::Removed { id } => {
                    eprintln!("[-] REMOVED {id}");
                }
                DeviceEvent::DefaultChanged { kind, id } => {
                    eprintln!("[*] DEFAULT {:<7} -> {}", source_kind_label(kind), id,);
                }
                // DeviceEvent は #[non_exhaustive]。将来バリアント追加に備えた前方互換アーム
                // （未知種別もデバッグ表現で表示し、握り潰さない）。
                other => {
                    eprintln!("[?] UNKNOWN  {other:?}");
                }
            }
        }
        // 着脱は低頻度。空転を避けて適度に眠る（応答性 100ms で十分）。
        thread::sleep(Duration::from_millis(100));
    }

    watcher.stop();
    eprintln!();
    eprintln!("デバイス着脱監視を停止しました（Ctrl-C）。");
    Ok(())
}

/// [`SourceKind`] を CLI 表示用の短いラベルへ。
fn source_kind_label(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Mic => "mic",
        SourceKind::SystemLoopback => "system",
        SourceKind::ProcessLoopback => "process",
    }
}

/// 表示用に文字列を `max` 文字（char 単位）で切り詰める（超過分は `…`）。
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let keep = max.saturating_sub(1);
        let mut t: String = s.chars().take(keep).collect();
        t.push('…');
        t
    }
}

/// WAV 出力経路（従来挙動）。N 秒（>0）収集して 16-bit WAV を書き出し、stdout にサマリ表示。
/// `output` は出力フォーマット（WAV ヘッダの rate/ch・録れた秒数の算出に使う）。
fn run_wav(
    cli: &Cli,
    stream: &mut Stream,
    output: OutputFormat,
    segments: Option<&[Segment]>,
) -> std::result::Result<(), String> {
    // 総録音時間: --sources 指定時はセグメント総和、無ければ --seconds。
    // WAV 経路で --seconds 0 は意味を持たない（無限に貯め続けてしまう）。従来通り拒否。
    // （--sources の各 secs は parse_sources で 1 以上を強制済みなので 0 にはならない。）
    if segments.is_none() && cli.seconds == 0 {
        stream.stop();
        return Err(
            "--seconds 0（無限）は raw PCM ストリーミング（--out -）専用です。\
             WAV 出力では 1 以上を指定してください。"
                .into(),
        );
    }

    let start = Instant::now();
    let total = match segments {
        Some(segs) => SwitchScheduler::total_duration(segs),
        None => Duration::from_secs(cli.seconds),
    };
    // ホットスワップスケジューラ（--sources 指定時のみ・出力先は 1 本のまま）。
    let mut scheduler = segments.map(|segs| SwitchScheduler::new(cli, segs, start));

    // --- 総録音時間ぶん poll_chunk をループして全チャンクを収集 ---
    let mut chunks: Vec<AudioChunk> = Vec::new();
    let deadline = start + total;
    while Instant::now() < deadline {
        // セグメント境界に達していればソースを切り替える（貯める先は同一＝1 ファイル）。
        if let Some(sch) = scheduler.as_mut() {
            sch.tick(stream, Instant::now());
        }

        let mut got_any = false;
        while let Some(chunk) = stream.poll_chunk() {
            chunks.push(chunk);
            got_any = true;
        }
        // poll_event は表示用に消化（イベントがあれば出す）。
        while let Some(ev) = stream.poll_event() {
            println!("  イベント: {ev:?}");
        }
        if !got_any {
            // チャンク 1 つ ≈ 20ms。空転を避けて適度に眠る。
            thread::sleep(Duration::from_millis(10));
        }
    }

    let dropped = stream.dropped_chunks();
    stream.stop();

    // stop 後にもリングへ残ったチャンクを取り切る。
    while let Some(chunk) = stream.poll_chunk() {
        chunks.push(chunk);
    }

    if chunks.is_empty() {
        return Err(
            "チャンクを 1 つも取得できませんでした。\
             デバイスは開けましたがサンプルが流れていません（ミュート/権限等を確認してください）。"
                .into(),
        );
    }

    // --- 解析（ピーク / RMS）と WAV 書き出し ---
    // NOTE: WAV は現状 s16 固定。将来 f32 WAV が必要になれば encoding を WAV にも波及させる余地あり。
    // ヘッダの rate/ch は出力フォーマット（output）に追従する。
    let total_frames: usize = chunks.iter().map(|c| c.frames).sum();
    let stats =
        write_wav(&cli.out, &chunks, output).map_err(|e| format!("WAV 書き出し失敗: {e}"))?;

    let captured_secs = total_frames as f64 / output.sample_rate as f64;
    let rms_dbfs = if stats.rms > 0.0 {
        20.0 * stats.rms.log10()
    } else {
        f64::NEG_INFINITY
    };
    let peak_dbfs = if stats.peak > 0.0 {
        20.0 * (stats.peak as f64).log10()
    } else {
        f64::NEG_INFINITY
    };

    println!();
    println!("=== 結果 ===");
    println!("取得チャンク数     : {}", chunks.len());
    println!("総フレーム数       : {total_frames}");
    println!("録れた秒数         : {captured_secs:.3} 秒");
    println!("ドロップチャンク数 : {dropped}");
    println!(
        "ピーク             : {:.4}（{}）",
        stats.peak,
        fmt_dbfs(peak_dbfs)
    );
    println!(
        "RMS                : {:.6}（{}）",
        stats.rms,
        fmt_dbfs(rms_dbfs)
    );
    println!("WAV 書き出し       : {}", cli.out.display());

    Ok(())
}

/// stdout raw PCM ストリーミング経路。
///
/// チャンク到着次第すぐ stdout へ書き、各回 flush して溜め込まない（低レイテンシ）。
/// `--seconds 0` なら無限（Ctrl-C / パイプ切れ `BrokenPipe` で正常停止）、
/// `--seconds N>0` なら N 秒で停止。どちらも stop 後にリング残チャンクを出し切る。
fn run_stdout_stream(
    cli: &Cli,
    stream: &mut Stream,
    segments: Option<&[Segment]>,
) -> std::result::Result<(), String> {
    // --sources 指定時はセグメント総和ぶんの有限録音（--seconds は上書き）。
    let infinite = segments.is_none() && cli.seconds == 0;

    // Ctrl-C(SIGINT) フラグ。無限時に押されたら綺麗に停止する。
    // ctrlc は重複登録すると Err を返すので、無限時のみ登録する。
    let running = Arc::new(AtomicBool::new(true));
    if infinite {
        let r = running.clone();
        ctrlc::set_handler(move || {
            r.store(false, Ordering::SeqCst);
        })
        .map_err(|e| format!("Ctrl-C ハンドラの登録に失敗しました: {e}"))?;
    }

    // stdout をロックして BufWriter で包む。チャンクごとに flush するので溜め込みは無い。
    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());

    let start = Instant::now();
    // 有限録音の deadline: --sources 指定時はセグメント総和、無ければ --seconds。
    let deadline = if infinite {
        None
    } else {
        let dur = match segments {
            Some(segs) => SwitchScheduler::total_duration(segs),
            None => Duration::from_secs(cli.seconds),
        };
        Some(start + dur)
    };
    // ホットスワップスケジューラ（--sources 指定時のみ・stdout は 1 パイプのまま）。
    let mut scheduler = segments.map(|segs| SwitchScheduler::new(cli, segs, start));

    let mut wrote_any = false;
    let mut broken_pipe = false;

    // メインループ: ポーリングしてチャンクを stdout へ流す。
    'outer: loop {
        // 停止条件チェック（無限時は Ctrl-C、有限時は deadline）。
        if infinite {
            if !running.load(Ordering::SeqCst) {
                break;
            }
        } else if let Some(dl) = deadline {
            if Instant::now() >= dl {
                break;
            }
        }

        // セグメント境界に達していればソースを切り替える（出力先は同一＝1 パイプ）。
        if let Some(sch) = scheduler.as_mut() {
            sch.tick(stream, Instant::now());
        }

        let mut got_any = false;
        while let Some(chunk) = stream.poll_chunk() {
            got_any = true;
            match write_chunk(&mut out, &chunk, cli.encoding) {
                Ok(()) => wrote_any = true,
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                    // 受け手が閉じた（| head 等）。エラーにせず正常停止へ。
                    broken_pipe = true;
                    break 'outer;
                }
                Err(e) => return Err(format!("stdout への書き込みに失敗しました: {e}")),
            }
        }

        // イベントは stderr へ（stdout は PCM 専用）。
        while let Some(ev) = stream.poll_event() {
            eprintln!("  イベント: {ev:?}");
        }

        if !got_any {
            // チャンク 1 つ ≈ 20ms。空転を避けて適度に眠る。
            thread::sleep(Duration::from_millis(10));
        }
    }

    let dropped = stream.dropped_chunks();
    stream.stop();

    // stop 後にもリングへ残ったチャンクを出し切る（パイプ切れ後はスキップ）。
    if !broken_pipe {
        while let Some(chunk) = stream.poll_chunk() {
            match write_chunk(&mut out, &chunk, cli.encoding) {
                Ok(()) => wrote_any = true,
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                    broken_pipe = true;
                    break;
                }
                Err(e) => return Err(format!("stdout への書き込みに失敗しました: {e}")),
            }
        }
    }

    // 最終 flush。パイプ切れはここでも正常扱い。
    if !broken_pipe {
        if let Err(e) = out.flush() {
            if e.kind() != io::ErrorKind::BrokenPipe {
                return Err(format!("stdout の flush に失敗しました: {e}"));
            }
            broken_pipe = true;
        }
    }

    // --- サマリ（stderr） ---
    eprintln!();
    eprintln!("=== 結果（stderr） ===");
    if broken_pipe {
        eprintln!("停止理由           : 受け手がパイプを閉じました（正常終了）");
    } else if infinite {
        eprintln!("停止理由           : Ctrl-C（正常終了）");
    } else {
        eprintln!("停止理由           : {} 秒経過", cli.seconds);
    }
    eprintln!("ドロップチャンク数 : {dropped}");

    // パイプ切れ・Ctrl-C は「受け手都合の停止」なので、サンプル 0 でもエラーにしない。
    // 有限秒指定で素直に終わったのに 1 サンプルも出ていない場合だけ警告する。
    if !wrote_any && !broken_pipe && !infinite {
        return Err(
            "チャンクを 1 つも取得できませんでした。\
             デバイスは開けましたがサンプルが流れていません（ミュート/権限等を確認してください）。"
                .into(),
        );
    }

    Ok(())
}

/// 1 チャンクの interleaved f32 を指定 encoding で `out` へ書く（little-endian）。
///
/// 各サンプル単位の小書き込みを避けるため、チャンク分のバイト列を一旦まとめてから
/// 1 回の `write_all` で出す。書き込み後すぐ flush（低レイテンシ・溜め込み無し）。
fn write_chunk<W: Write>(out: &mut W, chunk: &AudioChunk, encoding: EncodingArg) -> io::Result<()> {
    match encoding {
        EncodingArg::F32 => {
            // f32 LE: 契約そのまま。1 サンプル 4 byte。
            let mut buf = Vec::with_capacity(chunk.data.len() * 4);
            for &x in &chunk.data {
                buf.extend_from_slice(&x.to_le_bytes());
            }
            out.write_all(&buf)?;
        }
        EncodingArg::S16 => {
            // s16 LE: (clamp(-1,1) * 32767) as i16。1 サンプル 2 byte。
            let mut buf = Vec::with_capacity(chunk.data.len() * 2);
            for &x in &chunk.data {
                let s = (x.clamp(-1.0, 1.0) * 32767.0) as i16;
                buf.extend_from_slice(&s.to_le_bytes());
            }
            out.write_all(&buf)?;
        }
    }
    // 届いたら即出す。BufWriter に溜め込まない。
    out.flush()
}

/// WAV 書き出しと同時に計算する信号統計。
struct Stats {
    /// 全サンプルの絶対値の最大（線形 0.0..=1.0 目安）。
    peak: f32,
    /// 全サンプルの二乗平均平方根（線形）。
    rms: f64,
}

/// チャンク列を `output.sample_rate` / `output.channels` / 16-bit PCM WAV として
/// `path` へ書き出す。
///
/// 各チャンクの interleaved f32 を `(x.clamp(-1,1) * 32767) as i16` で量子化する。
/// 併せてピーク / RMS（線形）を計算して返す。
fn write_wav(
    path: &std::path::Path,
    chunks: &[AudioChunk],
    output: OutputFormat,
) -> hound::Result<Stats> {
    let spec = hound::WavSpec {
        channels: output.channels,
        sample_rate: output.sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)?;

    let mut peak: f32 = 0.0;
    let mut sum_sq: f64 = 0.0;
    let mut count: u64 = 0;

    for chunk in chunks {
        for &x in &chunk.data {
            let a = x.abs();
            if a > peak {
                peak = a;
            }
            sum_sq += (x as f64) * (x as f64);
            count += 1;

            let clamped = x.clamp(-1.0, 1.0);
            let s = (clamped * 32767.0) as i16;
            writer.write_sample(s)?;
        }
    }
    writer.finalize()?;

    let rms = if count > 0 {
        (sum_sq / count as f64).sqrt()
    } else {
        0.0
    };

    Ok(Stats { peak, rms })
}

/// dBFS を読みやすく整形する（無音時は `-inf dBFS`）。
fn fmt_dbfs(db: f64) -> String {
    if db.is_finite() {
        format!("{db:.1} dBFS")
    } else {
        "-inf dBFS（無音）".into()
    }
}

/// `flexaudio` の [`Error`] を人間向けメッセージへ変換する。
///
/// デバイス不在（`DeviceNotFound`）は実機での実行を促す案内に置き換える。
fn describe_error(err: Error) -> String {
    match err {
        Error::DeviceNotFound => {
            "入力デバイスが見つかりません。マイクのある実機で実行してください。".into()
        }
        Error::PermissionDenied => {
            "マイクへのアクセス権限がありません。OS のマイク権限設定を確認してください。".into()
        }
        Error::DeviceLost => "キャプチャ中に入力デバイスが失われました（切断など）。".into(),
        other => format!("ストリーム初期化に失敗しました: {other}"),
    }
}
