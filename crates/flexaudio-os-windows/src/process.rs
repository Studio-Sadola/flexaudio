//! [`WasapiProcessBackend`] — プロセス別 WASAPI loopback。
//!
//! `ActivateAudioInterfaceAsync` + `AUDIOCLIENT_ACTIVATION_PARAMS`（プロセス
//! ループバック）で特定 PID（そのプロセスツリー）の音声を録る。`mode`
//! （[`ProcessMode::Include`]）で「対象ツリーの音だけ」、[`ProcessMode::Exclude`]
//! で「対象ツリーを除く全システム音」を録る。Linux の
//! [`PwProcessBackend`](../flexaudio_os_linux)（link-factory fan-out）相当。
//!
//! このモジュールの [`setup_process_loopback`] は `pub(crate)` で、`system` モジュール
//! （[`WasapiSystemBackend`](crate::WasapiSystemBackend)）の `exclude_self == true`
//! 経路からも呼ばれる（自ホスト PID を EXCLUDE して全システム音を録る）。
//!
//! # PROPVARIANT（VT_BLOB）の難所
//!
//! `ActivateAudioInterfaceAsync` の `activationparams` は
//! `Option<*const windows_core::PROPVARIANT>` で、`AUDIOCLIENT_ACTIVATION_PARAMS` を
//! VT_BLOB の PROPVARIANT に詰めて渡す必要がある。windows-core 0.54 では
//! `PROPVARIANT::from_raw` の引数が private な `imp::PROPVARIANT` で、外から型名を書けない
//! （公開の生構造体も無い）。そこで SDK の PROPVARIANT レイアウトに厳密一致させた
//! `#[repr(C)]` ミラー構造体 [`RawPropVariant`] を自前定義し、`transmute` で `from_raw`
//! へ渡す。

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use flexaudio_core::backend::{CaptureBackend, RawSink};
use flexaudio_core::types::{Error, ProcessMode, Result};

use windows::core::{implement, Interface, HRESULT, PROPVARIANT};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioClient, AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    WAVEFORMATEX,
};
use windows::Win32::Media::Multimedia::WAVE_FORMAT_IEEE_FLOAT;
use windows::Win32::System::Threading::{CreateEventW, SetEvent};

use crate::common::{capture_loop, init_loopback_capture, map_hr, wait_event_signaled, ComThread};
use windows::core::PCWSTR;

/// プロセスループバックのネイティブフォーマットは固定 `(48000, 2)`。
/// プロセスループバックは `GetMixFormat` を使えないため、自前で WAVEFORMATEX を組む。
const NATIVE_RATE: u32 = 48_000;
const NATIVE_CHANNELS: u16 = 2;

/// SDK の `PROPVARIANT` に厳密一致させた x64 24 バイトのミラー構造体。
///
/// windows-core 0.54 は `PROPVARIANT::from_raw` の引数 `imp::PROPVARIANT` が private で
/// 外から型名を書けないため、レイアウト一致のミラーを作り `transmute` で渡す。
///
/// レイアウトは実測した windows-core 0.54 の生 `PROPVARIANT_0_0` / `PROPVARIANT_0_0_0`
/// と一致: `vt: u16` + `wReserved1/2/3: u16 ×3`（ここまで 8 バイトで value union 境界へ
/// 整列）+ 16 バイトの value union。value union（`PROPVARIANT_0_0_0`）は `CAUB`/`BLOB`/
/// `CAFILETIME` 等の「`u32` カウント + ポインタ」型を含むので x64 で 16 バイト、ゆえに
/// PROPVARIANT 全体は 8+16 = 24 バイト。VT_BLOB の union 先頭は
/// `BLOB { cbSize: u32, pBlobData: *mut u8 }`。x64 ではポインタ整列のため cbSize(offset 8)
/// の後に 4 バイトパディングが入り、pBlobData は offset 16。
#[repr(C)]
#[derive(Clone, Copy)]
struct RawPropVariant {
    /// VARENUM。VT_BLOB = 65。
    vt: u16,
    w_reserved1: u16,
    w_reserved2: u16,
    w_reserved3: u16,
    /// BLOB.cbSize（バイト数）。offset 8。
    blob_cb_size: u32,
    /// x64 のポインタ整列パディング（cbSize:u32 → pBlobData:ptr は 8 バイト境界）。offset 12。
    _pad: u32,
    /// BLOB.pBlobData（ブロブ実体への参照。コピーされないので生存させること）。offset 16。
    blob_p_data: *mut u8,
}

