//! 出力デバイスの列挙と名前→UID 解決。
//!
//! [`list_output_devices`] が出力（再生）デバイスを列挙して [`DeviceInfo`] のリストを返す。
//! [`MacSystemBackend`](crate::MacSystemBackend) が特定デバイスを対象に tap を作るとき、
//! 公開 ID（= デバイス名）から CoreAudio の device UID を引く [`uid_for_device_name`] を使う。
//!
//! 全 OS バックエンドの `DeviceInfo` 形に合わせる: `id` と `name` はどちらもデバイス名、
//! `sample_rate`/`channels` はデバイスのフォーマット、`is_loopback` は常に `true`（出力の
//! monitor）、`is_default` は既定出力デバイスと一致するとき `true`。
//!
//! # 名前を ID に使う理由
//! 出力デバイスは安定キーとして UID を持つが、他 OS の `DeviceInfo.id` 表示と揃えるため
//! 公開 ID にはデバイス名を使い、tap 直前に内部で UID へ解決する。同名デバイスが複数ある場合は
//! 最初に一致したものを使う。

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2_core_audio::{
    kAudioDevicePropertyDeviceUID, kAudioDevicePropertyNominalSampleRate,
    kAudioDevicePropertyStreamConfiguration, kAudioDevicePropertyStreams,
    kAudioHardwarePropertyDefaultOutputDevice, kAudioHardwarePropertyDevices,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal,
    kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject, AudioObjectGetPropertyData,
    AudioObjectGetPropertyDataSize, AudioObjectID, AudioObjectPropertyAddress,
};
use objc2_core_audio_types::AudioBufferList;
use objc2_core_foundation::{CFRetained, CFString};

use flexaudio_core::types::{DeviceInfo, Result, SourceKind};

use crate::common::FALLBACK_FORMAT;

/// プロパティアドレスを scope/element 指定で作る。
fn address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// system object の `kAudioHardwarePropertyDevices` を読み、全 `AudioObjectID` を返す。
///
/// 取得できなければ空 vec（呼び出し側は空リストを返すだけ）。
fn all_device_ids() -> Vec<AudioObjectID> {
    let addr = address(
        kAudioHardwarePropertyDevices,
        kAudioObjectPropertyScopeGlobal,
    );
    let mut size: u32 = 0;
    // SAFETY: addr/size は有効なローカル。qualifier 不要。
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            kAudioObjectSystemObject as AudioObjectID,
            NonNull::from(&addr),
            0,
            core::ptr::null(),
            NonNull::from(&mut size),
        )
    };
    if status != 0 || size == 0 {
        return Vec::new();
    }
    let count = size as usize / core::mem::size_of::<AudioObjectID>();
    let mut ids: Vec<AudioObjectID> = vec![0; count];
    // SAFETY: ids は count 要素ぶん確保済み。size はその総バイト数。
    let status = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject as AudioObjectID,
            NonNull::from(&addr),
            0,
            core::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new_unchecked(ids.as_mut_ptr().cast::<c_void>()),
        )
    };
    if status != 0 {
        return Vec::new();
    }
    // 実際に書かれた要素数に詰める（size が縮むことがある）。
    let written = size as usize / core::mem::size_of::<AudioObjectID>();
    ids.truncate(written);
    ids
}

/// CFString 型プロパティ（名前 / UID）を読んで `String` にする。取得できなければ `None`。
///
/// これらのプロパティは `CFStringRef` を +1 retain で返す（CF の Copy 規約）。
/// `CFRetained::from_raw` で所有権を受け取り、drop で release する。
fn read_cfstring_property(object: AudioObjectID, selector: u32, scope: u32) -> Option<String> {
    let addr = address(selector, scope);
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
    if status != 0 || cf_ref.is_null() {
        return None;
    }
    // SAFETY: cf_ref は OS が +1 retain して返した有効な CFString。from_raw で所有権を取り、
    // この関数を抜けるときに drop が release する。
    let cf = unsafe { CFRetained::from_raw(NonNull::new_unchecked(cf_ref as *mut CFString)) };
    Some(cf.to_string())
}

