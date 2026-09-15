//! macOS バックエンドの共通ヘルパ。PID→AudioObjectID 変換、`AudioObjectGetPropertyData`
//! の薄いラッパ、ASBD から `(rate, channels)` と float 判定の読み取り、`OSStatus`→[`Error`]
//! 変換、単調クロック。
//!
//! [`MacSystemBackend`](crate::MacSystemBackend) と
//! [`MacProcessBackend`](crate::MacProcessBackend) はどちらも [`tap`](crate::tap) の
//! チェーン（process tap → aggregate device → IOProc）を回す。両者の違いは
//! [`CATapDescription`] の作り方（INCLUDE = mixdown / EXCLUDE = global）だけなので、
//! tap 生成から aggregate・IOProc・破棄までは共通にしてある。

use std::ffi::c_void;
use std::ptr::NonNull;

use flexaudio_core::clock::monotonic_now_ns;
use flexaudio_core::types::Error;

use objc2_core_audio::{
    kAudioHardwareBadPropertySizeError, kAudioHardwarePropertyTranslatePIDToProcessObject,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
    kAudioTapPropertyFormat, AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize,
    AudioObjectID, AudioObjectPropertyAddress,
};
use objc2_core_audio_types::{kAudioFormatFlagIsFloat, AudioStreamBasicDescription};
use objc2_core_foundation::{CFRetained, CFString};

/// CoreAudio の `OSStatus` 成功値 `noErr`。
pub(crate) const NO_ERR: i32 = 0;

/// フォーマット取得に失敗したときのフォールバック `(48000, 2)`。
pub(crate) const FALLBACK_FORMAT: (u32, u16) = (48_000, 2);

/// 単調クロック（ns）。下流の `ClockNormalizer` が初回原点を取るので、ここは到着時刻の
/// 単調近似で足りる。
pub(crate) fn now_ns() -> i64 {
    monotonic_now_ns()
}

/// `OSStatus` を文脈文字列付きで [`Error`] へ変換する。
///
/// 権限拒否系（`kAudioHardwareIllegalOperationError`。TCC で tap 作成が弾かれると来る）を
/// [`Error::PermissionDenied`] へ、デバイス不在系（`kAudioHardwareBadDeviceError`）を
/// [`Error::DeviceNotFound`] へ寄せ、他 OS と error 種別を揃える。確実な権限判定は初回
/// キャプチャの OS プロンプトに委ねる方針（private TCC SPI 不使用）なので、ここは「拒否
/// らしき」コードを最善努力でマップするだけ。それ以外は [`Error::Backend`]。
pub(crate) fn map_os_status(ctx: &str, status: i32) -> Error {
    // CoreAudio の代表的 OSStatus（4cc）。
    // 'who?' = kAudioHardwareUnknownPropertyError, '!obj' = kAudioHardwareBadObjectError,
    // 'nope' = kAudioHardwareIllegalOperationError, 'stop' = kAudioHardwareNotRunningError,
    // '!dev' = kAudioHardwareBadDeviceError。
    const ILLEGAL_OPERATION: i32 = 0x6e6f7065; // 'nope' — TCC 不許可時に来やすい
    const NOT_RUNNING: i32 = 0x73746f70; // 'stop'
    const BAD_OBJECT: i32 = 0x216f626a; // '!obj'
    const BAD_DEVICE: i32 = 0x21646576; // '!dev' — 指定デバイス不在/不正

    match status {
        // 'nope'（不正操作）は権限未許可で tap/aggregate 生成が拒否されたときも来るので
        // PermissionDenied に寄せる（OS プロンプト未承認時の典型）。
        ILLEGAL_OPERATION => Error::PermissionDenied,
        // '!dev'（不正デバイス）は指定デバイス/エンドポイント不在に当たるので DeviceNotFound へ。
        BAD_DEVICE => Error::DeviceNotFound,
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

/// プロパティアドレスを scope / main element 指定で作る。
fn property_address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// global scope / main element のプロパティアドレスを作る。
fn global_address(selector: u32) -> AudioObjectPropertyAddress {
    property_address(selector, kAudioObjectPropertyScopeGlobal)
}

/// CFString 型プロパティ（デバイス名 / UID / プロセスの bundle ID）を読んで `String`
/// にする。取得できなければ `None`。
///
/// これらのプロパティは `CFStringRef` を +1 retain で返す（CF の Copy 規約）。
/// `CFRetained::from_raw` で所有権を受け取り、drop で release する。
pub(crate) fn read_cfstring_property(
    object: AudioObjectID,
    selector: u32,
    scope: u32,
) -> Option<String> {
    let addr = property_address(selector, scope);
    let mut cf_ref: *const CFString = core::ptr::null();
    let mut size = core::mem::size_of::<*const CFString>() as u32;
    // SAFETY: addr/size は有効なローカル。out は CFStringRef 1 個ぶんのポインタ領域。
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&addr),
            0,
            core::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new_unchecked((&mut cf_ref as *mut *const CFString).cast::<c_void>()),
        )
    };
    if status != NO_ERR || cf_ref.is_null() {
        return None;
    }
    // SAFETY: cf_ref は OS が +1 retain して返した有効な CFString。from_raw で所有権を取り、
    // この関数を抜けるときに drop が release する。
    let cf = unsafe { CFRetained::from_raw(NonNull::new_unchecked(cf_ref as *mut CFString)) };
    Some(cf.to_string())
}