const VT_BLOB_U16: u16 = 65;

// レイアウト（24 バイト / 8 バイトアライン）を SDK PROPVARIANT と一致させていることを
// コンパイル時に確認する。`PROPVARIANT` 自体のサイズとも突き合わせる（生 imp::PROPVARIANT
// の薄ラッパなので同サイズのはず）。
//
// このミラーは 64bit ポインタ前提の 24 バイトレイアウト（value union が「u32 カウント +
// ポインタ」型で 16 バイト）。32bit ターゲットではポインタが 4 バイトとなり PROPVARIANT の
// サイズ/アライン/パディングが変わるので、その場合は以下の const assert がコンパイル不可に
// なってビルド時に弾く。flexaudio の Windows サポートは 64bit のみ（x86_64 / aarch64）。
const _: () = {
    assert!(core::mem::size_of::<RawPropVariant>() == 24);
    assert!(core::mem::align_of::<RawPropVariant>() == 8);
    assert!(core::mem::size_of::<PROPVARIANT>() == core::mem::size_of::<RawPropVariant>());
    assert!(core::mem::align_of::<PROPVARIANT>() == 8);
};

/// `AUDIOCLIENT_ACTIVATION_PARAMS` を指す VT_BLOB の `PROPVARIANT` を組む。
///
/// `params` は呼び出し元が `ActivateAudioInterfaceAsync` + 完了待ちまで生存させること
/// （BLOB はコピーされず参照される）。
///
/// 戻り値を [`ManuallyDrop`] で包むのはメモリ安全性のため。windows-core 0.54 の
/// `PROPVARIANT` は `Drop` で `PropVariantClear` を呼び、VT_BLOB では `pBlobData` を
/// `CoTaskMemFree` で解放しようとする。だが本関数の `pBlobData` は呼び出し元のスタック上の
/// `params` を指す（COM 確保メモリではない）ので、素の `PROPVARIANT` を drop させると
/// スタックポインタを free してヒープ破壊（STATUS_HEAP_CORRUPTION）になる。ミラーは BLOB
/// ポインタを借用しているだけで自前のヒープ資源を持たないため、`Drop` を抑止して中身を
/// leak させてもリークの実害は無い（params 本体は呼び出し元が所有・解放する）。
///
/// # Safety
/// `params` は有効な `AUDIOCLIENT_ACTIVATION_PARAMS` を指し、戻り値の `PROPVARIANT` の
/// 生存期間を通じて生きていること。
// `transmute` の宛先型 `windows_core::imp::bindings::PROPVARIANT` は private で型名を書けず
// `_` 推論にするしかないため、clippy::missing_transmute_annotations を満たせない。レイアウト
// 一致は上の const assert で担保済みなので局所許可する。
#[allow(clippy::missing_transmute_annotations)]
unsafe fn make_blob_propvariant(
    params: *mut AUDIOCLIENT_ACTIVATION_PARAMS,
) -> core::mem::ManuallyDrop<PROPVARIANT> {
    let raw = RawPropVariant {
        vt: VT_BLOB_U16,
        w_reserved1: 0,
        w_reserved2: 0,
        w_reserved3: 0,
        blob_cb_size: core::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
        _pad: 0,
        blob_p_data: params as *mut u8,
    };
    // from_raw の引数型 imp::PROPVARIANT は private で名前を書けないため、`_` 推論で
    // transmute する（RawPropVariant とレイアウト一致を上の const assert で担保）。
    core::mem::ManuallyDrop::new(PROPVARIANT::from_raw(core::mem::transmute::<
        RawPropVariant,
        _,
    >(raw)))
}

