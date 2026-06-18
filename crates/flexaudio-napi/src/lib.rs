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

// ErrorStrategy::Fatal の TSFN 別名。`.call(value, mode)` が値を直接取れる
// （CalleeHandled だと `.call(Result<T>, mode)` になり Result ラップが要る）。
type ChunkTsfn = ThreadsafeFunction<JsAudioChunk, ErrorStrategy::Fatal>;
type EventTsfn = ThreadsafeFunction<JsStreamEvent, ErrorStrategy::Fatal>;
type DeviceTsfn = ThreadsafeFunction<JsDeviceEvent, ErrorStrategy::Fatal>;

/// flexaudio::Error → napi::Error。メッセージを文字列化して GenericFailure にする。
fn to_napi_err(err: flexaudio::Error) -> NapiError {
    NapiError::new(Status::GenericFailure, err.to_string())
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
    /// process の対象 PID の扱い（process 専用）。"include"（既定）| "exclude"。
    /// include=対象 PID だけ録る / exclude=対象 PID 以外の全システム音（process_id 必須）。
    /// mic / system では無視。Linux では "exclude" は未実装で start 時に例外。
    pub mode: Option<String>,
    /// システム音から自ホスト（自プロセス）の音を除くか（system 専用）。既定 false。
    /// mic / process では無視。Linux では true は未実装で start 時に例外。
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

/// "include" | "exclude" を [`ProcessMode`] へ（process 専用）。`None`/未指定は既定 Include。
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
        // Vec<f32> を Float32Array 化（所有権をスレッド側に残さない）。
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
        // mode は process 専用 / exclude_self は system 専用。混ぜないのは facade 側が見る。
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

/// bridge スレッドへソース切替を依頼するコマンド。
///
/// Stream は bridge スレッドが所有しているので `switch_source` を直接呼べない。JS から
/// 来た切替要求をこのコマンドで bridge スレッドへ送り、`result_tx` で結果を同期的に
/// 受け取る（JS 側は同期返却を期待する）。
struct SwitchCmd {
    config: StreamConfig,
    result_tx: mpsc::Sender<std::result::Result<(), String>>,
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
    fn spawn(
        mut stream: flexaudio::Stream,
        on_chunk: ChunkTsfn,
        on_event: Option<EventTsfn>,
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
                    }
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
        // 切替チャネルを閉じて bridge スレッドの try_recv を Disconnected にする。
        self.cmd_tx = None;
        if let Some(h) = self.handle.take() {
            // 二重 stop / Drop でも安全（handle が take 済みなら何もしない）。
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
    /// `options` から構築した `StreamConfig` への切替を bridge スレッドへ依頼し、結果を
    /// 同期的に返す（成功で `Ok`、失敗で例外）。出力フォーマット（`outputRate`/
    /// `outputChannels`）は切替では変えられない（連続ストリームの frames が変わるため）。
    /// 変更を要求すると `switch_source` が InvalidArg を返し、ここで例外になる。切替前後で
    /// チャンクの `seq` は連続し、切替後最初のチャンクには DISCONTINUITY フラグが立つ。
    ///
    /// 既に `stop()` 済み（bridge スレッド停止後）なら例外を返す。
    #[napi]
    pub fn switch_source(&self, options: OpenOptions) -> NapiResult<()> {
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
    pub fn pause(&self) -> NapiResult<()> {
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
    pub fn resume(&self) -> NapiResult<()> {
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

    #[test]
    fn build_config_defaults() {
        let opts = OpenOptions {
            kind: "mic".to_string(),
            device_id: None,
            process_id: None,
            mode: None,
            exclude_self: None,
            output_rate: None,
            output_channels: None,
            chunk_ms: None,
        };
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
    }

    #[test]
    fn build_config_rejects_unknown_kind() {
        let opts = OpenOptions {
            kind: "speaker".to_string(),
            device_id: None,
            process_id: None,
            mode: None,
            exclude_self: None,
            output_rate: None,
            output_channels: None,
            chunk_ms: None,
        };
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
}
