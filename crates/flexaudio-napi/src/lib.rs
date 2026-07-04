//! flexaudio-napi — Node.js (N-API) addon。
//!
//! Node.js アプリが flexaudio をインプロセスで使うためのバインディング。低レイテンシの
//! ストリーミング録音をコールバック経由で Node へ届ける。
//!
//! 設計:
//! - 公開関数は camelCase（`#[napi]` が JS 名へ変換）。
//! - チャンク/イベントは `ThreadsafeFunction`（ErrorStrategy::Fatal）で JS コールバックへ送る。
//! - `FlexStream` 構築時に bridge スレッドを spawn し、`stream.start()` 後に
//!   `poll_chunk` / `poll_event` を 1ms 間隔でポーリングして TSFN へ NonBlocking で渡す。
//! - 停止は `Arc<AtomicBool>` のフラグ + `JoinHandle::join()`。Drop でも止める。
//!
//! 実行時にネットワーク通信はしない（napi は N-API ブリッジのみ）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use napi::bindgen_prelude::{BigInt, Float32Array};
use napi::threadsafe_function::{ErrorStrategy, ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi::{Error as NapiError, Status};
use napi_derive::napi;

use flexaudio::{
    AudioChunk, DeviceEvent, DeviceInfo, Event, OutputFormat, ProcessMode, SourceKind, StreamConfig,
};

// アドオン 3 種のコア型。`#[napi]` ラッパ（Vad / Denoiser）と同名なので別名で取り込む。
use flexaudio_denoise::{DenoiseError, Denoiser as CoreDenoiser};
use flexaudio_encode::{EncodeError, FlacWriter};
use flexaudio_vad::{Vad as CoreVad, VadConfig, VadError, VadEvent};

// bridge スレッドのポーリング間隔。20ms チャンクに対し十分小さく、空転も避ける。
const POLL_INTERVAL: Duration = Duration::from_millis(1);
// デバイス着脱は低頻度。応答性 100ms で十分。
const DEVICE_POLL_INTERVAL: Duration = Duration::from_millis(100);

// ErrorStrategy::Fatal の TSFN 別名。`.call(value, mode)` が値を直接取れる
// （CalleeHandled だと `.call(Result<T>, mode)` になり Result ラップが要る）。
type ChunkTsfn = ThreadsafeFunction<JsAudioChunk, ErrorStrategy::Fatal>;
type EventTsfn = ThreadsafeFunction<JsStreamEvent, ErrorStrategy::Fatal>;
type DeviceTsfn = ThreadsafeFunction<JsDeviceEvent, ErrorStrategy::Fatal>;

/// flexaudio::Error → napi::Error。メッセージを文字列化して GenericFailure にする。
fn to_napi_err(err: flexaudio::Error) -> NapiError {
    NapiError::new(Status::GenericFailure, err.to_string())
}

/// VadError → napi::Error。設定不正は呼び出し側のミスなので InvalidArg、
/// モデルロード/推論失敗は環境要因なので GenericFailure に振り分ける。
fn vad_err(err: VadError) -> NapiError {
    let status = match err {
        VadError::InvalidConfig(_) => Status::InvalidArg,
        _ => Status::GenericFailure,
    };
    NapiError::new(status, err.to_string())
}

/// DenoiseError → napi::Error。どちらのバリアントも引数不正なので InvalidArg。
fn denoise_err(err: DenoiseError) -> NapiError {
    NapiError::new(Status::InvalidArg, err.to_string())
}

/// EncodeError → napi::Error。非対応パラメータは InvalidArg、IO/エンコーダ内部は
/// GenericFailure。`#[non_exhaustive]` なので `_` で将来バリアントも受ける。
fn encode_err(err: EncodeError) -> NapiError {
    let status = match err {
        EncodeError::Unsupported(_) => Status::InvalidArg,
        _ => Status::GenericFailure,
    };
    NapiError::new(status, err.to_string())
}

// ---------------------------------------------------------------------------
// JS 向けデータ型（`#[napi(object)]` でプレーンオブジェクトとして JS と相互変換）
// ---------------------------------------------------------------------------

/// JS 側 DeviceInfo。`sourceKind` は文字列（"mic"|"system"|"process"）。
#[napi(object)]
pub struct JsDeviceInfo {
    pub id: String,
    pub name: String,
    pub source_kind: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub is_loopback: bool,
    pub is_default: bool,
}

/// JS 側 AudioChunk。`data` は interleaved f32（len = frames * channels）。
/// `seq`(u64) は精度欠落を避けて BigInt。`flags` は ChunkFlags のビット(u32)。
///
/// `vadEvents` は `openStream` に `vad` を指定したときだけ埋まる。VAD 無効時は未設定
/// （`undefined`）。有効でもそのチャンクで確定イベントが無ければ空配列になる。
#[napi(object)]
pub struct JsAudioChunk {
    pub data: Float32Array,
    pub frames: u32,
    pub pts_ns: i64,
    pub seq: BigInt,
    pub flags: u32,
    pub dropped_before: u32,
    pub peak: f64,
    pub rms: f64,
    /// このチャンクで確定した VAD イベント（統合 VAD 有効時のみ）。
    pub vad_events: Option<Vec<JsVadEvent>>,
}

/// JS 側 VAD イベント。`type` は "speechStart" | "speechEnd"。
///
/// `atSample` は **VAD の内部レート（`sampleRate`＝8000 か 16000、既定 16000）基準**の
/// 絶対サンプル位置で、入力チャンクのサンプル基準ではない。秒に直すなら
/// `atSample / sampleRate`、入力サンプル位置の目安は
/// `atSample * inputSampleRate / sampleRate` で近似できる。
#[napi(object)]
pub struct JsVadEvent {
    #[napi(js_name = "type")]
    pub kind: String,
    pub at_sample: i64,
}

/// JS 側ネイティブフォーマット（`FlexStream.nativeFormat` の戻り）。
#[napi(object)]
pub struct JsNativeFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

/// 統合 VAD の設定（`OpenOptions.vad` と `Vad` コンストラクタが共有）。
///
/// 各フィールドは省略可で、省略時は silero 準拠の既定値（`VadConfig::default`）。
#[napi(object)]
pub struct VadOptions {
    /// 発話開始とみなす確率しきい値 (>=)。既定 0.5。
    pub threshold: Option<f64>,
    /// 無音開始とみなす負側しきい値 (<)。省略時は `max(threshold - 0.15, 0.01)`。
    pub neg_threshold: Option<f64>,
    /// 採用する発話の最小長 (ms)。既定 250。
    pub min_speech_ms: Option<u32>,
    /// 発話終了の確定に必要な無音長 (ms)。既定 100。
    pub min_silence_ms: Option<u32>,
    /// セグメント境界を前後に広げるパディング (ms)。既定 30。
    pub speech_pad_ms: Option<u32>,
    /// 1 セグメントの最大長 (ms)。0 = 無制限。既定 0。
    pub max_speech_ms: Option<u32>,
    /// VAD の内部サンプルレート。8000 または 16000 のみ。既定 16000。
    pub sample_rate: Option<u32>,
}

/// JS 側ストリームイベント。`type` で種別、`count`/`message` は任意。
#[napi(object)]
pub struct JsStreamEvent {
    #[napi(js_name = "type")]
    pub kind: String,
    pub count: Option<i64>,
    pub message: Option<String>,
}

/// JS 側デバイスイベント。`type` で種別、device/id/sourceKind は任意。
#[napi(object)]
pub struct JsDeviceEvent {
    #[napi(js_name = "type")]
    pub kind: String,
    pub device: Option<JsDeviceInfo>,
    pub id: Option<String>,
    pub source_kind: Option<String>,
}

/// openStream / __openMockStream のオプション。
#[napi(object)]
pub struct OpenOptions {
    /// "mic" | "system" | "process" | "mix"
    pub kind: String,
    pub device_id: Option<String>,
    pub process_id: Option<u32>,
    /// process の対象 PID の扱い（process 専用）。"include"（既定）| "exclude"。
    /// include=対象 PID だけ録る / exclude=対象 PID 以外の全システム音（process_id 必須）。
    /// mic / system では無視。Linux / Windows / macOS の 3 OS とも対応。
    pub mode: Option<String>,
    /// システム音から自ホスト（自プロセス）の音を除くか（system 専用。mix では
    /// system 側に適用）。既定 false。mic / process では無視。
    /// Linux / Windows / macOS の 3 OS とも対応。
    pub exclude_self: Option<bool>,
    /// 既定 48000
    pub output_rate: Option<u32>,
    /// 既定 2
    pub output_channels: Option<u16>,
    /// 既定 20
    pub chunk_ms: Option<u32>,
    /// 開始時の入力ゲイン（線形倍率）。既定 1.0。1.0=そのまま、2.0=約+6dB、0.0=無音。
    /// 実行時変更は `setGain`。
    pub gain: Option<f64>,
    /// mix の mic 側で選ぶ入力デバイス ID（mix 専用）。未指定なら既定入力。
    pub mic_device_id: Option<String>,
    /// mix の system 側で選ぶ出力エンドポイント ID（mix 専用）。未指定なら既定出力。
    pub system_device_id: Option<String>,
    /// mix の mic 側の合成前倍率（線形・mix 専用）。既定 1.0。合成後に `gain` が掛かる。
    pub mic_gain: Option<f64>,
    /// mix の system 側の合成前倍率（線形・mix 専用）。既定 1.0。
    pub system_gain: Option<f64>,
    /// 統合 VAD の設定。指定すると各チャンクを VAD に通し、確定イベントをそのチャンクの
    /// `vadEvents` に添える（音声自体は加工しない）。省略時は VAD 無効。
    pub vad: Option<VadOptions>,
    /// true で録音時ノイズ抑制を有効化。**出力が 48000 Hz のときだけ使える**
    /// （RNNoise は 48kHz 固定）。有効時は配信/保存されるチャンクの `data` 自体が
    /// ノイズ抑制後の音に置き換わる。48kHz 以外で true にすると `openStream` が
    /// InvalidArg を投げる。省略/false でノイズ抑制なし。
    pub denoise: Option<bool>,
}

// ---------------------------------------------------------------------------
// 変換ヘルパ
// ---------------------------------------------------------------------------

fn source_kind_str(k: SourceKind) -> String {
    match k {
        SourceKind::Mic => "mic",
        SourceKind::SystemLoopback => "system",
        SourceKind::ProcessLoopback => "process",
        SourceKind::Mix => "mix",
    }
    .to_string()
}

fn parse_source_kind(s: &str) -> napi::Result<SourceKind> {
    match s {
        "mic" => Ok(SourceKind::Mic),
        "system" => Ok(SourceKind::SystemLoopback),
        "process" => Ok(SourceKind::ProcessLoopback),
        "mix" => Ok(SourceKind::Mix),
        other => Err(NapiError::new(
            Status::InvalidArg,
            format!("unknown kind: {other:?} (expected mic|system|process|mix)"),
        )),
    }
}

/// "include" | "exclude" を [`ProcessMode`] へ（process 専用）。`None`/未指定は既定 Include。
fn parse_process_mode(s: Option<&str>) -> napi::Result<ProcessMode> {
    match s {
        None | Some("include") => Ok(ProcessMode::Include),
        Some("exclude") => Ok(ProcessMode::Exclude),
        Some(other) => Err(NapiError::new(
            Status::InvalidArg,
            format!("unknown mode: {other:?} (expected include|exclude)"),
        )),
    }
}

fn device_info_to_js(info: DeviceInfo) -> JsDeviceInfo {
    JsDeviceInfo {
        id: info.id,
        name: info.name,
        source_kind: source_kind_str(info.source_kind),
        sample_rate: info.sample_rate,
        channels: info.channels,
        is_loopback: info.is_loopback,
        is_default: info.is_default,
    }
}

fn chunk_to_js(chunk: AudioChunk) -> JsAudioChunk {
    let frames = chunk.frames as u32;
    JsAudioChunk {
        // Vec<f32> を Float32Array 化（所有権をスレッド側に残さない）。
        data: Float32Array::new(chunk.data),
        frames,
        pts_ns: chunk.pts_ns,
        seq: BigInt::from(chunk.seq),
        flags: chunk.flags.bits(),
        dropped_before: chunk.dropped_before,
        peak: chunk.peak as f64,
        rms: chunk.rms as f64,
        // 既定は未設定。統合 VAD 有効時は emit_chunk が上書きする。
        vad_events: None,
    }
}

fn vad_event_to_js(ev: VadEvent) -> JsVadEvent {
    match ev {
        VadEvent::SpeechStart { at_sample } => JsVadEvent {
            kind: "speechStart".to_string(),
            at_sample: at_sample as i64,
        },
        VadEvent::SpeechEnd { at_sample } => JsVadEvent {
            kind: "speechEnd".to_string(),
            at_sample: at_sample as i64,
        },
    }
}

/// [`VadOptions`] → [`VadConfig`]。省略フィールドは silero 準拠の既定へ倒す。
/// `neg_threshold` の省略は `None` のまま（`VadConfig` 側の既定式が効く）。
fn build_vad_config(o: &VadOptions) -> VadConfig {
    let d = VadConfig::default();
    VadConfig {
        threshold: o.threshold.map(|v| v as f32).unwrap_or(d.threshold),
        neg_threshold: o.neg_threshold.map(|v| v as f32),
        min_speech_ms: o.min_speech_ms.unwrap_or(d.min_speech_ms),
        min_silence_ms: o.min_silence_ms.unwrap_or(d.min_silence_ms),
        speech_pad_ms: o.speech_pad_ms.unwrap_or(d.speech_pad_ms),
        max_speech_ms: o.max_speech_ms.unwrap_or(d.max_speech_ms),
        sample_rate: o.sample_rate.unwrap_or(d.sample_rate),
    }
}

/// 統合 denoise の 48kHz 前提を検証する（純関数・テスト用に分離）。
///
/// RNNoise は 48kHz 固定なので、`enabled` かつ出力レートが 48000 でなければ
/// InvalidArg を返す。`open_stream` はストリームを開く前にこれで弾く。
fn check_denoise_rate(enabled: bool, output_rate: u32) -> napi::Result<()> {
    if enabled && output_rate != 48_000 {
        return Err(NapiError::new(
            Status::InvalidArg,
            format!(
                "denoise は 48000 Hz 出力のみ対応（RNNoise は 48kHz 固定）。\
                 outputRate={output_rate} では使えません"
            ),
        ));
    }
    Ok(())
}

/// FLAC ローテーションの `index` 番目（1 始まり）のパスを作る（純関数）。
///
/// CLI の `split_file_path` と同じ流儀: `rec.flac` なら `rec-001.flac, rec-002.flac, …`
/// と拡張子の前へ 3 桁ゼロ詰め連番を挟む。1000 以降は桁が自然に増える。拡張子が無い
/// パスは末尾に連番を足す。親ディレクトリは保たれる。
fn split_flac_path(base: &Path, index: u64) -> PathBuf {
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

fn event_to_js(ev: Event) -> JsStreamEvent {
    match ev {
        Event::ChunkDropped { count } => JsStreamEvent {
            kind: "chunkDropped".to_string(),
            count: Some(count as i64),
            message: None,
        },
        Event::StreamStalled => JsStreamEvent {
            kind: "stalled".to_string(),
            count: None,
            message: None,
        },
        Event::StreamRecovered => JsStreamEvent {
            kind: "recovered".to_string(),
            count: None,
            message: None,
        },
        Event::PermissionDenied => JsStreamEvent {
            kind: "permissionDenied".to_string(),
            count: None,
            message: None,
        },
        Event::DeviceLost => JsStreamEvent {
            kind: "deviceLost".to_string(),
            count: None,
            message: None,
        },
        Event::Error(msg) => JsStreamEvent {
            kind: "error".to_string(),
            count: None,
            message: Some(msg),
        },
        // Event は #[non_exhaustive]。将来のバリアント追加に備えて、未知種別は "error"
        // + デバッグ表現で JS へ通知する（握り潰さない）。
        other => JsStreamEvent {
            kind: "error".to_string(),
            count: None,
            message: Some(format!("unknown event: {other:?}")),
        },
    }
}

fn device_event_to_js(ev: DeviceEvent) -> JsDeviceEvent {
    match ev {
        DeviceEvent::Added(info) => JsDeviceEvent {
            kind: "added".to_string(),
            device: Some(device_info_to_js(info)),
            id: None,
            source_kind: None,
        },
        DeviceEvent::Removed { id } => JsDeviceEvent {
            kind: "removed".to_string(),
            device: None,
            id: Some(id),
            source_kind: None,
        },
        DeviceEvent::DefaultChanged { kind, id } => JsDeviceEvent {
            kind: "defaultChanged".to_string(),
            device: None,
            id: Some(id),
            source_kind: Some(source_kind_str(kind)),
        },
        // DeviceEvent は #[non_exhaustive]。将来のバリアント追加に備えて、未知種別は
        // "unknown" として JS へ渡す（握り潰さない）。
        _ => JsDeviceEvent {
            kind: "unknown".to_string(),
            device: None,
            id: None,
            source_kind: None,
        },
    }
}

fn build_config(options: &OpenOptions) -> napi::Result<StreamConfig> {
    let kind = parse_source_kind(&options.kind)?;
    let mode = parse_process_mode(options.mode.as_deref())?;
    let output = OutputFormat {
        sample_rate: options.output_rate.unwrap_or(48_000),
        channels: options.output_channels.unwrap_or(2),
    };
    let mut config = StreamConfig {
        kind,
        output,
        device_id: options.device_id.clone(),
        target_pid: options.process_id,
        // mode は process 専用 / exclude_self は system 専用。混ぜないのは facade 側が見る。
        mode,
        exclude_self: options.exclude_self.unwrap_or(false),
        gain: options.gain.unwrap_or(1.0) as f32,
        // mix 専用（mic/system/process では facade が無視する）。側別ゲインは未指定 1.0。
        mix_mic_device_id: options.mic_device_id.clone(),
        mix_system_device_id: options.system_device_id.clone(),
        mix_mic_gain: options.mic_gain.unwrap_or(1.0) as f32,
        mix_system_gain: options.system_gain.unwrap_or(1.0) as f32,
        ..Default::default()
    };
    if let Some(ms) = options.chunk_ms {
        config.chunk_ms = ms;
    }
    Ok(config)
}

// ---------------------------------------------------------------------------
// FlexStream（class）。bridge スレッドの所有・停止を担う。
// ---------------------------------------------------------------------------

/// bridge スレッドへソース切替を依頼するコマンド。
///
/// Stream は bridge スレッドが所有しているので `switch_source` を直接呼べない。JS から
/// 来た切替要求をこのコマンドで bridge スレッドへ送り、`result_tx` で結果を同期的に
/// 受け取る（JS 側は同期返却を期待する）。
struct SwitchCmd {
    config: StreamConfig,
    result_tx: mpsc::Sender<std::result::Result<(), String>>,
}

/// bridge スレッドへストリームの現在値の読み出しを依頼するコマンド。
///
/// `is_paused` / `gain` / `native_format` / `dropped_chunks` はいずれも Stream 上の
/// メソッドで、Stream は bridge スレッドが所有しているため直接は読めない。1 回の問い合わせで
/// まとめて [`StreamSnapshot`] を受け取り、各ゲッタが必要なフィールドだけ取り出す。
struct QueryCmd {
    result_tx: mpsc::Sender<StreamSnapshot>,
}

/// bridge スレッドが読み取ったストリームの現在値のスナップショット。
struct StreamSnapshot {
    is_paused: bool,
    gain: f32,
    native_sample_rate: u32,
    native_channels: u16,
    dropped_chunks: u64,
}

/// bridge スレッドへ送るコマンド。Stream を触るのは bridge スレッドだけなので、JS から
/// の操作はすべてこのチャネル経由で依頼する。
enum BridgeCmd {
    /// 入力ソースのホットスワップ（結果を同期で返す）。
    Switch(SwitchCmd),
    /// 配信を一時停止する。
    Pause,
    /// 配信を再開する。
    Resume,
    /// 入力ゲイン（線形倍率）を変更する。値は送信前に napi 側で検証済み。
    SetGain(f32),
    /// 現在値のスナップショットを同期で返す（ゲッタ用）。
    Query(QueryCmd),
}

/// 統合 denoise / VAD をかけてチャンクを JS へ送る。
///
/// 順序は **denoise → VAD**。ノイズ抑制で定常ノイズを削ってから発話判定する方が、
/// ノイズ由来の誤検出が減って自然なため。denoise はチャンクの `data` を配信前に
/// その場で書き換える（保存・配信される音そのものが加工される）。VAD は加工後の
/// `data` を読むだけで音は変えず、確定イベントを `vadEvents` に添える。
///
/// `denoiser` / `vad` はストリーム開始時に一度だけ作られ、以後この 1 インスタンスを
/// 使い続ける（`switchSource` でも作り直さない。出力フォーマットは切替で変わらない仕様
/// なので、denoiser のチャンネル数も 48kHz 前提も保たれる）。
fn emit_chunk(
    mut chunk: AudioChunk,
    denoiser: &mut Option<CoreDenoiser>,
    vad: &mut Option<CoreVad>,
    output_rate: u32,
    output_channels: u16,
    on_chunk: &ChunkTsfn,
) {
    if let Some(dn) = denoiser.as_mut() {
        // data 長は frames * output_channels なので必ずチャンネル数の倍数＝process は
        // InvalidLength にならない。48kHz 前提は open_stream が保証済み。
        let _ = dn.process(&mut chunk.data);
    }
    let vad_events = vad.as_mut().map(|v| {
        v.process_pcm(&chunk.data, output_rate, output_channels)
            .into_iter()
            .map(vad_event_to_js)
            .collect::<Vec<_>>()
    });
    let mut js = chunk_to_js(chunk);
    js.vad_events = vad_events;
    on_chunk.call(js, ThreadsafeFunctionCallMode::NonBlocking);
}

/// 録音ストリームのハンドル。内部で bridge スレッドが `flexaudio::Stream` を
/// 所有・ポーリングし、チャンク/イベントを TSFN 経由で JS へ送る。
#[napi]
pub struct FlexStream {
    stop_flag: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    /// bridge スレッドへ切替コマンドを送るチャネル。`shutdown` で drop してスレッド側の
    /// `try_recv` を打ち切る（停止自体は stop_flag が担う）。
    cmd_tx: Option<mpsc::Sender<BridgeCmd>>,
}

impl FlexStream {
    /// 既に `start()` 済みの Stream を受け取り、bridge スレッドを spawn する。
    /// Stream は Send なのでスレッドへ move する（poll_chunk が &mut self なので所有は
    /// スレッド側に置く）。
    ///
    /// `denoiser` / `vad` は統合機能。`Some` のときだけチャンクに適用する（構築は
    /// open_stream 側で行い、失敗はそちらで先に弾く）。`output_rate` / `output_channels`
    /// は VAD の `process_pcm` へ渡す出力フォーマット。
    fn spawn(
        mut stream: flexaudio::Stream,
        on_chunk: ChunkTsfn,
        on_event: Option<EventTsfn>,
        mut denoiser: Option<CoreDenoiser>,
        mut vad: Option<CoreVad>,
        output_rate: u32,
        output_channels: u16,
    ) -> Self {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let thread_stop = stop_flag.clone();
        let (cmd_tx, cmd_rx) = mpsc::channel::<BridgeCmd>();

        let handle = thread::spawn(move || {
            loop {
                if thread_stop.load(Ordering::SeqCst) {
                    break;
                }
                // コマンドを poll と同じ周回でまとめて処理する。
                while let Ok(cmd) = cmd_rx.try_recv() {
                    match cmd {
                        BridgeCmd::Switch(sw) => {
                            let r = stream.switch_source(sw.config).map_err(|e| e.to_string());
                            // 受け手（switch_source 呼び出し元）が drop していても無視。
                            let _ = sw.result_tx.send(r);
                        }
                        BridgeCmd::Pause => stream.pause(),
                        BridgeCmd::Resume => stream.resume(),
                        BridgeCmd::SetGain(g) => {
                            // 送信前に napi 側で検証済みなので Err は起きない前提。
                            // 万一の Err もイベントにはしない（結果は捨てる）。
                            let _ = stream.set_gain(g);
                        }
                        BridgeCmd::Query(q) => {
                            let (native_sample_rate, native_channels) = stream.native_format();
                            let snap = StreamSnapshot {
                                is_paused: stream.is_paused(),
                                gain: stream.gain(),
                                native_sample_rate,
                                native_channels,
                                dropped_chunks: stream.dropped_chunks(),
                            };
                            // 受け手が drop していても無視。
                            let _ = q.result_tx.send(snap);
                        }
                    }
                }
                // チャンクは到着し次第すべて吐く（denoise → VAD を通して JS へ）。
                while let Some(chunk) = stream.poll_chunk() {
                    emit_chunk(
                        chunk,
                        &mut denoiser,
                        &mut vad,
                        output_rate,
                        output_channels,
                        &on_chunk,
                    );
                }
                // イベントも消化。
                while let Some(ev) = stream.poll_event() {
                    if let Some(cb) = &on_event {
                        cb.call(event_to_js(ev), ThreadsafeFunctionCallMode::NonBlocking);
                    }
                }
                thread::sleep(POLL_INTERVAL);
            }
            // 停止前にリングへ残ったチャンクを取り切ってから stop。
            while let Some(chunk) = stream.poll_chunk() {
                emit_chunk(
                    chunk,
                    &mut denoiser,
                    &mut vad,
                    output_rate,
                    output_channels,
                    &on_chunk,
                );
            }
            stream.stop();
        });

        Self {
            stop_flag,
            handle: Some(handle),
            cmd_tx: Some(cmd_tx),
        }
    }

    fn shutdown(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        // 切替チャネルを閉じて bridge スレッドの try_recv を Disconnected にする。
        self.cmd_tx = None;
        if let Some(h) = self.handle.take() {
            // 二重 stop / Drop でも安全（handle が take 済みなら何もしない）。
            let _ = h.join();
        }
    }

    /// bridge スレッドへ Query を送り、ストリームの現在値スナップショットを同期受信する。
    /// 各ゲッタ（`is_paused`/`gain`/`native_format`/`dropped_chunks`）の実体。既に
    /// `stop()` 済みなら例外。
    fn query_snapshot(&self) -> napi::Result<StreamSnapshot> {
        let cmd_tx = self.cmd_tx.as_ref().ok_or_else(|| {
            NapiError::new(Status::GenericFailure, "stream already stopped".to_string())
        })?;
        let (result_tx, result_rx) = mpsc::channel();
        cmd_tx
            .send(BridgeCmd::Query(QueryCmd { result_tx }))
            .map_err(|_| {
                NapiError::new(
                    Status::GenericFailure,
                    "bridge thread is not running".to_string(),
                )
            })?;
        result_rx.recv().map_err(|_| {
            NapiError::new(
                Status::GenericFailure,
                "bridge thread dropped before responding".to_string(),
            )
        })
    }
}

#[napi]
impl FlexStream {
    /// 録音を停止し bridge スレッドを join する。二重呼び出し安全。
    #[napi]
    pub fn stop(&mut self) {
        self.shutdown();
    }

    /// 録音を止めずに入力ソース（mic/system/process）をホットスワップする。
    ///
    /// `options` から構築した `StreamConfig` への切替を bridge スレッドへ依頼し、結果を
    /// 同期的に返す（成功で `Ok`、失敗で例外）。出力フォーマット（`outputRate`/
    /// `outputChannels`）は切替では変えられない（連続ストリームの frames が変わるため）。
    /// 変更を要求すると `switch_source` が InvalidArg を返し、ここで例外になる。切替前後で
    /// チャンクの `seq` は連続し、切替後最初のチャンクには DISCONTINUITY フラグが立つ。
    /// `options.gain` は無視される（ゲインはストリームの状態。変更は `setGain`）。
    ///
    /// 既に `stop()` 済み（bridge スレッド停止後）なら例外を返す。
    #[napi]
    pub fn switch_source(&self, options: OpenOptions) -> napi::Result<()> {
        // openStream と同じく build_config で options → StreamConfig。
        let config = build_config(&options)?;

        // bridge スレッドへコマンドを送り、結果を同期受信する。
        let cmd_tx = self.cmd_tx.as_ref().ok_or_else(|| {
            NapiError::new(Status::GenericFailure, "stream already stopped".to_string())
        })?;
        let (result_tx, result_rx) = mpsc::channel();
        cmd_tx
            .send(BridgeCmd::Switch(SwitchCmd { config, result_tx }))
            .map_err(|_| {
                NapiError::new(
                    Status::GenericFailure,
                    "bridge thread is not running".to_string(),
                )
            })?;
        // bridge スレッドが switch_source を実行して結果を返すのを待つ（同期）。
        match result_rx.recv() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(msg)) => Err(NapiError::new(Status::GenericFailure, msg)),
            Err(_) => Err(NapiError::new(
                Status::GenericFailure,
                "bridge thread dropped before responding".to_string(),
            )),
        }
    }

    /// 録音を一時停止する。デバイスは動かしたまま配信だけ止める。`resume` で再開し、
    /// 再開後の最初のチャンクに DISCONTINUITY が立つ。既に `stop()` 済みなら例外。
    #[napi]
    pub fn pause(&self) -> napi::Result<()> {
        let cmd_tx = self.cmd_tx.as_ref().ok_or_else(|| {
            NapiError::new(Status::GenericFailure, "stream already stopped".to_string())
        })?;
        cmd_tx.send(BridgeCmd::Pause).map_err(|_| {
            NapiError::new(
                Status::GenericFailure,
                "bridge thread is not running".to_string(),
            )
        })?;
        Ok(())
    }

    /// 一時停止を解除して配信を再開する。既に `stop()` 済みなら例外。
    #[napi]
    pub fn resume(&self) -> napi::Result<()> {
        let cmd_tx = self.cmd_tx.as_ref().ok_or_else(|| {
            NapiError::new(Status::GenericFailure, "stream already stopped".to_string())
        })?;
        cmd_tx.send(BridgeCmd::Resume).map_err(|_| {
            NapiError::new(
                Status::GenericFailure,
                "bridge thread is not running".to_string(),
            )
        })?;
        Ok(())
    }

    /// 入力ゲイン（線形倍率）を変更する。1.0=そのまま、2.0=約+6dB、0.0=無音。録音中
    /// いつでも呼べて、次のチャンクから効く（20ms 粒度）。乗算後のサンプルは ±1.0 に
    /// クランプされる。有限かつ 0 以上でなければ例外。既に `stop()` 済みなら例外。
    #[napi]
    pub fn set_gain(&self, gain: f64) -> napi::Result<()> {
        // f64→f32 変換後の値で検証する（f32 で表せない巨大値が無限大になるのも弾く）。
        let gain = gain as f32;
        if !gain.is_finite() || gain < 0.0 {
            return Err(NapiError::new(
                Status::InvalidArg,
                format!("gain must be finite and >= 0.0, got {gain}"),
            ));
        }
        let cmd_tx = self.cmd_tx.as_ref().ok_or_else(|| {
            NapiError::new(Status::GenericFailure, "stream already stopped".to_string())
        })?;
        cmd_tx.send(BridgeCmd::SetGain(gain)).map_err(|_| {
            NapiError::new(
                Status::GenericFailure,
                "bridge thread is not running".to_string(),
            )
        })?;
        Ok(())
    }

    /// 現在ポーズ中かどうか。既に `stop()` 済みなら例外。
    #[napi]
    pub fn is_paused(&self) -> napi::Result<bool> {
        Ok(self.query_snapshot()?.is_paused)
    }

    /// 現在の入力ゲイン（線形倍率）。既に `stop()` 済みなら例外。
    #[napi]
    pub fn gain(&self) -> napi::Result<f64> {
        Ok(self.query_snapshot()?.gain as f64)
    }

    /// 現在の backend のネイティブフォーマット `{ sampleRate, channels }`。表示・診断用
    /// （実際に配信されるチャンクは出力フォーマット `outputRate`/`outputChannels`）。
    /// `switchSource` でソースを変えると新 backend の値に更新される。既に `stop()` 済み
    /// なら例外。
    #[napi]
    pub fn native_format(&self) -> napi::Result<JsNativeFormat> {
        let s = self.query_snapshot()?;
        Ok(JsNativeFormat {
            sample_rate: s.native_sample_rate,
            channels: s.native_channels,
        })
    }

    /// チャンクリングが DROP_OLDEST で捨てた累計チャンク数（BigInt）。既に `stop()` 済み
    /// なら例外。
    #[napi]
    pub fn dropped_chunks(&self) -> napi::Result<BigInt> {
        Ok(BigInt::from(self.query_snapshot()?.dropped_chunks))
    }
}