/// `ActivateCompleted` で起動側スレッドへ完了を知らせるだけの完了ハンドラ。
///
/// 結果（`IAudioClient`）の取り出しは起動側スレッドが `op.GetActivateResult` で行う
/// （COM オブジェクトをスレッド跨ぎさせない）。本ハンドラは `SetEvent` だけなので
/// 内部可変も不要（`&self` で足りる）。
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivationHandler {
    /// 完了通知用イベント（手動リセット）。`ActivateCompleted` で `SetEvent`。
    done: HANDLE,
}

// windows-implement 0.53（windows 0.54 が引く版）の `#[implement]` は、`_Impl` サフィックス
// のトレイトを元の構造体（ここでは `ActivationHandler`）に対して実装させる。生成される
// `ActivationHandler_Impl` はラッパで、`this: ActivationHandler` を内包し Deref で元構造体の
// フィールドへ到達するので、`self.done` で素直にアクセスできる。
impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationHandler {
    fn ActivateCompleted(
        &self,
        _operation: Option<&IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        // これは OS（WASAPI の activation 基盤）が呼ぶ FFI 境界コールバック。境界を越える
        // panic は UB なので本体を catch_unwind で包む。現状は SetEvent だけで panic しないが、
        // 将来の変更に備えた保険。
        let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
            let _ = SetEvent(self.done);
        }));
        Ok(())
    }
}

/// プロセス別 loopback で特定 PID（そのツリー）の音声をキャプチャする
/// [`CaptureBackend`]。
///
/// 専用スレッド上で COM を初期化し、`ActivateAudioInterfaceAsync`
/// （`VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK` + VT_BLOB の
/// `AUDIOCLIENT_ACTIVATION_PARAMS`）で `IAudioClient` を取得、固定 WAVEFORMATEX
/// （48k/2ch/f32）で Initialize し、イベント駆動でパケットを [`RawSink::push`] へ流す。
///
/// この型は `Send`（保持するのは `target_pid` / `mode` / 停止フラグ /
/// [`JoinHandle`] / 固定フォーマット。`!Send` な COM は専用スレッド内に閉じ込める）。
pub struct WasapiProcessBackend {
    /// キャプチャ対象プロセスの PID。
    target_pid: u32,
    /// 録音モード。[`ProcessMode::Include`] で INCLUDE（対象ツリーの音だけ）、
    /// [`ProcessMode::Exclude`] で EXCLUDE（対象ツリー以外の全システム音）。
    mode: ProcessMode,
    /// 起動中フラグ（二重 start ガード／停止指示／drop 判定）。`Send`。
    stop_flag: Arc<AtomicBool>,
    /// COM/キャプチャを所有するスレッドのハンドル（start 後に `Some`）。
    handle: Option<JoinHandle<()>>,
    /// 固定ネイティブフォーマット `(48000, 2)`。
    native: (u32, u16),
}

impl WasapiProcessBackend {
    /// 対象 PID と `mode` からバックエンドを構築する（この時点では接続しない）。
    pub fn new(target_pid: u32, mode: ProcessMode) -> Self {
        Self {
            target_pid,
            mode,
            stop_flag: Arc::new(AtomicBool::new(false)),
            handle: None,
            native: (NATIVE_RATE, NATIVE_CHANNELS),
        }
    }

    /// キャプチャ対象の PID。
    pub fn target_pid(&self) -> u32 {
        self.target_pid
    }

    /// 保持している録音モード（[`ProcessMode::Include`] / [`ProcessMode::Exclude`]）。
    pub fn mode(&self) -> ProcessMode {
        self.mode
    }
}