/// デバイスの `kAudioDevicePropertyNominalSampleRate`（output scope, Float64）を読む。
fn device_sample_rate(device: AudioObjectID) -> Option<u32> {
    let addr = address(
        kAudioDevicePropertyNominalSampleRate,
        kAudioObjectPropertyScopeOutput,
    );
    let mut rate: f64 = 0.0;
    let mut size = core::mem::size_of::<f64>() as u32;
    // SAFETY: addr/size/rate は有効なローカル。
    let status = unsafe {
        AudioObjectGetPropertyData(
            device,
            NonNull::from(&addr),
            0,
            core::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new_unchecked((&mut rate as *mut f64).cast::<c_void>()),
        )
    };
    if status != 0 || rate <= 0.0 {
        return None;
    }
    Some(rate as u32)
}

/// デバイスの output scope のチャンネル数を `kAudioDevicePropertyStreamConfiguration` から数える。
///
/// `AudioBufferList` の各 `AudioBuffer.mNumberChannels` を合計する。取得できなければ `None`。
fn device_channels(device: AudioObjectID) -> Option<u16> {
    let addr = address(
        kAudioDevicePropertyStreamConfiguration,
        kAudioObjectPropertyScopeOutput,
    );
    let mut size: u32 = 0;
    // SAFETY: addr/size は有効なローカル。
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            device,
            NonNull::from(&addr),
            0,
            core::ptr::null(),
            NonNull::from(&mut size),
        )
    };
    if status != 0 || size == 0 {
        return None;
    }
    // AudioBufferList は可変長（mBuffers が末尾の flexible array）。報告サイズぶんのバッファを
    // 確保して読み、先頭を AudioBufferList として解釈する。AudioBufferList は内部に *mut の
    // フィールドを持つので 8 バイトアラインが要る。Vec<u8>（align 1）に置くと OS が書いた
    // AudioBufferList を読むときミスアラインの参照外しになりうるので、AudioBufferList 自体の
    // Vec で確保してアラインを保証する（その align で報告サイズを丸ごとカバーする要素数を取る）。
    let elem = core::mem::size_of::<AudioBufferList>();
    let count = (size as usize).div_ceil(elem).max(1);
    // SAFETY: AudioBufferList は数値/ポインタだけの #[repr(C)] POD なのでゼロ初期化が有効。
    let mut storage: Vec<AudioBufferList> = vec![unsafe { core::mem::zeroed() }; count];
    // SAFETY: storage は size バイト以上を 8 バイトアラインで確保済み。
    let status = unsafe {
        AudioObjectGetPropertyData(
            device,
            NonNull::from(&addr),
            0,
            core::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new_unchecked(storage.as_mut_ptr().cast::<c_void>()),
        )
    };
    if status != 0 {
        return None;
    }
    // SAFETY: storage 先頭は OS が書いた AudioBufferList（適正アライン）。mNumberBuffers ぶんの
    // AudioBuffer が続く。
    let list = storage.as_ptr();
    let num_buffers = unsafe { (*list).mNumberBuffers } as usize;
    if num_buffers == 0 {
        return None;
    }
    // SAFETY: mBuffers は num_buffers 本の AudioBuffer の先頭。報告サイズ内に収まっている。
    let buffers = unsafe { std::slice::from_raw_parts((*list).mBuffers.as_ptr(), num_buffers) };
    let total: u32 = buffers.iter().map(|b| b.mNumberChannels).sum();
    if total == 0 {
        return None;
    }
    Some(total.min(u16::MAX as u32) as u16)
}

/// デバイスが出力（再生）デバイスかどうか。output scope に stream が 1 本以上あれば出力とみなす。
fn is_output_device(device: AudioObjectID) -> bool {
    let addr = address(kAudioDevicePropertyStreams, kAudioObjectPropertyScopeOutput);
    let mut size: u32 = 0;
    // SAFETY: addr/size は有効なローカル。
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            device,
            NonNull::from(&addr),
            0,
            core::ptr::null(),
            NonNull::from(&mut size),
        )
    };
    status == 0 && size > 0
}

