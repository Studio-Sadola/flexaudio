//! [`WasapiSystemBackend`] — システム音声出力の WASAPI loopback。
//!
//! `exclude_self == false`（既定）: render endpoint へ流れているミックスを
//! `AUDCLNT_STREAMFLAGS_LOOPBACK` で録る古典 loopback。Linux の
//! [`PwSystemBackend`](../flexaudio_os_linux) 相当。`device_id` で出力エンドポイントを
//! 選べる（`None` で既定 render、`Some(id)` で FriendlyName が一致する eRender
//! エンドポイント）。`id` は [`list_output_devices`] が返す FriendlyName。
//!
//! `exclude_self == true`: 自ホストプロセス（そのツリー）の音だけを除いた全システム音を
//! 録る（フィードバック防止）。古典 loopback ではなく、`process` モジュールのプロセス
//! ループバック機構を [`ProcessMode::Exclude`] + 自 PID（`std::process::id()`）で呼ぶ
//! （[`crate::process::setup_process_loopback`]）。この経路のネイティブフォーマットは
//! プロセスループバック固定の `(48000, 2)`。エンドポイントに紐づかない機構なので
//! `device_id` は無視する。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use flexaudio_core::backend::{CaptureBackend, RawSink};
use flexaudio_core::types::{DeviceInfo, Error, ProcessMode, Result, SourceKind};

use windows::core::Interface;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    DEVICE_STATE_ACTIVE, WAVEFORMATEX,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL, STGM_READ};

use crate::common::{capture_loop, init_loopback_capture, map_hr, parse_mix_format, ComThread};

/// 既定 render endpoint を取得できなかったときに [`native_format`] が返す
/// 無難なフォールバック `(48000, 2)`（panic しない）。実際の `start` で
/// 取得失敗すれば [`Error`] を返す。
const FALLBACK_FORMAT: (u32, u16) = (48_000, 2);

/// `exclude_self == true`（プロセスループバック EXCLUDE）経路の固定ネイティブフォーマット
/// `(48000, 2)`。プロセスループバックは `GetMixFormat` を使えず固定 WAVEFORMATEX
/// （[`crate::process`] の `fixed_process_format`）で Initialize するため、native も
/// この固定値となる。
const PROCESS_LOOPBACK_FORMAT: (u32, u16) = (48_000, 2);

/// システム音声出力をキャプチャする [`CaptureBackend`]。
///
/// `exclude_self == false`（既定）: 専用スレッド上で COM を初期化し、
/// `MMDeviceEnumerator` → `GetDefaultAudioEndpoint(eRender, eConsole)` →
/// `IAudioClient` を取得して `AUDCLNT_STREAMFLAGS_LOOPBACK` で Initialize する古典
/// loopback。イベント駆動でパケットを [`RawSink::push`] へ流す。
///
/// `exclude_self == true`: 自ホスト PID（そのツリー）を除く全システム音を録る
/// （フィードバック防止）。古典 loopback ではなく [`crate::process::setup_process_loopback`]
/// を [`ProcessMode::Exclude`] + `std::process::id()` で呼び、同じ
/// [`capture_loop`] を回す。
///
/// この型は `Send`（保持するのは停止フラグ・[`JoinHandle`]・`exclude_self`・キャッシュ済み
/// フォーマットのみ。`!Send` な COM インターフェイスは専用スレッド内に閉じ込める）。
pub struct WasapiSystemBackend {
    /// 自ホスト除外フラグ。`true` でプロセスループバック EXCLUDE 経路、`false` で古典
    /// loopback 経路。
    exclude_self: bool,
    /// 出力エンドポイントの選択。`None` で既定 render、`Some(id)` で FriendlyName が
    /// `id` と一致する eRender エンドポイント。`exclude_self == true` では使わない。
    device_id: Option<String>,
    /// 起動中フラグ（二重 start ガード／停止指示／drop 判定）。`Send`。
    stop_flag: Arc<AtomicBool>,
    /// COM/キャプチャを所有するスレッドのハンドル（start 後に `Some`）。
    handle: Option<JoinHandle<()>>,
    /// `new` 時に決めてキャッシュしたネイティブフォーマット。
    native: (u32, u16),
}