impl CaptureBackend for WasapiProcessBackend {
    fn native_format(&self) -> (u32, u16) {
        self.native
    }

    fn start(&mut self, sink: RawSink) -> Result<()> {
        if self.handle.is_some() {
            return Ok(());
        }
        self.stop_flag.store(false, Ordering::SeqCst);

        let stop_flag = self.stop_flag.clone();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let target_pid = self.target_pid;
        let mode = self.mode;

        let handle = thread::Builder::new()
            .name("flexaudio-wasapi-process".into())
            .spawn(move || {
                run_process_thread(target_pid, mode, sink, stop_flag, ready_tx);
            })
            .map_err(|e| Error::Backend(format!("spawn wasapi process thread: {e}")))?;

        match ready_rx.recv() {
            Ok(Ok(())) => {
                self.handle = Some(handle);
                Ok(())
            }
            Ok(Err(e)) => {
                self.stop_flag.store(false, Ordering::SeqCst);
                let _ = handle.join();
                Err(e)
            }
            Err(_) => {
                self.stop_flag.store(false, Ordering::SeqCst);
                let _ = handle.join();
                Err(Error::Backend(
                    "wasapi process thread exited before reporting readiness".into(),
                ))
            }
        }
    }

    fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for WasapiProcessBackend {
    fn drop(&mut self) {
        self.stop();
    }
}

/// プロセス別 loopback 用の固定 WAVEFORMATEX（48000 / 2ch / f32）。
/// プロセスループバックは `GetMixFormat` を使えないため自前で組む。
fn fixed_process_format() -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16, // = 3
        nChannels: NATIVE_CHANNELS,                // 2
        nSamplesPerSec: NATIVE_RATE,               // 48000
        wBitsPerSample: 32,
        nBlockAlign: 8,                   // channels * bits/8 = 2 * 4
        nAvgBytesPerSec: NATIVE_RATE * 8, // rate * blockAlign
        cbSize: 0,
    }
}

