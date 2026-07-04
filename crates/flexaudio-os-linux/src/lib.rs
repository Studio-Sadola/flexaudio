//! flexaudio-os-linux — Linux バックエンド: PipeWire (`pipewire` 0.10)
//!
//! 「システム音声出力（既定 sink の monitor）」をキャプチャする
//! [`PwSystemBackend`] を提供する。WASAPI ループバックの Linux 相当であり、
//! スピーカーへ流れている音そのものを `Stream/Input/Audio` ストリームの
//! `stream.capture.sink=true` 経由で録る。
//!
//! # `!Send` の扱い
//!
//! PipeWire の `MainLoop` / `Context` / `Core` / `Stream` は `!Send`（生ポインタと
//! thread-local な loop を抱える）。一方 [`CaptureBackend`] は `Send` を要求する。
//! そこで PipeWire の生成・実行・破棄を専用スレッド 1 本に閉じ込め、
//! [`PwSystemBackend`] が持つのは `Send` なものだけ（停止用
//! [`pipewire::channel::Sender`]・[`JoinHandle`]・起動結果受信用の
//! [`std::sync::mpsc`]）にする。`MainLoop` 等はスレッド境界を跨がない。
//!
//! # フォーマット
//!
//! 48000 Hz / 2ch / f32 を要求する。グラフのレート/チャンネルが違っても PipeWire が
//! `audioconvert` を自動挿入して変換するので、コア側でリサンプル/リミックスしなくてよい。
//!
//! # 非 Linux
//!
//! `#![cfg(target_os = "linux")]` により非 Linux では空コンパイルになり、`pipewire`
//! 依存も `Cargo.toml` の `target.'cfg(...linux)'` セクションでしか引かれない。

#![cfg(target_os = "linux")]
#![warn(missing_docs)]

use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use flexaudio_core::backend::{CaptureBackend, RawSink};
use flexaudio_core::clock::monotonic_now_ns;
use flexaudio_core::types::{DeviceEvent, DeviceInfo, Error, ProcessMode, Result, SourceKind};

use pipewire as pw;
use pw::spa;
use pw::{properties::properties, stream::StreamFlags};
use spa::param::format::{MediaSubtype, MediaType};
use spa::param::format_utils;
use spa::pod::Pod;

/// ネイティブサンプルレート（Hz）。48kHz を要求し PipeWire に変換させる。
const NATIVE_RATE: u32 = 48_000;
/// ネイティブチャンネル数。ステレオを要求し PipeWire に変換させる。
const NATIVE_CHANNELS: u16 = 2;

/// 監視キューの上限（イベント数）。消費側が `poll_event` を長く呼ばない、あるいは
/// デバイスが連続着脱するケースで `VecDeque` が際限なく膨らむのを防ぐ。超過時は最古を捨てる。
const MAX_WATCH_EVENTS: usize = 1024;

/// [`enumerate_pw`] の同期待ちループのデッドライン（ミリ秒）。`done` は通常すぐ来るが、
/// 来ない場合に `while !done { run() }` が無限ループ/ハングするのを防ぐ。超過したら
/// 打ち切って収集済み分を返す。
const ENUMERATE_DEADLINE_MS: u128 = 2_000;

/// [`pipewire::init`] をプロセス全体で 1 回だけ呼ぶ。
///
/// `pw::init()` はライブラリ内部のグローバル初期化で、複数のバックエンドスレッド
/// （system / process / watch / enumerate）から並行に呼ばれ得る。多重呼び出しは
/// スレッド競合の懸念があるので [`std::sync::Once`] で 1 回にまとめる。
fn pw_init_once() {
    use std::sync::Once;
    static PW_INIT: Once = Once::new();
    PW_INIT.call_once(|| {
        pw::init();
    });
}

/// PipeWire 経由でシステム音声出力（sink の monitor）をキャプチャする
/// [`CaptureBackend`]。
///
/// 専用スレッド上で PipeWire `MainLoop` + 入力 `Stream` を構築し、`process`
/// コールバックで dequeue した interleaved f32 サンプルを [`RawSink::push`] へ
/// 非ブロッキングに流す。`stream.capture.sink=true` を指定すると、対象は録音デバイス
/// ではなく sink（スピーカー）の monitor、つまりシステム音声出力になる。
///
/// `device_id` が `None` なら既定 sink の monitor、`Some(node.name)` ならその sink の
/// monitor を録る（`target.object` で指定）。指定した sink が無ければ
/// [`start`](CaptureBackend::start) が [`Error::DeviceNotFound`] を返す。
///
/// PipeWire/sink が無い環境（ヘッドレスサーバ等）では panic せず、
/// [`start`](CaptureBackend::start) が [`Error::Backend`] を返す。
///
/// ```no_run
/// use flexaudio_os_linux::PwSystemBackend;
/// use flexaudio_core::backend::CaptureBackend;
///
/// let backend = PwSystemBackend::new(false, None);
/// assert_eq!(backend.native_format(), (48_000, 2));
/// // let mut backend = backend;
/// // backend.start(sink)?;   // PipeWire 不在/動作中 sink 無しなら Err(Backend)
/// // ...
/// // backend.stop();
/// ```
pub struct PwSystemBackend {
    /// 自プロセスの再生音を除外するか（フィードバック防止）。`true` のとき
    /// [`start`](CaptureBackend::start) はプロセス Exclude 機構を流用し、除外 PID =
    /// `std::process::id()` として自分以外の全アプリ出力（`Stream/Output/Audio`）を
    /// fan-in リンクして録る。sink monitor は混合済みで自分だけ引けないので、これが
    /// 自分を除く唯一の手段。`false` なら sink の monitor をそのまま録る。
    exclude_self: bool,
    /// 録る sink を `node.name` で選ぶ。`None` なら既定 sink の monitor。`Some(id)` なら
    /// その sink の monitor を target.object で指定して録る（[`list_devices`] が返す
    /// `DeviceInfo.id` がこの `node.name`）。`exclude_self == true` の fan-in 経路では
    /// 効かない（特定 sink を狙う経路ではないので無視する）。
    device_id: Option<String>,
    /// 起動中フラグ（二重 start ガード／drop 判定用）。`Send`。
    running: Arc<AtomicBool>,
    /// ループスレッドへ停止を伝える送信端。`start` で `Some`。送ると、loop に attach
    /// 済みの受信端コールバックがループスレッド自身から `main_loop.quit()` を呼び、
    /// `run()` を抜ける。
    stop_tx: Option<pw::channel::Sender<Terminate>>,
    /// PipeWire ループスレッドのハンドル。`start` で `Some`。
    handle: Option<JoinHandle<()>>,
}

/// ループスレッドへ送る停止メッセージ（ゼロサイズ）。
struct Terminate;

impl PwSystemBackend {
    /// バックエンドを構築する（この時点では PipeWire へ接続しない）。
    ///
    /// `exclude_self` が `false`（既定）なら sink の monitor をそのまま録る。`true`
    /// なら自分以外の全アプリ出力を fan-in して録る（プロセス Exclude 機構の流用。
    /// 除外 PID = `std::process::id()`）。
    ///
    /// `device_id` で録る sink を `node.name` で選ぶ。`None` なら既定 sink。
    /// `exclude_self == true` のときは無視する（fan-in は特定 sink を狙わない）。
    /// 実際の接続・ストリーム作成は [`start`](CaptureBackend::start) 内で専用
    /// スレッド上で行う。
    pub fn new(exclude_self: bool, device_id: Option<String>) -> Self {
        Self {
            exclude_self,
            device_id,
            running: Arc::new(AtomicBool::new(false)),
            stop_tx: None,
            handle: None,
        }
    }

    /// `exclude_self` フラグ。
    pub fn exclude_self(&self) -> bool {
        self.exclude_self
    }
}

impl Default for PwSystemBackend {
    fn default() -> Self {
        Self::new(false, None)
    }
}

impl CaptureBackend for PwSystemBackend {
    fn native_format(&self) -> (u32, u16) {
        (NATIVE_RATE, NATIVE_CHANNELS)
    }

    fn start(&mut self, sink: RawSink) -> Result<()> {
        // 二重 start に安全（既に動作中なら何もしない）。
        if self.running.load(Ordering::SeqCst) {
            return Ok(());
        }

        // device_id 指定（かつ通常の monitor 経路）なら、その sink が居るか先に確かめる。
        // 居なければ DeviceNotFound。enumerate_pw が Err（デーモン不在等）のときは握らず
        // 通常の setup へ進ませ、接続失敗を Backend として返させる（不在と「sink 無し」を
        // 取り違えないため）。exclude_self の fan-in 経路は特定 sink を狙わないので見ない。
        let device_id = self.device_id.clone();
        if !self.exclude_self {
            if let Some(id) = device_id.as_deref() {
                if let Ok(devs) = enumerate_pw() {
                    let found = devs.iter().any(|d| d.is_loopback && d.id == id);
                    if !found {
                        return Err(Error::DeviceNotFound);
                    }
                }
            }
        }

        // ループスレッドへの停止チャネル（受信端は loop に attach する）。
        let (stop_tx, stop_rx) = pw::channel::channel::<Terminate>();
        // セットアップ成否を start() へ同期返却するチャネル。init→mainloop→context→
        // connect→stream→connect まで成功なら Ok(())、途中失敗なら Err(エラー文字列)。
        let (ready_tx, ready_rx) = mpsc::channel::<std::result::Result<(), String>>();

        let running = self.running.clone();
        running.store(true, Ordering::SeqCst);

        // exclude_self はプロセス Exclude 機構を流用する。除外 PID = std::process::id()
        // として自分以外の全アプリ出力（Stream/Output/Audio）を自キャプチャ入力へ fan-in
        // リンクし、「システム音 − 自プロセスの再生音」を録る。sink monitor は混合済みで、
        // そこから自プロセス分だけ引く OS プリミティブが PipeWire に無いため、自分を除く
        // にはこのアプリ出力 fan-in しかない。exclude_self == false は sink monitor のまま。
        // exclude_self のときは fan-in なので device_id は使わない。
        let exclude_self = self.exclude_self;
        let handle = thread::Builder::new()
            .name(
                if exclude_self {
                    "flexaudio-pw-system-excl"
                } else {
                    "flexaudio-pw-system"
                }
                .into(),
            )
            .spawn(move || {
                if exclude_self {
                    // 自分（std::process::id()）以外を録る Exclude 機構へ委ねる。
                    // 停止/ready チャネルと Terminate は system 経路と共通。
                    run_pw_process_loop(
                        PidSelect::Exclude(std::process::id()),
                        sink,
                        stop_rx,
                        &ready_tx,
                    );
                } else {
                    run_pw_loop(device_id, sink, stop_rx, &ready_tx);
                }
            })
            .map_err(|e| Error::Backend(format!("spawn pipewire thread: {e}")))?;

        // セットアップ結果を待つ。スレッドが ready を送らずに終了した場合
        // （recv エラー）も失敗として扱う。
        match ready_rx.recv() {
            Ok(Ok(())) => {
                // セットアップ成功。停止用の送信端とハンドルを保持。
                self.stop_tx = Some(stop_tx);
                self.handle = Some(handle);
                Ok(())
            }
            Ok(Err(msg)) => {
                // セットアップ失敗（pipewire 不在・sink 無し・connect 失敗等）。
                // スレッドは既に return しているので join して片付ける。
                //
                // 失敗は一律 Error::Backend にする。PipeWire は接続失敗・stream 生成失敗・
                // format ネゴ失敗のどれについても、権限拒否（portal/Flatpak/RTKit 不許可）と
                // 不在（sink/source/session 無し）を型で区別する API を持たない（返るのは
                // errno/汎用文字列で、PermissionDenied と NotFound を分ける HRESULT/OSStatus
                // 相当が無い）。なので macOS/Windows のような型分類はできない。指定 sink の
                // 不在は start の頭で enumerate_pw を見て DeviceNotFound を先に返すので、
                // ここへは来ない。
                running.store(false, Ordering::SeqCst);
                let _ = handle.join();
                Err(Error::Backend(msg))
            }
            Err(_) => {
                // ready を一度も送らずスレッドが消えた（想定外パニック等）。
                running.store(false, Ordering::SeqCst);
                let _ = handle.join();
                Err(Error::Backend(
                    "pipewire setup thread terminated before signaling readiness".into(),
                ))
            }
        }
    }

    fn stop(&mut self) {
        // 二重 stop / 未 start に安全。
        if !self.running.swap(false, Ordering::SeqCst) {
            // running が false → 未起動 or 既に停止済み。念のため残骸を join。
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
            self.stop_tx = None;
            return;
        }

        // ループスレッドへ停止を通知（受信端コールバックが loop.quit() を呼ぶ）。
        // 送信端を drop する前に send。失敗（受信端消失）は無視（既に終わっている）。
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(Terminate);
        }

