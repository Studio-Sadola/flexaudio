//! flexaudio-cli — 実機テスト用キャプチャ CLI。
//!
//! 既定マイクから N 秒キャプチャし、固定契約（48000 Hz / stereo 2ch /
//! interleaved f32）のチャンクを集めて 16-bit PCM WAV に書き出す。
//! ピーク / RMS(dBFS) / チャンク数 / ドロップ数などのサマリも表示する。
//!
//! ```text
//! flexaudio-cli --source mic --seconds 5 --out mic.wav
//! ```
//!
//! 入力デバイスが無い環境（homelab 等）では実キャプチャはできず、
//! 分かりやすいメッセージを表示して非ゼロ終了する（panic しない）。

use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};

use flexaudio::core::{
    AudioChunk, CaptureBackend, Error, SourceKind, StreamConfig, CHANNELS, SAMPLE_RATE,
};
use flexaudio::Stream;
use flexaudio_mic::CpalMicBackend;
#[cfg(target_os = "linux")]
use flexaudio_os_linux::PwSystemBackend;

/// キャプチャするソース種別（CLI 引数用）。
#[derive(Debug, Clone, Copy, ValueEnum)]
enum SourceArg {
    /// 既定マイク入力。
    Mic,
    // TODO: system（システム出力ループバック）・process（特定プロセスのループバック）は
    // 対応バックエンド（flexaudio-os-*）が配線され次第ここへ追加する。
    /// システム出力ループバック（未対応）。
    System,
    /// プロセス出力ループバック（未対応）。
    Process,
}

/// flexaudio キャプチャ CLI。
#[derive(Debug, Parser)]
#[command(name = "flexaudio-cli", about = "flexaudio キャプチャ CLI（実機テスト用）")]
struct Cli {
    /// キャプチャするソース（当面は mic のみ対応）。
    #[arg(long, value_enum, default_value_t = SourceArg::Mic)]
    source: SourceArg,

    /// キャプチャ秒数。
    #[arg(long, default_value_t = 5)]
    seconds: u64,

    /// 出力 WAV パス。
    #[arg(long, default_value = "capture.wav")]
    out: PathBuf,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
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
    if cli.seconds == 0 {
        return Err("--seconds は 1 以上を指定してください。".into());
    }

    // --- ソース種別から backend / SourceKind / 表示ラベルを解決 ---
    // Stream::open は Box<dyn CaptureBackend> を取る汎用設計なので、
    // 以降のキャプチャ→WAV 本体は全ソース共通。
    let (backend, kind, source_label): (Box<dyn CaptureBackend>, SourceKind, &str) =
        match cli.source {
            SourceArg::Mic => (
                Box::new(CpalMicBackend::new()),
                SourceKind::Mic,
                "mic（既定入力デバイス）",
            ),
            SourceArg::System => {
                #[cfg(target_os = "linux")]
                {
                    (
                        Box::new(PwSystemBackend::new()),
                        SourceKind::SystemLoopback,
                        "system（既定出力の monitor / PipeWire）",
                    )
                }
                #[cfg(not(target_os = "linux"))]
                {
                    return Err(
                        "--source system（システム出力ループバック）は現在 Linux のみ対応です。"
                            .into(),
                    );
                }
            }
            SourceArg::Process => {
                return Err(
                    "--source process（プロセス出力ループバック）は未対応です。\
                     対応バックエンドが配線され次第サポート予定です。"
                        .into(),
                );
            }
        };

    // --- ネイティブフォーマット表示 ---
    let (native_rate, native_ch) = backend.native_format();
    println!("ソース            : {source_label}");
    println!("ネイティブフォーマット: {native_rate} Hz / {native_ch} ch");
    println!(
        "出力フォーマット   : {SAMPLE_RATE} Hz / {CHANNELS} ch / 16-bit PCM（固定契約）"
    );
    println!("キャプチャ秒数     : {} 秒", cli.seconds);
    println!("出力パス           : {}", cli.out.display());
    println!();

    // --- ストリームを開いて開始 ---
    let config = StreamConfig {
        kind,
        ..Default::default()
    };
    let mut stream = Stream::open(config, backend).map_err(describe_error)?;
    stream.start().map_err(describe_error)?;

    println!("キャプチャ中 ...");

    // --- N 秒間 poll_chunk をループして全チャンクを収集 ---
    let mut chunks: Vec<AudioChunk> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(cli.seconds);
    while Instant::now() < deadline {
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
    let total_frames: usize = chunks.iter().map(|c| c.frames).sum();
    let stats = write_wav(&cli.out, &chunks).map_err(|e| format!("WAV 書き出し失敗: {e}"))?;

    let captured_secs = total_frames as f64 / SAMPLE_RATE as f64;
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

/// WAV 書き出しと同時に計算する信号統計。
struct Stats {
    /// 全サンプルの絶対値の最大（線形 0.0..=1.0 目安）。
    peak: f32,
    /// 全サンプルの二乗平均平方根（線形）。
    rms: f64,
}

/// チャンク列を 48000/2ch/16-bit PCM WAV として `path` へ書き出す。
///
/// 各チャンクの interleaved f32 を `(x.clamp(-1,1) * 32767) as i16` で量子化する。
/// 併せてピーク / RMS（線形）を計算して返す。
fn write_wav(path: &std::path::Path, chunks: &[AudioChunk]) -> hound::Result<Stats> {
    let spec = hound::WavSpec {
        channels: CHANNELS,
        sample_rate: SAMPLE_RATE,
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
        Error::DeviceLost => {
            "キャプチャ中に入力デバイスが失われました（切断など）。".into()
        }
        other => format!("ストリーム初期化に失敗しました: {other}"),
    }
}
