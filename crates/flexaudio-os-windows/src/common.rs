//! Windows バックエンド共通ヘルパ: COM 初期化ガード・WAVEFORMATEX 解析・
//! HRESULT から [`Error`] への変換・WASAPI キャプチャループ。
//!
//! [`WasapiSystemBackend`](crate::WasapiSystemBackend) と
//! [`WasapiProcessBackend`](crate::WasapiProcessBackend) は専用スレッド上で同じキャプチャ
//! ループ（[`capture_loop`]）を回す。両者で違うのは `IAudioClient` の取得経路（古典
//! loopback か プロセスループバック activation か）とフォーマットの決め方（`GetMixFormat`
//! か 固定 WAVEFORMATEX か）だけで、Initialize〜GetService〜Start〜キャプチャ〜Stop は共通。

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use flexaudio_core::backend::RawSink;
use flexaudio_core::clock::monotonic_now_ns;
use flexaudio_core::types::Error;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    IAudioCaptureClient, IAudioClient, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// アクセス拒否系・デバイス不在系の HRESULT を型付き [`Error`] バリアントへ分類する。
///
/// WASAPI/COM の代表的な HRESULT を OS 横断のエラー型へ寄せる:
/// - アクセス拒否 → [`Error::PermissionDenied`]: `E_ACCESSDENIED`（マイク/音声
///   キャプチャのプライバシー拒否で来る）・`AUDCLNT_E_DEVICE_IN_USE`（排他使用中で
///   開けない）・`AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED`（排他不可）。
/// - デバイス不在/失効 → [`Error::DeviceNotFound`]: `AUDCLNT_E_DEVICE_INVALIDATED`
///   （対象エンドポイント/デバイスが消えた・失効）・`E_NOTFOUND`（要素/エンドポイント不在）。
///
/// それ以外は分類できないので `None` を返し、[`map_hr`] が文脈付き [`Error::Backend`] へ
/// フォールバックする。定数 import は `windows` crate の版差で揺れるので、ここでは値が安定
/// している生の i32 で比較する（16 進は下のコメントに併記）。
pub(crate) fn classify_hr(code: i32) -> Option<Error> {
    // アクセス拒否系 → PermissionDenied
    const E_ACCESSDENIED: i32 = 0x80070005u32 as i32;
    const AUDCLNT_E_DEVICE_IN_USE: i32 = 0x8889000Au32 as i32;
    const AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED: i32 = 0x8889000Eu32 as i32;
    // デバイス不在/失効系 → DeviceNotFound
    const AUDCLNT_E_DEVICE_INVALIDATED: i32 = 0x88890004u32 as i32;
    // E_NOTFOUND（ERROR_NOT_FOUND を HRESULT 化した 0x80070490）。指定エンドポイント/要素不在。
    const E_NOTFOUND: i32 = 0x80070490u32 as i32;

    match code {
        E_ACCESSDENIED | AUDCLNT_E_DEVICE_IN_USE | AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED => {
            Some(Error::PermissionDenied)
        }
        AUDCLNT_E_DEVICE_INVALIDATED | E_NOTFOUND => Some(Error::DeviceNotFound),
        _ => None,
    }
}

/// HRESULT を文脈文字列付きで [`Error`] へ変換する。
///
/// アクセス拒否系/デバイス不在系は [`classify_hr`] で型付きバリアント
/// （[`Error::PermissionDenied`] / [`Error::DeviceNotFound`]）へ寄せ、分類できないものは
/// 文脈付き [`Error::Backend`] にフォールバックする。
pub(crate) fn map_hr(ctx: &str, e: windows::core::Error) -> Error {
    if let Some(mapped) = classify_hr(e.code().0) {
        return mapped;
    }
    Error::Backend(format!("{ctx}: {e}"))
}

/// 単調クロック（ns）。コア [`monotonic_now_ns`] をそのまま使う。下流の
/// `ClockNormalizer` が初回原点を取るため、到着時刻の単調近似で足りる。
pub(crate) fn now_ns() -> i64 {
    monotonic_now_ns()
}

