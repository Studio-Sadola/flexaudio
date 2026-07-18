//! flexaudio-cli — リファレンス・キャプチャ CLI。
//!
//! 既定マイク等から N 秒キャプチャし、出力フォーマット（既定 48000 Hz / stereo 2ch /
//! interleaved f32）のチャンクを集めて 16-bit PCM WAV に書き出す。ピーク / RMS(dBFS) /
//! チャンク数 / ドロップ数のサマリも出す。
//!
//! 出力フォーマットは `--output-rate <Hz>`（既定 48000）と
//! `--output-channels <1|2>`（既定 2）で指定する。内部正規形（48k/stereo）から
//! 第 2 段リサンプラ（rubato・アンチエイリアス込み）で再変換し、WAV ヘッダ・stdout の
//! rate/ch もこれに追従する。チャンクは時間ベース 20ms 固定なので、レートで 1 チャンクの
//! フレーム数が変わる（48k=960 / 16k=320）。
//!
//! ```text
//! flexaudio-cli --source mic --seconds 5 --out mic.wav
//! flexaudio-cli --source system --output-rate 16000 --output-channels 1 --out 16k.wav --seconds 3
//! ```
//!
//! `--device-id <ID>` でデバイスを選べる（ID は `--list-devices` の ID 列）。mic では
//! 入力デバイス、system では出力エンドポイントを選ぶ。省略で既定（mic=既定入力 /
//! system=既定出力）。process は `--process-id` で対象を決めるので device_id は無視される。
//!
//! process と system の概念は別フラグに分けてある（混ぜない）:
//! - `--mode include|exclude`（process 専用・既定 include）: include=対象 PID だけ録る /
//!   exclude=対象 PID 以外の全システム音を録る（`--process-id` 必須）。
//! - `--exclude-self`（system 専用）: システム音から自プロセスの再生音を除く
//!   （フィードバック防止）。
//!   process ソースは `--exclude-self` を、system ソースは `--mode` を無視する。
//!
//! ```text
//! flexaudio-cli --list-devices
//! flexaudio-cli --source mic --device-id "ステレオ ミキサー (Realtek(R) Audio)" --out cap.wav
//! ```
//!
//! `--out -` を指定すると、WAV ではなくヘッダ無し raw PCM を stdout（バイナリ）へ
//! チャンク到着次第ストリーミングする。受け手（例: ホストアプリが
//! `spawn('flexaudio-cli', ...)` して stdout を読む）がリアルタイムに音声を受け取れる。
//! `--encoding f32|s16` で標本形式を選ぶ。このモードでは stdout を PCM バイト専用とし、
//! サマリ等のログは stderr へ出す。`--seconds 0` で無限ストリーミング（パイプ切れ /
//! Ctrl-C で停止）。raw PCM のレート/ch も出力フォーマットに追従する（受け手の `-r/-c`
//! を合わせること）。
//!
//! ```text
//! flexaudio-cli --source system --out - --encoding s16 --seconds 0 | aplay -f S16_LE -r 48000 -c 2
//! flexaudio-cli --source system --out - --encoding s16 --output-rate 16000 --output-channels 1 --seconds 0 | aplay -f S16_LE -r 16000 -c 1
//! ```
//!
//! `--split-seconds <N>`（既定 0 = 分割なし）で WAV を N 秒毎の連番ファイルへ分割
//! 録音できる。`--out rec.wav` なら `rec-001.wav, rec-002.wav, ...`（拡張子の前に
//! 3 桁ゼロ詰め連番。1000 ファイル目以降は桁が自然に増える）。境界はチャンク粒度
//! （20ms）で「書き込んだフレーム数が `N × 出力サンプルレート` 以上になったら次へ」
//! なので、各ファイルは指定秒より最大 1 チャンク（±20ms）長くなりうる。チャンクは
//! 分割せず取りこぼしも無い（次ファイルは次のチャンクから始まる）。フレーム数ベース
//! なので `--sources`（ホットスワップ）や mix とも直交して効く。stdout ストリーミング
//! （`--out -`）とは併用できない（起動時にエラー）。
//!
//! ```text
//! flexaudio-cli --source mic --seconds 30 --split-seconds 10 --out rec.wav
//! ```
//!
//! 入力デバイスが無い環境（サーバー・CI 等）では実キャプチャはできず、分かりやすい
//! メッセージを表示して非ゼロ終了する（panic しない）。

use std::fs::File;
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
    /// マイク + システム音声のミックス（Linux / Windows / macOS）。
    /// デバイスは `--mic-device-id` / `--system-device-id`、側別ゲインは
    /// `--mic-gain` / `--system-gain` で指定する。
    Mix,
}

/// `--source process` の対象 PID の扱い（process 専用）。
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
    /// interleaved f32 little-endian（内部正規形そのまま）。
    F32,
    /// interleaved i16 little-endian。`aplay -f S16_LE` 等の外部ツール互換。
    S16,
}

/// flexaudio キャプチャ CLI。
#[derive(Debug, Parser)]
#[command(name = "flexaudio-cli", about = "flexaudio キャプチャ CLI")]
struct Cli {
    /// 録音せず、利用可能なオーディオデバイスを一覧表示して終了する
    /// （`devices()` の統合列挙。`--source` 等とは独立に動く）。
    #[arg(long)]
    list_devices: bool,

    /// 録音せず、デバイスの着脱（ホットプラグ）を監視して stderr に表示し続ける
    /// （`watch_devices()`。Ctrl-C で停止。`--source` 等とは独立に動く）。
    #[arg(long)]
    watch_devices: bool,

    /// キャプチャするソース（mic / system / process）。
    #[arg(long, value_enum, default_value_t = SourceArg::Mic)]
    source: SourceArg,

    /// 録音中にソースをシームレスにホットスワップするスケジュール。
    /// `<src>:<secs>` をカンマ区切りで並べる（例: `mic:2,system:2,process:2`）。
    /// 各セグメントを指定秒だけ録ってから次ソースへ `switch_source` で切り替える。
    /// 出力先（ファイル/パイプ）は 1 本のままで、単一の連続チャンクストリームになる。
    /// 指定すると `--source` / `--seconds` を上書きする（先頭セグメントが初期ソース、
    /// 各 secs の総和が総録音時間）。`process` を含むなら `--process-id` が必須。
    /// 切替境界で `[switch] -> <kind>` を stderr に出す。切替失敗時は警告を出して
    /// 録音は継続する（旧ソースのまま）。WAV 出力では各 secs は 1 以上であること。
    /// `--mode` / `--exclude-self` は全セグメントに一様に渡り、process セグメントは
    /// `--mode`、system セグメントは `--exclude-self` だけが効く。
    #[arg(long)]
    sources: Option<String>,

    /// `--source process` の対象プロセス PID（Linux / Windows / macOS・process では必須）。
    /// 対象 PID のアプリ出力ノードへ fan-out リンクして複製で録る。非侵襲で、ユーザーの
    /// スピーカーは鳴ったまま。対象が後から鳴り始めるのは正常。
    #[arg(long)]
    process_id: Option<u32>,

    /// 選ぶデバイスの ID（`--list-devices` の ID 列からコピーする）。mic では入力デバイス、
    /// system では出力エンドポイントを選ぶ。省略すると既定（mic=既定入力 / system=既定出力）。
    /// process は `--process-id` で対象を決めるのでこの値は無視される。一致するデバイスが
    /// 無ければクラッシュせず DeviceNotFound で終了する。
    #[arg(long)]
    device_id: Option<String>,