/// 所有スレッド本体。COM を初期化し、プロセスループバック activation で `IAudioClient`
/// を取得して固定フォーマットでキャプチャループを回す。setup の成否を `ready_tx` で
/// [`WasapiProcessBackend::start`] へ報告する。
fn run_process_thread(
    target_pid: u32,
    mode: ProcessMode,
    sink: RawSink,
    stop_flag: Arc<AtomicBool>,
    ready_tx: mpsc::Sender<Result<()>>,
) {
    let _com = ComThread::new();

    let setup = unsafe { setup_process_loopback(target_pid, mode) };
    let (client, capture, event, channels) = match setup {
        Ok(t) => t,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    if ready_tx.send(Ok(())).is_err() {
        return;
    }

    unsafe { capture_loop(&client, &capture, event, channels, sink, &stop_flag) };
}

/// プロセスループバックをセットアップし、Initialize 済みの `IAudioClient` /
/// `IAudioCaptureClient` / イベントハンドル / チャンネル数（固定 2）を返す。
///
/// `mode` で INCLUDE（対象ツリーだけ）／EXCLUDE（対象ツリー以外の全システム音）を選ぶ。
/// `system` モジュールの `exclude_self == true` 経路は、自ホスト PID を
/// [`ProcessMode::Exclude`] で渡してこれを再利用する（だから `pub(crate)`）。
///
/// # Safety
/// 呼び出しスレッドで COM が初期化済みであること。
#[allow(clippy::type_complexity)]
pub(crate) unsafe fn setup_process_loopback(
    target_pid: u32,
    mode: ProcessMode,
) -> Result<(
    IAudioClient,
    windows::Win32::Media::Audio::IAudioCaptureClient,
    HANDLE,
    u16,
)> {
    // activation params を組む。mode で INCLUDE/EXCLUDE を切り替える。
    let loopback_mode = match mode {
        ProcessMode::Include => PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
        ProcessMode::Exclude => PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
    };
    let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: target_pid,
                ProcessLoopbackMode: loopback_mode,
            },
        },
    };

    // VT_BLOB の PROPVARIANT を組む。params / prop は ActivateAudioInterfaceAsync +
    // 完了待ち（GetActivateResult）まで生存させる（BLOB は参照）。`prop` が `ManuallyDrop`
    // なのは、スタックの BLOB を `PropVariantClear` で free するとヒープ破壊になるため
    // （`make_blob_propvariant` の doc 参照）。
    let prop = make_blob_propvariant(&mut params as *mut _);

    // 完了通知イベント（手動リセット=true / 初期非シグナル）。
    let done_event = CreateEventW(None, true, false, PCWSTR::null())
        .map_err(|e| map_hr("CreateEventW(activation done)", e))?;

    // 完了ハンドラ（SetEvent するだけ）。WaitForSingleObject 完了まで drop しない
    // （参照カウント生存）。
    let handler: IActivateAudioInterfaceCompletionHandler =
        ActivationHandler { done: done_event }.into();

    let op: IActivateAudioInterfaceAsyncOperation = match ActivateAudioInterfaceAsync(
        VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
        &IAudioClient::IID,
        // `&*prop` で ManuallyDrop を剥がして `&PROPVARIANT` → `*const PROPVARIANT`。
        Some(&*prop as *const _),
        &handler,
    ) {
        Ok(op) => op,
        Err(e) => {
            let _ = CloseHandle(done_event);
            // 古い OS（プロセスループバック非対応）は E_NOINTERFACE/E_NOTIMPL 等。
            return Err(map_process_activation_err("ActivateAudioInterfaceAsync", e));
        }
    };

    // 完了を待つ（5 秒）。タイムアウトは Backend エラーに写す。
    if !wait_event_signaled(done_event, 5000) {
        let _ = CloseHandle(done_event);
        return Err(Error::Backend(
            "process loopback activation timed out".into(),
        ));
    }
    // 完了イベントはもう不要。params/prop/handler はこの関数末尾まで生存させる。
    let _ = CloseHandle(done_event);

    // activation 結果を取り出す（起動側スレッドで。COM をスレッド跨ぎさせない）。
    let mut hr = HRESULT(0);
    let mut unknown: Option<windows::core::IUnknown> = None;
    op.GetActivateResult(&mut hr, &mut unknown)
        .map_err(|e| map_hr("GetActivateResult", e))?;
    if let Err(e) = hr.ok() {
        return Err(map_process_activation_err("activation result HRESULT", e));
    }
    let unknown =
        unknown.ok_or_else(|| Error::Backend("activation returned null interface".into()))?;
    let client: IAudioClient = unknown
        .cast()
        .map_err(|e| map_hr("cast activated IUnknown to IAudioClient", e))?;

    // 固定フォーマットで Initialize（LOOPBACK|EVENTCALLBACK）→ event → capture。
    let wfx = fixed_process_format();
    let (capture, event) = init_loopback_capture(&client, &wfx as *const WAVEFORMATEX)?;

    // params/prop/handler/op をここまで生かしてから drop（BLOB 参照・ハンドラ生存）。
    drop(op);
    drop(handler);
    // `prop` は `ManuallyDrop`。中身は BLOB ポインタ（params への借用）だけで自前資源を
    // 持たないため、`PropVariantClear` を呼ばずに leak させてよい。これでスタックポインタの
    // free（ヒープ破壊）を避ける。リークの実害は無い。
    let _ = prop; // Initialize 完了まで prop を生存させるための明示 touch。
    let _ = params; // params も Initialize 完了まで生かす。

    Ok((client, capture, event, NATIVE_CHANNELS))
}