/// COM 初期化ガード。`new` で `CoInitializeEx(MULTITHREADED)` し、`Drop` で
/// `CoUninitialize` する（同一スレッド上で対称に呼ぶ）。
///
/// 既に別モードで初期化済み（`RPC_E_CHANGED_MODE`）でも失敗扱いにしない。他所が STA で
/// 初期化していても WASAPI 呼び出しは通るからで、その場合は `uninit_on_drop=false` にして
/// 他所の初期化に対して `CoUninitialize` を呼ばないようにする。
pub(crate) struct ComThread {
    uninit_on_drop: bool,
}

impl ComThread {
    /// このスレッドで COM を初期化する。panic しない。
    pub(crate) fn new() -> Self {
        // CoInitializeEx は 0.54 では HRESULT を返す（Result ではない）。
        // S_OK / S_FALSE は成功、RPC_E_CHANGED_MODE は「既に別モードで初期化済み」。
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        // 成功（S_OK=0 / S_FALSE=1）なら自分が初期化したので drop で uninit する。
        // RPC_E_CHANGED_MODE 等は他所が初期化済み → uninit しない。
        let uninit_on_drop = hr.is_ok();
        Self { uninit_on_drop }
    }
}

impl Drop for ComThread {
    fn drop(&mut self) {
        if self.uninit_on_drop {
            unsafe { CoUninitialize() };
        }
    }
}

/// `WAVEFORMATEX`（必要なら `WAVEFORMATEXTENSIBLE`）を解析し、サブフォーマットが
/// IEEE float なら `Ok((rate, channels))` を返す。PCM 系（int）は非対応で
/// [`Error::Backend`]。共有モードの MixFormat は実機では float が常態。
///
/// `WAVEFORMATEX` / `WAVEFORMATEXTENSIBLE` は `#[repr(C, packed(1))]` なので、packed
/// フィールドへの参照生成は UB。参照を取らず `addr_of!` + `read_unaligned` で値コピーする。
///
/// # Safety
/// `pwfx` は有効な `WAVEFORMATEX` を指していること（`GetMixFormat` の戻り値）。
pub(crate) unsafe fn parse_mix_format(pwfx: *const WAVEFORMATEX) -> Result<(u32, u16), Error> {
    use core::ptr::addr_of;

    if pwfx.is_null() {
        return Err(Error::Backend("GetMixFormat returned null format".into()));
    }
    // packed フィールドは値コピーで読む。
    let format_tag = addr_of!((*pwfx).wFormatTag).read_unaligned();
    let rate = addr_of!((*pwfx).nSamplesPerSec).read_unaligned();
    let channels = addr_of!((*pwfx).nChannels).read_unaligned();
    let bits = addr_of!((*pwfx).wBitsPerSample).read_unaligned();
    let cb_size = addr_of!((*pwfx).cbSize).read_unaligned();

    // 定数は u32。match パターンに識別子を置くと束縛と誤解されるため `==` 比較で判定する。
    let tag = format_tag as u32;
    let is_float = if tag == WAVE_FORMAT_IEEE_FLOAT {
        true
    } else if tag == WAVE_FORMAT_EXTENSIBLE {
        // cbSize が EXTENSIBLE 拡張ぶん（22）以上あるなら EXTENSIBLE として読む。
        if (cb_size as usize) >= 22 {
            let sub = addr_of!((*(pwfx as *const WAVEFORMATEXTENSIBLE)).SubFormat).read_unaligned();
            sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
        } else {
            false
        }
    } else {
        false
    };

    if !is_float {
        return Err(Error::Backend(format!(
            "unsupported mix format (not IEEE float): tag={tag} bits={bits}"
        )));
    }
    Ok((rate, channels))
}