        // run() を抜けてスレッドが終了するのを待つ。スレッド終了時に Stream→Core→
        // Context→MainLoop が drop 順に破棄される（すべてループスレッド上で）。
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for PwSystemBackend {
    fn drop(&mut self) {
        self.stop();
    }
}

// ============================================================================
// プロセス出力ループバック（特定 PID のアプリ音声を fan-out 複製でキャプチャ）
// ============================================================================

/// PipeWire 経由で特定プロセス（PID）の音声出力をキャプチャする [`CaptureBackend`]。
/// WASAPI の process-loopback（`AUDIOCLIENT_ACTIVATION_PARAMS`）の Linux 相当。
///
/// # link-factory で出力ポート→自入力ポートを明示リンクする
///
/// `stream.connect` の target/`target.object` でノードを指定する方式は実機検証で
/// WirePlumber に無視され、capture が既定ソース＝マイクへ繋がってしまった。なので
/// 自前のキャプチャ stream の入力ポートと対象プロセスの出力ノードのポートを link-factory
/// で明示リンクする（`pw-link out_FL→in_FL / out_FR→in_FR` の API 版）。アプリ→既定
/// sink の本来のリンクはそのまま残る（fan-out）ので、ユーザーのスピーカーは鳴ったまま。
///
/// PID とノードの対応は二段で解決する。PipeWire では PID はノードでなく Client
/// オブジェクトに載り、`pipewire.sec.pid`（`*pw::keys::SEC_PID`）が registry の Client
/// global props に常在する（デーモンがソケット資格情報から付与するので詐称できない。
/// 実機の stock 構成で確認）。ノードは `client.id` で所有 Client を指すだけ。よって
/// 「PID → `pipewire.sec.pid == target_pid` の Client の global id → その id を
/// `client.id` に持つ `Stream/Output/Audio` ノード」と辿る（`resolve_node_pid` 参照）。
///
/// 自前 stream は `stream.connect(Direction::Input, None, ...)` で接続するが
/// `AUTOCONNECT` を付けない（マイクへの自動リンクを防ぎ、明示リンクだけにする）。これで
/// 入力ポート（input_FL/FR）が生成され、リンクされるまでデータは来ない。対象出力ポートと
/// 自入力ポートが揃ったら `core.create_object::<Link>("link-factory", ...)` で
/// `LINK_OUTPUT_NODE/PORT`・`LINK_INPUT_NODE/PORT` を指定してチャンネル対応リンクを張る。
///
/// # `!Send` の扱い
///
/// [`PwSystemBackend`] と同じく専用スレッド 1 本に閉じ込める。`MainLoop`/`Context`/
/// `Core`/`Registry`/`Stream` は `!Send` なので専用スレッド（`flexaudio-pw-process`）に
/// 置き、本体は `Send` なものだけ持つ（停止用 [`pipewire::channel::Sender`]・
/// [`JoinHandle`]・[`AtomicBool`]）。
///
/// # 後から鳴り始める/消えるのは正常系
///
/// 対象 PID のノードがまだ出ていない/後から現れるのは正常系。PipeWire デーモンに接続でき
/// registry を取れたら [`start`](CaptureBackend::start) は成功扱いで待機し、registry の
/// `global` で対象出力ポートと自入力ポートが揃った瞬間に link-factory でリンクする。
/// `global_remove` でターゲット消失を検知したらリンクを drop して再待機する（冪等に
/// 再リンクできる）。PipeWire デーモン不在・registry 取得失敗のときだけ
/// [`Error::Backend`] を即返す（panic しない）。
///
/// # `mode`: Include / Exclude
///
/// - [`ProcessMode::Include`]（既定）: 対象 PID のノードだけ録る（fan-out リンク。代表 1 ノード）。
/// - [`ProcessMode::Exclude`]: 対象 PID 以外の全アプリ出力（`Stream/Output/Audio`）を
///   自キャプチャ入力へ fan-in リンクして録る（Include の述語を反転して多ノード化したもの）。
///   PID が未解決のノードは Client 到着まで保留し、除外プロセスを取り違えないようにする。
///
/// system ソースの `exclude_self` はこのプロセス backend とは無関係。
///
/// ```no_run
/// use flexaudio_os_linux::PwProcessBackend;
/// use flexaudio_core::backend::CaptureBackend;
/// use flexaudio_core::types::ProcessMode;
///
/// let backend = PwProcessBackend::new(12345, ProcessMode::Include);
/// assert_eq!(backend.native_format(), (48_000, 2));
/// // let mut backend = backend;
/// // backend.start(sink)?;  // PipeWire 不在/registry 失敗なら Err(Backend)、
/// //                        // それ以外は成功して待機（Include は対象 PID 出現待ち、
/// //                        // Exclude は対象 PID 以外を順次 fan-in リンク）。
/// // ...
/// // backend.stop();
/// ```
pub struct PwProcessBackend {
    /// キャプチャ対象プロセスの PID。registry の Client オブジェクトの
    /// `pipewire.sec.pid`（`*pw::keys::SEC_PID`）と突合し、その Client を `client.id` で
    /// 指す出力ノードを対象にする（二段照合。[`resolve_node_pid`] 参照）。
    target_pid: u32,
    /// 対象 PID の扱い。[`ProcessMode::Include`] は対象 PID だけ録る。
    /// [`ProcessMode::Exclude`] は対象 PID 以外の全アプリ出力を fan-in して録る。
    mode: ProcessMode,
    /// 起動中フラグ（二重 start ガード／drop 判定用）。`Send`。
    running: Arc<AtomicBool>,
    /// ループスレッドへ停止を伝える送信端。`start` で `Some`。
    /// [`PwSystemBackend`] と同じ [`Terminate`] を使う。
    stop_tx: Option<pw::channel::Sender<Terminate>>,
    /// PipeWire ループスレッドのハンドル。`start` で `Some`。
    handle: Option<JoinHandle<()>>,
}

impl PwProcessBackend {
    /// 対象 PID と `mode` からバックエンドを構築する（この時点では PipeWire へ
    /// 接続しない）。実際の接続・ストリーム作成・link-factory リンクは
    /// [`start`](CaptureBackend::start) 内で専用スレッド上で行う。
    ///
    /// [`ProcessMode::Include`] は対象 PID だけ録る。[`ProcessMode::Exclude`] は
    /// 対象 PID 以外の全アプリ出力を fan-in して録る。
    pub fn new(target_pid: u32, mode: ProcessMode) -> Self {
        Self {
            target_pid,
            mode,
            running: Arc::new(AtomicBool::new(false)),
            stop_tx: None,
            handle: None,
        }
    }

    /// キャプチャ対象の PID。
    pub fn target_pid(&self) -> u32 {
        self.target_pid
    }

    /// `mode`（Include/Exclude）。
    pub fn mode(&self) -> ProcessMode {
        self.mode
    }
}

impl CaptureBackend for PwProcessBackend {
    fn native_format(&self) -> (u32, u16) {
        (NATIVE_RATE, NATIVE_CHANNELS)
    }

    fn start(&mut self, sink: RawSink) -> Result<()> {
        // 二重 start に安全（既に動作中なら何もしない）。
        if self.running.load(Ordering::SeqCst) {
            return Ok(());
        }

        // mode をノード選択述語へ写す。
        // - Include: 対象 PID のノードだけリンク（代表 1 ノード）。
        // - Exclude: 対象 PID 以外の全 Stream/Output/Audio ノードをリンク（fan-in）。
        let select = match self.mode {
            ProcessMode::Include => PidSelect::Include(self.target_pid),
            ProcessMode::Exclude => PidSelect::Exclude(self.target_pid),
        };

        // ループスレッドへの停止チャネル（受信端は loop に attach する）。
        let (stop_tx, stop_rx) = pw::channel::channel::<Terminate>();
        // セットアップ成否を start() へ同期返却するチャネル。ここでの成功は「PipeWire
        // 接続 + registry 取得 + stream 生成 + registry リスナ登録」まで。対象 PID への
        // fan-out リンクは成功条件に含めない（未出現は正常系で、出現時に registry
        // コールバックからリンクする）。
        let (ready_tx, ready_rx) = mpsc::channel::<std::result::Result<(), String>>();

        let running = self.running.clone();
        running.store(true, Ordering::SeqCst);

        let handle = thread::Builder::new()
            .name("flexaudio-pw-process".into())
            .spawn(move || {
                run_pw_process_loop(select, sink, stop_rx, &ready_tx);
            })
            .map_err(|e| Error::Backend(format!("spawn pipewire process thread: {e}")))?;

        // セットアップ結果を待つ。ready を送らずスレッドが終了した場合も失敗扱い。
        match ready_rx.recv() {
            Ok(Ok(())) => {
                // セットアップ成功（接続〜registry リスナ登録まで）。以後は対象 PID
                // 出現までスレッドが待機し、出力ポート/自入力ポートが揃った時点で
                // link-factory リンクを張る。
                self.stop_tx = Some(stop_tx);
                self.handle = Some(handle);
                Ok(())
            }
            Ok(Err(msg)) => {
                // セットアップ失敗（pipewire 不在・connect/registry 失敗等）。一律
                // Error::Backend（理由は PwSystemBackend::start の同箇所参照: PipeWire は
                // 権限拒否/不在を型で区別できない）。対象 PID の不在は正常系の待機であって
                // エラーではない（registry 出現待ち）ので、ここで DeviceNotFound にはしない。
                running.store(false, Ordering::SeqCst);
                let _ = handle.join();
                Err(Error::Backend(msg))
            }
            Err(_) => {
                // ready を一度も送らずスレッドが消えた（想定外パニック等）。
                running.store(false, Ordering::SeqCst);
                let _ = handle.join();
                Err(Error::Backend(
                    "pipewire process setup thread terminated before signaling readiness".into(),
                ))
            }
        }
    }

    fn stop(&mut self) {
        // 二重 stop / 未 start に安全（PwSystemBackend::stop と同型）。
        if !self.running.swap(false, Ordering::SeqCst) {
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
            self.stop_tx = None;
            return;
        }

        // ループスレッドへ停止を通知（受信端コールバックが loop.quit() を呼ぶ）。
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(Terminate);
        }