/// プロセスループバック activation 由来の HRESULT エラーを、古い OS（非対応）の場合は
/// [`Error::UnsupportedOsVersion`] へ、それ以外は [`Error::Backend`] へ写す。
fn map_process_activation_err(ctx: &str, e: windows::core::Error) -> Error {
    // E_NOTIMPL = 0x80004001 / E_NOINTERFACE = 0x80004002。プロセスループバック未対応
    // OS（古い Windows 10 等）はこれらで弾かれることがある。
    const E_NOTIMPL: i32 = 0x80004001u32 as i32;
    const E_NOINTERFACE: i32 = 0x80004002u32 as i32;
    let code = e.code().0;
    if code == E_NOTIMPL || code == E_NOINTERFACE {
        Error::UnsupportedOsVersion
    } else {
        map_hr(ctx, e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio_core::raw_ring;
    // `ProcessMode` は親モジュールの `use` 経由で `super::*` から見える。

    /// `new` + `native_format` は固定 `(48000, 2)` を返し panic しない。
    #[test]
    fn new_and_native_format_are_fixed() {
        let backend = WasapiProcessBackend::new(1234, ProcessMode::Include);
        assert_eq!(backend.native_format(), (48_000, 2));
        assert_eq!(backend.target_pid(), 1234);
        assert_eq!(backend.mode(), ProcessMode::Include);
    }

    /// PROPVARIANT ミラーが SDK レイアウト（24B/8 アライン）と一致すること（const assert
    /// に加えランタイムでも確認）。
    #[test]
    fn raw_propvariant_layout_matches_sdk() {
        assert_eq!(core::mem::size_of::<RawPropVariant>(), 24);
        assert_eq!(core::mem::align_of::<RawPropVariant>(), 8);
        assert_eq!(core::mem::size_of::<PROPVARIANT>(), 24);
    }

    /// `start` → `stop` がデバイス/対象 PID 有無を問わず panic しないこと。
    /// 対象 PID が無効/非対応 OS では `Err` を許容（panic だけ不可）。
    #[test]
    fn start_then_stop_tolerates_missing_target() {
        // 存在しない PID。activation 自体は通り得るが Initialize/capture で失敗し得る。
        let mut backend = WasapiProcessBackend::new(0xFFFF_FFFE, ProcessMode::Include);
        let (rate, channels) = backend.native_format();
        let cap = (rate as usize * channels as usize).max(1);
        let (prod, _cons) = raw_ring(cap);
        let sink = RawSink::new(prod, rate, channels);

        match backend.start(sink) {
            Ok(()) => {
                backend.stop();
                backend.stop();
            }
            Err(_e) => { /* 非対応 OS / activation 失敗は許容 */ }
        }
    }

    /// End-to-end test that records from a real process playing audio.
    /// Set `FLEXAUDIO_TEST_PID` and run with
    /// `cargo test -p flexaudio-os-windows -- --ignored`.
    #[test]
    #[ignore = "requires a real process playing audio; run with `FLEXAUDIO_TEST_PID=<pid> cargo test -p flexaudio-os-windows -- --ignored` on a Windows machine"]
    fn end_to_end_captures_real_audio() {
        use std::time::Duration;

        let pid: u32 = std::env::var("FLEXAUDIO_TEST_PID")
            .ok()
            .and_then(|s| s.parse().ok())
            .expect("set FLEXAUDIO_TEST_PID to the PID of a process playing audio");

        let mut backend = WasapiProcessBackend::new(pid, ProcessMode::Include);
        let (rate, channels) = backend.native_format();
        let cap = rate as usize * channels as usize * 2; // 約 2 秒
        let (prod, mut cons) = raw_ring(cap);
        let sink = RawSink::new(prod, rate, channels);

        backend.start(sink).expect("start should succeed");
        thread::sleep(Duration::from_millis(800));
        backend.stop();

        let mut buf = vec![0.0f32; cap];
        let got = cons.pop_slice(&mut buf);
        assert!(got > 0, "expected captured samples, got none");
    }
}