impl WasapiSystemBackend {
    /// 新しいシステム loopback バックエンドを構築する（この時点では接続しない）。
    ///
    /// `exclude_self == false`（既定経路）: `device_id` が指すエンドポイント（`None` で
    /// 既定 render）の MixFormat を一度問い合わせてキャッシュする。取得失敗時は
    /// [`FALLBACK_FORMAT`]（`(48000, 2)`）をキャッシュする（panic しない）。`device_id` の
    /// 一致確認は構築時にはせず、実際に開く [`start`](CaptureBackend::start) で行う。
    ///
    /// `exclude_self == true`（プロセスループバック EXCLUDE 経路）: native は
    /// プロセスループバック固定の [`PROCESS_LOOPBACK_FORMAT`]（`(48000, 2)`）。MixFormat の
    /// 問い合わせは行わず、`device_id` も使わない（機構がエンドポイントに紐づかない）。
    pub fn new(exclude_self: bool, device_id: Option<String>) -> Self {
        let native = if exclude_self {
            PROCESS_LOOPBACK_FORMAT
        } else {
            query_native_format(device_id.as_deref()).unwrap_or(FALLBACK_FORMAT)
        };
        Self {
            exclude_self,
            device_id,
            stop_flag: Arc::new(AtomicBool::new(false)),
            handle: None,
            native,
        }
    }
}

impl Default for WasapiSystemBackend {
    /// 既定は既定 render endpoint の古典 loopback（`exclude_self == false` / `device_id == None`）。
    fn default() -> Self {
        Self::new(false, None)
    }
}

/// `device_id` が指す render endpoint（`None` で既定）の MixFormat から
/// `(rate, channels)` を取得する。取得できなければ `None`（panic しない）。一時的に COM を
/// 初期化して問い合わせる。
fn query_native_format(device_id: Option<&str>) -> Option<(u32, u16)> {
    let _com = ComThread::new();
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let device = resolve_render_endpoint(&enumerator, device_id).ok()?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).ok()?;
        let pwfx = client.GetMixFormat().ok()?;
        if pwfx.is_null() {
            return None;
        }
        // packed フィールドは値コピーで読む（parse_mix_format と同方針）。
        let rate = core::ptr::addr_of!((*pwfx).nSamplesPerSec).read_unaligned();
        let channels = core::ptr::addr_of!((*pwfx).nChannels).read_unaligned();
        CoTaskMemFree(Some(pwfx as *const _ as *const _));
        Some((rate, channels))
    }
}

/// `device_id` を render endpoint へ解決する。
///
/// `None` なら `GetDefaultAudioEndpoint(eRender, eConsole)`。`Some(id)` なら eRender の
/// ACTIVE エンドポイントを列挙し、FriendlyName が `id` と一致する最初のものを返す。一致が
/// 無ければ [`Error::DeviceNotFound`]。
///
/// # Safety
/// 呼び出しスレッドで COM が初期化済みであること。`enumerator` は有効。
unsafe fn resolve_render_endpoint(
    enumerator: &IMMDeviceEnumerator,
    device_id: Option<&str>,
) -> Result<IMMDevice> {
    let Some(id) = device_id else {
        return enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(|_e| Error::DeviceNotFound);
    };

    let collection = enumerator
        .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
        .map_err(|e| map_hr("IMMDeviceEnumerator::EnumAudioEndpoints", e))?;
    let count = collection
        .GetCount()
        .map_err(|e| map_hr("IMMDeviceCollection::GetCount", e))?;

    for i in 0..count {
        let device = match collection.Item(i) {
            Ok(d) => d,
            Err(_e) => continue,
        };
        if endpoint_friendly_name(&device).as_deref() == Some(id) {
            return Ok(device);
        }
    }
    Err(Error::DeviceNotFound)
}

