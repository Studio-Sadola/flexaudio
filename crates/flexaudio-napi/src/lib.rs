//! flexaudio-napi — Node.js (N-API) addon。
//!
//! WhisperApp(TS/Electron) が flexaudio をインプロセスで使う第二経路。
//! 第一は CLI パイプ。低レイテンシのストリーミング録音を Node へ届ける。
//!
//! 設計:
//! - 公開関数は camelCase（`#[napi]` が JS 名へ変換）。
//! - チャンク/イベントは `ThreadsafeFunction`（ErrorStrategy::Fatal）で JS コールバックへ送る。
//! - `FlexStream` 構築時に bridge スレッドを spawn し、`stream.start()` 後に
//!   `poll_chunk` / `poll_event` を 1ms 間隔でポーリングして TSFN へ NonBlocking で渡す。
//! - 停止は `Arc<AtomicBool>` のフラグ + `JoinHandle::join()`。Drop でも確実に止める。
//!
//! 実行時はネットワーク通信を一切行わない（napi は N-API ブリッジのみ）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use napi::bindgen_prelude::{BigInt, Float32Array};
use napi::threadsafe_function::{ErrorStrategy, ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi::{Error as NapiError, Result as NapiResult, Status};
use napi_derive::napi;

use flexaudio::{
    AudioChunk, DeviceEvent, DeviceInfo, Event, OutputFormat, ProcessMode, SourceKind, StreamConfig,
};

// bridge スレッドのポーリング間隔。20ms チャンクに対し十分小さく、空転も避ける。
const POLL_INTERVAL: Duration = Duration::from_millis(1);
// デバイス着脱は低頻度。応答性 100ms で十分。
const DEVICE_POLL_INTERVAL: Duration = Duration::from_millis(100);

// ErrorStrategy::Fatal を使う TSFN 別名。`.call(value, mode)` が値を直接取れる
// （CalleeHandled だと `.call(Result<T>, mode)` になり Result ラップが必要）。
type ChunkTsfn = ThreadsafeFunction<JsAudioChunk, ErrorStrategy::Fatal>;
type EventTsfn = ThreadsafeFunction<JsStreamEvent, ErrorStrategy::Fatal>;
type DeviceTsfn = ThreadsafeFunction<JsDeviceEvent, ErrorStrategy::Fatal>;

/// flexaudio::Error → napi::Error。メッセージを文字列化して GenericFailure にする。
fn to_napi_err(err: flexaudio::Error) -> NapiError {
    NapiError::new(Status::GenericFailure, err.to_string())
}

// ---------------------------------------------------------------------------
// JS 向けデータ型（`#[napi(object)]` = プレーンオブジェクトとして JS と相互変換）
// ---------------------------------------------------------------------------

/// JS 側 DeviceInfo。`sourceKind` は文字列化（"mic"|"system"|"process"）。
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
    /// "mic" | "system" | "process"
    pub kind: String,
    pub device_id: Option<String>,
    pub process_id: Option<u32>,
    /// process の対象 PID の扱い（①・process 専用）。"include"（既定）| "exclude"。
    /// include=対象 PID だけ録る / exclude=対象 PID 以外の全システム音（process_id 必須）。
    /// mic / system では無視される。Linux では "exclude" は未実装で start 時に例外。
    pub mode: Option<String>,
    /// システム音から自ホスト（自プロセス）の音を除外するか（②・system 専用）。既定 false。
    /// mic / process では無視される。Linux では true は未実装で start 時に例外。
    pub exclude_self: Option<bool>,
    /// 既定 48000
    pub output_rate: Option<u32>,
    /// 既定 2
    pub output_channels: Option<u16>,
    /// 既定 20
    pub chunk_ms: Option<u32>,
}

// ---------------------------------------------------------------------------
// 変換ヘルパ
// ---------------------------------------------------------------------------