/// イベント駆動 WASAPI キャプチャループ（専用スレッド上で実行）。
///
/// 既に Initialize 済みの `client`（共有モード・LOOPBACK|EVENTCALLBACK で初期化済み）と
/// その capture サービス・イベントハンドル・チャンネル数を受け取り、`stop_flag` が
/// 立つまでパケットを取り出して [`RawSink::push`] へ流す。終了時に `client.Stop()` し、
/// イベントハンドルを閉じる。
///
/// パケットは f32 interleaved（`channels` ch）として読む。WASAPI のバッファは 8 バイト
/// 境界以上で確保されるので `*const f32` キャストは安全。無音フラグ
/// （`AUDCLNT_BUFFERFLAGS_SILENT`）時は 0 を `frames*channels` 個 push する（DC 化防止）。
///
/// # Safety
/// `client` / `capture` / `event` は同一スレッドで Initialize 済みかつ有効な COM
/// オブジェクト/ハンドルであること。`channels >= 1`。
pub(crate) unsafe fn capture_loop(
    client: &IAudioClient,
    capture: &IAudioCaptureClient,
    event: HANDLE,
    channels: u16,
    mut sink: RawSink,
    stop_flag: &Arc<AtomicBool>,
) {
    let channels = channels.max(1) as usize;
    // 無音フラグ時に 0 を流すための再利用バッファ。RT ループに入る前（Start 前）に最大
    // 想定長で確保しておき、ループ内でアロケートしないようにする。1 パケットの最大フレーム
    // 数はエンジンのバッファサイズ（`GetBufferSize`）が上限なので、`buffer_frames * channels`
    // ぶん取っておけばループ内の `resize` は容量内 no-op で済む。`GetBufferSize` が失敗した
    // ときだけ空のままで、その場合はループ内初回の resize にフォールバックする。
    let mut silence: Vec<f32> = Vec::new();
    if let Ok(buffer_frames) = client.GetBufferSize() {
        let max_silence = (buffer_frames as usize).saturating_mul(channels);
        silence.resize(max_silence, 0.0);
    }

    if client.Start().is_err() {
        // Start に失敗したら何もせず戻る（setup 側で既に Start 済みのため通常来ない）。
        let _ = CloseHandle(event);
        return;
    }

    while !stop_flag.load(Ordering::SeqCst) {
        // 100ms 経過かイベント発火で起きる。タイムアウト付きにして停止指示を取りこぼさない。
        let _ = WaitForSingleObject(event, 100);
        if stop_flag.load(Ordering::SeqCst) {
            break;
        }

        loop {
            let packet = match capture.GetNextPacketSize() {
                Ok(p) => p,
                Err(_e) => {
                    // 対象 PID 終了等で DEVICE_INVALIDATED になり得る。ループを抜けて停止。
                    stop_flag.store(true, Ordering::SeqCst);
                    break;
                }
            };
            if packet == 0 {
                break;
            }

            let mut pdata: *mut u8 = std::ptr::null_mut();
            let mut frames: u32 = 0;
            let mut flags: u32 = 0;
            if capture
                .GetBuffer(&mut pdata, &mut frames, &mut flags, None, None)
                .is_err()
            {
                stop_flag.store(true, Ordering::SeqCst);
                break;
            }

            let n = frames as usize * channels;
            // push を catch_unwind で包む。ここは自前スレッドで FFI 境界ではないが、万一
            // `RawSink::push` 等が panic しても、下の `ReleaseBuffer` / `client.Stop()` /
            // `CloseHandle` のクリーンアップを飛ばさないため。捕捉しても処理は続行する。
            let _ = catch_unwind(AssertUnwindSafe(|| {
                if (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 {
                    // 無音: 0 を n 個 push（下流のギャップ判定/DC 化防止のため）。
                    if silence.len() < n {
                        silence.resize(n, 0.0);
                    }
                    if n > 0 {
                        sink.push(&silence[..n], now_ns());
                    }
                } else if !pdata.is_null() && n > 0 {
                    let slice = std::slice::from_raw_parts(pdata as *const f32, n);
                    sink.push(slice, now_ns());
                }
            }));

            // 取得した frames を必ず解放する（成功/失敗を問わず frames を渡す）。
            let _ = capture.ReleaseBuffer(frames);
        }
    }

    let _ = client.Stop();
    let _ = CloseHandle(event);
}

/// 共有モード・LOOPBACK|EVENTCALLBACK で `client` を Initialize し、イベントハンドルを
/// 結び付けて capture サービスを取り出す共通シーケンス。成功時に
/// `(IAudioCaptureClient, event_handle)` を返す。
///
/// `pwfx` は Initialize に渡すフォーマット（System は `GetMixFormat` の生ポインタ、
/// Process は自前固定 WAVEFORMATEX のポインタ）。
///
/// # Safety
/// `client` は同一スレッドで Activate 済みの有効な COM オブジェクト。`pwfx` は有効な
/// `WAVEFORMATEX` を指すこと。
pub(crate) unsafe fn init_loopback_capture(
    client: &IAudioClient,
    pwfx: *const WAVEFORMATEX,
) -> Result<(IAudioCaptureClient, HANDLE), Error> {
    client
        .Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            0, // hnsBufferDuration: 0 = エンジン既定
            0, // hnsPeriodicity: 共有モードは 0
            pwfx,
            None,
        )
        .map_err(|e| map_hr("IAudioClient::Initialize", e))?;

    // 手動リセット=false / 初期非シグナル / 無名イベント。
    let event =
        CreateEventW(None, false, false, PCWSTR::null()).map_err(|e| map_hr("CreateEventW", e))?;

    if let Err(e) = client.SetEventHandle(event) {
        let _ = CloseHandle(event);
        return Err(map_hr("IAudioClient::SetEventHandle", e));
    }

    let capture: IAudioCaptureClient = match client.GetService() {
        Ok(c) => c,
        Err(e) => {
            let _ = CloseHandle(event);
            return Err(map_hr("IAudioClient::GetService(IAudioCaptureClient)", e));
        }
    };

    Ok((capture, event))
}