/// エンドポイントのプロパティストアから FriendlyName を読む。読めなければ `None`。
///
/// # Safety
/// 呼び出しスレッドで COM が初期化済みであること。`device` は有効な `IMMDevice`。
unsafe fn endpoint_friendly_name(device: &IMMDevice) -> Option<String> {
    let store = device.OpenPropertyStore(STGM_READ).ok()?;
    let value = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
    // PROPVARIANT の Display は PropVariantToBSTR 経由で文字列化する（VT_LPWSTR も拾う）。
    let name = value.to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// アクティブな render（出力）エンドポイントを列挙して [`DeviceInfo`] のリストを返す。
///
/// 各 `DeviceInfo` の `id` / `name` はエンドポイントの FriendlyName、`sample_rate` /
/// `channels` は MixFormat 由来、`source_kind` は [`SourceKind::SystemLoopback`]、
/// `is_loopback` は常に `true`。`is_default` は既定 render エンドポイントと FriendlyName が
/// 一致するエントリに付く。FriendlyName が読めないエンドポイントは飛ばす。
pub fn list_output_devices() -> Result<Vec<DeviceInfo>> {
    let _com = ComThread::new();
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| map_hr("CoCreateInstance(MMDeviceEnumerator)", e))?;

        // 既定 render の FriendlyName（is_default の突き合わせ用）。無くても列挙は続ける。
        let default_name = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .ok()
            .and_then(|d| endpoint_friendly_name(&d));

        let collection = enumerator
            .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
            .map_err(|e| map_hr("IMMDeviceEnumerator::EnumAudioEndpoints", e))?;
        let count = collection
            .GetCount()
            .map_err(|e| map_hr("IMMDeviceCollection::GetCount", e))?;

        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            let device = match collection.Item(i) {
                Ok(d) => d,
                Err(_e) => continue,
            };
            // FriendlyName が無いエンドポイントは id を作れないので飛ばす。
            let Some(name) = endpoint_friendly_name(&device) else {
                continue;
            };
            // MixFormat から rate/channels。取れなければ要求ネイティブ（48000/2）。
            let (sample_rate, channels) = endpoint_mix_format(&device).unwrap_or(FALLBACK_FORMAT);
            let is_default = default_name.as_deref() == Some(name.as_str());
            out.push(DeviceInfo {
                id: name.clone(),
                name,
                source_kind: SourceKind::SystemLoopback,
                sample_rate,
                channels,
                is_loopback: true,
                is_default,
            });
        }
        Ok(out)
    }
}

/// エンドポイントの MixFormat から `(rate, channels)` を取得する。取れなければ `None`。
///
/// # Safety
/// 呼び出しスレッドで COM が初期化済みであること。`device` は有効な `IMMDevice`。
unsafe fn endpoint_mix_format(device: &IMMDevice) -> Option<(u32, u16)> {
    let client: IAudioClient = device.Activate(CLSCTX_ALL, None).ok()?;
    let pwfx = client.GetMixFormat().ok()?;
    if pwfx.is_null() {
        return None;
    }
    let rate = core::ptr::addr_of!((*pwfx).nSamplesPerSec).read_unaligned();
    let channels = core::ptr::addr_of!((*pwfx).nChannels).read_unaligned();
    CoTaskMemFree(Some(pwfx as *const _ as *const _));
    Some((rate, channels))
}

impl CaptureBackend for WasapiSystemBackend {
    fn native_format(&self) -> (u32, u16) {
        self.native
    }

    fn start(&mut self, sink: RawSink) -> Result<()> {
        // 二重 start に安全: 既にスレッドが生きていれば何もしない。
        if self.handle.is_some() {
            return Ok(());
        }
        // 前回 stop 後でも再 start できるようフラグをリセット。
        self.stop_flag.store(false, Ordering::SeqCst);

        let stop_flag = self.stop_flag.clone();
        let exclude_self = self.exclude_self;
        let device_id = self.device_id.clone();
        // setup（COM init〜Initialize〜Start 直前）の成否を同期返却するチャネル。
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();

        let handle = thread::Builder::new()
            .name("flexaudio-wasapi-system".into())
            .spawn(move || {
                run_system_thread(exclude_self, device_id, sink, stop_flag, ready_tx);
            })
            .map_err(|e| Error::Backend(format!("spawn wasapi system thread: {e}")))?;

        match ready_rx.recv() {
            Ok(Ok(())) => {
                self.handle = Some(handle);
                Ok(())
            }
            Ok(Err(e)) => {
                // setup 失敗。スレッドは ready 送信後すぐ終了するので join。
                self.stop_flag.store(false, Ordering::SeqCst);
                let _ = handle.join();
                Err(e)
            }
            Err(_) => {
                self.stop_flag.store(false, Ordering::SeqCst);
                let _ = handle.join();
                Err(Error::Backend(
                    "wasapi system thread exited before reporting readiness".into(),
                ))
            }
        }
    }

