//! macOS バックエンド共通ヘルパ: PID→AudioObjectID 変換・`AudioObjectGetPropertyData`
//! 薄ラッパ・ASBD→`(rate, channels, interleaved)` 解釈・`OSStatus`→[`Error`] 写経・
//! 単調クロック・planar→interleaved インターリーブ。
//!
//! [`MacSystemBackend`](crate::MacSystemBackend) と
//! [`MacProcessBackend`](crate::MacProcessBackend) はいずれも [`tap`](crate::tap) の
//! チェーン（process tap → aggregate device → IOProc）を回す。両者の違いは
//! 「[`CATapDescription`] をどう作るか（INCLUDE = mixdown / EXCLUDE = global）」だけで、
//! tap 生成〜aggregate〜IOProc〜破棄は共通化できる。

use std::ffi::c_void;
use std::ptr::NonNull;

use flexaudio_core::clock::monotonic_now_ns;
use flexaudio_core::types::Error;

use objc2_core_audio::{
    kAudioHardwarePropertyTranslatePIDToProcessObject, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, kAudioTapPropertyFormat,
    AudioObjectGetPropertyData, AudioObjectID, AudioObjectPropertyAddress,
};
use objc2_core_audio_types::{kAudioFormatFlagIsFloat, AudioStreamBasicDescription};

/// CoreAudio の `OSStatus` 成功値（= `noErr`）。0 が成功。
pub(crate) const NO_ERR: i32 = 0;

/// 取得失敗時のフォールバックフォーマット `(48000, 2)`（panic しない）。
pub(crate) const FALLBACK_FORMAT: (u32, u16) = (48_000, 2);

/// 単調クロック（ns）。コア [`monotonic_now_ns`] をそのまま使う。下流の
/// `ClockNormalizer` が初回原点を取るため、到着時刻の単調近似で足りる。
pub(crate) fn now_ns() -> i64 {
    monotonic_now_ns()
}

/// `OSStatus` を [`Error`] へ写す。文脈文字列付き。
///
/// 権限拒否系（`kAudioHardwareIllegalOperationError` 等、TCC で tap 作成が弾かれる
/// 場合に来る代表的なコード）を [`Error::PermissionDenied`] へ寄せる。確実な権限判定は
/// 初回キャプチャの OS プロンプト挙動に委ねる方針なので（private TCC SPI 不使用）、
/// ここでは「拒否らしき」コードを最善努力でマップするに留め、それ以外は
/// [`Error::Backend`] にする。
pub(crate) fn map_os_status(ctx: &str, status: i32) -> Error {
    // CoreAudio の代表的 OSStatus（4cc）。
    // 'who?' = kAudioHardwareUnknownPropertyError, '!obj' = kAudioHardwareBadObjectError,
    // 'nope' = kAudioHardwareIllegalOperationError, 'stop' = kAudioHardwareNotRunningError,
    // '!dev' = kAudioHardwareBadDeviceError。
    const ILLEGAL_OPERATION: i32 = 0x6e6f7065; // 'nope' — TCC 不許可時に来やすい
    const NOT_RUNNING: i32 = 0x73746f70; // 'stop'
    const BAD_OBJECT: i32 = 0x216f626a; // '!obj'

    match status {
        // 'nope'（不正操作）は権限未許可で tap/aggregate 生成が拒否されたケースを含むため
        // PermissionDenied に寄せる（OS プロンプト未承認時の典型）。
        ILLEGAL_OPERATION => Error::PermissionDenied,
        NOT_RUNNING => Error::Backend(format!("{ctx}: CoreAudio not running (OSStatus 'stop')")),
        BAD_OBJECT => Error::Backend(format!("{ctx}: bad audio object (OSStatus '!obj')")),
        other => {
            // 4cc を可読化（印字可能 ASCII なら4文字、そうでなければ10進）。
            let be = (other as u32).to_be_bytes();
            if be.iter().all(|&b| (0x20..=0x7e).contains(&b)) {
                Error::Backend(format!(
                    "{ctx}: OSStatus '{}' ({other})",
                    String::from_utf8_lossy(&be)
                ))
            } else {
                Error::Backend(format!("{ctx}: OSStatus {other}"))
            }
        }
    }
}