/// 既定出力デバイスの `AudioObjectID`。取得できなければ `0`。
fn default_output_device() -> AudioObjectID {
    let addr = address(
        kAudioHardwarePropertyDefaultOutputDevice,
        kAudioObjectPropertyScopeGlobal,
    );
    let mut device: AudioObjectID = 0;
    let mut size = core::mem::size_of::<AudioObjectID>() as u32;
    // SAFETY: addr/size/device は有効なローカル。
    let status = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject as AudioObjectID,
            NonNull::from(&addr),
            0,
            core::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new_unchecked((&mut device as *mut AudioObjectID).cast::<c_void>()),
        )
    };
    if status != 0 {
        return 0;
    }
    device
}

/// デバイスの名前（`kAudioObjectPropertyName`）を読む。取得できなければ `None`。
fn device_name(device: AudioObjectID) -> Option<String> {
    read_cfstring_property(
        device,
        kAudioObjectPropertyName,
        kAudioObjectPropertyScopeGlobal,
    )
}

/// デバイスの UID（`kAudioDevicePropertyDeviceUID`）を読む。取得できなければ `None`。
fn device_uid(device: AudioObjectID) -> Option<String> {
    read_cfstring_property(
        device,
        kAudioDevicePropertyDeviceUID,
        kAudioObjectPropertyScopeGlobal,
    )
}

/// 出力（再生）デバイスを列挙する。
///
/// 各 [`DeviceInfo`]:
/// - `id` / `name`: デバイス名（`kAudioObjectPropertyName`）。
/// - `source_kind`: [`SourceKind::SystemLoopback`]。
/// - `sample_rate` / `channels`: デバイスの output フォーマット（取れなければ
///   [`FALLBACK_FORMAT`]）。
/// - `is_loopback`: 常に `true`（出力 monitor）。
/// - `is_default`: 既定出力デバイスと一致すれば `true`。
///
/// 列挙だけなので TCC は要らない。名前を取れないデバイスは飛ばす。
pub fn list_output_devices() -> Result<Vec<DeviceInfo>> {
    let default_id = default_output_device();
    let mut out: Vec<DeviceInfo> = Vec::new();
    for id in all_device_ids() {
        if !is_output_device(id) {
            continue;
        }
        let Some(name) = device_name(id) else {
            continue;
        };
        let (fallback_rate, fallback_ch) = FALLBACK_FORMAT;
        out.push(DeviceInfo {
            id: name.clone(),
            name,
            source_kind: SourceKind::SystemLoopback,
            sample_rate: device_sample_rate(id).unwrap_or(fallback_rate),
            channels: device_channels(id).unwrap_or(fallback_ch),
            is_loopback: true,
            is_default: id == default_id && default_id != 0,
        });
    }
    Ok(out)
}

/// 出力デバイス名から CoreAudio の device UID を引く。
///
/// [`list_output_devices`] の `id`（= デバイス名）で受けた指定を、tap が要求する UID へ変換する。
/// 同名が複数あれば最初の一致を使う。一致するデバイスが無ければ `None`（呼び出し側が
/// [`Error::DeviceNotFound`](flexaudio_core::types::Error) を返す）。
pub(crate) fn uid_for_device_name(name: &str) -> Option<String> {
    for id in all_device_ids() {
        if !is_output_device(id) {
            continue;
        }
        if device_name(id).as_deref() == Some(name) {
            return device_uid(id);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 列挙が panic せず `Ok` を返すこと（headless/CI でも出力デバイスは 0 個以上）。
    /// 各 DeviceInfo は契約どおり loopback=true / SystemLoopback で、妥当な rate/channels を持つ。
    #[test]
    fn list_output_devices_is_well_formed() {
        let devices = list_output_devices().expect("list_output_devices should not error");
        for d in &devices {
            assert_eq!(d.source_kind, SourceKind::SystemLoopback);
            assert!(d.is_loopback);
            assert!(!d.id.is_empty());
            assert_eq!(d.id, d.name);
            assert!(d.sample_rate > 0);
            assert!(d.channels > 0);
        }
    }

    /// 存在しない名前の UID 解決は `None`。
    #[test]
    fn uid_for_unknown_device_is_none() {
        assert!(uid_for_device_name("flexaudio-no-such-output-device-xyzzy").is_none());
    }
}