impl Drop for FlexStream {
    fn drop(&mut self) {
        // JS が stop を呼ばずに捨てても、ゾンビスレッドを残さない。
        self.shutdown();
    }
}

// ---------------------------------------------------------------------------
// DeviceWatcherHandle（class）
// ---------------------------------------------------------------------------

/// デバイス着脱監視のハンドル。bridge スレッドが `DeviceWatcher` を poll する。
#[napi]
pub struct DeviceWatcherHandle {
    stop_flag: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl DeviceWatcherHandle {
    fn shutdown(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[napi]
impl DeviceWatcherHandle {
    /// 監視を停止し bridge スレッドを join する。二重呼び出し安全。
    #[napi]
    pub fn stop(&mut self) {
        self.shutdown();
    }
}

impl Drop for DeviceWatcherHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ---------------------------------------------------------------------------
// 公開関数
// ---------------------------------------------------------------------------

/// 利用可能なデバイスを列挙する。ヘッドレス環境では空配列でも throw しない。
#[napi]
pub fn devices() -> napi::Result<Vec<JsDeviceInfo>> {
    let list = flexaudio::devices().map_err(to_napi_err)?;
    Ok(list.into_iter().map(device_info_to_js).collect())
}

/// ストリームを開いて開始し、チャンク/イベントをコールバックへ送る `FlexStream` を返す。
///
/// `options.denoise` / `options.vad` を指定すると統合ノイズ抑制・VAD が有効になる
/// （詳細は [`OpenOptions`] と内部の `emit_chunk`）。denoise の 48kHz 前提や VAD 設定の
/// 不正は、ここでストリームを開く前に検証して弾く。
#[napi]
pub fn open_stream(
    options: OpenOptions,
    on_chunk: ChunkTsfn,
    on_event: Option<EventTsfn>,
) -> napi::Result<FlexStream> {
    let config = build_config(&options)?;
    let output_rate = config.output.sample_rate;
    let output_channels = config.output.channels;

    // 統合 denoise: 48kHz 前提を先に検証し、通れば denoiser を構築（チャンネル数不正も
    // ここで InvalidArg として弾く）。
    let denoise_enabled = options.denoise.unwrap_or(false);
    check_denoise_rate(denoise_enabled, output_rate)?;
    let denoiser = if denoise_enabled {
        Some(CoreDenoiser::new(output_channels).map_err(denoise_err)?)
    } else {
        None
    };

    // 統合 VAD: 指定時に構築（モデルロード・設定不正はここで例外化）。
    let vad = match &options.vad {
        Some(o) => Some(CoreVad::new(build_vad_config(o)).map_err(vad_err)?),
        None => None,
    };

    let mut stream = flexaudio::open(config).map_err(to_napi_err)?;
    stream.start().map_err(to_napi_err)?;
    Ok(FlexStream::spawn(
        stream,
        on_chunk,
        on_event,
        denoiser,
        vad,
        output_rate,
        output_channels,
    ))
}

/// デバイス着脱を監視し、イベントをコールバックへ送る `DeviceWatcherHandle` を返す。
#[napi]
pub fn watch_devices(on_event: DeviceTsfn) -> napi::Result<DeviceWatcherHandle> {
    let mut watcher = flexaudio::watch_devices().map_err(to_napi_err)?;
    let stop_flag = Arc::new(AtomicBool::new(false));
    let thread_stop = stop_flag.clone();

    let handle = thread::spawn(move || {
        loop {
            if thread_stop.load(Ordering::SeqCst) {
                break;
            }
            while let Some(ev) = watcher.poll_event() {
                on_event.call(
                    device_event_to_js(ev),
                    ThreadsafeFunctionCallMode::NonBlocking,
                );
            }
            thread::sleep(DEVICE_POLL_INTERVAL);
        }
        watcher.stop();
    });

    Ok(DeviceWatcherHandle {
        stop_flag,
        handle: Some(handle),
    })
}

/// テスト専用・公開 API 外。
///
/// 低レベル `Stream::open` に `MockBackend` を渡してストリームを作り、`open_stream` と
/// 同じ bridge / TSFN 経路で回す。実音なしで marshaling 全経路（Float32Array・BigInt・
/// peak/rms・frames）を end-to-end 検証する。本番コードからは使わないこと。
///
/// JS 名は `__openMockStream`。先頭 `__` で公開 API 外を示す。napi の既定変換は先頭
/// アンダースコアを落として `openMockStream` にしてしまうので `js_name` で固定する。
#[napi(js_name = "__openMockStream")]
pub fn open_mock_stream(
    sample_rate: u32,
    channels: u16,
    freq_hz: f64,
    on_chunk: ChunkTsfn,
) -> napi::Result<FlexStream> {
    let config = StreamConfig {
        kind: SourceKind::Mic,
        output: OutputFormat {
            sample_rate,
            channels,
        },
        ..Default::default()
    };
    let backend = Box::new(flexaudio::MockBackend::new(
        sample_rate,
        channels,
        freq_hz as f32,
    ));
    let mut stream = flexaudio::Stream::open(config, backend).map_err(to_napi_err)?;
    stream.start().map_err(to_napi_err)?;
    // モック経路は統合 denoise / VAD を通さない（marshaling 経路の検証が目的）。
    Ok(FlexStream::spawn(
        stream,
        on_chunk,
        None,
        None,
        None,
        sample_rate,
        channels,
    ))
}

// ---------------------------------------------------------------------------
// 独立アドオン 1: Vad（silero-VAD をストリーミング実行する小さなラッパ）
// ---------------------------------------------------------------------------

/// オフライン VAD（silero-VAD on ONNX、モデル埋め込み）のハンドル。
///
/// 1 インスタンスが ONNX セッションを 1 つ持つ。任意フォーマット（`inputSampleRate` /
/// `inputChannels` の interleaved f32）を [`Vad::process`] に流すと、内部で VAD レートの
/// mono に変換してから発話区間を検出し、確定した [`JsVadEvent`] を返す。`openStream` の
/// 統合 VAD を使わず、任意のサンプル列を自前で判定したいときに使う。
#[napi]
pub struct Vad {
    inner: CoreVad,
}

#[napi]
impl Vad {
    /// 設定オブジェクトから VAD を構築する（埋め込みモデルをロードする）。設定が不正
    /// （sampleRate が 8000/16000 以外、threshold が `[0,1]` 外など）なら InvalidArg、
    /// モデルロード失敗なら GenericFailure。
    #[napi(constructor)]
    pub fn new(options: VadOptions) -> napi::Result<Self> {
        let inner = CoreVad::new(build_vad_config(&options)).map_err(vad_err)?;
        Ok(Vad { inner })
    }

    /// 任意フォーマットの interleaved f32 を処理し、確定した [`JsVadEvent`] を返す。
    ///
    /// 端数フレームは内部に持ち越すので任意の位置で分割して渡してよい。`atSample` は
    /// VAD 内部レート基準（[`JsVadEvent`] を参照）。
    #[napi]
    pub fn process(
        &mut self,
        samples: Float32Array,
        input_sample_rate: u32,
        input_channels: u16,
    ) -> Vec<JsVadEvent> {
        self.inner
            .process_pcm(&samples[..], input_sample_rate, input_channels)
            .into_iter()
            .map(vad_event_to_js)
            .collect()
    }

    /// 内部状態（state / context / 状態機械 / サンプル位置 / リサンプラ）を初期化する。
    #[napi]
    pub fn reset(&mut self) {
        self.inner.reset();
    }
}

// ---------------------------------------------------------------------------
// 独立アドオン 2: FlacEncoder（逐次 FLAC 書き出し + 秒数ローテーション）
// ---------------------------------------------------------------------------

/// 録音チャンクを逐次 FLAC ファイルへ可逆圧縮保存するライター。
///
/// `splitSeconds` を 1 以上にすると、書き込みフレーム数が `splitSeconds × sampleRate` に
/// 達するたびに現在のファイルを閉じ、`name-001.flac, name-002.flac, …` と 3 桁連番で
/// 次ファイルへローテーションする（CLI の WAV 分割と同じ流儀）。境界はチャンク粒度の
/// 「以上で次へ」なので、各ファイルは指定秒より最大 1 チャンク長くなりうるが、チャンクは
/// 分割されず取りこぼしも無い。`splitSeconds` 省略/0 なら単一ファイル。
#[napi]
pub struct FlacEncoder {
    /// 出力ベースパス（分割時は連番の元、単一時はこのまま使う）。
    base: PathBuf,
    sample_rate: u32,
    channels: u16,
    /// 1 ファイルあたりのフレーム数しきい値（splitSeconds × sampleRate）。0 = 単一。
    frames_per_file: u64,
    /// 現在書き込み中のライター。ローテーション直後は None（次チャンクで遅延生成）。
    writer: Option<FlacWriter>,
    /// 現在のファイルへ書いたフレーム数（ローテーションで 0 に戻る）。
    frames_in_current: u64,
    /// 次に開くファイルの連番（1 始まり・分割時のみ意味を持つ）。
    file_index: u64,
}

#[napi]
impl FlacEncoder {
    /// FLAC ライターを作る。`splitSeconds` 省略/0 で単一ファイル、1 以上で秒数ローテ。
    ///
    /// `channels` は 1..=2、`sampleRate` は 1..=96000 Hz（範囲外は InvalidArg）。分割時は
    /// 最初のファイル（`name-001.flac`）を即作成する。
    #[napi(factory)]
    pub fn create(
        path: String,
        sample_rate: u32,
        channels: u16,
        split_seconds: Option<u32>,
    ) -> napi::Result<FlacEncoder> {
        let base = PathBuf::from(path);
        let frames_per_file = u64::from(split_seconds.unwrap_or(0)) * u64::from(sample_rate);
        let file_index = 1;
        // 単一なら base、分割なら name-001.ext を最初のファイルとして開く。
        let first_path = if frames_per_file > 0 {
            split_flac_path(&base, file_index)
        } else {
            base.clone()
        };
        let writer = FlacWriter::create(&first_path, sample_rate, channels).map_err(encode_err)?;
        Ok(FlacEncoder {
            base,
            sample_rate,
            channels,
            frames_per_file,
            writer: Some(writer),
            frames_in_current: 0,
            file_index,
        })
    }

    /// 分割時に次に開くファイルのパス。
    fn next_path(&self) -> PathBuf {
        if self.frames_per_file > 0 {
            split_flac_path(&self.base, self.file_index)
        } else {
            self.base.clone()
        }
    }

    /// interleaved f32（長さは `channels` の倍数）を追記する。倍数でなければ InvalidArg。
    ///
    /// 書き込み後、現在のファイルのフレーム数がしきい値以上なら即 finalize して次ファイルへ
    /// ローテーションする（次の `writeChunk` が新ファイルの先頭になる）。
    #[napi]
    pub fn write_chunk(&mut self, samples: Float32Array) -> napi::Result<()> {
        // ローテーション直後は writer=None。次ファイルをここで開く（遅延生成）。
        if self.writer.is_none() {
            let path = self.next_path();
            self.writer = Some(
                FlacWriter::create(&path, self.sample_rate, self.channels).map_err(encode_err)?,
            );
        }
        let writer = self.writer.as_mut().expect("直前で開いている");
        writer.write_chunk(&samples[..]).map_err(encode_err)?;

        // フレーム数 = サンプル数 / チャンネル数。write_chunk が倍数を検証済みで割り切れる。
        let frames = samples.len() as u64 / u64::from(self.channels);
        self.frames_in_current += frames;

        if self.frames_per_file > 0 && self.frames_in_current >= self.frames_per_file {
            // しきい値到達。現ファイルを確定し、次チャンクから次ファイルへ。
            let done = self.writer.take().expect("直前で書いた");
            done.finalize().map_err(encode_err)?;
            self.file_index += 1;
            self.frames_in_current = 0;
        }
        Ok(())
    }

    /// 端数を書き切ってヘッダを確定し、開いているファイルを閉じる。二重呼び出し安全
    /// （2 回目以降は no-op）。呼ばずに捨てても `FlacWriter` の Drop がベストエフォートで
    /// 閉じるが、書き込みエラーを検知したいならこれを呼ぶこと。
    #[napi]
    pub fn finalize(&mut self) -> napi::Result<()> {
        if let Some(writer) = self.writer.take() {
            writer.finalize().map_err(encode_err)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 独立アドオン 3: Denoiser（RNNoise によるオフラインノイズ抑制）
// ---------------------------------------------------------------------------

/// オフラインのノイズ抑制器（RNNoise via nnnoiseless、重み埋め込み）。**48kHz 前提**で、
/// マイク録音の定常ノイズ（ファン・空調・打鍵など）の低減を想定する。
///
/// [`FRAME_SIZE`](flexaudio_denoise::FRAME_SIZE)（48kHz で 10ms）固定の遅延があり、出力は
/// 入力を 1 フレーム分遅らせた列になる。ストリーム先頭の 1 フレームは無音の詰め物で、
/// 末尾に残る 1 フレーム分は [`Denoiser::flush`] で取り出す。
#[napi]
pub struct Denoiser {
    inner: CoreDenoiser,
}

#[napi]
impl Denoiser {
    /// チャンネル数（1 = mono, 2 = stereo interleaved）を指定して構築する。範囲外は
    /// InvalidArg。
    #[napi(constructor)]
    pub fn new(channels: u16) -> napi::Result<Self> {
        let inner = CoreDenoiser::new(channels).map_err(denoise_err)?;
        Ok(Denoiser { inner })
    }

    /// 任意長の interleaved f32（±1.0 正規化・48kHz・長さは channels の倍数）を
    /// ノイズ抑制して**新しい配列**で返す（napi ではインプレースが扱いにくいのでコピー）。
    /// 長さが channels の倍数でなければ InvalidArg。
    #[napi]
    pub fn process(&mut self, samples: Float32Array) -> napi::Result<Float32Array> {
        let mut buf = samples.to_vec();
        self.inner.process(&mut buf).map_err(denoise_err)?;
        Ok(Float32Array::new(buf))
    }

    /// 持ち越し中の端数を処理して末尾の遅延分（1 フレーム/ch）を返し、ストリームを閉じる。
    /// 呼び出し後は生成直後と同じ状態に戻り、続けて別ストリームを処理できる。
    #[napi]
    pub fn flush(&mut self) -> Float32Array {
        Float32Array::new(self.inner.flush())
    }

    /// RNN 状態・持ち越し・遅延線をすべて初期化する。
    #[napi]
    pub fn reset(&mut self) {
        self.inner.reset();
    }
}

#[cfg(test)]
mod tests {
    //! marshalling の純粋部分を JS ランタイム無しで検証する。
    //!
    //! ここで見るのは「Rust 値 → JS 向け中間表現」の純粋変換だけ:
    //! - `parse_source_kind` / `source_kind_str`（往復）
    //! - `parse_process_mode`（既定/明示/未知）
    //! - `build_config`（OpenOptions → StreamConfig の既定・反映）
    //! - `to_napi_err`（flexaudio::Error → napi 文字列・Status）
    //! - `event_to_js` / `device_event_to_js`（種別文字列・payload）
    //! - `chunk_to_js`（seq u64 → BigInt・data・frames・peak/rms）
    //!
    //! `Float32Array::new(Vec)` と `BigInt::from(u64)` は純 Rust フィールドへ値を入れ、
    //! `Deref<[f32]>` / `get_u64()` で JS ランタイム無しに読み戻せる（napi 2.16）。

    use super::*;

    // --- source kind 往復 ---

    #[test]
    fn source_kind_roundtrips() {
        for (s, k) in [
            ("mic", SourceKind::Mic),
            ("system", SourceKind::SystemLoopback),
            ("process", SourceKind::ProcessLoopback),
            ("mix", SourceKind::Mix),
        ] {
            assert_eq!(parse_source_kind(s).unwrap(), k);
            assert_eq!(source_kind_str(k), s);
        }
    }

    #[test]
    fn parse_source_kind_rejects_unknown() {
        let err = parse_source_kind("bogus").unwrap_err();
        assert_eq!(err.status, Status::InvalidArg);
    }

    // --- process mode ---

    #[test]
    fn parse_process_mode_defaults_and_explicit() {
        // None / "include" は既定 Include。
        assert_eq!(parse_process_mode(None).unwrap(), ProcessMode::Include);
        assert_eq!(
            parse_process_mode(Some("include")).unwrap(),
            ProcessMode::Include
        );
        // "exclude" は Exclude。
        assert_eq!(
            parse_process_mode(Some("exclude")).unwrap(),
            ProcessMode::Exclude
        );
    }

    #[test]
    fn parse_process_mode_rejects_unknown() {
        let err = parse_process_mode(Some("nope")).unwrap_err();
        assert_eq!(err.status, Status::InvalidArg);
    }

    // --- build_config ---

    /// OpenOptions を全フィールド未指定（kind のみ）で作るヘルパ。
    fn options_with_kind(kind: &str) -> OpenOptions {
        OpenOptions {
            kind: kind.to_string(),
            device_id: None,
            process_id: None,
            mode: None,
            exclude_self: None,
            output_rate: None,
            output_channels: None,
            chunk_ms: None,
            gain: None,
            mic_device_id: None,
            system_device_id: None,
            mic_gain: None,
            system_gain: None,
            vad: None,
            denoise: None,
        }
    }

    #[test]
    fn build_config_defaults() {
        let opts = options_with_kind("mic");
        let cfg = build_config(&opts).unwrap();
        assert_eq!(cfg.kind, SourceKind::Mic);
        // 既定 output {48000, 2}。
        assert_eq!(cfg.output.sample_rate, 48_000);
        assert_eq!(cfg.output.channels, 2);
        assert_eq!(cfg.mode, ProcessMode::Include);
        assert!(!cfg.exclude_self);
        assert_eq!(cfg.target_pid, None);
        assert_eq!(cfg.device_id, None);
        // chunk_ms 未指定なら StreamConfig 既定（20）。
        assert_eq!(cfg.chunk_ms, 20);
        // gain 未指定なら既定 1.0。
        assert_eq!(cfg.gain, 1.0);
        // mix 専用フィールドの既定（デバイス未指定・側別ゲイン 1.0）。
        assert_eq!(cfg.mix_mic_device_id, None);
        assert_eq!(cfg.mix_system_device_id, None);
        assert_eq!(cfg.mix_mic_gain, 1.0);
        assert_eq!(cfg.mix_system_gain, 1.0);
    }

    #[test]
    fn build_config_reflects_all_fields() {
        let opts = OpenOptions {
            kind: "process".to_string(),
            device_id: Some("dev-x".to_string()),
            process_id: Some(9999),
            mode: Some("exclude".to_string()),
            exclude_self: Some(true),
            output_rate: Some(16_000),
            output_channels: Some(1),
            chunk_ms: Some(20),
            gain: Some(2.5),
            mic_device_id: None,
            system_device_id: None,
            mic_gain: None,
            system_gain: None,
            vad: None,
            denoise: None,
        };
        let cfg = build_config(&opts).unwrap();
        assert_eq!(cfg.kind, SourceKind::ProcessLoopback);
        assert_eq!(cfg.device_id.as_deref(), Some("dev-x"));
        assert_eq!(cfg.target_pid, Some(9999));
        assert_eq!(cfg.mode, ProcessMode::Exclude);
        assert!(cfg.exclude_self);
        assert_eq!(cfg.output.sample_rate, 16_000);
        assert_eq!(cfg.output.channels, 1);
        assert_eq!(cfg.chunk_ms, 20);
        assert_eq!(cfg.gain, 2.5);
    }

    #[test]
    fn build_config_reflects_mix_fields() {
        let mut opts = options_with_kind("mix");
        opts.mic_device_id = Some("mic-a".to_string());
        opts.system_device_id = Some("sink-b".to_string());
        opts.mic_gain = Some(0.5);
        opts.system_gain = Some(2.0);
        let cfg = build_config(&opts).unwrap();
        assert_eq!(cfg.kind, SourceKind::Mix);
        assert_eq!(cfg.mix_mic_device_id.as_deref(), Some("mic-a"));
        assert_eq!(cfg.mix_system_device_id.as_deref(), Some("sink-b"));
        assert_eq!(cfg.mix_mic_gain, 0.5);
        assert_eq!(cfg.mix_system_gain, 2.0);
    }

    #[test]
    fn build_config_rejects_unknown_kind() {
        let opts = options_with_kind("speaker");
        let err = build_config(&opts).unwrap_err();
        assert_eq!(err.status, Status::InvalidArg);
    }

    // --- to_napi_err ---

    #[test]
    fn to_napi_err_carries_message_and_status() {
        let err = to_napi_err(flexaudio::Error::DeviceNotFound);
        assert_eq!(err.status, Status::GenericFailure);
        // Display 文字列が reason に入る。
        assert_eq!(err.reason, flexaudio::Error::DeviceNotFound.to_string());
        assert!(err.reason.contains("device not found"));
    }

    // --- event_to_js ---

    #[test]
    fn event_to_js_maps_each_variant() {
        let dropped = event_to_js(Event::ChunkDropped { count: 7 });
        assert_eq!(dropped.kind, "chunkDropped");
        assert_eq!(dropped.count, Some(7));
        assert_eq!(dropped.message, None);

        assert_eq!(event_to_js(Event::StreamStalled).kind, "stalled");
        assert_eq!(event_to_js(Event::StreamRecovered).kind, "recovered");
        assert_eq!(
            event_to_js(Event::PermissionDenied).kind,
            "permissionDenied"
        );
        assert_eq!(event_to_js(Event::DeviceLost).kind, "deviceLost");

        let errev = event_to_js(Event::Error("boom".to_string()));
        assert_eq!(errev.kind, "error");
        assert_eq!(errev.message, Some("boom".to_string()));
    }

    // --- device_event_to_js ---

    #[test]
    fn device_event_to_js_maps_variants() {
        let info = DeviceInfo {
            id: "node-1".to_string(),
            name: "Mic A".to_string(),
            source_kind: SourceKind::Mic,
            sample_rate: 48_000,
            channels: 2,
            is_loopback: false,
            is_default: true,
        };
        let added = device_event_to_js(DeviceEvent::Added(info));
        assert_eq!(added.kind, "added");
        let dev = added.device.expect("device present");
        assert_eq!(dev.id, "node-1");
        assert_eq!(dev.source_kind, "mic");
        assert!(dev.is_default);

        let removed = device_event_to_js(DeviceEvent::Removed {
            id: "gone".to_string(),
        });
        assert_eq!(removed.kind, "removed");
        assert_eq!(removed.id.as_deref(), Some("gone"));

        let changed = device_event_to_js(DeviceEvent::DefaultChanged {
            kind: SourceKind::SystemLoopback,
            id: "sink-2".to_string(),
        });
        assert_eq!(changed.kind, "defaultChanged");
        assert_eq!(changed.id.as_deref(), Some("sink-2"));
        assert_eq!(changed.source_kind.as_deref(), Some("system"));
    }

    // seq u64 → BigInt の変換（marshalling の純粋部分）。
    //
    // `chunk_to_js` 全体は `Float32Array` を生成するのでここではテストできない。napi
    // 2.16 の `Float32Array` は `Drop` が `napi_call_threadsafe_function` を無条件参照
    // するため、cdylib のユニットテストバイナリ（Node ホスト不在）ではリンクできず
    // `cargo test -p flexaudio-napi` が壊れる。そこで JS ランタイムに依存しない
    // seq→BigInt 変換だけを同じロジック（`BigInt::from(u64)` + `get_u64`）で見る。
    // data/Float32Array 経路は Node 側の E2E（`__openMockStream`）でカバーする。

    #[test]
    fn seq_u64_to_bigint_is_lossless() {
        // chunk_to_js は `BigInt::from(chunk.seq)` で seq を BigInt 化する。
        // 2^53+1（f64 では表せない大きさ）でも無損失で往復することを確認する。
        let seq: u64 = 9_007_199_254_740_993; // 2^53 + 1。
        let big = BigInt::from(seq);
        let (sign, value, lossless) = big.get_u64();
        assert!(!sign, "seq は非負");
        assert_eq!(value, seq, "seq 値が無損失で保持される（f64 では落ちる桁）");
        assert!(lossless, "u64 1 ワードなので lossless");

        // u64::MAX 境界でも無損失。
        let (_, max_val, max_lossless) = BigInt::from(u64::MAX).get_u64();
        assert_eq!(max_val, u64::MAX);
        assert!(max_lossless);
    }

    #[test]
    fn device_info_to_js_maps_all_fields() {
        let info = DeviceInfo {
            id: "id-x".to_string(),
            name: "Name X".to_string(),
            source_kind: SourceKind::SystemLoopback,
            sample_rate: 44_100,
            channels: 1,
            is_loopback: true,
            is_default: false,
        };
        let js = device_info_to_js(info);
        assert_eq!(js.id, "id-x");
        assert_eq!(js.name, "Name X");
        assert_eq!(js.source_kind, "system");
        assert_eq!(js.sample_rate, 44_100);
        assert_eq!(js.channels, 1);
        assert!(js.is_loopback);
        assert!(!js.is_default);
    }

    // --- 統合オプション: OpenOptions に vad/denoise が乗る ---

    #[test]
    fn open_options_carries_vad_and_denoise() {
        // vad/denoise は StreamConfig ではなく open_stream 側で解釈するので、build_config は
        // これらに影響されず通ること（＝録音本体の設定と直交している）を確認する。
        let mut opts = options_with_kind("mic");
        opts.denoise = Some(true);
        opts.vad = Some(VadOptions {
            threshold: Some(0.4),
            neg_threshold: None,
            min_speech_ms: None,
            min_silence_ms: None,
            speech_pad_ms: None,
            max_speech_ms: None,
            sample_rate: None,
        });
        let cfg = build_config(&opts).unwrap();
        assert_eq!(cfg.kind, SourceKind::Mic);
        assert_eq!(cfg.output.sample_rate, 48_000);
    }

    // --- build_vad_config（VadOptions → VadConfig） ---

    /// 全フィールド未指定の VadOptions。
    fn empty_vad_options() -> VadOptions {
        VadOptions {
            threshold: None,
            neg_threshold: None,
            min_speech_ms: None,
            min_silence_ms: None,
            speech_pad_ms: None,
            max_speech_ms: None,
            sample_rate: None,
        }
    }

    #[test]
    fn build_vad_config_defaults_match_silero() {
        let cfg = build_vad_config(&empty_vad_options());
        let d = VadConfig::default();
        assert_eq!(cfg.threshold, d.threshold);
        // 未指定の negThreshold は None のまま（VadConfig 側の既定式が効く）。
        assert_eq!(cfg.neg_threshold, None);
        assert_eq!(cfg.min_speech_ms, d.min_speech_ms);
        assert_eq!(cfg.min_silence_ms, d.min_silence_ms);
        assert_eq!(cfg.speech_pad_ms, d.speech_pad_ms);
        assert_eq!(cfg.max_speech_ms, d.max_speech_ms);
        assert_eq!(cfg.sample_rate, d.sample_rate);
    }

    #[test]
    fn build_vad_config_reflects_all_fields() {
        let opts = VadOptions {
            threshold: Some(0.7),
            neg_threshold: Some(0.2),
            min_speech_ms: Some(120),
            min_silence_ms: Some(200),
            speech_pad_ms: Some(40),
            max_speech_ms: Some(5000),
            sample_rate: Some(8000),
        };
        let cfg = build_vad_config(&opts);
        assert_eq!(cfg.threshold, 0.7);
        assert_eq!(cfg.neg_threshold, Some(0.2));
        assert_eq!(cfg.min_speech_ms, 120);
        assert_eq!(cfg.min_silence_ms, 200);
        assert_eq!(cfg.speech_pad_ms, 40);
        assert_eq!(cfg.max_speech_ms, 5000);
        assert_eq!(cfg.sample_rate, 8000);
    }

    // --- check_denoise_rate（denoise の 48kHz 前提） ---

    #[test]
    fn denoise_requires_48k() {
        // 有効 + 48000 は OK。
        assert!(check_denoise_rate(true, 48_000).is_ok());
        // 有効 + 48000 以外は InvalidArg。
        let err = check_denoise_rate(true, 16_000).unwrap_err();
        assert_eq!(err.status, Status::InvalidArg);
        // 無効ならレートに関係なく OK（検証しない）。
        assert!(check_denoise_rate(false, 16_000).is_ok());
        assert!(check_denoise_rate(false, 48_000).is_ok());
    }

    // --- split_flac_path（連番命名・CLI と同じ流儀） ---

    #[test]
    fn split_flac_path_numbering() {
        // 拡張子ありは拡張子の前へ 3 桁ゼロ詰め連番。
        assert_eq!(
            split_flac_path(Path::new("rec.flac"), 1),
            PathBuf::from("rec-001.flac")
        );
        assert_eq!(
            split_flac_path(Path::new("rec.flac"), 12),
            PathBuf::from("rec-012.flac")
        );
        // 1000 以降は桁が自然に増える。
        assert_eq!(
            split_flac_path(Path::new("rec.flac"), 1000),
            PathBuf::from("rec-1000.flac")
        );
        // 拡張子なしは末尾に連番。
        assert_eq!(
            split_flac_path(Path::new("rec"), 3),
            PathBuf::from("rec-003")
        );
        // 親ディレクトリは保たれる。
        assert_eq!(
            split_flac_path(Path::new("/tmp/out/meeting.flac"), 2),
            PathBuf::from("/tmp/out/meeting-002.flac")
        );
    }

    // --- 各アドオンのエラー写像 ---

    #[test]
    fn error_mappers_carry_status() {
        // denoise: チャンネル不正は InvalidArg。
        let e = denoise_err(DenoiseError::InvalidChannels(3));
        assert_eq!(e.status, Status::InvalidArg);
        // encode: 非対応パラメータは InvalidArg。
        let e = encode_err(EncodeError::Unsupported("bad".to_string()));
        assert_eq!(e.status, Status::InvalidArg);
        // encode: エンコーダ内部は GenericFailure。
        let e = encode_err(EncodeError::Encoder("boom".to_string()));
        assert_eq!(e.status, Status::GenericFailure);
        // vad: 設定不正は InvalidArg。
        let e = vad_err(VadError::InvalidConfig("nope".to_string()));
        assert_eq!(e.status, Status::InvalidArg);
        // vad: モデルロード失敗は GenericFailure。
        let e = vad_err(VadError::ModelLoad("x".to_string()));
        assert_eq!(e.status, Status::GenericFailure);
    }

    // --- vad_event_to_js（種別文字列・atSample） ---

    #[test]
    fn vad_event_to_js_maps_variants() {
        let start = vad_event_to_js(VadEvent::SpeechStart { at_sample: 512 });
        assert_eq!(start.kind, "speechStart");
        assert_eq!(start.at_sample, 512);
        let end = vad_event_to_js(VadEvent::SpeechEnd { at_sample: 4096 });
        assert_eq!(end.kind, "speechEnd");
        assert_eq!(end.at_sample, 4096);
    }
}