    /// `--source process` の対象 PID の扱い（process 専用・既定 include）。
    /// `include`=対象 PID だけ録る / `exclude`=対象 PID 以外の全システム音を録る
    /// （`--process-id` 必須・Linux / Windows / macOS 対応）。mic / system では無視される。
    /// 対象を除外したい用途は `--mode exclude` を使う（自プロセス除外用途ではない）。
    #[arg(long, value_enum, default_value_t = ModeArg::Include)]
    mode: ModeArg,

    /// システム音から自プロセスの再生音を除く（system 専用・フィードバック
    /// ループ防止・Linux / Windows / macOS 対応）。`--source system` でのみ効き、
    /// mic / process では無視。対象 PID の除外用途は `--mode exclude` を使う。
    #[arg(long, default_value_t = false)]
    exclude_self: bool,

    /// キャプチャ秒数。`0` で無限ストリーミング（`--out -` 想定、Ctrl-C / パイプ切れで停止）。
    #[arg(long, default_value_t = 5)]
    seconds: u64,

    /// 出力先。ファイルパスなら WAV 書き出し、`-` なら stdout へ raw PCM ストリーミング。
    #[arg(long, default_value = "capture.wav")]
    out: PathBuf,

    /// WAV 分割録音の 1 ファイルあたりの秒数。既定 0 = 分割なし（従来どおり 1 ファイル）。
    /// 1 以上を指定すると、書き込んだフレーム数が `split-seconds × 出力サンプルレート` に
    /// 達するたびに現在のファイルを確定（WAV ヘッダ確定）して次の連番ファイルへ切り替える
    /// （`--out rec.wav` なら `rec-001.wav, rec-002.wav, ...`）。境界はチャンク粒度（20ms）の
    /// 「以上になったら次へ」なので、各ファイルは指定秒より最大 1 チャンク長くなりうる。
    /// チャンクは分割せず取りこぼしも無い（次ファイルは次のチャンクから始まる）。
    /// stdout ストリーミング（`--out -`）とは併用できない。
    #[arg(long, default_value_t = 0)]
    split_seconds: u64,

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

    /// 入力ゲイン（線形倍率）。既定 1.0。1.0 でそのまま、2.0 で約 +6dB、0.0 で無音。
    /// 乗算後のサンプルは ±1.0 にクランプされる。負・NaN はエラー。
    #[arg(long, default_value_t = 1.0)]
    gain: f32,

    /// `--source mix` の mic 側で選ぶ入力デバイスの ID（mix 専用・`--list-devices` の
    /// ID 列からコピーする）。省略すると既定入力。mic / system / process では無視される。
    #[arg(long)]
    mic_device_id: Option<String>,

    /// `--source mix` の system 側で選ぶ出力エンドポイントの ID（mix 専用）。
    /// 省略すると既定出力。mic / system / process では無視される。
    #[arg(long)]
    system_device_id: Option<String>,

    /// `--source mix` の mic 側の合成前倍率（線形・mix 専用）。既定 1.0。合成後に
    /// `--gain` が掛かる。負・NaN はエラー。
    #[arg(long, default_value_t = 1.0)]
    mic_gain: f32,

    /// `--source mix` の system 側の合成前倍率（線形・mix 専用）。既定 1.0。
    /// 負・NaN はエラー。
    #[arg(long, default_value_t = 1.0)]
    system_gain: f32,
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
        // mode は process セグメントでのみ効く（mic/system では facade が無視）。
        mode: cli.mode.into(),
        // exclude_self は system セグメントでのみ効く（mic/process では無視）。
        exclude_self: cli.exclude_self,
        // device_id は mic（入力）と system（出力エンドポイント）で効く（process では
        // facade が無視）。全セグメントに一様に載せておけば該当セグメントが拾う。
        device_id: cli.device_id.clone(),
        // gain は切替では変わらない（core が無視する）が、初期 config と揃えておく。
        gain: cli.gain,
        // mix 専用（mix 以外のセグメントでは facade が無視する）。
        mix_mic_device_id: cli.mic_device_id.clone(),
        mix_system_device_id: cli.system_device_id.clone(),
        mix_mic_gain: cli.mic_gain,
        mix_system_gain: cli.system_gain,
        ..Default::default()
    }
}