        // run() を抜けてスレッドが終了するのを待つ。終了時に Stream→Registry→Core→
        // Context→MainLoop が drop 順に破棄される（すべてループスレッド上で）。
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for PwProcessBackend {
    fn drop(&mut self) {
        self.stop();
    }
}

/// プロセスキャプチャ用 PipeWire ループスレッド本体。
///
/// `MainLoop`/`Context`/`Core`/`Registry`/`Stream`（いずれも `!Send`）をこの関数の中だけで
/// 生成・実行・破棄する。セットアップ完了/失敗を `ready_tx` で呼び出し元へ返し、成功時は
/// `main_loop.run()` で停止指示（[`Terminate`]）まで回る。対象 PID のノードを registry で
/// 待ち受け、対象出力ポートと自入力ポートが揃った時点で link-factory リンクを張る。
/// `select` で Include（対象 PID のみ録る）か Exclude（対象 PID 以外を録る）かを切り替える。
fn run_pw_process_loop(
    select: PidSelect,
    sink: RawSink,
    stop_rx: pw::channel::Receiver<Terminate>,
    ready_tx: &mpsc::Sender<std::result::Result<(), String>>,
) {
    // セットアップ（接続・stream 生成・registry リスナ登録）は別関数。
    // 戻り値は run 中ずっと生かす（drop すると監視/リンクが止まる）。
    let (main_loop, _keep) = match setup_pw_process(select, sink) {
        Ok(t) => t,
        Err(msg) => {
            // セットアップ失敗を通知して終了（panic しない）。
            let _ = ready_tx.send(Err(msg));
            return;
        }
    };

    // 停止チャネルの受信端を loop に attach。Terminate 受信で quit()。
    // quit() は loop 駆動のコールバック内、つまりこのスレッド上から呼ばれる。
    let main_loop_for_quit = main_loop.clone();
    let _attached = stop_rx.attach(main_loop.loop_(), move |_terminate| {
        main_loop_for_quit.quit();
    });

    // セットアップ成功を通知。以後は run() がブロックし、対象 PID の出現を待つ。
    if ready_tx.send(Ok(())).is_err() {
        // 呼び出し元が消えている（start が drop 済み等）。起動しない。
        return;
    }

    // Terminate 受信 or プロセス終了まで回る。対象 PID が未出現の間もここで待機し、
    // registry コールバックがリンクする。
    main_loop.run();
    // 抜けると _attached → _keep（listener→stream→registry→core→main_loop）の順で
    // drop され、PipeWire リソースがこのスレッド上で破棄される。
}

/// プロセスキャプチャの run 中ずっと保持する所有物。drop するとキャプチャが止まる。
///
/// - `CoreRc`: `core.create_object("link-factory", ...)` の主体。registry コールバック
///   から link を生成するため `Rc` で共有しつつ、drop 順の末尾に置く。
/// - `StreamRc`: 自前キャプチャ stream 本体（`Direction::Input` で接続済み。入力ポートを
///   持ち、対象出力ポートとのリンク確立でデータが流れ込む）。
/// - `StreamListener`: param_changed/process コールバック登録。drop で外れる。
/// - `RegistryRc`: registry プロキシ本体。
/// - `Registry Listener`: global/global_remove リスナ（drop で外れる）。
/// - `links`: link-factory で生成した [`pw::link::Link`] プロキシ群を、リンク先の出力
///   ノードの registry global id ごとに束ねたマップ。drop するとリンクが切れるので、
///   ループスレッド上で生かし続ける。registry コールバックがここへ insert / remove /
///   clear するので `Rc<RefCell<…>>` で共有する。Include は高々 1 エントリ、Exclude は
///   多数（マップごと drop すれば全リンクが切れる）。
#[allow(clippy::type_complexity)]
struct ProcessKeep {
    _stream: pw::stream::StreamRc,
    _listener: pw::stream::StreamListener<UserData>,
    _registry: pw::registry::RegistryRc,
    _registry_listener: pw::registry::Listener,
    _links: std::rc::Rc<std::cell::RefCell<std::collections::HashMap<u32, Vec<pw::link::Link>>>>,
    _core: pw::core::CoreRc,
}

/// 監視中の Stream/Output/Audio ノード 1 件の登録情報（registry global から拾う）。
///
/// PipeWire では PID はノードでなく Client オブジェクトの `pipewire.sec.pid` に載る。
/// ノード側には通常 PID が無く、`client.id` で所有 Client を指すだけ。なので PID 解決は
/// 二段（ノード→client.id→Client の PID）。ノード自身に PID が載っていれば `app_pid` に
/// 控える（将来構成への備え）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NodeEntry {
    /// このノードを所有する Client の registry global id（ノード props の `client.id`）。
    /// 無いこともある（その場合は `app_pid` も client_pid 解決も当たらない）。
    owning_client_id: Option<u32>,
    /// ノード自身の props に PID が載っていた場合の PID（通常は `None`。将来 PipeWire が
    /// ノードに PID を載せる構成への備え）。
    app_pid: Option<u32>,
}

/// 1 ポートの登録情報（registry の `ObjectType::Port` global から拾う）。
///
/// 対象出力ノードの出力ポート（`direction == "out"`）と、自前キャプチャ stream の
/// 入力ポート（`direction == "in"`）の双方をここに蓄積し、チャンネル名（`audio.channel`）
/// で対応付けてリンクする。
#[derive(Clone, Debug, PartialEq, Eq)]
struct PortEntry {
    /// このポートを持つノードの registry global id（ポート props の `node.id`）。
    node_id: u32,
    /// 方向（`"out"` = 出力ポート / `"in"` = 入力ポート）。
    direction: String,
    /// オーディオチャンネル名（`"FL"` / `"FR"` / `"MONO"` 等）。無ければ空。
    channel: String,
}

/// 出力ポートと入力ポートをチャンネルで対応付け、張るべきリンクのペアを返す
/// （PipeWire 非依存・到着順非依存）。`(out_port_id, channel)` の出力ポート集合と
/// `(in_port_id, channel)` の入力ポート集合から `(out_port_id, in_port_id)` を作る。
///
/// 対応規則:
/// 1. チャンネル名一致（FL→FL / FR→FR / MONO→MONO 等）を優先。
/// 2. モノラル出力の複製: 出力が 1 ポート（典型は MONO）で入力が複数なら、その単一出力を
///    全入力ポートへ複製リンクする（モノ→FL/FR 両方）。
/// 3. 順序フォールバック: チャンネル名が取れない/一致しないときは、残った出力ポートと
///    入力ポートを並び順で best-effort に対応付ける。
///
/// 戻り値は重複の無いリンクペア列。1 つも作れなければ空 `Vec`。
fn pair_ports(out_ports: &[(u32, String)], in_ports: &[(u32, String)]) -> Vec<(u32, u32)> {
    let mut pairs: Vec<(u32, u32)> = Vec::new();

    // 対応付けた入力ポートを記録（同一入力ポートへ二重リンクしない）。
    let mut used_in: Vec<bool> = vec![false; in_ports.len()];

    // チャンネル名一致を優先。出力ポートごとに、同じ非空チャンネル名の未使用入力ポートを探す。
    for (out_id, out_ch) in out_ports {
        if out_ch.is_empty() {
            continue;
        }
        if let Some(idx) = in_ports
            .iter()
            .enumerate()
            .position(|(i, (_in_id, in_ch))| !used_in[i] && in_ch == out_ch)
        {
            used_in[idx] = true;
            pairs.push((*out_id, in_ports[idx].0));
        }
    }

    // モノラル出力の複製。出力が 1 ポートだけで未対応の入力ポートが残っているなら、その
    // 単一出力を残り全入力へ複製する（モノ → FL/FR 両方など）。チャンネル一致で対応済みの
    // 入力は除く。
    if out_ports.len() == 1 {
        let (out_id, _out_ch) = &out_ports[0];
        for (i, _in_port) in in_ports.iter().enumerate() {
            if !used_in[i] {
                used_in[i] = true;
                pairs.push((*out_id, in_ports[i].0));
            }
        }
        return pairs;
    }

    // 順序フォールバック。チャンネル名一致で対応付かなかった出力ポート（空チャンネル含む）
    // を、残った入力ポートへ並び順で対応付ける。
    let mut paired_out: Vec<u32> = pairs.iter().map(|(o, _)| *o).collect();
    for (out_id, _out_ch) in out_ports {
        if paired_out.contains(out_id) {
            continue;
        }
        if let Some(idx) = used_in.iter().position(|used| !*used) {
            used_in[idx] = true;
            paired_out.push(*out_id);
            pairs.push((*out_id, in_ports[idx].0));
        }
    }

    pairs
}

/// ノードの PID を解決する（PipeWire 非依存・到着順非依存）。
///
/// ノード自身に PID があればそれを使い、無ければ `client.id` で所有 Client を引いて
/// `client_pid` 表（Client global id → その Client の `pipewire.sec.pid`）から解決する。
/// Client と Node はどちらが先に来てもよく、各 global 到着時にこの関数で再評価すれば、
/// 両方揃った時点で `Some(pid)` になる。
fn resolve_node_pid(
    entry: &NodeEntry,
    client_pid: &std::collections::HashMap<u32, u32>,
) -> Option<u32> {
    if let Some(pid) = entry.app_pid {
        // ノードに直接 PID が載る将来構成。Client を介さず確定。
        return Some(pid);
    }
    // 通常経路: client.id → Client の PID。
    let client_id = entry.owning_client_id?;
    client_pid.get(&client_id).copied()
}

/// 自前キャプチャ stream のノード名。registry で自分の入力ポートを引くための固有名で、
/// 対象 PID を埋めて衝突を避ける。
fn capture_node_name(target_pid: u32) -> String {
    format!("flexaudio-capture-{target_pid}")
}

/// プロセスキャプチャループのノード選択述語。
///
/// Include / Exclude / exclude_self の 3 経路を 1 本の fan-in リンク機構で扱う。内包する
/// `u32` はいずれも比較対象の PID で、Include は一致を、Exclude は不一致（その PID を残す）
/// をリンク条件にする。
#[derive(Clone, Copy, PartialEq, Eq)]
enum PidSelect {
    /// 解決済み PID == この PID のノードだけリンクする（Include。代表 1 ノード）。
    Include(u32),
    /// 解決済み PID != この PID の `Stream/Output/Audio` ノードをすべてリンクする
    /// （Exclude / exclude_self）。内包 PID は録音から除外するプロセスの PID。
    Exclude(u32),
}

impl PidSelect {
    /// 比較対象 PID（Include は録る側、Exclude は除外する側）。`global_remove` で
    /// 対象/除外 Client の消失を判定するのに使う。
    fn pid(self) -> u32 {
        match self {
            PidSelect::Include(p) | PidSelect::Exclude(p) => p,
        }
    }
}

/// プロセスキャプチャのセットアップ一式。失敗は `Err(String)`（panic しない）。
///
/// [`setup_pw`]（システム monitor）との違い:
/// - `STREAM_CAPTURE_SINK` も `AUTOCONNECT` も付けない（マイクへの自動リンクを防ぎ、明示
///   リンクだけにする）。`node.name` に固有名（[`capture_node_name`]）を付け、registry で
///   自分の入力ポートを引けるようにする。
/// - ここで一度だけ `stream.connect(Direction::Input, None, ...)` する。これで入力ポート
///   （input_FL/FR）が生成されるが、リンクされるまでデータは来ない（リンク確立で
///   format ネゴ→データ流入）。
/// - registry の `global` を張りっぱなしで購読し、Client / Node / Port を追跡する。PID は
///   Client の `pipewire.sec.pid`（`*pw::keys::SEC_PID`）に常在する（デーモンがソケット
///   資格情報から付与＝詐称できない。実機 stock 構成で確認）。ノードは `client.id` で
///   Client を指すだけなので、PID 照合は二段（node → client.id → Client の PID。
///   [`resolve_node_pid`]）。Client が先でも Node が先でも、各 global 到着時に再評価する。
/// - `select`（[`PidSelect`]）の述語でリンク対象ノードを決める。Include は対象 PID に属する
///   Stream/Output/Audio ノード 1 件、Exclude は除外 PID 以外の解決済み PID を持つ
///   Stream/Output/Audio ノード全件（PID 未解決は Client 到着まで保留）。各対象ノードの
///   出力ポートと自ノードの入力ポートが揃った時点で、registry コールバック（ループスレッド
///   実行）から `core.create_object::<pw::link::Link>("link-factory", ...)` で
///   チャンネル対応（[`pair_ports`]: FL→FL/FR→FR、モノは複製）のリンクを張る。リンクは
///   ノード単位で `linked`（node_id → Links）マップに保持する。
/// - `global_remove` で個別リンク中ノード/その出力ポートの消失を検知したらそのノードの
///   エントリだけ drop（Exclude では他ノードのリンクは保つ）、自ノード/自入力ポート/対象
///   Client の消失は全エントリを drop して再待機する（いずれも冪等に再リンクできる）。
///
/// 使うキー定数（いずれも crate `keys.rs` で feature gate 外を確認済み）:
/// `*pw::keys::SEC_PID`(="pipewire.sec.pid")・`*pw::keys::CLIENT_ID`(="client.id")・
/// `*pw::keys::NODE_ID`(="node.id")・`*pw::keys::PORT_DIRECTION`(="port.direction")・
/// `*pw::keys::AUDIO_CHANNEL`(="audio.channel")・`*pw::keys::LINK_OUTPUT_NODE`/
/// `LINK_OUTPUT_PORT`/`LINK_INPUT_NODE`/`LINK_INPUT_PORT`。
#[allow(clippy::type_complexity)]
fn setup_pw_process(
    select: PidSelect,
    sink: RawSink,
) -> std::result::Result<(pw::main_loop::MainLoopRc, ProcessKeep), String> {
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::rc::Rc;

    pw_init_once();

    let main_loop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| format!("create pipewire main loop failed: {e}"))?;
    let context = pw::context::ContextRc::new(&main_loop, None)
        .map_err(|e| format!("create pipewire context failed: {e}"))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| format!("connect to pipewire daemon failed (is PipeWire running?): {e}"))?;
    let registry = core
        .get_registry_rc()
        .map_err(|e| format!("get pipewire registry failed: {e}"))?;

    // 入力（キャプチャ）ストリームのプロパティ。
    // - media.type=Audio / media.category=Capture: 音声キャプチャストリーム
    // - media.class=Stream/Input/Audio: グラフ上の役割（入力＝録る側）
    // - media.role=Music: ヒント
    // - node.name=flexaudio-capture-<pid>: registry で自分の入力ポートを引くための固有名
    // STREAM_CAPTURE_SINK も AUTOCONNECT も付けない（マイクへの自動リンクを防ぎ、明示
    // link-factory リンクだけにする）。node.name に select の比較 PID を埋めて衝突を避ける。
    let node_name = capture_node_name(select.pid());
    let props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_CLASS => "Stream/Input/Audio",
        *pw::keys::MEDIA_ROLE => "Music",
        *pw::keys::NODE_NAME => node_name.as_str(),
    };

    let stream = pw::stream::StreamRc::new(core.clone(), "flexaudio-process-capture", props)
        .map_err(|e| format!("create pipewire capture stream failed: {e}"))?;

    let user_data = UserData {
        format: spa::param::audio::AudioInfoRaw::new(),
        sink,
    };
    // コールバック登録（共通ヘルパ。システム経路と同じ param_changed/process 挙動）。
    let listener = add_capture_listener(&stream, user_data)?;

    // 自前 stream を一度だけ connect する（Direction::Input・target=None・AUTOCONNECT なし）。
    // これで入力ポート（input_FL/FR）が生成される。リンクされるまでデータは来ない
    // （リンク確立で format ネゴ→データ流入）。フォーマット POD は F32LE/48000/2ch。
    {
        let values = build_format_pod_bytes()?;
        let pod = Pod::from_bytes(&values)
            .ok_or_else(|| "build audio format pod from bytes failed".to_string())?;
        let mut params = [pod];
        stream
            .connect(
                spa::utils::Direction::Input,
                None,
                StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
                &mut params,
            )
            .map_err(|e| format!("connect pipewire capture stream failed: {e}"))?;
    }

    // 自ノードの registry global id（自分の入力ポートを `node.id` で引くために使う）。
    // connect 直後は未確定（0）のことがあるが、入力ポートが registry に出る頃には
    // 確定している。Port 到着のたびに stream.node_id() で読み直す。
    let self_node_id: Rc<Cell<Option<u32>>> = Rc::new(Cell::new(None));

    // 状態表。registry コールバックはループスレッド 1 本からしか呼ばれないので、内部可変は
    // Cell/RefCell で足りる（Mutex は要らない）。

    // 監視中ノード表: registry node global id → 登録情報（owning client.id / 直 PID）。
    let nodes: Rc<RefCell<HashMap<u32, NodeEntry>>> = Rc::new(RefCell::new(HashMap::new()));
    // Client 表: Client の registry global id → その Client の pipewire.sec.pid。
    let client_pid: Rc<RefCell<HashMap<u32, u32>>> = Rc::new(RefCell::new(HashMap::new()));
    // 比較対象 PID（Include は録る PID / Exclude は除外する PID）の Client の registry
    // global id（判明時 Some）。global_remove で対象/除外 Client の消失を判定するのに使う。
    let target_client_id: Rc<Cell<Option<u32>>> = Rc::new(Cell::new(None));
    // ポート表: registry port global id → 登録情報（所有 node.id / direction / channel）。
    let ports: Rc<RefCell<HashMap<u32, PortEntry>>> = Rc::new(RefCell::new(HashMap::new()));
    // 現在リンク中の出力ノード表: 出力ノードの registry global id → そのノード向けに生成した
    // Link プロキシ群。drop でリンクが切れるので run 中ずっと保持する。Include は高々 1
    // エントリ、Exclude は多数。エントリ単位 remove で個別に、map ごと clear で一括にリンクを切れる。
    let linked: Rc<RefCell<HashMap<u32, Vec<pw::link::Link>>>> =
        Rc::new(RefCell::new(HashMap::new()));

    // 状態が更新されるたびに、リンクすべき出力ノードのうち未リンクのものを再評価し、
    // 対象出力ポートと自入力ポートが揃っていれば link-factory でチャンネル対応リンクを張る。
    // `select` の述語で対象ノード集合を決める:
    // - Include(pid): 解決済み PID == pid のノード（代表 1 ノードのみ。`linked` が既に
    //   非空なら何もしない＝単一ノードのまま）。
    // - Exclude(pid): 解決済み PID != pid の `Stream/Output/Audio` ノードをすべて。PID 未解決
    //   （None）のノードはまだリンクしない（Client 到着で PID が解けるまで待ち、除外
    //   プロセスを取り違えない）。
    // 既に `linked` のキーになっているノードは二重リンクしない。
    // ループスレッド上で呼ばれる（`!Send` な core/stream を触ってよい）。
    #[allow(clippy::too_many_arguments)]
    fn try_link(
        core: &pw::core::CoreRc,
        stream: &pw::stream::StreamRc,
        select: PidSelect,
        self_node_id: &Cell<Option<u32>>,
        nodes: &RefCell<HashMap<u32, NodeEntry>>,
        client_pid: &RefCell<HashMap<u32, u32>>,
        ports: &RefCell<HashMap<u32, PortEntry>>,
        linked: &RefCell<HashMap<u32, Vec<pw::link::Link>>>,
    ) {
        // Include は代表 1 ノードのみ。既にリンク済みなら何もしない。
        if let PidSelect::Include(_) = select {
            if !linked.borrow().is_empty() {
                return;
            }
        }

        // 自ノード id を stream から読み直す（connect 直後は未確定のことがある）。
        // 未確定時は SPA_ID_INVALID(=ID_ANY=u32::MAX) または 0 が返る。
        let sid = stream.node_id();
        if sid != 0 && sid != pw::constants::ID_ANY {
            self_node_id.set(Some(sid));
        }
        let Some(self_nid) = self_node_id.get() else {
            return;
        };

        // リンクすべき出力ノード id 集合を述語で決める。
        // - Include: 解決済み PID == pid のノードを 1 件だけ。
        // - Exclude: 解決済み PID（!= pid）のノードを全件（PID 未解決は除く）。
        let targets: Vec<u32> = {
            let nodes = nodes.borrow();
            let client_pid = client_pid.borrow();
            let linked = linked.borrow();
            match select {
                PidSelect::Include(pid) => nodes
                    .iter()
                    .find(|(id, entry)| {
                        !linked.contains_key(id)
                            && resolve_node_pid(entry, &client_pid) == Some(pid)
                    })
                    .map(|(&node_id, _)| node_id)
                    .into_iter()
                    .collect(),
                PidSelect::Exclude(pid) => nodes
                    .iter()
                    .filter(|(id, entry)| {
                        if linked.contains_key(id) {
                            return false;
                        }
                        // 解決済みかつ除外 PID 以外のときだけ対象にする。未解決（None）は
                        // Client 到着まで保留（除外プロセスを取り違えない）。
                        matches!(resolve_node_pid(entry, &client_pid), Some(other) if other != pid)
                    })
                    .map(|(&node_id, _)| node_id)
                    .collect(),
            }
        };

        if targets.is_empty() {
            return;
        }

        // 自ノードの入力ポートを ports 表から引く（全対象ノードで共有）。
        let in_ports: Vec<(u32, String)> = {
            let ports = ports.borrow();
            ports
                .iter()
                .filter(|(_pid, p)| p.node_id == self_nid && p.direction == "in")
                .map(|(&pid, p)| (pid, p.channel.clone()))
                .collect()
        };
        // 自入力ポートがまだ無ければリンクできない（次の global 到着で再評価）。
        if in_ports.is_empty() {
            return;
        }

        for target_node_id in targets {
            // 対象ノードの出力ポートを ports 表から引く。
            let out_ports: Vec<(u32, String)> = {
                let ports = ports.borrow();
                ports
                    .iter()
                    .filter(|(_pid, p)| p.node_id == target_node_id && p.direction == "out")
                    .map(|(&pid, p)| (pid, p.channel.clone()))
                    .collect()
            };
            // 出力ポートが未出現ならこのノードはまだリンクできない（次回再評価）。
            if out_ports.is_empty() {
                continue;
            }

            // チャンネル対応（FL→FL/FR→FR、モノは複製、取れなければ順序）でペアを作る。
            let pairs = pair_ports(&out_ports, &in_ports);
            if pairs.is_empty() {
                continue;
            }
            let want = pairs.len();

            // link-factory で各ペアをリンクする。
            let mut created: Vec<pw::link::Link> = Vec::with_capacity(want);
            for (out_port_id, in_port_id) in pairs {
                let link_props = properties! {
                    *pw::keys::LINK_OUTPUT_NODE => target_node_id.to_string(),
                    *pw::keys::LINK_OUTPUT_PORT => out_port_id.to_string(),
                    *pw::keys::LINK_INPUT_NODE => self_nid.to_string(),
                    *pw::keys::LINK_INPUT_PORT => in_port_id.to_string(),
                };
                match core.create_object::<pw::link::Link>("link-factory", &link_props) {
                    Ok(link) => created.push(link),
                    Err(_e) => {
                        // このペアのリンク生成に失敗。残りは試さず部分リンクを避ける。
                        break;
                    }
                }
            }

            // 全ペアが張れたときだけリンク確立とみなす。片チャンネルだけ成功した部分リンク
            // （例: FL だけ繋がり FR が落ちる）を確立扱いすると、対象が実質モノラルに固定化
            // されてしまう。全ペア揃わなければここで作った Link を drop してこのノードは
            // 未リンクのままにし、次の global 到着で再評価する（残りポートが後から出る/
            // リンクが一時的に落ちた状態にリトライがかかる）。他の対象ノードの処理は続ける。
            if created.len() != want {
                // created を drop してリンクを残さない（部分リンクを確定させない）。
                drop(created);
                continue;
            }

            // 全ペア確立。Link プロキシをノード単位で保持する。
            linked.borrow_mut().insert(target_node_id, created);
        }
    }

    // registry global / global_remove リスナ。
    // global: Client→client_pid 表 / Stream/Output/Audio ノード→nodes 表 /
    // Port→ports 表 に登録し、毎回 try_link でリンクを再評価する。
    let core_for_global = core.clone();
    let stream_for_global = stream.clone();
    let self_node_for_global = self_node_id.clone();
    let nodes_for_global = nodes.clone();
    let client_pid_for_global = client_pid.clone();
    let target_client_for_global = target_client_id.clone();
    let ports_for_global = ports.clone();
    let linked_for_global = linked.clone();

    let core_for_remove = core.clone();
    let stream_for_remove = stream.clone();
    let self_node_for_remove = self_node_id.clone();
    let nodes_for_remove = nodes.clone();
    let client_pid_for_remove = client_pid.clone();
    let target_client_for_remove = target_client_id.clone();
    let ports_for_remove = ports.clone();
    let linked_for_remove = linked.clone();

    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            // FFI 越えの panic は UB なので本体を catch_unwind で包む。
            let _ = catch_unwind(AssertUnwindSafe(|| {
                let Some(props) = global.props else {
                    return;
                };
                match global.type_ {
                    pw::types::ObjectType::Client => {
                        // PID は Client の pipewire.sec.pid に常在する（デーモンが付与する
                        // ので詐称できない）。
                        let Some(pid_str) = props.get(*pw::keys::SEC_PID) else {
                            return;
                        };
                        let Ok(pid) = pid_str.parse::<u32>() else {
                            return;
                        };
                        client_pid_for_global.borrow_mut().insert(global.id, pid);
                        // 比較対象 PID の Client を控える（global_remove で消失検知に使う）。
                        if pid == select.pid() {
                            target_client_for_global.set(Some(global.id));
                        }
                    }
                    pw::types::ObjectType::Node => {
                        // アプリの出力ノード（再生ストリーム）だけを対象にする。
                        let media_class = props.get(*pw::keys::MEDIA_CLASS).unwrap_or("");
                        if media_class != "Stream/Output/Audio" {
                            return;
                        }
                        // 所有 Client を指す client.id。
                        let owning_client_id = props
                            .get(*pw::keys::CLIENT_ID)
                            .and_then(|s| s.parse::<u32>().ok());
                        // ノード自身に PID が載れば直接照合できる（将来構成への備え）。
                        let app_pid = props
                            .get(*pw::keys::SEC_PID)
                            .and_then(|s| s.parse::<u32>().ok());
                        nodes_for_global.borrow_mut().insert(
                            global.id,
                            NodeEntry {
                                owning_client_id,
                                app_pid,
                            },
                        );
                    }
                    pw::types::ObjectType::Port => {
                        // ポートを蓄積する（対象出力ポートと自入力ポートの両方をここから引く）。
                        let Some(node_id) = props
                            .get(*pw::keys::NODE_ID)
                            .and_then(|s| s.parse::<u32>().ok())
                        else {
                            return;
                        };
                        let direction = props
                            .get(*pw::keys::PORT_DIRECTION)
                            .unwrap_or("")
                            .to_string();
                        if direction != "out" && direction != "in" {
                            return;
                        }
                        let channel = props
                            .get(*pw::keys::AUDIO_CHANNEL)
                            .unwrap_or("")
                            .to_string();
                        ports_for_global.borrow_mut().insert(
                            global.id,
                            PortEntry {
                                node_id,
                                direction,
                                channel,
                            },
                        );
                    }
                    _ => return,
                }

                // Client / Node / Port のどれが来ても状態が更新されたので再評価する。
                // ループスレッド上なので `!Send` core/stream を触ってよい。
                try_link(
                    &core_for_global,
                    &stream_for_global,
                    select,
                    &self_node_for_global,
                    &nodes_for_global,
                    &client_pid_for_global,
                    &ports_for_global,
                    &linked_for_global,
                );
            }));
        })
        .global_remove(move |id| {
            // FFI 越えの panic は UB なので本体を catch_unwind で包む。
            let _ = catch_unwind(AssertUnwindSafe(|| {
                // 消えた id の種類に応じて表から除去し、リンク状態を見直す。借用衝突を避ける
                // ため、まず scoped borrow で何をするかを bool / owner として確定させ、その後
                // linked を変更し try_link を呼ぶ。
                let mut relink_needed = false;

                // 消えた id がリンク中ノード/対象・除外 Client/自ノードのどれか。
                let was_linked_node = linked_for_remove.borrow().contains_key(&id);
                let was_target_client = target_client_for_remove.get() == Some(id);
                // 自ノード（自前キャプチャ stream のノード）自体が消えたか。
                let was_self_node = self_node_for_remove.get() == Some(id);

                // 消えた id がリンク中いずれかのノードに属する出力ポートなら、その所有ノード
                // id を求める。また自ノードに属する入力ポートが消えたかも判定する。自入力
                // ポートの消失を見逃すと、入力が落ちているのに linked 扱いのまま固着して無音
                // から復帰しなくなる。ports.borrow() を try_link 呼び出し跨ぎで保持しないよう、
                // この scope 内で owner / bool を計算してから抜ける。
                let (linked_out_owner, was_self_in_port): (Option<u32>, bool) = {
                    let ports = ports_for_remove.borrow();
                    let owner = ports.get(&id).and_then(|p| {
                        if p.direction == "out"
                            && linked_for_remove.borrow().contains_key(&p.node_id)
                        {
                            Some(p.node_id)
                        } else {
                            None
                        }
                    });
                    let self_in = if let Some(self_nid) = self_node_for_remove.get() {
                        ports
                            .get(&id)
                            .map(|p| p.node_id == self_nid && p.direction == "in")
                            .unwrap_or(false)
                    } else {
                        false
                    };
                    (owner, self_in)
                };

                // 自ノード/自入力ポート/対象 Client の消失は全リンクを一括解除して再評価に
                // 委ねる。
                // - 自ノード/自入力ポート: 入力側が消えたので全リンクが無効。
                // - 対象/除外 Client: Include ならその PID の全ノードが消える（録る対象消滅）。
                //   Exclude でも一括解除→再リンクで結果は正しい（除外 Client のノードはこの後
                //   nodes 表から消えるので再リンクされず、残す側だけ張り直される）。
                if was_self_node || was_self_in_port || was_target_client {
                    // 保持中の Link を全部 drop（= リンク解除）して未リンクに戻す。
                    linked_for_remove.borrow_mut().clear();
                    relink_needed = true;
                } else {
                    // 個別ノードの消失だけ解除する（Exclude で他ノードのリンクは保つ）。
                    if was_linked_node {
                        linked_for_remove.borrow_mut().remove(&id);
                        relink_needed = true;
                    }
                    if let Some(owner) = linked_out_owner {
                        linked_for_remove.borrow_mut().remove(&owner);
                        relink_needed = true;
                    }
                }

                if was_target_client {
                    target_client_for_remove.set(None);
                }
                if was_self_node {
                    // 自ノードが消えたら id キャッシュをクリア。try_link が stream から
                    // 読み直し、再生成時に新 id を拾える。
                    self_node_for_remove.set(None);
                }

                // 各表から消えた id を除去（pid/port 解決が古い値を引かないように）。
                nodes_for_remove.borrow_mut().remove(&id);
                client_pid_for_remove.borrow_mut().remove(&id);
                ports_for_remove.borrow_mut().remove(&id);

                // 再待機状態になったら、別の対象が既に揃っていれば即再リンクを試みる。
                if relink_needed {
                    try_link(
                        &core_for_remove,
                        &stream_for_remove,
                        select,
                        &self_node_for_remove,
                        &nodes_for_remove,
                        &client_pid_for_remove,
                        &ports_for_remove,
                        &linked_for_remove,
                    );
                }
            }));
        })
        .register();

    Ok((
        main_loop,
        ProcessKeep {
            _stream: stream,
            _listener: listener,
            _registry: registry,
            _registry_listener,
            _links: linked,
            _core: core,
        },
    ))
}