/// 大域プロパティ（system object / global scope / main element）の
/// [`AudioObjectPropertyAddress`] を作る共通ヘルパ。
fn global_address(selector: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// PID を `AudioObjectID`（プロセスオブジェクト）へ変換する。
///
/// system object に対し `kAudioHardwarePropertyTranslatePIDToProcessObject` を、
/// qualifier に `pid`(i32) を渡して問い合わせる。`Ok(0)` は「該当プロセスが無音/不在で
/// 対応するオーディオプロセスオブジェクトが無い」を意味する（呼び出し側が
/// [`Error::DeviceNotFound`] 等に解釈する）。
pub(crate) fn translate_pid_to_object(pid: i32) -> Result<AudioObjectID, Error> {
    let address = global_address(kAudioHardwarePropertyTranslatePIDToProcessObject);
    let mut out_object: AudioObjectID = 0;
    let mut size = core::mem::size_of::<AudioObjectID>() as u32;

    // SAFETY: address/size/out は有効なローカル。qualifier は pid(i32) への有効ポインタ。
    let status = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject as AudioObjectID,
            NonNull::from(&address),
            core::mem::size_of::<i32>() as u32,
            (&pid as *const i32).cast::<c_void>(),
            NonNull::from(&mut size),
            NonNull::new_unchecked((&mut out_object as *mut AudioObjectID).cast::<c_void>()),
        )
    };
    if status != NO_ERR {
        return Err(map_os_status(
            "AudioObjectGetPropertyData(TranslatePIDToProcessObject)",
            status,
        ));
    }
    Ok(out_object)
}

/// tap の `kAudioTapPropertyFormat`（ASBD）を読む共通ヘルパ。
///
/// 取得できなければ `None`（呼び出し側がフォールバックを使う・panic しない）。
/// rate/channels と `mFormatFlags`（float 判定）の双方を読む元として使う。
fn read_tap_asbd(tap_id: AudioObjectID) -> Option<AudioStreamBasicDescription> {
    let address = global_address(kAudioTapPropertyFormat);
    // ASBD は plain-old-data。ゼロ初期化してから OS に埋めさせる（Default 未実装）。
    // SAFETY: AudioStreamBasicDescription は数値フィールドのみの `#[repr(C)]` POD。
    let mut asbd: AudioStreamBasicDescription = unsafe { core::mem::zeroed() };
    let mut size = core::mem::size_of::<AudioStreamBasicDescription>() as u32;

    // SAFETY: address/size/asbd は有効なローカル。qualifier 不要（null/0）。
    let status = unsafe {
        AudioObjectGetPropertyData(
            tap_id,
            NonNull::from(&address),
            0,
            core::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new_unchecked(
                (&mut asbd as *mut AudioStreamBasicDescription).cast::<c_void>(),
            ),
        )
    };
    if status != NO_ERR {
        return None;
    }
    Some(asbd)
}

/// tap の `kAudioTapPropertyFormat`（ASBD）から `(sample_rate, channels)` を読む。
///
/// 取得できなければ `None`（呼び出し側がフォールバックを使う・panic しない）。
pub(crate) fn tap_native_format(tap_id: AudioObjectID) -> Option<(u32, u16)> {
    let asbd = read_tap_asbd(tap_id)?;
    let rate = asbd.mSampleRate as u32;
    let channels = asbd.mChannelsPerFrame as u16;
    if rate == 0 || channels == 0 {
        return None;
    }
    Some((rate, channels))
}

/// tap の ASBD が **float サンプル**（`kAudioFormatFlagIsFloat`）であることを検証する。
///
/// IOProc では `mData as *const f32` でサンプルをそのまま f32 として読むため、tap が
/// 非 float（int PCM 等）だと UB になり得る。そこで build 時にこの判定を行い、
/// **float でないと確定した場合のみ** `Some(false)` を返す。ASBD を取得できなかった
/// （`None`）場合は「検証できない」を表し、呼び出し側はフォールバック挙動（従来どおり
/// float 決め打ち）を続ける（実機の tap は float 常態のため、取得不能で弾かない）。
///
/// 返り値:
/// - `Some(true)`  : ASBD を読めて float ビットが立っている（安全）。
/// - `Some(false)` : ASBD を読めたが float ビットが無い（非 float = 危険、弾くべき）。
/// - `None`        : ASBD を取得できなかった（判定不能 = 従来挙動を継続）。
pub(crate) fn tap_format_is_float(tap_id: AudioObjectID) -> Option<bool> {
    let asbd = read_tap_asbd(tap_id)?;
    Some((asbd.mFormatFlags & kAudioFormatFlagIsFloat) != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `map_os_status` が代表コードを期待どおり写経すること。
    #[test]
    fn map_os_status_maps_known_codes() {
        assert!(matches!(
            map_os_status("x", 0x6e6f7065),
            Error::PermissionDenied
        ));
        assert!(matches!(map_os_status("x", 0x73746f70), Error::Backend(_)));
        // 4cc 可読化（印字可能 ASCII）。
        let e = map_os_status("ctx", i32::from_be_bytes(*b"abcd"));
        assert!(format!("{e}").contains("abcd"));
    }

    /// フォールバックフォーマットは契約どおり `(48000, 2)`。
    #[test]
    fn fallback_format_is_48k_stereo() {
        assert_eq!(FALLBACK_FORMAT, (48_000, 2));
    }
}