    fn stop(&mut self) {
        // 再入・二重 stop に安全。
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for WasapiSystemBackend {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 所有スレッド本体。COM を初期化し、`exclude_self` に応じて loopback を構成して
/// キャプチャループを回す。setup の成否を `ready_tx` で [`WasapiSystemBackend::start`]
/// へ報告する。COM ガード（`_com`）はこの関数のスコープに閉じ、スレッド終了時に
/// `CoUninitialize` される（COM オブジェクトより後に drop される宣言順）。
///
/// - `exclude_self == false`: `device_id` が指す render endpoint（`None` で既定）の古典
///   loopback（[`setup_system_loopback`]）。
/// - `exclude_self == true`: 自ホスト PID を [`ProcessMode::Exclude`] で渡す
///   プロセスループバック（[`crate::process::setup_process_loopback`]）。`device_id` は使わない。
///
/// どちらも同一の 4-tuple `(IAudioClient, IAudioCaptureClient, HANDLE, u16)` を返すので、
/// 以降は共通の [`capture_loop`] へ合流する。
fn run_system_thread(
    exclude_self: bool,
    device_id: Option<String>,
    sink: RawSink,
    stop_flag: Arc<AtomicBool>,
    ready_tx: mpsc::Sender<Result<()>>,
) {
    // COM をこのスレッドで初期化（drop で uninit）。最初に宣言＝最後に drop。
    let _com = ComThread::new();

    // setup を行い、Initialize 済み client / capture / event / channels を得る。
    // exclude_self で経路を分岐するが、戻り値の型は両者で同一。
    let setup = if exclude_self {
        // 自ホスト PID（そのツリー）を EXCLUDE して全システム音を録る（フィードバック防止）。
        // `process` モジュールのプロセスループバック機構をそのまま再利用する。
        unsafe { crate::process::setup_process_loopback(std::process::id(), ProcessMode::Exclude) }
    } else {
        // device_id が指す render endpoint（None で既定）の古典 loopback。
        unsafe { setup_system_loopback(device_id.as_deref(), &sink) }
    };
    let (client, capture, event, channels) = match setup {
        Ok(t) => t,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    // setup 成功を報告。以後はキャプチャループ（内部で client.Start()）。
    if ready_tx.send(Ok(())).is_err() {
        // 呼び出し元が消えている。Start せず戻る（COM は drop で片付く）。
        return;
    }

    unsafe { capture_loop(&client, &capture, event, channels, sink, &stop_flag) };
    // capture_loop が client.Stop() と CloseHandle(event) を行う。
    // ここを抜けると capture → client → _com の順で drop（宣言の逆順）。
}

/// `device_id` が指す render endpoint（`None` で既定）の古典 loopback をセットアップし、
/// Initialize 済みの `IAudioClient` / `IAudioCaptureClient` / イベントハンドル /
/// チャンネル数を返す。`device_id` が一致しなければ [`Error::DeviceNotFound`]。
///
/// `sink` はチャンネル数のチェック（native と一致確認）には使わず、参照のみ受け取る
/// （所有はループへ渡す呼び出し側が持つ）。
///
/// # Safety
/// 呼び出しスレッドで COM が初期化済みであること。
#[allow(clippy::type_complexity)]
unsafe fn setup_system_loopback(
    device_id: Option<&str>,
    _sink: &RawSink,
) -> Result<(
    IAudioClient,
    windows::Win32::Media::Audio::IAudioCaptureClient,
    windows::Win32::Foundation::HANDLE,
    u16,
)> {
    let enumerator: IMMDeviceEnumerator =
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .map_err(|e| map_hr("CoCreateInstance(MMDeviceEnumerator)", e))?;

    // device_id が指す render endpoint（None で既定）。一致しなければ DeviceNotFound。
    let device: IMMDevice = resolve_render_endpoint(&enumerator, device_id)?;

    let client: IAudioClient = device
        .Activate(CLSCTX_ALL, None)
        .map_err(|e| map_hr("IMMDevice::Activate(IAudioClient)", e))?;

    // 共有 MixFormat（使用後 CoTaskMemFree 必須）。
    let pwfx: *mut WAVEFORMATEX = client
        .GetMixFormat()
        .map_err(|e| map_hr("IAudioClient::GetMixFormat", e))?;

    // フォーマット判定（IEEE float のみ直結）。rate/channels はここで控える。
    let parsed = parse_mix_format(pwfx as *const WAVEFORMATEX);
    let (_rate, channels) = match parsed {
        Ok(v) => v,
        Err(e) => {
            CoTaskMemFree(Some(pwfx as *const _ as *const _));
            return Err(e);
        }
    };

    // Initialize（LOOPBACK|EVENTCALLBACK）→ event → capture サービス。
    let init = init_loopback_capture(&client, pwfx as *const WAVEFORMATEX);
    // pwfx は Initialize がフォーマットをコピーするので、ここで解放してよい。
    CoTaskMemFree(Some(pwfx as *const _ as *const _));
    let (capture, event) = init?;

    // 念のため Interface が生きていることを保証（使わないが drop 順の明示）。
    let _ = client.as_raw();

    Ok((client, capture, event, channels))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio_core::raw_ring;

    /// `new` + `native_format` が panic しないこと（render endpoint 有無を問わず）。
    #[test]
    fn new_and_native_format_do_not_panic() {
        let backend = WasapiSystemBackend::new(false, None);
        let (rate, channels) = backend.native_format();
        assert!(rate > 0);
        assert!(channels > 0);
    }

    /// device_id を渡しても `new` は panic しない（一致確認は start まで遅延する）。
    /// 存在しない id でも構築できて native は妥当なフォールバックになる。
    #[test]
    fn new_with_device_id_does_not_panic() {
        let backend = WasapiSystemBackend::new(false, Some("no-such-endpoint".into()));
        let (rate, channels) = backend.native_format();
        assert!(rate > 0);
        assert!(channels > 0);
    }

    /// `exclude_self == true` の native は常にプロセスループバック固定の `(48000, 2)`。
    /// MixFormat を問い合わせないので render endpoint 有無・device_id に依存せず確定する。
    #[test]
    fn new_exclude_self_native_is_fixed() {
        let backend = WasapiSystemBackend::new(true, Some("ignored".into()));
        assert_eq!(backend.native_format(), (48_000, 2));
    }

    /// `list_output_devices` が panic しないこと。返ったエントリは loopback 扱い。
    /// render endpoint が無い環境では空リストや `Err` を許容（panic だけ不可）。
    #[test]
    fn list_output_devices_does_not_panic() {
        if let Ok(devices) = list_output_devices() {
            for d in &devices {
                assert!(d.is_loopback);
                assert_eq!(d.source_kind, SourceKind::SystemLoopback);
            }
        }
    }

    /// `start` → `stop` がデバイス有無を問わず panic しないこと（古典 loopback 経路）。
    /// render endpoint が無い/開けない環境では `Err` を許容（panic だけ不可）。
    #[test]
    fn start_then_stop_tolerates_missing_endpoint() {
        let mut backend = WasapiSystemBackend::new(false, None);
        let (rate, channels) = backend.native_format();
        let cap = (rate as usize * channels as usize).max(1);
        let (prod, _cons) = raw_ring(cap);
        let sink = RawSink::new(prod, rate, channels);

        match backend.start(sink) {
            Ok(()) => {
                backend.stop();
                backend.stop(); // 二重 stop も安全。
            }
            Err(_e) => { /* render endpoint 無し/非 float 等は許容 */ }
        }
    }

    /// 存在しない device_id での `start` は `DeviceNotFound`（panic しない）。
    #[test]
    fn start_with_unknown_device_id_is_device_not_found() {
        let mut backend = WasapiSystemBackend::new(false, Some("no-such-endpoint".into()));
        let (rate, channels) = backend.native_format();
        let cap = (rate as usize * channels as usize).max(1);
        let (prod, _cons) = raw_ring(cap);
        let sink = RawSink::new(prod, rate, channels);

        match backend.start(sink) {
            Ok(()) => {
                // 万一一致してしまう環境では成功も許容（停止だけ確認）。
                backend.stop();
            }
            Err(e) => assert!(matches!(e, Error::DeviceNotFound)),
        }
    }

    /// `exclude_self == true`（プロセスループバック EXCLUDE 経路）でも
    /// `start` → `stop` が panic しないこと。非対応 OS / activation 失敗は `Err` 許容。
    #[test]
    fn start_then_stop_exclude_self_tolerates_failure() {
        let mut backend = WasapiSystemBackend::new(true, None);
        let (rate, channels) = backend.native_format();
        let cap = (rate as usize * channels as usize).max(1);
        let (prod, _cons) = raw_ring(cap);
        let sink = RawSink::new(prod, rate, channels);

        match backend.start(sink) {
            Ok(()) => {
                backend.stop();
                backend.stop(); // 二重 stop も安全。
            }
            Err(_e) => { /* 非対応 OS / プロセスループバック activation 失敗は許容 */
            }
        }
    }
}