/// `process` コールバックと `param_changed` の間で共有する状態。
///
/// 確定したフォーマット（channels）を `process` から参照するために保持する。
struct UserData {
    /// PipeWire が確定したキャプチャフォーマット。`param_changed` で更新。
    format: spa::param::audio::AudioInfoRaw,
    /// 生フレームを流す先。`process` から `&mut` で push する。
    sink: RawSink,
}

/// キャプチャ stream へ `param_changed` / `process` コールバックを登録する。
///
/// [`PwSystemBackend`]（システム monitor）と [`PwProcessBackend`]（プロセス fan-out）で
/// 同じコールバック挙動を使うので共通ヘルパにする。`param_changed` で確定フォーマットを
/// 控え、`process` で dequeue した interleaved f32 を [`RawSink::push`] へ非ブロッキングに流す。
///
/// 登録した [`StreamListener`](pw::stream::StreamListener) を返す（drop すると
/// コールバックが外れるので、呼び出し元が run 中ずっと保持する）。
fn add_capture_listener(
    stream: &pw::stream::StreamRc,
    user_data: UserData,
) -> std::result::Result<pw::stream::StreamListener<UserData>, String> {
    // RT の process コールバックが f32 詰め替えに使う thread-local スクラッチを、stream
    // セットアップ時（このループスレッド上）に最大想定ブロック長まで事前確保しておく。これで
    // process 内の reserve（RT アロケート＝xrun リスク）を定常状態で避ける。setup_pw /
    // setup_pw_process は登録後にこの関数を呼ぶので、reserve は非 RT のセットアップ局面で 1 回。
    PROC_SCRATCH.with(|cell| {
        let mut s = cell.borrow_mut();
        let cap = s.capacity();
        if cap < PROC_SCRATCH_CAP {
            s.reserve(PROC_SCRATCH_CAP - cap);
        }
    });

    stream
        .add_local_listener_with_user_data(user_data)
        .param_changed(|_stream, user_data, id, param| {
            // FFI 越えの panic は UB なので本体を catch_unwind で包む。
            let _ = catch_unwind(AssertUnwindSafe(|| {
                // NULL は format クリア。
                let Some(param) = param else {
                    return;
                };
                if id != pw::spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let (media_type, media_subtype) = match format_utils::parse_format(param) {
                    Ok(v) => v,
                    Err(_) => return,
                };
                // raw audio のみ受理。
                if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
                    return;
                }
                // 確定フォーマットを控える（process でチャンネル数として使う）。
                if user_data.format.parse(param).is_err() {
                    // パース失敗時は更新しない（直前の値を保持）。
                }
            }));
        })
        .process(|stream, user_data| {
            // RT スレッドで呼ばれる。ブロック・確保は避ける。
            // FFI 越えの panic は UB なので本体を catch_unwind で包む。
            let _ = catch_unwind(AssertUnwindSafe(|| {
                // バッファが無ければ何もしない（panic しない）。
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }
                let data = &mut datas[0];
                // 有効バイト数とオフセット（リング上の位置）を控えてから data() を借りる。
                let chunk = data.chunk();
                let size = chunk.size() as usize;
                let offset = chunk.offset() as usize;
                if size == 0 {
                    return;
                }
                let Some(bytes) = data.data() else {
                    return;
                };
                // [offset, offset+size) が有効領域。範囲外は弾く（防御的）。
                let end = offset.saturating_add(size);
                if end > bytes.len() {
                    return;
                }
                let valid = &bytes[offset..end];
                // f32 の倍数だけ取り出す（端数バイトは無視）。
                let n_floats = valid.len() / std::mem::size_of::<f32>();
                if n_floats == 0 {
                    return;
                }
                // バイト列を f32 interleaved として読む。`data` のアライメントは保証されない
                // ので、align_to ではなく from_le_bytes で読む。事前確保済みの再利用バッファに
                // 詰めてから 1 回で push する（RawSink::push は非ブロッキングで満杯時 DROP）。
                PROC_SCRATCH.with(|cell| {
                    let mut scratch = cell.borrow_mut();
                    // 事前確保済み（PROC_SCRATCH_CAP）なら定常状態で reserve は no-op で RT
                    // アロケートが起きない。想定を超えるブロックのときだけ一度広げる
                    // （以後その容量を保つ）。
                    let cap = scratch.capacity();
                    if n_floats > cap {
                        scratch.reserve(n_floats - cap);
                    }
                    scratch.clear();
                    for i in 0..n_floats {
                        let b = i * 4;
                        let v = f32::from_le_bytes([
                            valid[b],
                            valid[b + 1],
                            valid[b + 2],
                            valid[b + 3],
                        ]);
                        scratch.push(v);
                    }
                    // PTS: 現状は到着時刻の単調クロック（`monotonic_now_ns`）で代用する。
                    // 下流の ClockNormalizer が初回原点を取るため単調近似でも破綻しない。
                    // 将来は `pw_buffer.time` の device クロックに置き換え可能。
                    user_data.sink.push(&scratch, monotonic_now_ns());
                });
            }));
        })
        .register()
        .map_err(|e| format!("register pipewire stream listener failed: {e}"))
}

/// 要求フォーマット POD（f32 / 48000 / 2ch）のバイト列を組み立てる。
///
/// rate/channels を明示するので、グラフが違えば PipeWire が `audioconvert` を自動挿入して
/// 48k/stereo/f32 に変換する。返り値のバイト列から [`Pod::from_bytes`] で POD を作る
/// （バイト列が POD の指す実体なので、connect 呼び出しまで生かしておくこと）。
fn build_format_pod_bytes() -> std::result::Result<Vec<u8>, String> {
    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::F32LE);
    audio_info.set_rate(NATIVE_RATE);
    audio_info.set_channels(NATIVE_CHANNELS as u32);

    let obj = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .map_err(|e| format!("serialize audio format pod failed: {e}"))?
    .0
    .into_inner();
    Ok(values)
}

/// PipeWire ループスレッド本体。
///
/// `MainLoop`/`Context`/`Core`/`Stream`（いずれも `!Send`）をこの関数の中だけで生成・実行・
/// 破棄し、スレッド境界を跨がせない。セットアップ完了/失敗を `ready_tx` で呼び出し元へ返し、
/// 成功時は `main_loop.run()` で停止指示まで回る。
fn run_pw_loop(
    device_id: Option<String>,
    sink: RawSink,
    stop_rx: pw::channel::Receiver<Terminate>,
    ready_tx: &mpsc::Sender<std::result::Result<(), String>>,
) {
    // セットアップは別関数。戻り値は run 中ずっと生かす所有物（drop すると停止する）。
    let (main_loop, _stream, _listener) = match setup_pw(device_id, sink) {
        Ok(t) => t,
        Err(msg) => {
            // セットアップ失敗を通知して終了（panic しない）。
            let _ = ready_tx.send(Err(msg));
            return;
        }
    };

    // 停止チャネルの受信端を loop に attach。Terminate 受信で quit()。attach はこのローカル
    // `main_loop` を借用するだけなので、戻り値の AttachedReceiver はこのスタックフレームに
    // 閉じる（自己参照構造体にならず unsafe な寿命延長も要らない）。quit() は loop 駆動の
    // コールバック内、つまりこのスレッド上から呼ばれる。
    let main_loop_for_quit = main_loop.clone();
    let _attached = stop_rx.attach(main_loop.loop_(), move |_terminate| {
        main_loop_for_quit.quit();
    });

    // セットアップ成功を通知。以後は run() がブロックする。
    if ready_tx.send(Ok(())).is_err() {
        // 呼び出し元が消えている（start が drop 済み等）。起動しない。
        return;
    }

    // Terminate 受信 or プロセス終了まで回る。
    main_loop.run();
    // 抜けると _attached → _listener → _stream → main_loop の順（宣言の逆順）で drop され、
    // PipeWire リソースがこのスレッド上で破棄される。
}