fn source_kind_str(k: SourceKind) -> String {
    match k {
        SourceKind::Mic => "mic",
        SourceKind::SystemLoopback => "system",
        SourceKind::ProcessLoopback => "process",
    }
    .to_string()
}

fn parse_source_kind(s: &str) -> NapiResult<SourceKind> {
    match s {
        "mic" => Ok(SourceKind::Mic),
        "system" => Ok(SourceKind::SystemLoopback),
        "process" => Ok(SourceKind::ProcessLoopback),
        other => Err(NapiError::new(
            Status::InvalidArg,
            format!("unknown kind: {other:?} (expected mic|system|process)"),
        )),
    }
}

/// "include" | "exclude" を [`ProcessMode`] へ（①・process 専用）。`None`/未指定は既定 Include。
fn parse_process_mode(s: Option<&str>) -> NapiResult<ProcessMode> {
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
        // Vec<f32> をコピーして Float32Array 化（所有権はスレッド側に無い形にする）。
        data: Float32Array::new(chunk.data),
        frames,
        pts_ns: chunk.pts_ns,
        seq: BigInt::from(chunk.seq),
        flags: chunk.flags.bits(),
        dropped_before: chunk.dropped_before,
        peak: chunk.peak as f64,
        rms: chunk.rms as f64,
    }
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
        // Event は #[non_exhaustive]。将来バリアント追加に備えた前方互換アーム
        // （未知種別は "error" + デバッグ表現で JS へ通知し、握り潰さない）。
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
        // DeviceEvent は #[non_exhaustive]。将来バリアント追加に備えた前方互換アーム
        // （未知種別は "unknown" として JS へ渡し、握り潰さない）。
        _ => JsDeviceEvent {
            kind: "unknown".to_string(),
            device: None,
            id: None,
            source_kind: None,
        },
    }
}