/// PID を `AudioObjectID`（プロセスオブジェクト）へ変換する。
///
/// system object に `kAudioHardwarePropertyTranslatePIDToProcessObject` を、qualifier に
/// `pid`(i32) を渡して問い合わせる。`Ok(0)` はそのプロセスが無音/不在で対応するオーディオ
/// オブジェクトが無いという意味（呼び出し側が [`Error::DeviceNotFound`] 等に解釈する）。
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

/// system object の「`AudioObjectID` の配列」型プロパティを読む
/// （`kAudioHardwarePropertyDevices` のデバイス一覧、`kAudioHardwarePropertyProcessObjectList`
/// のプロセスオブジェクト一覧など）。
///
/// size 照会と data 読みのあいだに一覧の大きさが変わると
/// `kAudioHardwareBadPropertySizeError` になり得るので、一時的な失敗は数回読み直す。
/// `GetPropertyData` に渡す大きさは要素数 × 要素の大きさ（確保したバッファちょうどの
/// バイト数）。
///
/// 失敗時は生の `OSStatus` を返す。空扱いにするか [`map_os_status`] で型付きエラーにするかは
/// 呼び出し側が決める（デバイス列挙は空扱い、プロセス列挙は型付きエラー）。
pub(crate) fn read_system_object_list(selector: u32) -> Result<Vec<AudioObjectID>, i32> {
    const MAX_ATTEMPTS: u32 = 4;
    let mut last_status = NO_ERR;
    for _ in 0..MAX_ATTEMPTS {
        match read_system_object_list_once(selector) {
            Ok(ids) => return Ok(ids),
            Err(status) if status == kAudioHardwareBadPropertySizeError => {
                last_status = status;
            }
            Err(status) => return Err(status),
        }
    }
    Err(last_status)
}

fn read_system_object_list_once(selector: u32) -> Result<Vec<AudioObjectID>, i32> {
    let address = global_address(selector);
    let mut size: u32 = 0;
    // SAFETY: address/size は有効なローカル。qualifier 不要（null/0）。
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            kAudioObjectSystemObject as AudioObjectID,
            NonNull::from(&address),
            0,
            core::ptr::null(),
            NonNull::from(&mut size),
        )
    };
    if status != NO_ERR {
        return Err(status);
    }
    let elem = core::mem::size_of::<AudioObjectID>();
    let count = size as usize / elem;
    if count == 0 {
        return Ok(Vec::new());
    }
    let mut ids: Vec<AudioObjectID> = vec![0; count];
    let mut data_size = (count * elem) as u32;
    // SAFETY: ids は count 要素ぶん確保済み。data_size は要素数×要素の大きさ。
    let status = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject as AudioObjectID,
            NonNull::from(&address),
            0,
            core::ptr::null(),
            NonNull::from(&mut data_size),
            NonNull::new_unchecked(ids.as_mut_ptr().cast::<c_void>()),
        )
    };
    if status != NO_ERR {
        return Err(status);
    }
    // 実際に書かれた要素数に詰める（2 回の呼び出しの間に一覧が縮むことがある）。
    ids.truncate(data_size as usize / elem);
    Ok(ids)
}

/// tap の `kAudioTapPropertyFormat`（ASBD）を読む。
///
/// 取得できなければ `None`（呼び出し側がフォールバックを使う）。rate/channels と
/// `mFormatFlags`（float 判定）の両方をここから読む。
fn read_tap_asbd(tap_id: AudioObjectID) -> Option<AudioStreamBasicDescription> {
    let address = global_address(kAudioTapPropertyFormat);
    // ASBD には Default が無いので、ゼロ初期化してから OS に埋めさせる。
    // SAFETY: AudioStreamBasicDescription は数値フィールドだけの `#[repr(C)]` POD なので
    // ゼロ初期化が有効な値になる。
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

/// tap の ASBD から `(sample_rate, channels)` を読む。取得できなければ `None`。
pub(crate) fn tap_native_format(tap_id: AudioObjectID) -> Option<(u32, u16)> {
    let asbd = read_tap_asbd(tap_id)?;
    let rate = asbd.mSampleRate as u32;
    let channels = asbd.mChannelsPerFrame as u16;
    if rate == 0 || channels == 0 {
        return None;
    }
    Some((rate, channels))
}

/// tap の ASBD が float サンプル（`kAudioFormatFlagIsFloat`）かどうかを調べる。
///
/// IOProc は `mData as *const f32` でサンプルをそのまま f32 として読むので、tap が非 float
/// （int PCM 等）だと UB になり得る。build 時にこれを呼び、非 float と確定したときだけ弾く。
/// ASBD を取得できなかった（`None`）ときは判定不能なので、呼び出し側はフォールバック挙動
/// （float 決め打ち）を続ける。実機の tap は常に float なので取得不能で弾く必要は無い。
///
/// - `Some(true)`  : float ビットが立っている。
/// - `Some(false)` : float ビットが無い（非 float なので弾くべき）。
/// - `None`        : ASBD を取得できず判定不能。
pub(crate) fn tap_format_is_float(tap_id: AudioObjectID) -> Option<bool> {
    let asbd = read_tap_asbd(tap_id)?;
    Some((asbd.mFormatFlags & kAudioFormatFlagIsFloat) != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `map_os_status` が代表コードを期待どおり変換すること。
    #[test]
    fn map_os_status_maps_known_codes() {
        assert!(matches!(
            map_os_status("x", 0x6e6f7065),
            Error::PermissionDenied
        ));
        // '!dev'（不正デバイス）は DeviceNotFound。
        assert!(matches!(
            map_os_status("x", 0x21646576),
            Error::DeviceNotFound
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