/// PipeWire のセットアップ一式。失敗は `Err(String)`（panic しない）。
///
/// `device_id` が `Some(node.name)` ならその sink を `target.object` で狙う（`None` は
/// 既定 sink）。sink の存在確認は呼び出し前（`start`）で済ませてある。
///
/// 返すのは run 中ずっと生かすハンドル群:
/// - `MainLoopRc`: `run()`/`quit()` の主体
/// - `StreamRc`: キャプチャストリーム本体
/// - `StreamListener`: コールバック登録。drop で外れる
///
/// 停止チャネルの loop への attach は呼び出し元（[`run_pw_loop`]）がやる。そうすると
/// `AttachedReceiver` が返り値タプル（`MainLoopRc` を含む）を借用する自己参照構造体に
/// ならずに済む。
#[allow(clippy::type_complexity)]
fn setup_pw(
    device_id: Option<String>,
    sink: RawSink,
) -> std::result::Result<
    (
        pw::main_loop::MainLoopRc,
        pw::stream::StreamRc,
        pw::stream::StreamListener<UserData>,
    ),
    String,
> {
    // pw::init はプロセス全体で 1 回だけ（Once でスレッド競合を防ぐ）。
    pw_init_once();

    let main_loop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| format!("create pipewire main loop failed: {e}"))?;
    let context = pw::context::ContextRc::new(&main_loop, None)
        .map_err(|e| format!("create pipewire context failed: {e}"))?;
    // 既定の PipeWire デーモンへ接続。デーモン不在ならここで Err。
    let core = context
        .connect_rc(None)
        .map_err(|e| format!("connect to pipewire daemon failed (is PipeWire running?): {e}"))?;

    // 入力（キャプチャ）ストリームのプロパティ。
    // - media.type=Audio / media.category=Capture: 音声キャプチャストリーム
    // - media.class=Stream/Input/Audio: グラフ上の役割（入力＝録る側）
    // - stream.capture.sink=true: 録音デバイスではなく sink の monitor（システム音声出力）を録る
    // - media.role: 既定 sink への autoconnect 用ヒント
    let mut props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_CLASS => "Stream/Input/Audio",
        *pw::keys::MEDIA_ROLE => "Music",
    };
    // monitor（sink の出力＝システム音声）を録る指定。
    props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
    // device_id を指定したら、その sink を target.object（node.name）で狙う。autoconnect は
    // 残すが、target.object があれば WirePlumber は既定でなくこの sink の monitor へ繋ぐ。
    // stream.connect の target 引数（下の None）はかつて WirePlumber に無視されたので使わない
    // ＝こちらの props 指定を使う。sink 不在は start で先に弾いているのでここでは確認しない。
    // pw::keys::TARGET_OBJECT は crate の v0_3_44 feature 下なので、キー文字列を直接書く
    // （他の feature gate なキーも同様に文字列指定している）。
    if let Some(id) = device_id {
        props.insert("target.object", id);
    }

    let stream = pw::stream::StreamRc::new(core, "flexaudio-system-capture", props)
        .map_err(|e| format!("create pipewire capture stream failed: {e}"))?;

    let user_data = UserData {
        format: spa::param::audio::AudioInfoRaw::new(),
        sink,
    };

    // コールバック登録。`param_changed` で確定 format を控え、`process` で dequeue した
    // バッファを RawSink へ流す（共通ヘルパ）。
    let listener = add_capture_listener(&stream, user_data)?;

    // 要求フォーマット param: f32 / 48000 / 2ch。rate/channels を明示するので、グラフが違えば
    // PipeWire が audioconvert を自動挿入して 48k/stereo/f32 に変換する。
    let values = build_format_pod_bytes()?;
    let pod = Pod::from_bytes(&values)
        .ok_or_else(|| "build audio format pod from bytes failed".to_string())?;
    let mut params = [pod];

    // 入力方向で connect。AUTOCONNECT で sink の monitor へ繋ぐ（target.object 指定が
    // あればその sink、無ければ既定 sink）。MAP_BUFFERS でバッファを直接読めるようにし、
    // RT_PROCESS で process を RT 実行。
    stream
        .connect(
            spa::utils::Direction::Input,
            None,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(|e| format!("connect pipewire capture stream failed: {e}"))?;

    Ok((main_loop, stream, listener))
}

/// `process` の f32 詰め替えスクラッチを事前確保する容量（f32 個数）。ネイティブ要求は
/// 48000 Hz / 2ch なので 1 秒ぶん = 96000 にする。実機の process ブロックは数百〜数千 frames
/// （1 秒よりずっと小さい）なので、これだけ確保すれば RT 内の reserve は起きない。
const PROC_SCRATCH_CAP: usize = (NATIVE_RATE as usize) * (NATIVE_CHANNELS as usize);

thread_local! {
    /// `process` コールバックの f32 詰め替え用スクラッチ。実体は [`add_capture_listener`] が
    /// stream セットアップ時に [`PROC_SCRATCH_CAP`] まで事前確保するので、RT の process 内では
    /// 再確保が起きない。
    static PROC_SCRATCH: std::cell::RefCell<Vec<f32>> = const { std::cell::RefCell::new(Vec::new()) };
}

// ============================================================================
// デバイス列挙（`devices()` の Linux/PipeWire 分）
// ============================================================================

/// 列挙中に PipeWire レジストリの global イベントから集めた 1 ノードの生情報。
///
/// コールバックは `!Send` なローカル状態へ書くので、ここでは所有 `String` で控えておき、
/// 列挙ループ終了後に [`DeviceInfo`] へ組み立てる。
struct NodeRecord {
    /// 安定 ID に使う `node.name`（永続的）。
    node_name: String,
    /// 表示名。`node.description` 優先、無ければ `node.name`。
    description: String,
    /// `media.class`（`"Audio/Sink"` / `"Audio/Source"` 等）。
    media_class: String,
    /// `audio.rate` を読めた場合のレート（Hz）。
    rate: Option<u32>,
    /// `audio.channels` を読めた場合のチャンネル数。
    channels: Option<u16>,
}

/// 列挙ループ全体で共有する収集先（`!Send`・ループスレッド内に閉じる）。
#[derive(Default)]
struct EnumState {
    /// 集めた Audio/Sink・Audio/Source ノード。
    nodes: Vec<NodeRecord>,
    /// 既定 sink の `node.name`（`default.audio.sink` メタデータから）。
    default_sink: Option<String>,
    /// 既定 source の `node.name`（`default.audio.source` メタデータから）。
    default_source: Option<String>,
}

/// PipeWire 経由でオーディオデバイス（マイク + システム出力 sink）を列挙する。
///
/// レジストリの global イベントを 1 往復ぶん受け取り、
/// - `media.class == "Audio/Sink"` → システム音声出力（既定 sink の monitor を録る対象）。
///   `is_loopback = true` / `source_kind = SystemLoopback`。
/// - `media.class == "Audio/Source"` → マイク等の録音デバイス。
///   `is_loopback = false` / `source_kind = Mic`。
///
/// として [`DeviceInfo`] に写す。`id` は永続的な `node.name`、`name` は `node.description`
/// （無ければ `node.name`）。`sample_rate` / `channels` は `audio.rate` / `audio.channels` が
/// 取れればその値、無ければ既定 `48000 / 2`。既定デバイスは `default` メタデータ
/// （`default.audio.sink` / `default.audio.source`）の指す `node.name` と一致するものに
/// `is_default = true` を付ける。
///
/// 短命の `MainLoop` を 1 本回し、`core.sync()` の `done` で列挙完了を検知して `quit()` する。
/// PipeWire デーモン不在・接続失敗・レジストリ取得失敗は `Ok(空 Vec)` に握る（panic しない。
/// 列挙は「無い」と等価）。
pub fn list_devices() -> Result<Vec<DeviceInfo>> {
    match enumerate_pw() {
        Ok(v) => Ok(v),
        // デーモン不在等は「列挙対象なし」と同じに扱う（呼び出し側を壊さない）。
        Err(_msg) => Ok(Vec::new()),
    }
}

/// PipeWire レジストリ列挙の本体。失敗は `Err(String)`（panic しない）。
///
/// `MainLoop`/`Context`/`Core`/`Registry`（いずれも `!Send`）をこの関数内だけで生成・実行・
/// 破棄する。短命ループで列挙してすぐ終わるので、`list_devices` は専用スレッドを立てず
/// 呼び出しスレッドで同期実行する。
fn enumerate_pw() -> std::result::Result<Vec<DeviceInfo>, String> {
    use std::cell::RefCell;
    use std::rc::Rc;

    pw_init_once();

    let main_loop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| format!("create pipewire main loop failed: {e}"))?;
    let context = pw::context::ContextRc::new(&main_loop, None)
        .map_err(|e| format!("create pipewire context failed: {e}"))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| format!("connect to pipewire daemon failed (is PipeWire running?): {e}"))?;
    // RegistryRc はクローン可能で、global コールバックへ move して bind に使える。
    let registry = core
        .get_registry_rc()
        .map_err(|e| format!("get pipewire registry failed: {e}"))?;

    let state = Rc::new(RefCell::new(EnumState::default()));
    // default メタデータの property リスナを生かしておく保管庫。global コールバック内で
    // bind した Metadata プロキシ + リスナをここへ push する。
    type MetaKeep = (Box<dyn pw::proxy::ProxyT>, Box<dyn pw::proxy::Listener>);
    let meta_keep: Rc<RefCell<Vec<MetaKeep>>> = Rc::new(RefCell::new(Vec::new()));

    // registry global リスナ: Audio ノードと default メタデータを収集する。
    let state_for_global = state.clone();
    let registry_for_global = registry.clone();
    let meta_keep_for_global = meta_keep.clone();
    let _reg_listener = registry
        .add_listener_local()
        .global(move |global| {
            // FFI 越えの panic は UB なので本体を catch_unwind で包む。
            let _ = catch_unwind(AssertUnwindSafe(|| {
                let Some(props) = global.props else {
                    return;
                };
                match global.type_ {
                    pw::types::ObjectType::Node => {
                        // media.class が Audio/Sink|Source のノードだけ拾う。
                        let media_class = props.get(*pw::keys::MEDIA_CLASS).unwrap_or("");
                        if media_class != "Audio/Sink" && media_class != "Audio/Source" {
                            return;
                        }
                        let node_name = props.get(*pw::keys::NODE_NAME).unwrap_or("");
                        if node_name.is_empty() {
                            // 安定キーが無いノードは列挙できない（スキップ）。
                            return;
                        }
                        let description = props
                            .get(*pw::keys::NODE_DESCRIPTION)
                            .filter(|s| !s.is_empty())
                            .unwrap_or(node_name);
                        // audio.rate のキー定数は pipewire crate で feature gate 下なので
                        // 文字列で指定する。registry のノード props には載らないことも多く、
                        // その場合は下流で既定値（48000/2）にフォールバックする。
                        let rate = props.get("audio.rate").and_then(|s| s.parse::<u32>().ok());
                        let channels = props
                            .get(*pw::keys::AUDIO_CHANNELS)
                            .and_then(|s| s.parse::<u16>().ok());
                        state_for_global.borrow_mut().nodes.push(NodeRecord {
                            node_name: node_name.to_string(),
                            description: description.to_string(),
                            media_class: media_class.to_string(),
                            rate,
                            channels,
                        });
                    }
                    pw::types::ObjectType::Metadata => {
                        // 既定 sink/source を保持する "default" メタデータだけ bind する
                        // （"metadata.name" のキー定数は pipewire crate に無いので文字列指定）。
                        let meta_name = props.get("metadata.name").unwrap_or("");
                        if meta_name != "default" {
                            return;
                        }
                        let metadata: pw::metadata::Metadata =
                            match registry_for_global.bind(global) {
                                Ok(m) => m,
                                Err(_) => return,
                            };
                        let state_for_meta = state_for_global.clone();
                        let listener = metadata
                            .add_listener_local()
                            .property(move |_subject, key, _type, value| {
                                // property コールバックも FFI 越えなので catch_unwind で包む。
                                catch_unwind(AssertUnwindSafe(|| {
                                    // value は JSON（例: {"name":"alsa_output...."}）。name を抜く。
                                    if let (Some(key), Some(value)) = (key, value) {
                                        if key == "default.audio.sink" {
                                            state_for_meta.borrow_mut().default_sink =
                                                extract_json_name(value);
                                        } else if key == "default.audio.source" {
                                            state_for_meta.borrow_mut().default_source =
                                                extract_json_name(value);
                                        }
                                    }
                                }))
                                .ok();
                                0
                            })
                            .register();
                        meta_keep_for_global
                            .borrow_mut()
                            .push((Box::new(metadata), Box::new(listener)));
                    }
                    _ => {}
                }
            }));
        })
        .register();

    // 二段 sync→done バリアで列挙完了を待つ。
    //
    // 1 段目の done は registry の初期 global が出揃ったことは保証するが、その global 中で
    // bind した default メタデータの初期 property ダンプ（既定 sink/source の値）はまだ
    // 届いていないことがある（proxy 経由イベントは別途到着する）。そこで 1 段目の done を
    // 受けたらもう一度 sync し、2 段目の done で quit する。これで global 列挙と既定
    // メタデータの property の両方が揃ってから抜ける。done は必ず来るので無限化しない。
    let done = Rc::new(std::cell::Cell::new(false));
    let stage = Rc::new(std::cell::Cell::new(0u8));
    let pending1 = core
        .sync(0)
        .map_err(|e| format!("pipewire sync failed: {e}"))?;
    let pending1 = Rc::new(std::cell::Cell::new(pending1.seq()));

    let done_for_cb = done.clone();
    let stage_for_cb = stage.clone();
    let pending1_for_cb = pending1.clone();
    let loop_for_cb = main_loop.clone();
    let core_weak = core.downgrade();
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id != pw::core::PW_ID_CORE {
                return;
            }
            let seq = seq.seq();
            match stage_for_cb.get() {
                0 if seq == pending1_for_cb.get() => {
                    // 1 段目完了 → メタデータ property を待つため 2 段目の sync を打つ。
                    stage_for_cb.set(1);
                    if let Some(core) = core_weak.upgrade() {
                        match core.sync(0) {
                            Ok(p) => pending1_for_cb.set(p.seq()),
                            Err(_) => {
                                // 2 段目を打てない場合はここで打ち切る。
                                done_for_cb.set(true);
                                loop_for_cb.quit();
                            }
                        }
                    } else {
                        done_for_cb.set(true);
                        loop_for_cb.quit();
                    }
                }
                1 if seq == pending1_for_cb.get() => {
                    // 2 段目完了 → 列挙終了。
                    done_for_cb.set(true);
                    loop_for_cb.quit();
                }
                _ => {}
            }
        })
        .register();

    // done が立つ（= 2 段の往復完了）まで回す。done が来ないまま run() が即時 return を
    // 繰り返すと（spurious quit 等）タイトループ/ハングになるので、デッドラインで打ち切って
    // 収集済み分を返す。列挙は best-effort で、揃わなくても panic/ハングはさせない。
    let deadline = std::time::Instant::now();
    while !done.get() {
        main_loop.run();
        if deadline.elapsed().as_millis() >= ENUMERATE_DEADLINE_MS {
            // done が立たないまま上限超過。打ち切って収集済みを返す。
            break;
        }
    }

    // 収集した生ノードから DeviceInfo を組み立てる。
    let state = state.borrow();
    let mut out = Vec::with_capacity(state.nodes.len());
    for n in &state.nodes {
        let is_loopback = n.media_class == "Audio/Sink";
        let source_kind = if is_loopback {
            SourceKind::SystemLoopback
        } else {
            SourceKind::Mic
        };
        let is_default = if is_loopback {
            state.default_sink.as_deref() == Some(n.node_name.as_str())
        } else {
            state.default_source.as_deref() == Some(n.node_name.as_str())
        };
        out.push(DeviceInfo {
            id: n.node_name.clone(),
            name: n.description.clone(),
            source_kind,
            // 取れなければ要求ネイティブ（48000/2）を既定にする。
            sample_rate: n.rate.unwrap_or(NATIVE_RATE),
            channels: n.channels.unwrap_or(NATIVE_CHANNELS),
            is_loopback,
            is_default,
        });
    }
    Ok(out)
}