/// `--sources` のホットスワップスケジューラ。
///
/// 先頭セグメントは初期ソース（既に open/start 済み）。以降のセグメント境界（秒の累積）
/// に達したら `stream.switch_source()` で次ソースへ差し替える。出力先（ファイル/パイプ）
/// は呼び出し側が 1 本に保つので、切替は単一の連続チャンクストリームへ透過的に反映される
/// （seq/PTS の連続は Stream 層が保証）。
///
/// 収集ループから毎周回 [`tick`](Self::tick) を呼ぶと、境界時刻を過ぎた切替をまとめて
/// 実行する。切替失敗は `eprintln!` で警告して録音は継続する（旧ソースのまま）。境界で
/// `[switch] -> <kind>` を stderr に出す。
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
    // main からのエラーは常に stderr へ出す（stdout ストリーミング時に stdout を汚さない）。
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
    // デバイス一覧モード（録音せず列挙して終了）。`--source` 等とは独立に先に処理する。
    if cli.list_devices {
        return list_devices();
    }

    // デバイス着脱監視モード（録音せず監視し続ける）。これも `--source` 等とは独立。
    if cli.watch_devices {
        return watch_devices_loop();
    }

    let stdout_stream = cli.is_stdout_stream();

    // --split-seconds は WAV ファイル出力専用。stdout ストリーミング（--out -）には
    // 「ファイル」の境界が無く分割できないので、ストリームを開く前にここで弾く。
    if stdout_stream && cli.split_seconds > 0 {
        return Err(
            "--split-seconds（分割録音）は WAV ファイル出力専用です。--out -（stdout への \
             raw PCM ストリーミング）とは併用できません。--out にファイルパスを指定してください。"
                .into(),
        );
    }

    // ログ出力先の切り替え。stdout ストリーミング時は全ログを stderr へ（stdout は PCM
    // 専用）、ファイル出力時は stdout サマリでよい。以降の println!/eprintln! はこれを通す。
    macro_rules! log {
        ($($arg:tt)*) => {
            if stdout_stream {
                eprintln!($($arg)*);
            } else {
                println!($($arg)*);
            }
        };
    }

    // --sources（ホットスワップスケジュール）の解決。指定時は --source / --seconds を
    // 上書きし、先頭セグメントを初期ソースにする。process を含むなら --process-id 必須
    // （switch_source 経由の build_backend が PID 欠落で失敗するので、ここで先に弾く）。
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

    // ソース種別から SourceKind と表示ラベルを解決する。バックエンドの構築・選択は
    // facade `flexaudio::open` が行う（Box<dyn CaptureBackend> を内部で選んで Stream を
    // 返す）。CLI 側は SourceKind・表示ラベルの決定と、人間向けの事前チェック（process の
    // PID 必須・非 Linux の system/process 拒否）だけを担う。--sources 指定時は先頭
    // セグメントの kind を初期ソースに使う。
    let (kind, source_label): (SourceKind, &str) = if let Some(segs) = &segments {
        let first = segs[0].kind;
        let label = match first {
            SourceKind::Mic => "mic（既定入力デバイス）",
            SourceKind::SystemLoopback => "system（既定出力のループバック）",
            SourceKind::ProcessLoopback => "process（指定 PID の出力）",
            SourceKind::Mix => "mix（マイク + システム音声の合成）",
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
                        "system（既定出力のループバック）",
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
                // process では PID 必須。無ければ分かりやすいエラーで止める（facade も
                // InvalidArg を返すが、ここで人間向け文言で先に弾く）。このチェックは OS
                // 非依存。
                if cli.process_id.is_none() {
                    return Err("--source process には --process-id <PID> が必要です。\
                     （対象プロセスの PID を指定してください。例: \
                     speaker-test を鳴らして得た PID）"
                        .into());
                }
                #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
                {
                    (SourceKind::ProcessLoopback, "process（指定 PID の出力）")
                }
                #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
                {
                    return Err(
                    "--source process（プロセス出力ループバック）は現在 Linux / Windows / macOS のみ対応です。"
                        .into(),
                );
                }
            }
            SourceArg::Mix => {
                // system 側のループバックが必要なので、対応 OS は system と同じ。
                #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
                {
                    (SourceKind::Mix, "mix（マイク + システム音声の合成）")
                }
                #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
                {
                    return Err(
                    "--source mix（マイク + システム音声のミックス）は現在 Linux / Windows / macOS のみ対応です。"
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

    // ストリームを開く。open は config.kind に応じて Box<dyn CaptureBackend> を内部で
    // 選んで返す。まだ start しない（二段方式）。native_format は開いた Stream から取る。
    let config = StreamConfig {
        kind,
        output,
        target_pid: cli.process_id,
        // mode は process 専用。include 既定。
        mode: cli.mode.into(),
        // exclude_self は system 専用の自ホスト除外。
        exclude_self: cli.exclude_self,
        // device_id は mic（入力）と system（出力エンドポイント）の選択用（process では
        // facade が無視する）。
        device_id: cli.device_id.clone(),
        // 開始時の入力ゲイン（線形倍率）。不正値は open が InvalidArg で弾く。
        gain: cli.gain,
        // mix 専用のデバイス選択と側別ゲイン（mix 以外では facade が無視する。
        // 不正な側別ゲインは open が InvalidArg で弾く）。
        mix_mic_device_id: cli.mic_device_id.clone(),
        mix_system_device_id: cli.system_device_id.clone(),
        mix_mic_gain: cli.mic_gain,
        mix_system_gain: cli.system_gain,
        ..Default::default()
    };
    let mut stream = flexaudio::open(config).map_err(describe_error)?;

    // --- ネイティブフォーマット表示 ---
    let (native_rate, native_ch) = stream.native_format();
    log!("ソース            : {source_label}");
    // device_id 指定時は選択デバイスを明示（mic と system で有効。process / mix では無視）。
    if let Some(id) = &cli.device_id {
        match kind {
            SourceKind::ProcessLoopback => {
                log!("デバイス ID        : {id}（注: process では無視されます）");
            }
            SourceKind::Mix => {
                log!(
                    "デバイス ID        : {id}（注: mix では無視されます。\
                     --mic-device-id / --system-device-id を使ってください）"
                );
            }
            _ => log!("デバイス ID        : {id}"),
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
    } else if cli.split_seconds > 0 {
        // 分割録音時はベースパスそのままのファイルは作らないので、実際の連番名を見せる。
        log!(
            "出力パス           : {}, {}, ...（{} 秒毎に分割）",
            split_file_path(&cli.out, 1).display(),
            split_file_path(&cli.out, 2).display(),
            cli.split_seconds
        );
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

/// `--list-devices`: `devices()` でデバイスを取得して表形式で表示する。
///
/// 列: SOURCE（mic/system/process）/ LOOPBACK / DEFAULT / RATE / CH / NAME / ID。
/// id はデバイス名（cpal）または node.name（PipeWire）。デバイスが無い環境ではその旨を
/// 表示する（エラーにはしない）。
fn list_devices() -> std::result::Result<(), String> {
    let devices = flexaudio::devices().map_err(|e| format!("デバイス列挙に失敗しました: {e}"))?;

    if devices.is_empty() {
        println!("利用可能なオーディオデバイスが見つかりませんでした。");
        println!(
            "（オーディオデバイスのある環境で実行してください。\
             Linux で system を列挙するには PipeWire セッションが必要です。）"
        );
        return Ok(());
    }

    println!("利用可能なオーディオデバイス: {} 件", devices.len());
    println!();
    // ヘッダ。固定幅で揃える（可変長の id/name は末尾）。
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
        "（DEFAULT の * は OS 既定デバイス。ID は `--device-id <ID>` でデバイスを選ぶのに\
         使える安定キー。mic は入力デバイス、system は出力エンドポイント。process では無視。）"
    );
    Ok(())
}

/// `--watch-devices`: `watch_devices()` でデバイスの着脱（ホットプラグ）を監視し、
/// イベントを stderr へ表示し続ける（Ctrl-C で停止）。
///
/// stdout は将来の機械可読出力用に空けておくので、ログ・イベントは全て stderr へ出す。
/// 起動時に `devices()` で既存デバイスを数えて件数を表示する。
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
                // DeviceEvent は #[non_exhaustive]。将来のバリアント追加に備えて、未知
                // 種別もデバッグ表現で表示する（握り潰さない）。
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
        SourceKind::Mix => "mix",
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
    // WAV 経路で --seconds 0 は無限に貯め続けてしまうので拒否する（--sources の各 secs は
    // parse_sources で 1 以上を強制済みなので 0 にはならない）。
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

    // WAV ライター。チャンクは貯め込まず到着次第書く（分割境界で即 finalize するため。
    // 長時間録音でメモリも溜めない）。--split-seconds 0 なら従来どおり --out へ 1 ファイル、
    // 1 以上なら連番ファイルへローテーションする。ピーク / RMS は録音全体で通算する。
    let mut writer = RotatingWavWriter::new(&cli.out, output, cli.split_seconds);
    let mut chunk_count: u64 = 0;

    // 総録音時間ぶん poll_chunk をループして全チャンクを書き切る。
    let deadline = start + total;
    while Instant::now() < deadline {
        // セグメント境界に達していればソースを切り替える（書く先のローテーションとは独立）。
        if let Some(sch) = scheduler.as_mut() {
            sch.tick(stream, Instant::now());
        }

        let mut got_any = false;
        while let Some(chunk) = stream.poll_chunk() {
            got_any = true;
            chunk_count += 1;
            if let Err(e) = write_wav_chunk(&mut writer, &chunk) {
                stream.stop();
                return Err(e);
            }
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

    // stop 後にもリングへ残ったチャンクを書き切る（取りこぼしゼロ）。
    while let Some(chunk) = stream.poll_chunk() {
        chunk_count += 1;
        write_wav_chunk(&mut writer, &chunk)?;
    }

    // チャンクが 1 つも来なくても録音自体は走ったので失敗にはしない。空の WAV を書き出して
    // 警告だけ出す（ソースが無音・除外・選んだエンドポイントが非アクティブ等で起こる）。
    if chunk_count == 0 {
        eprintln!(
            "警告: 音声を取得できませんでした（ソースが無音・除外・選んだエンドポイントが\
             非アクティブ等の可能性）。空の WAV を書き出します。"
        );
    }

    // 書き終わり。開いているファイルの WAV ヘッダを確定し、録音全体のピーク / RMS を得る。
    // WAV は現状 s16 固定（f32 WAV が要るなら encoding を WAV 出力にも適用できる）。
    let summary = writer
        .finish()
        .map_err(|e| format!("WAV 書き出し失敗: {e}"))?;
    let total_frames = summary.total_frames;
    let stats = summary.stats;

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
    println!("取得チャンク数     : {chunk_count}");
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
    // 分割録音で複数ファイルになったときは先頭〜末尾の連番で示す（finish は最低 1 ファイル
    // を保証するので files は非空）。1 ファイルなら従来どおりパスをそのまま出す。
    if summary.files.len() == 1 {
        println!("WAV 書き出し       : {}", summary.files[0].display());
    } else {
        println!(
            "WAV 書き出し       : {} 〜 {}（{} ファイル・{} 秒毎に分割）",
            summary.files[0].display(),
            summary.files[summary.files.len() - 1].display(),
            summary.files.len(),
            cli.split_seconds
        );
    }

    // チャンクは来たが中身がほぼ無音（peak/RMS がほぼ 0）なら、空 WAV のときと同じ趣旨で
    // 警告する。OS 差で無音フレームが流れる場合（無音 WAV になる）にも「無音だった」と
    // 分かるようにする。無音判定は録音全体（全ファイル通算）の統計で行う。
    if chunk_count > 0 && stats.peak < SILENCE_PEAK && stats.rms < SILENCE_RMS {
        eprintln!(
            "警告: 音声を取得できませんでした（ソースが無音・除外・選んだエンドポイントが\
             非アクティブ等の可能性）。録音はほぼ無音です。"
        );
    }

    Ok(())
}

/// 「ほぼ無音」と判定するピーク / RMS（線形）のしきい値。これ未満なら無音扱いで警告する。
/// -60 dBFS 付近を目安にした緩いしきい値（厳密な値ではない）。
const SILENCE_PEAK: f32 = 1.0e-3;
const SILENCE_RMS: f64 = 1.0e-4;

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

    // Ctrl-C(SIGINT) フラグ。無限時に押されたら停止する。ctrlc は重複登録で Err を返すので
    // 無限時のみ登録する。
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

        // セグメント境界に達していればソースを切り替える（出力先は同じ 1 パイプ）。
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
/// サンプル単位の小書き込みを避けるため、チャンク分のバイト列をまとめてから 1 回の
/// `write_all` で出す。書き込み後すぐ flush する（低レイテンシ・溜め込み無し）。
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
            // s16 LE: 量子化は全層共通の正典 [`flexaudio::core::quantize_i16`]
            // （scale 32768・round・clamp・NaN→0）。1 サンプル 2 byte。
            let mut buf = Vec::with_capacity(chunk.data.len() * 2);
            for &x in &chunk.data {
                let s = flexaudio::core::quantize_i16(x);
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

/// 分割録音の `index` 番目（1 始まり）のファイルパスを作る（純関数）。
///
/// `rec.wav` なら `rec-001.wav, rec-002.wav, ...` のように拡張子の前へ 3 桁ゼロ詰め
/// 連番を挟む。1000 ファイル目以降は桁が自然に増える（`rec-1000.wav`）。拡張子が無い
/// パス（`rec`）は末尾に連番を足す（`rec-001`）。親ディレクトリは保たれる。
fn split_file_path(base: &Path, index: u64) -> PathBuf {
    let stem = base
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = match base.extension() {
        Some(ext) => format!("{stem}-{index:03}.{}", ext.to_string_lossy()),
        None => format!("{stem}-{index:03}"),
    };
    base.with_file_name(name)
}

/// 録音終了時のまとめ（書き出したファイル一覧・総フレーム数・録音全体の統計）。
struct WavSummary {
    /// 書き出したファイルのパス（書いた順・最低 1 つ）。
    files: Vec<PathBuf>,
    /// 全ファイル合計の総フレーム数。
    total_frames: u64,
    /// 録音全体（全ファイル通算）のピーク / RMS。
    stats: Stats,
}

/// WAV 書き出しの分割（ローテーション）を担う小さなライター。
///
/// `--split-seconds 0`（分割なし）では `--out` のパスへ従来どおり 1 ファイルを書く。
/// 1 以上では [`split_file_path`] の連番パスへ書き、書き込んだフレーム数が
/// `split_seconds × 出力サンプルレート` に達するたびに現在のファイルを finalize
/// （WAV ヘッダ確定）して閉じ、次のチャンクから次ファイルへ書く。境界はチャンク粒度
/// （20ms）の「以上になったら次へ」なので、各ファイルは指定秒より最大 1 チャンク
/// 長くなりうる（チャンクは分割せず、取りこぼしも無い）。
///
/// ファイルは次のチャンクが来るまで開かない（遅延生成）ので、録音がちょうど境界で
/// 終わっても空の末尾ファイルは残らない。ピーク / RMS は録音全体（全ファイル通算）で
/// 集計する（従来の単一ファイル時と同じ意味の統計・無音警告を保つため）。
/// 量子化は全層共通の正典 [`flexaudio::core::quantize_i16`]（scale 32768・round・clamp）の
/// 16-bit PCM 固定。
struct RotatingWavWriter {
    /// `--out` のベースパス（分割時は連番の元、分割なしはこのまま使う）。
    base: PathBuf,
    /// WAV ヘッダ仕様（出力フォーマット追従・16-bit PCM 固定）。
    spec: hound::WavSpec,
    /// 1 ファイルあたりのフレーム数しきい値（split_seconds × rate）。0 = 分割なし。
    frames_per_file: u64,
    /// 現在書き込み中のライター（遅延生成。ローテーション直後や書き込み前は None）。
    writer: Option<hound::WavWriter<BufWriter<File>>>,
    /// 現在のファイルへ書き込んだフレーム数（ローテーションで 0 に戻る）。
    frames_in_current: u64,
    /// これまでに書き始めたファイルのパス（書いた順）。
    files: Vec<PathBuf>,
    /// 録音全体のピーク（線形絶対値の最大）。
    peak: f32,
    /// 録音全体の二乗和（RMS 計算用）。
    sum_sq: f64,
    /// 録音全体のサンプル数（RMS 計算用）。
    samples: u64,
    /// 全ファイル合計の総フレーム数。
    total_frames: u64,
}

impl RotatingWavWriter {
    /// ベースパス・出力フォーマット・分割秒数（0 = 分割なし）からライターを作る。
    /// この時点ではファイルを開かない（最初のチャンクで開く）。
    fn new(out: &Path, output: OutputFormat, split_seconds: u64) -> Self {
        let spec = hound::WavSpec {
            channels: output.channels,
            sample_rate: output.sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        Self {
            base: out.to_path_buf(),
            spec,
            frames_per_file: split_seconds * output.sample_rate as u64,
            writer: None,
            frames_in_current: 0,
            files: Vec::new(),
            peak: 0.0,
            sum_sq: 0.0,
            samples: 0,
            total_frames: 0,
        }
    }

    /// 分割録音か（`--split-seconds` が 1 以上か）。
    fn is_split(&self) -> bool {
        self.frames_per_file > 0
    }

    /// 次に開くファイルのパス。分割なしはベースパスそのまま、分割ありは 1 始まり連番。
    fn next_path(&self) -> PathBuf {
        if self.is_split() {
            split_file_path(&self.base, self.files.len() as u64 + 1)
        } else {
            self.base.clone()
        }
    }

    /// 1 チャンクを書く。チャンクは丸ごと現在のファイルへ入れ（分割しない）、書き込み後に
    /// フレーム数がしきい値以上ならその場で finalize して次ファイルへローテーションする
    /// （次のチャンクが新しいファイルの先頭になる）。finalize したファイルのパスを返す
    /// （ローテーションの進捗表示用。ローテーションしなければ None）。
    fn write_chunk(&mut self, chunk: &AudioChunk) -> hound::Result<Option<PathBuf>> {
        // ファイルはチャンクが来た時点で開く（遅延生成）。
        if self.writer.is_none() {
            let path = self.next_path();
            self.writer = Some(hound::WavWriter::create(&path, self.spec)?);
            self.files.push(path);
        }
        let writer = self.writer.as_mut().expect("直前で開いている");
        for &x in &chunk.data {
            // 統計は録音全体（全ファイル通算）で集計する。
            let a = x.abs();
            if a > self.peak {
                self.peak = a;
            }
            self.sum_sq += (x as f64) * (x as f64);
            self.samples += 1;

            let s = flexaudio::core::quantize_i16(x);
            writer.write_sample(s)?;
        }
        self.frames_in_current += chunk.frames as u64;
        self.total_frames += chunk.frames as u64;

        // 分割境界: しきい値「以上」に達したら即 finalize（±1 チャンクの誤差は仕様どおり）。
        if self.is_split() && self.frames_in_current >= self.frames_per_file {
            let writer = self.writer.take().expect("上で書いたばかり");
            writer.finalize()?;
            self.frames_in_current = 0;
            return Ok(self.files.last().cloned());
        }
        Ok(None)
    }

    /// 録音終了。開いているファイルの WAV ヘッダを確定する。チャンクが 1 つも来なかった
    /// 場合は従来どおり空 WAV を 1 つ書く（分割ありなら連番 1 番）ので、返る `files` は
    /// 最低 1 つ。全体統計と総フレーム数も返す。
    fn finish(mut self) -> hound::Result<WavSummary> {
        if let Some(writer) = self.writer.take() {
            writer.finalize()?;
        } else if self.files.is_empty() {
            // 1 チャンクも来なかった。録音自体は走った証跡として空 WAV を残す（従来挙動）。
            let path = self.next_path();
            hound::WavWriter::create(&path, self.spec)?.finalize()?;
            self.files.push(path);
        }
        let rms = if self.samples > 0 {
            (self.sum_sq / self.samples as f64).sqrt()
        } else {
            0.0
        };
        Ok(WavSummary {
            files: self.files,
            total_frames: self.total_frames,
            stats: Stats {
                peak: self.peak,
                rms,
            },
        })
    }
}

/// `run_wav` の 1 チャンク書き込み。ローテーションが起きたら `[switch]` と同じ調子で
/// stderr に進捗を出し、書き込みエラーは人間向けメッセージへ変換する。
fn write_wav_chunk(
    writer: &mut RotatingWavWriter,
    chunk: &AudioChunk,
) -> std::result::Result<(), String> {
    match writer.write_chunk(chunk) {
        Ok(Some(done)) => {
            eprintln!(
                "[split] {} を確定（次のチャンクから次ファイル）",
                done.display()
            );
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(e) => Err(format!("WAV 書き出し失敗: {e}")),
    }
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
            "指定したデバイス／エンドポイントが見つかりません。`--list-devices` の ID を\
             確認してください。"
                .into()
        }
        Error::PermissionDenied => {
            "マイクへのアクセス権限がありません。OS のマイク権限設定を確認してください。".into()
        }
        Error::DeviceLost => "キャプチャ中に入力デバイスが失われました（切断など）。".into(),
        other => format!("ストリーム初期化に失敗しました: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CLI 引数列から `Cli` を組む（clap 経由）。最低限 `flexaudio-cli` を先頭に置く。
    fn cli_from(args: &[&str]) -> Cli {
        let mut full = vec!["flexaudio-cli"];
        full.extend_from_slice(args);
        Cli::parse_from(full)
    }

    // --- parse_sources ---

    /// `mic:2,system:2,process:2` を 3 セグメント（kind + secs）へ正しくパースする。
    #[test]
    fn parse_sources_three_segments() {
        let segs = parse_sources("mic:2,system:2,process:2").expect("valid spec");
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[0].kind, SourceKind::Mic);
        assert_eq!(segs[0].secs, 2);
        assert_eq!(segs[1].kind, SourceKind::SystemLoopback);
        assert_eq!(segs[2].kind, SourceKind::ProcessLoopback);
    }

    /// 空白や異なる秒数も許容し、trim される。
    #[test]
    fn parse_sources_trims_and_varies_secs() {
        let segs = parse_sources(" mic:1 , system:5 ").expect("valid spec");
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].kind, SourceKind::Mic);
        assert_eq!(segs[0].secs, 1);
        assert_eq!(segs[1].kind, SourceKind::SystemLoopback);
        assert_eq!(segs[1].secs, 5);
    }

    /// 単一セグメントも有効。
    #[test]
    fn parse_sources_single_segment() {
        let segs = parse_sources("mic:3").expect("valid");
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].kind, SourceKind::Mic);
        assert_eq!(segs[0].secs, 3);
    }

    /// 空文字列はエラー（空 spec）。
    #[test]
    fn parse_sources_rejects_empty_string() {
        assert!(parse_sources("").is_err());
    }

    /// 空セグメント（連続カンマ）はエラー。
    #[test]
    fn parse_sources_rejects_empty_segment() {
        assert!(parse_sources("mic:2,,system:2").is_err());
    }

    /// `<src>:<secs>` 形式でない（コロン無し）はエラー。
    #[test]
    fn parse_sources_rejects_missing_colon() {
        assert!(parse_sources("mic2").is_err());
    }

    /// 未知のソース名はエラー。
    #[test]
    fn parse_sources_rejects_unknown_source() {
        assert!(parse_sources("foo:2").is_err());
    }

    /// 秒数が非数値はエラー。
    #[test]
    fn parse_sources_rejects_non_numeric_secs() {
        assert!(parse_sources("mic:abc").is_err());
    }

    /// 秒数 0 はエラー（1 以上必須）。
    #[test]
    fn parse_sources_rejects_zero_secs() {
        assert!(parse_sources("mic:0").is_err());
    }

    // --- config_for_kind ---

    /// `config_for_kind` が CLI 共有設定（output / pid / mode / exclude_self / device_id）を
    /// StreamConfig へ正しく反映し、kind を引数で上書きする。
    #[test]
    fn config_for_kind_reflects_cli_settings() {
        let cli = cli_from(&[
            "--source",
            "process",
            "--process-id",
            "4321",
            "--mode",
            "exclude",
            "--output-rate",
            "16000",
            "--output-channels",
            "1",
            "--device-id",
            "my-mic",
            "--gain",
            "2.5",
        ]);
        let cfg = config_for_kind(&cli, SourceKind::ProcessLoopback);
        assert_eq!(cfg.kind, SourceKind::ProcessLoopback);
        assert_eq!(cfg.target_pid, Some(4321));
        assert_eq!(cfg.mode, ProcessMode::Exclude);
        assert_eq!(cfg.output.sample_rate, 16_000);
        assert_eq!(cfg.output.channels, 1);
        assert_eq!(cfg.device_id.as_deref(), Some("my-mic"));
        assert_eq!(cfg.gain, 2.5);
        // kind は引数で上書きされる（CLI の --source とは独立に指定できる）。
        let cfg_mic = config_for_kind(&cli, SourceKind::Mic);
        assert_eq!(cfg_mic.kind, SourceKind::Mic);
        // 他の共有設定は据え置き。
        assert_eq!(cfg_mic.output.sample_rate, 16_000);
    }

    /// `--exclude-self` が StreamConfig.exclude_self に反映される。
    #[test]
    fn config_for_kind_reflects_exclude_self() {
        let cli = cli_from(&["--source", "system", "--exclude-self"]);
        let cfg = config_for_kind(&cli, SourceKind::SystemLoopback);
        assert!(cfg.exclude_self);
        // 既定（未指定）は false。
        let cli2 = cli_from(&["--source", "system"]);
        assert!(!config_for_kind(&cli2, SourceKind::SystemLoopback).exclude_self);
    }

    /// 既定 CLI（引数最小）は output {48000,2} / mode Include / pid None になる。
    #[test]
    fn config_for_kind_defaults() {
        let cli = cli_from(&[]);
        let cfg = config_for_kind(&cli, SourceKind::Mic);
        assert_eq!(cfg.output.sample_rate, 48_000);
        assert_eq!(cfg.output.channels, 2);
        assert_eq!(cfg.mode, ProcessMode::Include);
        assert_eq!(cfg.target_pid, None);
        assert!(!cfg.exclude_self);
        assert_eq!(cfg.device_id, None);
        assert_eq!(cfg.gain, 1.0);
    }

    /// `Cli::output_format` / `is_stdout_stream` の基本動作。
    #[test]
    fn cli_output_format_and_stdout_detection() {
        let cli = cli_from(&["--output-rate", "8000", "--output-channels", "1"]);
        let of = cli.output_format();
        assert_eq!(of.sample_rate, 8_000);
        assert_eq!(of.channels, 1);
        assert!(!cli.is_stdout_stream());

        let cli_stream = cli_from(&["--out", "-"]);
        assert!(cli_stream.is_stdout_stream());
    }

    // --- describe_error ---

    /// 主要な Error バリアントが人間向け文言へ変換される（種別ごとに分岐）。
    #[test]
    fn describe_error_maps_known_variants() {
        assert!(describe_error(Error::DeviceNotFound).contains("見つかりません"));
        assert!(describe_error(Error::PermissionDenied).contains("権限"));
        assert!(describe_error(Error::DeviceLost).contains("失われました"));
        // その他は汎用文言 + Display を含む。
        let msg = describe_error(Error::Unsupported);
        assert!(msg.contains("ストリーム初期化に失敗しました"));
    }

    /// DeviceNotFound の文言は source 非依存（mic 前提の語を含まない）。system や process で
    /// 不正な device-id を指定したときにも適切な案内になる。
    #[test]
    fn describe_error_device_not_found_is_source_neutral() {
        let msg = describe_error(Error::DeviceNotFound);
        assert!(!msg.contains("マイク"));
        assert!(!msg.contains("入力デバイス"));
        // ID の確認を促す案内になっている。
        assert!(msg.contains("--list-devices"));
    }

    // --- source_kind_label / truncate ---

    /// ラベルは短い英語識別子。
    #[test]
    fn source_kind_label_is_short() {
        assert_eq!(source_kind_label(SourceKind::Mic), "mic");
        assert_eq!(source_kind_label(SourceKind::SystemLoopback), "system");
        assert_eq!(source_kind_label(SourceKind::ProcessLoopback), "process");
        assert_eq!(source_kind_label(SourceKind::Mix), "mix");
    }

    /// mix 用フラグ（--mic-device-id / --system-device-id / --mic-gain / --system-gain）
    /// が StreamConfig の mix_* フィールドへ反映される。既定は None / 1.0。
    #[test]
    fn config_for_kind_reflects_mix_settings() {
        let cli = cli_from(&[
            "--source",
            "mix",
            "--mic-device-id",
            "mic-a",
            "--system-device-id",
            "sink-b",
            "--mic-gain",
            "0.5",
            "--system-gain",
            "2.0",
        ]);
        let cfg = config_for_kind(&cli, SourceKind::Mix);
        assert_eq!(cfg.kind, SourceKind::Mix);
        assert_eq!(cfg.mix_mic_device_id.as_deref(), Some("mic-a"));
        assert_eq!(cfg.mix_system_device_id.as_deref(), Some("sink-b"));
        assert_eq!(cfg.mix_mic_gain, 0.5);
        assert_eq!(cfg.mix_system_gain, 2.0);

        // 未指定なら既定（デバイス None・側別ゲイン 1.0）。
        let cli2 = cli_from(&["--source", "mix"]);
        let cfg2 = config_for_kind(&cli2, SourceKind::Mix);
        assert_eq!(cfg2.mix_mic_device_id, None);
        assert_eq!(cfg2.mix_system_device_id, None);
        assert_eq!(cfg2.mix_mic_gain, 1.0);
        assert_eq!(cfg2.mix_system_gain, 1.0);
    }

    /// `truncate` は max 文字以下ならそのまま、超過なら … 付きで max 文字に収める。
    #[test]
    fn truncate_respects_char_boundary() {
        assert_eq!(truncate("abc", 5), "abc");
        // ちょうど max はそのまま。
        assert_eq!(truncate("abcde", 5), "abcde");
        // 超過は … 付きで max 文字（keep = max-1）。
        let t = truncate("abcdefgh", 5);
        assert_eq!(t.chars().count(), 5);
        assert!(t.ends_with('…'));
        assert!(t.starts_with("abcd"));
        // マルチバイト（日本語）でも char 単位で安全に切る（panic しない）。
        let jp = truncate("あいうえおかきくけこ", 3);
        assert_eq!(jp.chars().count(), 3);
        assert!(jp.ends_with('…'));
    }

    // --- split_file_path ---

    /// 拡張子の前に 3 桁ゼロ詰め連番を挟み、1000 以降は桁が自然に増える。
    #[test]
    fn split_file_path_inserts_padded_index() {
        assert_eq!(
            split_file_path(Path::new("rec.wav"), 1),
            PathBuf::from("rec-001.wav")
        );
        assert_eq!(
            split_file_path(Path::new("rec.wav"), 42),
            PathBuf::from("rec-042.wav")
        );
        assert_eq!(
            split_file_path(Path::new("rec.wav"), 999),
            PathBuf::from("rec-999.wav")
        );
        // 1000 ファイル目以降はゼロ詰め幅を超えて桁が自然に増える。
        assert_eq!(
            split_file_path(Path::new("rec.wav"), 1000),
            PathBuf::from("rec-1000.wav")
        );
    }

    /// 親ディレクトリは保たれ、拡張子なしのパスは末尾に連番を足す。
    #[test]
    fn split_file_path_keeps_parent_and_handles_no_extension() {
        assert_eq!(
            split_file_path(Path::new("/tmp/dir/rec.wav"), 3),
            PathBuf::from("/tmp/dir/rec-003.wav")
        );
        assert_eq!(
            split_file_path(Path::new("rec"), 1),
            PathBuf::from("rec-001")
        );
        // ドットを複数含む名前は最後の拡張子の前に挟む。
        assert_eq!(
            split_file_path(Path::new("a.b.wav"), 2),
            PathBuf::from("a.b-002.wav")
        );
    }

    // --- RotatingWavWriter ---

    /// テスト用チャンク（全サンプル同値・interleaved）を作る。
    fn chunk_of(frames: usize, channels: usize, value: f32) -> AudioChunk {
        AudioChunk {
            data: vec![value; frames * channels],
            frames,
            pts_ns: 0,
            seq: 0,
            flags: flexaudio::core::ChunkFlags::empty(),
            dropped_before: 0,
            peak: value.abs(),
            rms: value.abs(),
        }
    }

    /// テスト専用の空一時ディレクトリを作って返す（テスト名で分離・並列実行に安全）。
    fn test_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("flexaudio_cli_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    /// WAV を読み戻してフレーム数（= サンプル数 / チャンネル数）を返す。
    fn wav_frames(path: &Path) -> u64 {
        let reader = hound::WavReader::open(path).expect("open wav");
        (reader.len() / reader.spec().channels as u32) as u64
    }

    /// 分割なし（split 0）は従来どおりベースパスへ 1 ファイルを書き、ヘッダ
    /// （rate/ch/16bit）と peak/rms を正しく出す。書いた WAV を hound で読み戻して検証。
    #[test]
    fn rotating_writer_without_split_matches_legacy_single_file() {
        let output = OutputFormat {
            sample_rate: 16_000,
            channels: 1,
        };
        let dir = test_dir("nosplit");
        let path = dir.join("capture.wav");

        // 振幅 0.5 / -0.5 の交互（peak=0.5, rms=0.5）。
        let data: Vec<f32> = (0..320)
            .map(|i| if i % 2 == 0 { 0.5 } else { -0.5 })
            .collect();
        let chunk = AudioChunk {
            data,
            frames: 320,
            pts_ns: 0,
            seq: 0,
            flags: flexaudio::core::ChunkFlags::empty(),
            dropped_before: 0,
            peak: 0.5,
            rms: 0.5,
        };

        let mut writer = RotatingWavWriter::new(&path, output, 0);
        assert!(writer.write_chunk(&chunk).expect("write").is_none());
        let summary = writer.finish().expect("finish");

        // 分割なしは連番を付けずベースパスそのまま 1 ファイル（完全互換）。
        assert_eq!(summary.files, vec![path.clone()]);
        assert_eq!(summary.total_frames, 320);

        // peak/rms は既知（全サンプル |0.5| なので peak=0.5, rms=0.5）。
        assert!(
            (summary.stats.peak - 0.5).abs() < 1e-6,
            "peak: {}",
            summary.stats.peak
        );
        assert!(
            (summary.stats.rms - 0.5).abs() < 1e-6,
            "rms: {}",
            summary.stats.rms
        );

        // ヘッダを読み戻して rate/ch/bits を検証。
        let reader = hound::WavReader::open(&path).expect("open wav");
        let spec = reader.spec();
        assert_eq!(spec.sample_rate, 16_000);
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.bits_per_sample, 16);
        assert_eq!(spec.sample_format, hound::SampleFormat::Int);
        assert_eq!(reader.len(), 320);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 分割境界の計算（フレーム数ベースの決定論）: 16kHz mono / split 1 秒（しきい値
    /// 16000 フレーム）へ 20ms チャンク（320 フレーム）を 150 個書くと、ちょうど
    /// 50 個ずつの 3 ファイルになり、合計フレーム数が一致する（欠落ゼロ）。
    #[test]
    fn rotating_writer_splits_exactly_on_multiple() {
        let output = OutputFormat {
            sample_rate: 16_000,
            channels: 1,
        };
        let dir = test_dir("split_exact");
        let base = dir.join("rec.wav");
        let mut writer = RotatingWavWriter::new(&base, output, 1);

        let mut rotations = Vec::new();
        for i in 0..150 {
            if let Some(done) = writer.write_chunk(&chunk_of(320, 1, 0.25)).expect("write") {
                rotations.push((i, done));
            }
        }
        // 16000 / 320 = 50 なので 50・100・150 個目（0 始まりで 49・99・149）で確定する。
        assert_eq!(
            rotations.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
            vec![49, 99, 149]
        );

        let summary = writer.finish().expect("finish");
        // ちょうど境界で書き終えたので空の 4 本目は作られない。
        assert_eq!(summary.files.len(), 3);
        assert_eq!(
            summary.files,
            vec![
                dir.join("rec-001.wav"),
                dir.join("rec-002.wav"),
                dir.join("rec-003.wav"),
            ]
        );
        assert_eq!(summary.total_frames, 150 * 320);

        // 読み戻し: 各ファイルちょうど 1 秒分・合計 = 入力（取りこぼしゼロ）。
        let mut read_total = 0u64;
        for f in &summary.files {
            let frames = wav_frames(f);
            assert_eq!(frames, 16_000);
            read_total += frames;
        }
        assert_eq!(read_total, summary.total_frames);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 境界はチャンク粒度の「以上になったら次へ」: しきい値がチャンクの倍数でないとき、
    /// ファイルはしきい値を跨いだチャンクまで含む（最大 1 チャンクの超過）。チャンクは
    /// 分割されず、次ファイルは次のチャンクから始まる。
    #[test]
    fn rotating_writer_rounds_boundary_up_to_chunk() {
        // しきい値 1000 フレーム（1000Hz × 1 秒）・チャンク 320 フレーム。
        // 4 チャンク目で 1280 >= 1000 となり確定（±1 チャンクの誤差は仕様）。
        let output = OutputFormat {
            sample_rate: 1_000,
            channels: 1,
        };
        let dir = test_dir("split_roundup");
        let base = dir.join("rec.wav");
        let mut writer = RotatingWavWriter::new(&base, output, 1);

        // 7 チャンク: 4 個で 1 本目確定、残り 3 個（960 < 1000）は finish で 2 本目に確定。
        let mut rotated_at = Vec::new();
        for i in 0..7 {
            if writer
                .write_chunk(&chunk_of(320, 1, 0.25))
                .expect("write")
                .is_some()
            {
                rotated_at.push(i);
            }
        }
        assert_eq!(rotated_at, vec![3]);

        let summary = writer.finish().expect("finish");
        assert_eq!(summary.files.len(), 2);
        // 1 本目 = 4 チャンク（1280 フレーム・しきい値 1000 の切り上げ）、2 本目 = 残り全部。
        assert_eq!(wav_frames(&summary.files[0]), 1280);
        assert_eq!(wav_frames(&summary.files[1]), 960);
        assert_eq!(summary.total_frames, 7 * 320);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ステレオでも境界は「フレーム数」で数える（サンプル数ではない）。
    #[test]
    fn rotating_writer_counts_frames_not_samples_for_stereo() {
        // しきい値 640 フレーム・320 フレーム（640 サンプル）のステレオチャンク。
        // サンプル数で数えると 1 チャンク目で誤ローテーションする。
        let output = OutputFormat {
            sample_rate: 640,
            channels: 2,
        };
        let dir = test_dir("split_stereo");
        let base = dir.join("rec.wav");
        let mut writer = RotatingWavWriter::new(&base, output, 1);

        assert!(writer
            .write_chunk(&chunk_of(320, 2, 0.25))
            .expect("write")
            .is_none());
        assert!(writer
            .write_chunk(&chunk_of(320, 2, 0.25))
            .expect("write")
            .is_some());

        let summary = writer.finish().expect("finish");
        assert_eq!(summary.files.len(), 1);
        assert_eq!(wav_frames(&summary.files[0]), 640);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// チャンクが 1 つも来なくても finish は空 WAV を 1 つ残す（従来挙動の維持）。
    /// 分割なしはベースパス、分割ありは連番 1 番になる。
    #[test]
    fn rotating_writer_finish_writes_empty_wav_when_no_chunks() {
        let output = OutputFormat {
            sample_rate: 16_000,
            channels: 2,
        };
        let dir = test_dir("split_empty");

        // 分割なし → capture.wav（従来どおり）。
        let base = dir.join("capture.wav");
        let summary = RotatingWavWriter::new(&base, output, 0)
            .finish()
            .expect("finish");
        assert_eq!(summary.files, vec![base.clone()]);
        assert_eq!(summary.total_frames, 0);
        assert_eq!(wav_frames(&base), 0);

        // 分割あり → rec-001.wav（空の 1 本目）。
        let base2 = dir.join("rec.wav");
        let summary2 = RotatingWavWriter::new(&base2, output, 5)
            .finish()
            .expect("finish");
        assert_eq!(summary2.files, vec![dir.join("rec-001.wav")]);
        assert_eq!(wav_frames(&dir.join("rec-001.wav")), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// MockBackend で Stream を実駆動する統合テスト: 3 秒相当を split 1 秒で録ると
    /// 3 ファイルでき、読み戻した合計フレーム数が書き込んだフレーム数と一致する
    /// （欠落ゼロ）。停止条件は壁時計でなく「確定ファイル数」で数える決定論形。
    #[test]
    fn rotating_writer_splits_three_seconds_from_mock_backend() {
        use flexaudio::MockBackend;

        let output = OutputFormat {
            sample_rate: 16_000,
            channels: 1,
        };
        let config = StreamConfig {
            kind: SourceKind::Mic,
            output,
            ..Default::default()
        };
        // 48kHz mono のサイン波ソース（実機不要）。Stream が出力 16k/mono へ変換する。
        let backend = Box::new(MockBackend::new(48_000, 1, 440.0));
        let mut stream = Stream::open(config, backend).expect("open stream");
        stream.start().expect("start stream");

        let dir = test_dir("split_mock");
        let base = dir.join("rec.wav");
        let mut writer = RotatingWavWriter::new(&base, output, 1);

        // 3 本目が確定する（= 3 秒相当を書き切る）までチャンクを流し込む。届いたチャンクは
        // 全て書くので、書いたフレーム数（fed_frames）が正解値になる。安全弁として上限
        // チャンク数だけ設ける（比率・経過時間のアサーションはしない）。
        let mut fed_frames: u64 = 0;
        let mut max_chunk_frames: u64 = 0;
        let mut finalized = 0usize;
        let mut polled_chunks = 0u64;
        while finalized < 3 {
            match stream.poll_chunk() {
                Some(chunk) => {
                    polled_chunks += 1;
                    fed_frames += chunk.frames as u64;
                    max_chunk_frames = max_chunk_frames.max(chunk.frames as u64);
                    if writer.write_chunk(&chunk).expect("write").is_some() {
                        finalized += 1;
                    }
                }
                None => thread::sleep(Duration::from_millis(5)),
            }
            assert!(
                polled_chunks < 2_000,
                "3 ファイル確定前にチャンク上限へ到達（Stream が流れていない）"
            );
        }
        stream.stop();

        let summary = writer.finish().expect("finish");
        // ちょうど 3 本目の確定で止めたので、空の 4 本目は作られない。
        assert_eq!(summary.files.len(), 3);
        assert_eq!(
            summary.files,
            vec![
                dir.join("rec-001.wav"),
                dir.join("rec-002.wav"),
                dir.join("rec-003.wav"),
            ]
        );
        assert_eq!(summary.total_frames, fed_frames);

        // 読み戻し: 合計 = 書き込み量（欠落ゼロ）・各ファイルは 1 秒分以上かつ超過は
        // 1 チャンク未満（境界はチャンク粒度の切り上げ）。
        let mut read_total = 0u64;
        for f in &summary.files {
            let frames = wav_frames(f);
            assert!(frames >= 16_000, "各ファイルは split 秒分以上: {frames}");
            assert!(
                frames < 16_000 + max_chunk_frames,
                "超過は 1 チャンク未満: {frames}"
            );
            read_total += frames;
        }
        assert_eq!(read_total, fed_frames, "読み戻し合計 = 書き込みフレーム数");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- run: --out - と --split-seconds の併用拒否 ---

    /// stdout ストリーミング（--out -）と --split-seconds の併用は、ストリームを開く前に
    /// 分かりやすい日本語エラーで拒否される（デバイス不要で検証できる）。
    #[test]
    fn run_rejects_split_seconds_with_stdout_stream() {
        let cli = cli_from(&["--out", "-", "--split-seconds", "5"]);
        let err = run(&cli).expect_err("must be rejected");
        assert!(err.contains("--split-seconds"), "err: {err}");
        assert!(err.contains("併用できません"), "err: {err}");
    }

    /// `write_chunk` の s16 量子化: f32 → i16。全層共通の正典 `quantize_i16`
    /// （scale 32768・round・clamp・NaN→0）に一本化済み。負側フルスケール `-1.0` は
    /// `-32768`、範囲外は飽和。
    #[test]
    fn write_chunk_s16_quantizes_and_clamps() {
        let chunk = AudioChunk {
            // 0.0 / 1.0 / -1.0 / 範囲外 2.0(→clamp 32767) / -2.0(→clamp -32768)。
            data: vec![0.0, 1.0, -1.0, 2.0, -2.0],
            frames: 5,
            pts_ns: 0,
            seq: 0,
            flags: flexaudio::core::ChunkFlags::empty(),
            dropped_before: 0,
            peak: 1.0,
            rms: 0.5,
        };
        let mut buf: Vec<u8> = Vec::new();
        write_chunk(&mut buf, &chunk, EncodingArg::S16).expect("write");
        // s16 LE: 1 サンプル 2 byte × 5 = 10 byte。
        assert_eq!(buf.len(), 10);
        let s = |i: usize| i16::from_le_bytes([buf[i * 2], buf[i * 2 + 1]]);
        assert_eq!(s(0), 0); // 0.0
        assert_eq!(s(1), 32767); // 1.0 → clamp 32767
        assert_eq!(s(2), -32768); // -1.0 → 負側フルスケール -32768
        assert_eq!(s(3), 32767); // 2.0 → clamp 32767
        assert_eq!(s(4), -32768); // -2.0 → clamp -32768
    }

    /// `write_chunk` の f32 経路: バイト長 = サンプル数 × 4、LE 復元が一致する。
    #[test]
    fn write_chunk_f32_roundtrips() {
        let chunk = AudioChunk {
            data: vec![0.25, -0.5, 0.75],
            frames: 3,
            pts_ns: 0,
            seq: 0,
            flags: flexaudio::core::ChunkFlags::empty(),
            dropped_before: 0,
            peak: 0.75,
            rms: 0.5,
        };
        let mut buf: Vec<u8> = Vec::new();
        write_chunk(&mut buf, &chunk, EncodingArg::F32).expect("write");
        assert_eq!(buf.len(), 12); // 3 サンプル × 4 byte。
        let f = |i: usize| {
            f32::from_le_bytes([buf[i * 4], buf[i * 4 + 1], buf[i * 4 + 2], buf[i * 4 + 3]])
        };
        assert_eq!(f(0), 0.25);
        assert_eq!(f(1), -0.5);
        assert_eq!(f(2), 0.75);
    }

    /// `fmt_dbfs`: 有限値は dBFS 表記、無限は無音表記。
    #[test]
    fn fmt_dbfs_finite_and_infinite() {
        assert!(fmt_dbfs(-6.0).contains("dBFS"));
        assert!(fmt_dbfs(f64::NEG_INFINITY).contains("無音"));
    }
}