fn build_config(options: &OpenOptions) -> NapiResult<StreamConfig> {
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
        // mode（①・process 専用）/ exclude_self（②・system 専用）。非合成は facade が担保。
        mode,
        exclude_self: options.exclude_self.unwrap_or(false),
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

/// bridge スレッドへ「ソース切替」を依頼するコマンド。
///
/// Stream は bridge スレッドが所有しているため、`switch_source` を直接呼べない。
/// JS から来た切替要求をこのコマンドで bridge スレッドへ送り、`result_tx` で結果を
/// 同期的に受け取る（JS 側は同期返却を期待する）。
struct SwitchCmd {
    config: StreamConfig,
    result_tx: mpsc::Sender<std::result::Result<(), String>>,
}

/// 録音ストリームのハンドル。内部で bridge スレッドが `flexaudio::Stream` を
/// 所有・ポーリングし、チャンク/イベントを TSFN 経由で JS へ送る。
#[napi]
pub struct FlexStream {
    stop_flag: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    /// bridge スレッドへ切替コマンドを送るチャネル。`shutdown` で drop してスレッド
    /// 側の `try_recv` を打ち切る（停止は stop_flag が担うので必須ではないが明示）。
    cmd_tx: Option<mpsc::Sender<SwitchCmd>>,
}

impl FlexStream {
    /// 既に `start()` 済みの Stream を受け取り、bridge スレッドを spawn する。
    /// Stream は Send なのでスレッドへ move する（poll_chunk は &mut self ＝所有はスレッド側）。
    fn spawn(
        mut stream: flexaudio::Stream,
        on_chunk: ChunkTsfn,
        on_event: Option<EventTsfn>,
    ) -> Self {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let thread_stop = stop_flag.clone();
        let (cmd_tx, cmd_rx) = mpsc::channel::<SwitchCmd>();

        let handle = thread::spawn(move || {
            loop {
                if thread_stop.load(Ordering::SeqCst) {
                    break;
                }
                // 切替コマンドを poll と同じ周回で処理（到着分をまとめて）。
                while let Ok(cmd) = cmd_rx.try_recv() {
                    let r = stream.switch_source(cmd.config).map_err(|e| e.to_string());
                    // 受け手（switch_source 呼び出し元）が drop していても無視。
                    let _ = cmd.result_tx.send(r);
                }
                // チャンクは到着し次第すべて吐く。
                while let Some(chunk) = stream.poll_chunk() {
                    on_chunk.call(chunk_to_js(chunk), ThreadsafeFunctionCallMode::NonBlocking);
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
                on_chunk.call(chunk_to_js(chunk), ThreadsafeFunctionCallMode::NonBlocking);
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
        // 切替チャネルを閉じる（bridge スレッドの try_recv を Disconnected にする）。
        self.cmd_tx = None;
        if let Some(h) = self.handle.take() {
            // 二重 stop / Drop でも安全（handle は take 済みなら何もしない）。
            let _ = h.join();
        }
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
    /// `options` から構築した `StreamConfig` への切替を bridge スレッドへ依頼し、
    /// 結果を**同期的に**返す（成功で `Ok`、失敗で例外）。出力フォーマット
    /// （`outputRate`/`outputChannels`）は切替で変更不可（連続ストリームの frames が
    /// 変わるため）。変更を要求すると `switch_source` が InvalidArg を返し、ここで
    /// 例外になる。切替前後でチャンクの `seq` は連続し、切替後最初のチャンクには
    /// DISCONTINUITY フラグが立つ。
    ///
    /// 既に `stop()` 済み（bridge スレッド停止後）なら例外を返す。
    #[napi]
    pub fn switch_source(&self, options: OpenOptions) -> NapiResult<()> {
        // openStream と同じく build_config で options → StreamConfig（DRY）。
        let config = build_config(&options)?;

        // bridge スレッドへコマンドを送り、結果を同期受信する。
        let cmd_tx = self.cmd_tx.as_ref().ok_or_else(|| {
            NapiError::new(Status::GenericFailure, "stream already stopped".to_string())
        })?;
        let (result_tx, result_rx) = mpsc::channel();
        cmd_tx.send(SwitchCmd { config, result_tx }).map_err(|_| {
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

/// 利用可能なデバイスを列挙する。homelab では空配列でも throw しない。
#[napi]
pub fn devices() -> NapiResult<Vec<JsDeviceInfo>> {
    let list = flexaudio::devices().map_err(to_napi_err)?;
    Ok(list.into_iter().map(device_info_to_js).collect())
}

/// ストリームを開いて開始し、チャンク/イベントをコールバックへ送る `FlexStream` を返す。
#[napi]
pub fn open_stream(
    options: OpenOptions,
    on_chunk: ChunkTsfn,
    on_event: Option<EventTsfn>,
) -> NapiResult<FlexStream> {
    let config = build_config(&options)?;
    let mut stream = flexaudio::open(config).map_err(to_napi_err)?;
    stream.start().map_err(to_napi_err)?;
    Ok(FlexStream::spawn(stream, on_chunk, on_event))
}

/// デバイス着脱を監視し、イベントをコールバックへ送る `DeviceWatcherHandle` を返す。
#[napi]
pub fn watch_devices(on_event: DeviceTsfn) -> NapiResult<DeviceWatcherHandle> {
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
/// 同一の bridge / TSFN 経路で回す。実音なしで marshaling 全経路（Float32Array・BigInt・
/// peak/rms・frames）を end-to-end 検証するためのもの。本番コードからは使わないこと。
///
/// JS 名は `__openMockStream`（先頭 `__` で公開 API 外であることを示す。napi の既定変換は
/// 先頭アンダースコアを落として `openMockStream` にしてしまうため `js_name` で明示固定する）。
#[napi(js_name = "__openMockStream")]
pub fn open_mock_stream(
    sample_rate: u32,
    channels: u16,
    freq_hz: f64,
    on_chunk: ChunkTsfn,
) -> NapiResult<FlexStream> {
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
    Ok(FlexStream::spawn(stream, on_chunk, None))
}