/// PipeWire の `default.audio.{sink,source}` メタデータ値（JSON `{"name":"..."}`）から
/// `name` を取り出す。外部 JSON crate を足したくないので簡易抽出。値が想定外なら `None`。
fn extract_json_name(value: &str) -> Option<String> {
    // `"name"` キーの後の最初の文字列リテラルを取る。空白・コロンを飛ばす。
    let after_key = value.split("\"name\"").nth(1)?;
    let after_colon = after_key.split(':').nth(1)?;
    // 最初の `"` から次の `"` までを抜く。
    let start = after_colon.find('"')? + 1;
    let rest = &after_colon[start..];
    let end = rest.find('"')?;
    let name = &rest[..end];
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

// ============================================================================
// デバイス着脱監視（ホットプラグ通知 / `watch_devices()` の Linux/PipeWire 分）
// ============================================================================

/// PipeWire レジストリを永続的に監視して、デバイスの着脱（ホットプラグ）を
/// [`DeviceEvent`] として配信する watcher。
///
/// # [`PwSystemBackend`] / `enumerate_pw` との違い
///
/// [`PwSystemBackend`] と同じく専用スレッド 1 本所有だが、性質が違う:
/// - 短命でなく永続: `enumerate_pw` は `core.sync` の `done` で `quit()` して即終了するが、
///   こちらは `done` でも `quit()` せず回し続け、registry の `global` / `global_remove` を
///   [`stop`](Self::stop) まで受け取り続ける。
/// - RawSink 無し: 音声は録らず、registry の global/global_remove だけを見る。
///
/// `MainLoop` / `Context` / `Core` / `Registry` は `!Send` なので専用スレッド
/// （`flexaudio-pw-watch`）に閉じ込め、本体は `Send` なものだけ持つ（配信キュー
/// [`Arc<Mutex<VecDeque>>`]・停止フラグ・停止用 [`pipewire::channel::Sender`]・
/// [`JoinHandle`]）。
///
/// # 配信されるイベント
/// - [`DeviceEvent::Added`]: 初期スキャン完了後に出現した Audio/Sink|Source ノード。
///   初期スキャン中に既に存在したノードは登録だけして配信しない。
/// - [`DeviceEvent::Removed`]: 監視中に消えたノード（id = `node.name`）。
/// - [`DeviceEvent::DefaultChanged`]: 既定 sink / source の切替（default メタデータ監視）。
///
/// # PipeWire 不在
/// PipeWire デーモン不在・接続失敗時は [`start`](Self::start) が [`Error::Backend`] を
/// 返す（panic しない）。facade 層がこれを no-op 縮退として握る（着脱監視は変化が来なければ
/// 何も配信しなくてよい）。PipeWire セッションはあるが空、のときは正常に回る。
///
/// ```no_run
/// use flexaudio_os_linux::PwDeviceWatcher;
///
/// // PipeWire 不在なら Err（facade が NoopWatcher へ縮退）。
/// if let Ok(mut watcher) = PwDeviceWatcher::start() {
///     while let Some(ev) = watcher.poll_event() {
///         println!("device event: {ev:?}");
///     }
///     watcher.stop();
/// }
/// ```
pub struct PwDeviceWatcher {
    /// 配信キュー（着脱は低頻度・取りこぼし不可なので無制限）。`Send`。
    /// 監視スレッドのコールバックが push し、[`poll_event`](Self::poll_event) が pop する。
    events: Arc<Mutex<VecDeque<DeviceEvent>>>,
    /// 監視中フラグ（二重 start ガード／drop 判定用）。`Send`。
    running: Arc<AtomicBool>,
    /// 監視スレッドへ停止を伝える送信端。[`start`](Self::start) で `Some`。
    /// [`PwSystemBackend`] と同じ [`Terminate`] を使う。
    stop_tx: Option<pw::channel::Sender<Terminate>>,
    /// 監視スレッドのハンドル。[`start`](Self::start) で `Some`。
    handle: Option<JoinHandle<()>>,
}

impl PwDeviceWatcher {
    /// 監視を開始する。専用スレッド上で `MainLoop` + `Context` + `Core` + `Registry` を
    /// 生成し、registry に `global` / `global_remove` リスナを張って初期スキャンを終える
    /// ところまでをセットアップとし、成否を同期返却する。成功後はスレッドが `run()` で
    /// 回り続け、着脱イベントを配信キューへ push する。
    ///
    /// PipeWire デーモン不在・接続失敗は [`Error::Backend`] を返す（panic しない）。
    pub fn start() -> Result<Self> {
        // 配信キューは start 前に作り、セットアップへ clone して渡す。
        let events: Arc<Mutex<VecDeque<DeviceEvent>>> = Arc::new(Mutex::new(VecDeque::new()));

        // 監視スレッドへの停止チャネル（受信端は loop に attach する）。
        let (stop_tx, stop_rx) = pw::channel::channel::<Terminate>();
        // セットアップ成否を start() へ同期返却するチャネル
        // （registry リスナ登録 + 初期スキャン完了まで成功なら Ok）。
        let (ready_tx, ready_rx) = mpsc::channel::<std::result::Result<(), String>>();

        let running = Arc::new(AtomicBool::new(true));

        let events_for_thread = events.clone();
        let handle = thread::Builder::new()
            .name("flexaudio-pw-watch".into())
            .spawn(move || {
                run_watch_loop(events_for_thread, stop_rx, &ready_tx);
            })
            .map_err(|e| Error::Backend(format!("spawn pipewire watch thread: {e}")))?;

        // セットアップ結果を待つ。ready を送らずスレッドが終了した場合も失敗扱い。
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                events,
                running,
                stop_tx: Some(stop_tx),
                handle: Some(handle),
            }),
            Ok(Err(msg)) => {
                // セットアップ失敗（pipewire 不在・connect/registry 失敗等）。
                // スレッドは既に return しているので join して片付ける。
                running.store(false, Ordering::SeqCst);
                let _ = handle.join();
                Err(Error::Backend(msg))
            }
            Err(_) => {
                // ready を一度も送らずスレッドが消えた（想定外パニック等）。
                running.store(false, Ordering::SeqCst);
                let _ = handle.join();
                Err(Error::Backend(
                    "pipewire watch thread terminated before signaling readiness".into(),
                ))
            }
        }
    }

    /// 配信キューから次のホットプラグイベントを 1 つ取り出す（無ければ `None`）。
    /// 非ブロッキング。lock 失敗時も panic せず `None`。
    pub fn poll_event(&mut self) -> Option<DeviceEvent> {
        self.events.lock().ok().and_then(|mut q| q.pop_front())
    }

    /// 監視を停止する（二重 stop / 未 start 後の stop に安全）。
    ///
    /// [`PwSystemBackend::stop`] と同じく、監視スレッドへ `Terminate` を送ると、loop に
    /// attach 済みの受信端コールバックがスレッド自身から `main_loop.quit()` を呼び、
    /// `run()` を抜ける。`join()` で破棄完了まで待つ。
    pub fn stop(&mut self) {
        // 二重 stop / 未 start に安全。
        if !self.running.swap(false, Ordering::SeqCst) {
            // 既に停止済み or 未起動。念のため残骸を join。
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
            self.stop_tx = None;
            return;
        }

        // 監視スレッドへ停止を通知（受信端コールバックが loop.quit() を呼ぶ）。
        // 失敗（受信端消失）は無視（既に終わっている）。
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(Terminate);
        }

        // run() を抜けてスレッドが終了するのを待つ。終了時に Registry→Core→Context→
        // MainLoop が drop 順に破棄される（すべて監視スレッド上で）。
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for PwDeviceWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 監視ループスレッド全体で共有するローカル状態（`!Send`・スレッド内に閉じる）。
#[derive(Default)]
struct WatchState {
    /// registry の global id → 配信用 [`DeviceInfo`] の逆引き表。
    /// `global_remove` は数値 id しか渡さないため、この表で `node.name` を引き戻す。
    by_global_id: std::collections::HashMap<u32, DeviceInfo>,
    /// 初期スキャン（最初の二段 sync→done バリア）が完了したか。
    /// `false` の間に来た global は登録だけして `Added` を配信しない。
    initial_scan_done: bool,
    /// 既定 sink の `node.name`（`default.audio.sink` メタデータから）。
    /// 初期スキャン完了後の変化を [`DeviceEvent::DefaultChanged`] として配信する。
    default_sink: Option<String>,
    /// 既定 source の `node.name`（`default.audio.source` メタデータから）。
    default_source: Option<String>,
}

/// PipeWire 監視ループスレッド本体。
///
/// `MainLoop`/`Context`/`Core`/`Registry`（いずれも `!Send`）をこの関数の中だけで生成・実行・
/// 破棄する。セットアップ完了/失敗を `ready_tx` で呼び出し元へ返し、成功時は
/// `main_loop.run()` で停止指示（[`Terminate`]）まで回る。
fn run_watch_loop(
    events: Arc<Mutex<VecDeque<DeviceEvent>>>,
    stop_rx: pw::channel::Receiver<Terminate>,
    ready_tx: &mpsc::Sender<std::result::Result<(), String>>,
) {
    // セットアップ（接続・registry リスナ登録・初期スキャン）は別関数。
    // 戻り値は run 中ずっと生かす（drop すると監視が止まる）。
    let (main_loop, _core, _registry, _listeners) = match setup_watch(events) {
        Ok(t) => t,
        Err(msg) => {
            // セットアップ失敗を通知して終了（panic しない）。
            let _ = ready_tx.send(Err(msg));
            return;
        }
    };

    // 停止チャネルの受信端を loop に attach。Terminate 受信で quit()。
    // quit() は loop 駆動のコールバック内、つまりこのスレッド上から呼ばれる。
    let main_loop_for_quit = main_loop.clone();
    let _attached = stop_rx.attach(main_loop.loop_(), move |_terminate| {
        main_loop_for_quit.quit();
    });

    // セットアップ成功を通知。以後は run() がブロックし、着脱イベントを配信し続ける。
    if ready_tx.send(Ok(())).is_err() {
        // 呼び出し元が消えている（start が drop 済み等）。起動しない。
        return;
    }

    // Terminate 受信 or プロセス終了まで回る。enumerate_pw と違い done では quit しないので
    // 永続に回る。
    main_loop.run();
    // 抜けると _attached → _listeners → _registry → _core → main_loop の順（宣言の逆順）で
    // drop され、PipeWire リソースがこのスレッド上で破棄される。
}

/// 監視 watcher が run 中ずっと保持する所有物。drop すると監視が止まるので、
/// `run_watch_loop` のスタックに置いておく。
///
/// - `MainLoopRc`: `run()`/`quit()` の主体。
/// - `CoreRc`: registry / sync の親（downgrade して done コールバックで使う）。
/// - `RegistryRc`: registry プロキシ本体。
/// - リスナ群: registry リスナ・core(done) リスナ・bind した default メタデータの
///   プロキシ＋リスナ。drop でコールバックが外れるので Box で型消去して保持する。
#[allow(clippy::type_complexity)]
type WatchKeep = (
    pw::main_loop::MainLoopRc,
    pw::core::CoreRc,
    pw::registry::RegistryRc,
    WatchListeners,
);

/// bind した default メタデータのプロキシ＋リスナ 1 組（drop でコールバックが外れる）。
/// [`enumerate_pw`] のローカル `MetaKeep` と同型。
type MetaKeepEntry = (Box<dyn pw::proxy::ProxyT>, Box<dyn pw::proxy::Listener>);

/// `MetaKeepEntry` の保管庫（監視スレッド内で Rc 共有・`!Send`）。
type MetaKeepStore = std::rc::Rc<std::cell::RefCell<Vec<MetaKeepEntry>>>;

/// 監視で生かしておくリスナ群（drop でコールバックが外れる）。
struct WatchListeners {
    /// registry の global/global_remove リスナ。
    _registry_listener: pw::registry::Listener,
    /// core の done リスナ（初期スキャンの二段バリア完了検知）。
    _core_listener: pw::core::Listener,
    /// global コールバック内で bind した default メタデータのプロキシ＋リスナ保管庫
    /// （[`enumerate_pw`] と同じ型。Rc 共有で監視スレッド内に閉じる）。
    _meta_keep: MetaKeepStore,
}