/// `WaitForSingleObject` の戻り値がシグナル（`WAIT_OBJECT_0`）かどうか。
/// process backend の activation 完了待ちで使う。
pub(crate) fn wait_event_signaled(handle: HANDLE, timeout_ms: u32) -> bool {
    let r = unsafe { WaitForSingleObject(handle, timeout_ms) };
    r == WAIT_OBJECT_0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// アクセス拒否系の HRESULT は PermissionDenied に分類される（監査 P1-2）。
    #[test]
    fn classify_hr_maps_access_denied_to_permission_denied() {
        // E_ACCESSDENIED
        assert!(matches!(
            classify_hr(0x80070005u32 as i32),
            Some(Error::PermissionDenied)
        ));
        // AUDCLNT_E_DEVICE_IN_USE
        assert!(matches!(
            classify_hr(0x8889000Au32 as i32),
            Some(Error::PermissionDenied)
        ));
        // AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED
        assert!(matches!(
            classify_hr(0x8889000Eu32 as i32),
            Some(Error::PermissionDenied)
        ));
    }

    /// デバイス不在/失効系の HRESULT は DeviceNotFound に分類される（監査 P1-4）。
    #[test]
    fn classify_hr_maps_device_codes_to_device_not_found() {
        // AUDCLNT_E_DEVICE_INVALIDATED
        assert!(matches!(
            classify_hr(0x88890004u32 as i32),
            Some(Error::DeviceNotFound)
        ));
        // E_NOTFOUND
        assert!(matches!(
            classify_hr(0x80070490u32 as i32),
            Some(Error::DeviceNotFound)
        ));
    }

    /// 分類できない HRESULT は None（map_hr が Backend にフォールバックする）。
    #[test]
    fn classify_hr_unknown_is_none() {
        // E_FAIL（汎用失敗）は分類対象外。
        assert!(classify_hr(0x80004005u32 as i32).is_none());
        // S_OK は失敗ですらないので当然 None。
        assert!(classify_hr(0).is_none());
        // AUDCLNT_E_UNSUPPORTED_FORMAT は本質的 Backend（フォーマット非対応）。
        assert!(classify_hr(0x88890008u32 as i32).is_none());
    }
}