/// PipeWire 監視のセットアップ一式。失敗は `Err(String)`（panic しない）。
///
/// [`enumerate_pw`] の registry global 抽出ロジックと二段 sync→done バリアを流用するが、
/// `done` では `quit()` せず初期スキャン完了フラグを立てるだけにし、以後は永続的に
/// global/global_remove を受け続ける。
#[allow(clippy::type_complexity)]
fn setup_watch(
    events: Arc<Mutex<VecDeque<DeviceEvent>>>,
) -> std::result::Result<WatchKeep, String> {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    pw_init_once();

    let main_loop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| format!("create pipewire main loop failed: {e}"))?;
    let context = pw::context::ContextRc::new(&main_loop, None)
        .map_err(|e| format!("create pipewire context failed: {e}"))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| format!("connect to pipewire daemon failed (is PipeWire running?): {e}"))?;
    let registry = core
        .get_registry_rc()
        .map_err(|e| format!("get pipewire registry failed: {e}"))?;

    // 監視スレッド内ローカル状態（!Send）。各クロージャへ Rc で共有する。
    let state = Rc::new(RefCell::new(WatchState::default()));
    // 配信キュー（events: Arc<Mutex<VecDeque>>）は各クロージャへ clone して move する。

    // default メタデータの property リスナを生かしておく保管庫
    // （enumerate_pw と同じ型。MetaKeepStore = Rc<RefCell<Vec<MetaKeepEntry>>>）。
    let meta_keep: MetaKeepStore = Rc::new(RefCell::new(Vec::new()));

    // registry global / global_remove リスナ。
    let state_for_global = state.clone();
    let events_for_global = events.clone();
    let registry_for_global = registry.clone();
    let meta_keep_for_global = meta_keep.clone();
    let state_for_remove = state.clone();
    let events_for_remove = events.clone();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            // FFI 越えの panic は UB なので本体を catch_unwind で包む。
            let _ = catch_unwind(AssertUnwindSafe(|| {
                let Some(props) = global.props else {
                    return;
                };
                match global.type_ {
                    pw::types::ObjectType::Node => {
                        // enumerate_pw と同じ抽出ロジック。
                        // media.class が Audio/Sink|Source のノードだけ拾う。
                        let media_class = props.get(*pw::keys::MEDIA_CLASS).unwrap_or("");
                        if media_class != "Audio/Sink" && media_class != "Audio/Source" {
                            return;
                        }
                        let node_name = props.get(*pw::keys::NODE_NAME).unwrap_or("");
                        if node_name.is_empty() {
                            // 安定キーが無いノードは扱えない（スキップ）。
                            return;
                        }
                        let description = props
                            .get(*pw::keys::NODE_DESCRIPTION)
                            .filter(|s| !s.is_empty())
                            .unwrap_or(node_name);
                        let rate = props.get("audio.rate").and_then(|s| s.parse::<u32>().ok());
                        let channels = props
                            .get(*pw::keys::AUDIO_CHANNELS)
                            .and_then(|s| s.parse::<u16>().ok());

                        let is_loopback = media_class == "Audio/Sink";
                        let source_kind = if is_loopback {
                            SourceKind::SystemLoopback
                        } else {
                            SourceKind::Mic
                        };
                        // is_default は既知の default メタデータ値と突き合わせる。初期スキャン
                        // 中はメタデータがまだ来ていないこともあり、その場合は false になる
                        // （後で DefaultChanged が訂正する）。
                        let mut st = state_for_global.borrow_mut();
                        let is_default = if is_loopback {
                            st.default_sink.as_deref() == Some(node_name)
                        } else {
                            st.default_source.as_deref() == Some(node_name)
                        };

                        let info = DeviceInfo {
                            id: node_name.to_string(),
                            name: description.to_string(),
                            source_kind,
                            // 取れなければ要求ネイティブ（48000/2）を既定にする（enumerate_pw と同じ）。
                            sample_rate: rate.unwrap_or(NATIVE_RATE),
                            channels: channels.unwrap_or(NATIVE_CHANNELS),
                            is_loopback,
                            is_default,
                        };
                        st.by_global_id.insert(global.id, info.clone());
                        let initial_scan_done = st.initial_scan_done;
                        drop(st);

                        // 初期スキャン中は登録だけ。完了後の出現だけ Added を配信する。
                        if initial_scan_done {
                            enqueue_event(&events_for_global, DeviceEvent::Added(info));
                        }
                    }
                    pw::types::ObjectType::Metadata => {
                        // 既定 sink/source を保持する "default" メタデータだけ bind する
                        // （enumerate_pw と同じ）。
                        let meta_name = props.get("metadata.name").unwrap_or("");
                        if meta_name != "default" {
                            return;
                        }
                        let metadata: pw::metadata::Metadata =
                            match registry_for_global.bind(global) {
                                Ok(m) => m,
                                Err(_) => return,
                            };
                        let state_for_meta = state_for_global.clone();
                        let events_for_meta = events_for_global.clone();
                        let listener = metadata
                            .add_listener_local()
                            .property(move |_subject, key, _type, value| {
                                // property コールバックも FFI 越えなので catch_unwind で包む。
                                catch_unwind(AssertUnwindSafe(|| {
                                    // value は JSON（例: {"name":"alsa_output...."}）。name を抜く。
                                    if let (Some(key), Some(value)) = (key, value) {
                                        let new_name = extract_json_name(value);
                                        let mut st = state_for_meta.borrow_mut();
                                        if key == "default.audio.sink" {
                                            if st.default_sink != new_name {
                                                st.default_sink = new_name.clone();
                                                // 初期スキャン完了後の変化のみ配信。
                                                if st.initial_scan_done {
                                                    if let Some(id) = new_name {
                                                        drop(st);
                                                        enqueue_event(
                                                            &events_for_meta,
                                                            DeviceEvent::DefaultChanged {
                                                                kind: SourceKind::SystemLoopback,
                                                                id,
                                                            },
                                                        );
                                                    }
                                                }
                                            }
                                        } else if key == "default.audio.source"
                                            && st.default_source != new_name
                                        {
                                            st.default_source = new_name.clone();
                                            if st.initial_scan_done {
                                                if let Some(id) = new_name {
                                                    drop(st);
                                                    enqueue_event(
                                                        &events_for_meta,
                                                        DeviceEvent::DefaultChanged {
                                                            kind: SourceKind::Mic,
                                                            id,
                                                        },
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }))
                                .ok();
                                0
                            })
                            .register();
                        meta_keep_for_global
                            .borrow_mut()
                            .push((Box::new(metadata), Box::new(listener)));
                    }
                    _ => {}
                }
            }));
        })
        .global_remove(move |id| {
            // FFI 越えの panic は UB なので本体を catch_unwind で包む。
            let _ = catch_unwind(AssertUnwindSafe(|| {
                // 逆引き表にヒットしたノードだけ Removed を配信する。表に無い id は無視する
                // （Metadata 等の非ノード global の除去も来るが、表に無いので素通り）。
                let removed = state_for_remove.borrow_mut().by_global_id.remove(&id);
                if let Some(info) = removed {
                    enqueue_event(&events_for_remove, DeviceEvent::Removed { id: info.id });
                }
            }));
        })
        .register();

    // 二段 sync→done バリアで初期スキャン完了を検知する（enumerate_pw と同じ）。
    // ただし done では quit() せず initial_scan_done を立てるだけ。2 段目の done を受けた
    // 時点で初期 global 列挙と default メタデータの初期 property ダンプが揃っているので、
    // 以後の global/global_remove/property 変化をユーザー起因の着脱・既定変更として配信できる。
    let stage = Rc::new(Cell::new(0u8));
    let pending = core
        .sync(0)
        .map_err(|e| format!("pipewire sync failed: {e}"))?;
    let pending = Rc::new(Cell::new(pending.seq()));

    let stage_for_cb = stage.clone();
    let pending_for_cb = pending.clone();
    let state_for_done = state.clone();
    let loop_for_done = main_loop.clone();
    let core_weak = core.downgrade();
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id != pw::core::PW_ID_CORE {
                return;
            }
            let seq = seq.seq();
            match stage_for_cb.get() {
                0 if seq == pending_for_cb.get() => {
                    // 1 段目完了 → メタデータ property を待つため 2 段目の sync を打つ。
                    stage_for_cb.set(1);
                    if let Some(core) = core_weak.upgrade() {
                        match core.sync(0) {
                            Ok(p) => pending_for_cb.set(p.seq()),
                            Err(_) => {
                                // 2 段目を打てない場合は初期スキャン完了とみなす。
                                stage_for_cb.set(2);
                                state_for_done.borrow_mut().initial_scan_done = true;
                                loop_for_done.quit();
                            }
                        }
                    } else {
                        stage_for_cb.set(2);
                        state_for_done.borrow_mut().initial_scan_done = true;
                        loop_for_done.quit();
                    }
                }
                1 if seq == pending_for_cb.get() => {
                    // 2 段目完了 → 初期スキャン終了。ここで quit() を呼ぶのは初期スキャン用の
                    // run()（下の while ループ）を抜けるためだけ。永続監視の run() は
                    // run_watch_loop 側で回す。stage を 2 に進めてあるので、以後 done が来ても
                    // この match はどの腕にも当たらず quit() は二度と呼ばれない。
                    stage_for_cb.set(2);
                    state_for_done.borrow_mut().initial_scan_done = true;
                    loop_for_done.quit();
                }
                _ => {}
            }
        })
        .register();

    // 初期スキャン完了（= 2 段の往復完了）まで run() を回す。done で initial_scan_done を
    // 立て quit() するので、enumerate_pw と同じく必ず抜ける。これで初期 global 列挙と
    // default メタデータ初期ダンプが揃った状態にしてから返す。永続的な監視 run() は
    // run_watch_loop が回す。stage が 2 に達した後は done で quit されないので、その run() は
    // 止まらない。
    while !state.borrow().initial_scan_done {
        main_loop.run();
    }

    Ok((
        main_loop,
        core,
        registry,
        WatchListeners {
            _registry_listener,
            _core_listener,
            _meta_keep: meta_keep,
        },
    ))
}

/// 配信キューへイベントを 1 つ積む。lock 失敗時は何もしない（panic しない）。
///
/// 消費側が `poll_event` を長く呼ばない、あるいはデバイスが連続着脱すると `VecDeque` が
/// 際限なく膨らむ。これを防ぐため [`MAX_WATCH_EVENTS`] を上限にし、超過時は最古を捨てて
/// 新規を積む。
fn enqueue_event(events: &Arc<Mutex<VecDeque<DeviceEvent>>>, ev: DeviceEvent) {
    if let Ok(mut q) = events.lock() {
        // 上限に達していたら最古を捨ててから積む。
        while q.len() >= MAX_WATCH_EVENTS {
            q.pop_front();
        }
        q.push_back(ev);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio_core::raw_ring::raw_ring;

    /// [`CaptureBackend`] 契約どおり `PwSystemBackend: Send` であること
    /// （PipeWire の `!Send` を専用スレッドへ閉じ込められている証左）。
    /// コンパイルが通れば成立。
    #[test]
    fn backend_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<PwSystemBackend>();
    }

    /// 構築直後にネイティブフォーマットが固定契約どおり (48000, 2) であること。
    #[test]
    fn native_format_is_48k_stereo() {
        let be = PwSystemBackend::new(false, None);
        assert_eq!(be.native_format(), (NATIVE_RATE, NATIVE_CHANNELS));
        assert_eq!(be.native_format(), (48_000, 2));
        assert!(!be.exclude_self());
    }

    /// 未 start での stop / 二重 stop が安全（panic しない）。
    #[test]
    fn stop_without_start_is_safe() {
        let mut be = PwSystemBackend::new(false, None);
        be.stop();
        be.stop();
    }

    /// system の `exclude_self=true` はプロセス Exclude 機構を流用して実装してある。
    /// `start` は `Unsupported` を返さず、PipeWire 不在のヘッドレス環境では
    /// [`Error::Backend`]、PipeWire セッションがある環境では `Ok(())`（待機成功）になる。
    /// どちらでも panic しないこと・Ok なら `stop()` まで一巡できることを確認する。
    #[test]
    fn system_exclude_self_is_graceful() {
        let (prod, _cons) = raw_ring(1 << 16);
        let sink = RawSink::new(prod, NATIVE_RATE, NATIVE_CHANNELS);
        let mut be = PwSystemBackend::new(true, None);
        assert!(be.exclude_self());
        match be.start(sink) {
            Ok(()) => {
                // PipeWire セッションがある環境。自分以外を fan-in する Exclude 機構へ
                // 委ね、対象が未出現でも待機成功する。停止まで一巡できること。
                be.stop();
            }
            Err(Error::Backend(_)) => {
                // PipeWire 不在/registry 失敗: 想定内。panic していないことが要点。
            }
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    /// `extract_json_name` が PipeWire のメタデータ値（JSON）から name を抜けること。
    #[test]
    fn extract_json_name_parses_default_metadata_value() {
        assert_eq!(
            extract_json_name(r#"{"name":"alsa_output.pci-0000_00_1f.3.analog-stereo"}"#)
                .as_deref(),
            Some("alsa_output.pci-0000_00_1f.3.analog-stereo")
        );
        // 空白入りでも抜ける。
        assert_eq!(
            extract_json_name(r#"{ "name" : "foo.bar" }"#).as_deref(),
            Some("foo.bar")
        );
        // name キーが無い / 空 / 不正なら None。
        assert_eq!(extract_json_name(r#"{"other":"x"}"#), None);
        assert_eq!(extract_json_name(r#"{"name":""}"#), None);
        assert_eq!(extract_json_name("not json"), None);
    }

    /// `list_devices` は PipeWire が無いヘッドレス環境でも panic せず `Ok(Vec)` を返す
    /// （デーモン不在は「列挙対象なし」= 空 Vec に握る）。デバイスが返った場合は
    /// Sink→SystemLoopback / Source→Mic の整合と id（=node.name）非空を検証する。
    #[test]
    fn list_devices_is_graceful_without_pipewire() {
        let devices = list_devices().expect("list_devices は Err を返さない設計");
        for d in &devices {
            assert!(!d.id.is_empty(), "id（=node.name）は空でない");
            match d.source_kind {
                SourceKind::SystemLoopback => assert!(d.is_loopback, "Sink はループバック"),
                SourceKind::Mic => assert!(!d.is_loopback, "Source はループバックでない"),
                other => panic!("想定外の source_kind: {other:?}"),
            }
            assert!(d.sample_rate > 0);
            assert!(d.channels > 0);
        }
        // 既定 sink / 既定 source はそれぞれ高々 1 つ。
        let default_loopback = devices
            .iter()
            .filter(|d| d.is_default && d.is_loopback)
            .count();
        let default_mic = devices
            .iter()
            .filter(|d| d.is_default && !d.is_loopback)
            .count();
        assert!(default_loopback <= 1);
        assert!(default_mic <= 1);
    }

    /// スモークテスト: `start` は PipeWire/sink が無いヘッドレス環境では
    /// `Err(Error::Backend)` になり得るが panic はしない。Ok（PipeWire と動作中 sink が
    /// ある環境）と Err(Backend) の両方を許容する。
    ///
    /// PipeWire の動くデスクトップ/ラップトップでは Ok になり、`stop()` まで
    /// 一巡できる。実際の音声 end-to-end 検証は下の `#[ignore]` テスト参照。
    #[test]
    fn start_is_graceful_without_pipewire() {
        let (prod, _cons) = raw_ring(1 << 16);
        let sink = RawSink::new(prod, NATIVE_RATE, NATIVE_CHANNELS);
        let mut be = PwSystemBackend::new(false, None);
        match be.start(sink) {
            Ok(()) => {
                // 動作中 PipeWire/sink がある環境。停止まで一巡できること。
                be.stop();
            }
            Err(Error::Backend(_)) => {
                // PipeWire 不在/sink 無し: 想定内。panic していないことが要点。
            }
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    /// 居ない sink を `device_id` で指したときの `start`。PipeWire が動く環境では
    /// その sink が列挙に出ないので [`Error::DeviceNotFound`]。PipeWire 不在環境では
    /// enumerate_pw が Err で握られ、通常経路の接続失敗で [`Error::Backend`] になる。
    /// どちらでも panic しないこと・Ok にはならないことを確認する。
    #[test]
    fn start_with_unknown_device_id_is_not_found_or_backend() {
        let (prod, _cons) = raw_ring(1 << 16);
        let sink = RawSink::new(prod, NATIVE_RATE, NATIVE_CHANNELS);
        let mut be = PwSystemBackend::new(false, Some("flexaudio-no-such-sink-zzz".to_string()));
        match be.start(sink) {
            Err(Error::DeviceNotFound) => {}
            Err(Error::Backend(_)) => {}
            Ok(()) => {
                be.stop();
                panic!("start should not succeed for an unknown device_id");
            }
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    /// 実キャプチャ end-to-end（PipeWire が動くデスクトップ/ラップトップでのみ）。
    ///
    /// 実行方法（ラップトップ等、PipeWire + 何か音を鳴らした状態で）:
    /// ```text
    /// cargo test -p flexaudio-os-linux -- --ignored capture_smoke
    /// ```
    /// 既定 sink の monitor を一定時間録り、サンプルが流れてくる
    /// （overflow か pop で観測）ことを期待する。ヘッドレス環境/CI では音源も
    /// PipeWire も無いため `#[ignore]`。
    #[test]
    #[ignore = "requires a running PipeWire session with audio playing (desktop/laptop)"]
    fn capture_smoke() {
        use std::time::Duration;
        let (prod, mut cons) = raw_ring(1 << 18);
        let sink = RawSink::new(prod, NATIVE_RATE, NATIVE_CHANNELS);
        let mut be = PwSystemBackend::new(false, None);
        be.start(sink)
            .expect("start should succeed on a PipeWire desktop");
        // 録音が回るのを少し待つ。
        thread::sleep(Duration::from_millis(500));
        be.stop();
        // 何らかのサンプルが届いている（無音 sink でも 0.0 サンプルは流れる）。
        let mut out = vec![0.0f32; 1920];
        let got = cons.pop_slice(&mut out);
        assert!(
            got > 0,
            "expected captured samples from the default sink monitor"
        );
    }

    // ------------------------------------------------------------------------
    // PwProcessBackend（プロセス出力ループバック）
    // ------------------------------------------------------------------------

    /// [`CaptureBackend`] 契約どおり `PwProcessBackend: Send` であること
    /// （PipeWire の `!Send` を専用スレッドへ閉じ込められている証左）。
    /// コンパイルが通れば成立。
    #[test]
    fn process_backend_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<PwProcessBackend>();
    }

    /// 構築直後にネイティブフォーマットが固定契約どおり (48000, 2) であること。
    /// PID / mode の保持も確認する。
    #[test]
    fn process_native_format_is_48k_stereo() {
        let be = PwProcessBackend::new(4242, ProcessMode::Exclude);
        assert_eq!(be.native_format(), (NATIVE_RATE, NATIVE_CHANNELS));
        assert_eq!(be.native_format(), (48_000, 2));
        // 構築引数が保持されること。
        assert_eq!(be.target_pid(), 4242);
        assert_eq!(be.mode(), ProcessMode::Exclude);
        let be2 = PwProcessBackend::new(1, ProcessMode::Include);
        assert_eq!(be2.mode(), ProcessMode::Include);
    }

    /// 未 start での stop / 二重 stop が安全（panic しない）。
    #[test]
    fn process_stop_without_start_is_safe() {
        let mut be = PwProcessBackend::new(1234, ProcessMode::Include);
        be.stop();
        be.stop();
    }

    /// process の [`ProcessMode::Exclude`] は対象 PID 以外を fan-in して録る。
    /// `start` は `Unsupported` を返さず、PipeWire 不在のヘッドレス環境では [`Error::Backend`]、
    /// PipeWire セッションがある環境では `Ok(())`（待機成功）になる。どちらでも panic
    /// しないこと・Ok なら二重 start no-op + stop + 二重 stop まで一巡できることを確認する。
    #[test]
    fn process_exclude_mode_is_graceful() {
        let (prod, _cons) = raw_ring(1 << 16);
        let sink = RawSink::new(prod, NATIVE_RATE, NATIVE_CHANNELS);
        let mut be = PwProcessBackend::new(u32::MAX, ProcessMode::Exclude);
        match be.start(sink) {
            Ok(()) => {
                // PipeWire セッションがある環境。対象 PID 以外を fan-in する Exclude
                // 機構へ委ね、待機成功する。二重 start に安全（no-op で Ok）。
                let (prod2, _cons2) = raw_ring(1 << 16);
                let sink2 = RawSink::new(prod2, NATIVE_RATE, NATIVE_CHANNELS);
                assert!(be.start(sink2).is_ok());
                // 停止まで一巡できること（リンク前でも安全に破棄）。
                be.stop();
                // 二重 stop も安全。
                be.stop();
            }
            Err(Error::Backend(_)) => {
                // PipeWire 不在/registry 失敗: 想定内。panic していないことが要点。
            }
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    /// `resolve_node_pid` を検証する（PipeWire 非依存）。
    ///
    /// 実機 pw-dump で確認した事実: PID は Client に載り、ノードは `client.id` で Client を
    /// 指すだけ。なので PID 解決は二段（node → client.id → Client の PID）。Client と Node は
    /// どちらが先に来ても、各到着で再評価すれば正しく解決できる。これを `client_pid` 表に
    /// 値を入れる前後で確認する。
    #[test]
    fn resolve_node_pid_via_client_table() {
        use std::collections::HashMap;

        // pw-cat の実例: node.id=62 が client.id=60 を指し、client.id=60 の Client が
        // application.process.id=13394 を持つ。
        let node = NodeEntry {
            owning_client_id: Some(60),
            app_pid: None,
        };

        // --- Node が先に来て Client がまだ表に無い状態 → 未解決（None）。
        let mut client_pid: HashMap<u32, u32> = HashMap::new();
        assert_eq!(
            resolve_node_pid(&node, &client_pid),
            None,
            "client.id に対応する Client がまだ無ければ PID 未解決"
        );

        // --- 後から Client(global id=60, pid=13394) が到着して表へ → 解決される。
        client_pid.insert(60, 13394);
        assert_eq!(
            resolve_node_pid(&node, &client_pid),
            Some(13394),
            "client.id=60 → Client の pid=13394 を二段で解決"
        );

        // --- client.id が無いノードは（直 PID も無い限り）解決不能。
        let orphan = NodeEntry {
            owning_client_id: None,
            app_pid: None,
        };
        assert_eq!(resolve_node_pid(&orphan, &client_pid), None);

        // ノード自身に application.process.id が載れば Client を介さず直解決でき、
        // client_pid 表が空でも解決できる（将来構成への備え）。
        let node_with_pid = NodeEntry {
            owning_client_id: Some(99), // 表に無い client.id でも
            app_pid: Some(424242),
        };
        let empty: HashMap<u32, u32> = HashMap::new();
        assert_eq!(
            resolve_node_pid(&node_with_pid, &empty),
            Some(424242),
            "ノード自身の PID を優先して直解決"
        );

        // --- 別 client.id のノードは別 PID（取り違えないこと）。
        let other_node = NodeEntry {
            owning_client_id: Some(61),
            app_pid: None,
        };
        // client 61 は未登録なので None、登録すればその PID。
        assert_eq!(resolve_node_pid(&other_node, &client_pid), None);
        client_pid.insert(61, 555);
        assert_eq!(resolve_node_pid(&other_node, &client_pid), Some(555));
        // node(client 60) の解決は影響を受けない。
        assert_eq!(resolve_node_pid(&node, &client_pid), Some(13394));
    }

    /// `pair_ports` のチャンネル対応付けを検証する（PipeWire 非依存）。
    #[test]
    fn pair_ports_maps_channels() {
        // ステレオ→ステレオ: FL→FL / FR→FR（チャンネル名一致）。
        // 出力ポート: id 10=FL, 11=FR。入力ポート: id 20=FL, 21=FR。
        let out = vec![(10u32, "FL".to_string()), (11u32, "FR".to_string())];
        let inp = vec![(20u32, "FL".to_string()), (21u32, "FR".to_string())];
        let mut pairs = pair_ports(&out, &inp);
        pairs.sort();
        assert_eq!(pairs, vec![(10, 20), (11, 21)], "FL→FL / FR→FR");

        // 入力の並びが逆でもチャンネル名で正しく対応付く。
        let inp_rev = vec![(21u32, "FR".to_string()), (20u32, "FL".to_string())];
        let mut pairs = pair_ports(&out, &inp_rev);
        pairs.sort();
        assert_eq!(pairs, vec![(10, 20), (11, 21)], "並び逆でも FL→FL / FR→FR");

        // モノラル出力 → ステレオ入力: 単一出力を FL/FR 両方へ複製。
        let mono_out = vec![(30u32, "MONO".to_string())];
        let stereo_in = vec![(40u32, "FL".to_string()), (41u32, "FR".to_string())];
        let mut pairs = pair_ports(&mono_out, &stereo_in);
        pairs.sort();
        assert_eq!(pairs, vec![(30, 40), (30, 41)], "モノは FL/FR へ複製");

        // チャンネル名が取れない（空）出力 → 順序フォールバック。
        let out_noch = vec![(50u32, String::new()), (51u32, String::new())];
        let in_noch = vec![(60u32, String::new()), (61u32, String::new())];
        let pairs = pair_ports(&out_noch, &in_noch);
        // 2 ポート同士が 1 対 1 で対応する（各入力は高々 1 回）。
        assert_eq!(pairs.len(), 2);
        let ins: std::collections::HashSet<u32> = pairs.iter().map(|(_, i)| *i).collect();
        assert_eq!(ins.len(), 2, "各入力ポートは高々 1 回");

        // 空集合は空リンク（どちらかが未出現ならリンクしない）。
        assert!(pair_ports(&[], &inp).is_empty());
        assert!(pair_ports(&out, &[]).is_empty());

        // 一致するチャンネルが片方にしか無い場合でも、モノ複製でなく順序で埋める。
        // 出力 FL のみ、入力 FR のみ（名前不一致）→ 順序フォールバックで 1 対応。
        let out_fl = vec![(70u32, "FL".to_string())];
        let in_fr = vec![(80u32, "FR".to_string())];
        // 出力 1 ポートなのでモノ複製規則が走り、残り入力へ複製される。
        let pairs = pair_ports(&out_fl, &in_fr);
        assert_eq!(pairs, vec![(70, 80)], "出力1ポートは残り入力へ複製");
    }

    /// スモークテスト: プロセスキャプチャの `start` は PipeWire 不在/registry 取得
    /// 失敗のヘッドレス環境では `Err(Error::Backend)` になり得るが panic はしない。
    /// PipeWire セッションがある環境では、対象 PID が未出現でも成功扱いで待機する
    /// （registry が取れれば成功し、出現時にリンクするため）。Ok の場合は対象 PID が
    /// 鳴っていなくても `stop()` まで一巡できること（破棄が安全）を確認する。
    #[test]
    fn process_start_is_graceful_without_pipewire() {
        let (prod, _cons) = raw_ring(1 << 16);
        let sink = RawSink::new(prod, NATIVE_RATE, NATIVE_CHANNELS);
        // 実在しないであろう PID。出現しなくても start は待機成功し得る（Include）。
        let mut be = PwProcessBackend::new(u32::MAX, ProcessMode::Include);
        match be.start(sink) {
            Ok(()) => {
                // PipeWire セッションがある環境。対象 PID 未出現でも待機成功。
                // 二重 start に安全（no-op で Ok）。
                let (prod2, _cons2) = raw_ring(1 << 16);
                let sink2 = RawSink::new(prod2, NATIVE_RATE, NATIVE_CHANNELS);
                assert!(be.start(sink2).is_ok());
                // 停止まで一巡できること（リンク前でも安全に破棄）。
                be.stop();
                // 二重 stop も安全。
                be.stop();
            }
            Err(Error::Backend(_)) => {
                // PipeWire 不在/registry 失敗: 想定内。panic していないことが要点。
            }
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    /// 実キャプチャ end-to-end（PipeWire が動くデスクトップ/ラップトップでのみ）。
    ///
    /// 実行方法（ラップトップ等、PipeWire + 対象 PID で何か音を鳴らした状態で）:
    /// ```text
    /// # 例: speaker-test を鳴らして PID を取る
    /// speaker-test -t sine -f 1000 -c 2 &  # → PID を控える
    /// FLEXAUDIO_TEST_PID=<PID> \
    ///   cargo test -p flexaudio-os-linux -- --ignored process_capture_smoke
    /// ```
    /// 対象 PID のアプリ出力ポートへ link-factory でリンクし、サンプルが流れてくることを
    /// 期待する。`FLEXAUDIO_TEST_PID` 未指定ならスキップ（PID が分からないため）。
    /// ヘッドレス環境/CI では PipeWire も音源も無いため `#[ignore]`。
    #[test]
    #[ignore = "requires a running PipeWire session with the target PID playing audio (set FLEXAUDIO_TEST_PID)"]
    fn process_capture_smoke() {
        use std::time::Duration;
        let Ok(pid_str) = std::env::var("FLEXAUDIO_TEST_PID") else {
            eprintln!("FLEXAUDIO_TEST_PID 未指定のためスキップ");
            return;
        };
        let pid: u32 = pid_str.parse().expect("FLEXAUDIO_TEST_PID は u32");
        let (prod, mut cons) = raw_ring(1 << 18);
        let sink = RawSink::new(prod, NATIVE_RATE, NATIVE_CHANNELS);
        let mut be = PwProcessBackend::new(pid, ProcessMode::Include);
        be.start(sink)
            .expect("start should succeed on a PipeWire desktop");
        // リンク確立 + 録音が回るのを少し待つ。
        thread::sleep(Duration::from_millis(800));
        be.stop();
        let mut out = vec![0.0f32; 1920];
        let got = cons.pop_slice(&mut out);
        assert!(
            got > 0,
            "expected captured samples link-factory-linked from PID {pid}"
        );
    }

    // ------------------------------------------------------------------------
    // PwDeviceWatcher（ホットプラグ通知）
    // ------------------------------------------------------------------------

    /// [`PwDeviceWatcher`] が `Send` であること（PipeWire の `!Send` 型を専用
    /// スレッドへ閉じ込められている証左）。コンパイルが通れば成立。
    /// `PwSystemBackend` の同テストに倣う。
    #[test]
    fn watcher_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<PwDeviceWatcher>();
    }

    /// PipeWire 不在のヘッドレス環境でも `start()` が panic しないこと。PipeWire
    /// セッションがあれば `Ok`、無ければ `Err(Backend)` になり得るが、どちらでも panic
    /// していないことが要点（facade が Err を no-op 縮退に握る）。Ok になった場合は stop
    /// まで一巡できること（破棄が安全）も確認する。
    #[test]
    fn watcher_graceful_without_pipewire() {
        match PwDeviceWatcher::start() {
            Ok(mut w) => {
                // PipeWire セッションがある環境。poll_event は非ブロッキングで、
                // 初期スキャン分は抑制済みなので即 None になり得る（出ても問題ない）。
                let _ = w.poll_event();
                w.stop();
            }
            Err(Error::Backend(_)) => {
                // PipeWire 不在: 想定内。panic していないことが要点。
            }
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    /// `start()` に成功した後、`stop()` を二度呼んでも安全（panic しない・二度目は
    /// no-op）。PipeWire 不在で `start()` が Err の環境ではスキップ。
    #[test]
    fn watcher_double_stop_is_safe() {
        if let Ok(mut w) = PwDeviceWatcher::start() {
            w.stop();
            w.stop();
        }
        // start に失敗した環境（PipeWire 不在）では検証対象が無い＝panic しなければ OK。
    }

    /// `enqueue_event` / `poll` 相当のキュー入出力が FIFO で機能すること
    /// （PipeWire 非依存。配信キューのロジックだけを検証する）。
    #[test]
    fn enqueue_and_drain_is_fifo() {
        let events: Arc<Mutex<VecDeque<DeviceEvent>>> = Arc::new(Mutex::new(VecDeque::new()));
        let mic = DeviceInfo {
            id: "mic.a".into(),
            name: "Mic A".into(),
            source_kind: SourceKind::Mic,
            sample_rate: NATIVE_RATE,
            channels: NATIVE_CHANNELS,
            is_loopback: false,
            is_default: false,
        };
        enqueue_event(&events, DeviceEvent::Added(mic.clone()));
        enqueue_event(&events, DeviceEvent::Removed { id: "mic.a".into() });
        enqueue_event(
            &events,
            DeviceEvent::DefaultChanged {
                kind: SourceKind::SystemLoopback,
                id: "sink.x".into(),
            },
        );
        // poll_event 相当（FIFO で取り出す）。
        let mut drained = Vec::new();
        while let Some(ev) = events.lock().unwrap().pop_front() {
            drained.push(ev);
        }
        assert_eq!(
            drained,
            vec![
                DeviceEvent::Added(mic),
                DeviceEvent::Removed { id: "mic.a".into() },
                DeviceEvent::DefaultChanged {
                    kind: SourceKind::SystemLoopback,
                    id: "sink.x".into(),
                },
            ]
        );
    }

    /// `enqueue_event` は配信キューを [`MAX_WATCH_EVENTS`] で上限化し、超過時は最古を捨てて
    /// 新規を積む。上限 + α を積み、長さが上限を超えず最新側が残ることを確認する。
    #[test]
    fn enqueue_event_caps_queue_and_drops_oldest() {
        let events: Arc<Mutex<VecDeque<DeviceEvent>>> = Arc::new(Mutex::new(VecDeque::new()));
        // 上限 + 10 件積む。id を node 番号として埋め込み、どれが残ったか追える。
        let total = MAX_WATCH_EVENTS + 10;
        for i in 0..total {
            enqueue_event(
                &events,
                DeviceEvent::Removed {
                    id: format!("n{i}"),
                },
            );
        }
        let q = events.lock().unwrap();
        // 長さは上限を超えない。
        assert_eq!(q.len(), MAX_WATCH_EVENTS, "キュー長は上限で頭打ち");
        // 最古 10 件（n0..n9）は捨てられ、先頭は n10 になる。
        match q.front().unwrap() {
            DeviceEvent::Removed { id } => assert_eq!(id, "n10", "最古から捨てられる"),
            other => panic!("想定外イベント: {other:?}"),
        }
        // 最新（n{total-1}）は残る。
        match q.back().unwrap() {
            DeviceEvent::Removed { id } => assert_eq!(id, &format!("n{}", total - 1)),
            other => panic!("想定外イベント: {other:?}"),
        }
    }
}
