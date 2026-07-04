//! デバイス着脱監視（ホットプラグ）の C ABI。
//!
//! [`FlexWatcher`] は [`flexaudio::DeviceWatcher`] を包む不透明ハンドルで、デバイスの
//! 接続・切断・既定変更を pull 型（[`flexaudio_watcher_poll`]）で配信する。capture stream
//! 単位のイベント（`flexaudio_poll_event`）とは別系統で、デバイス単位の事象を扱う。
//! これまで C ABI に無かったパリティ欠落を埋める（napi 側には既にある）。
//!
//! 流儀はクレート全体と同じ（guard で panic を吸収・NULL 検査・失敗は last_error）。
//! poll が埋める `FlexDeviceEvent` の文字列（`id`/`name`）は flexaudio 所有で、
//! [`flexaudio_device_event_free`] で解放する（C の free は使わない）。

use std::ffi::CString;
use std::os::raw::c_char;

use flexaudio::{DeviceEvent, DeviceWatcher};

use crate::convert::{source_kind_to_c, string_to_c};
use crate::error::{clear_last_error, code, set_last_error};
use crate::types::FlexSourceKind;
use crate::{guard_i32, guard_ptr};

/// デバイス着脱イベントの種別（[`flexaudio::DeviceEvent`] に対応）。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlexDeviceEventKind {
    /// デバイスが追加された（`device`/`name` 等が埋まる）。
    Added = 0,
    /// デバイスが取り外された（`id` のみ）。
    Removed = 1,
    /// OS 既定デバイスが変わった（`id` と `source_kind`）。
    DefaultChanged = 2,
    /// 既知のどれにも当たらないイベント（将来のバリアント追加に備える）。
    Unknown = 3,
}

/// 取得した 1 つのデバイスイベント。`flexaudio_watcher_poll` が埋める。
///
/// フィールドの有効範囲は `kind` による:
/// - `Added`: `id`/`name` と `source_kind`/`sample_rate`/`channels`/`is_loopback`/`is_default`
///   がすべて埋まる（追加されたデバイスの完全な情報）。
/// - `Removed`: `id` のみ（`name` は NULL・数値は 0）。
/// - `DefaultChanged`: `id` と `source_kind`（既定が切り替わった側）のみ。
///
/// `id`/`name` は flexaudio 所有の UTF-8 NUL 終端文字列で、[`flexaudio_device_event_free`]
/// で解放する（C の free は使わない）。
#[repr(C)]
pub struct FlexDeviceEvent {
    /// イベント種別。
    pub kind: FlexDeviceEventKind,
    /// 安定 ID（`Added`/`Removed`/`DefaultChanged` で有効・`flexaudio_device_event_free`
    /// で解放）。`Unknown` では NULL。
    pub id: *mut c_char,
    /// 表示名（`Added` のみ・`flexaudio_device_event_free` で解放）。他では NULL。
    pub name: *mut c_char,
    /// `Added` では当該デバイスのソース種別、`DefaultChanged` では既定が切り替わった側
    /// （`Mic` = 既定 source / `System` = 既定 sink）。他では未使用（`Mic`）。
    pub source_kind: FlexSourceKind,
    /// ネイティブサンプルレート（`Added` のみ・他では 0）。
    pub sample_rate: u32,
    /// ネイティブチャンネル数（`Added` のみ・他では 0）。
    pub channels: u16,
    /// ループバック（`Added` のみ）。
    pub is_loopback: bool,
    /// OS の既定デバイス（`Added` のみ）。
    pub is_default: bool,
}

/// [`DeviceEvent`] を `FlexDeviceEvent` に写す（`id`/`name` は C へ所有権を渡す）。
fn device_event_to_c(ev: DeviceEvent) -> FlexDeviceEvent {
    match ev {
        DeviceEvent::Added(info) => FlexDeviceEvent {
            kind: FlexDeviceEventKind::Added,
            id: string_to_c(info.id),
            name: string_to_c(info.name),
            source_kind: source_kind_to_c(info.source_kind),
            sample_rate: info.sample_rate,
            channels: info.channels,
            is_loopback: info.is_loopback,
            is_default: info.is_default,
        },
        DeviceEvent::Removed { id } => FlexDeviceEvent {
            kind: FlexDeviceEventKind::Removed,
            id: string_to_c(id),
            name: std::ptr::null_mut(),
            source_kind: FlexSourceKind::Mic,
            sample_rate: 0,
            channels: 0,
            is_loopback: false,
            is_default: false,
        },
        DeviceEvent::DefaultChanged { kind, id } => FlexDeviceEvent {
            kind: FlexDeviceEventKind::DefaultChanged,
            id: string_to_c(id),
            name: std::ptr::null_mut(),
            source_kind: source_kind_to_c(kind),
            sample_rate: 0,
            channels: 0,
            is_loopback: false,
            is_default: false,
        },
        // DeviceEvent は #[non_exhaustive]。未知種別は Unknown にして握り潰さない。
        other => {
            set_last_error(format!("unknown device event: {other:?}"));
            FlexDeviceEvent {
                kind: FlexDeviceEventKind::Unknown,
                id: std::ptr::null_mut(),
                name: std::ptr::null_mut(),
                source_kind: FlexSourceKind::Mic,
                sample_rate: 0,
                channels: 0,
                is_loopback: false,
                is_default: false,
            }
        }
    }
}

/// デバイスの不透明ウォッチャハンドル。中身は [`flexaudio::DeviceWatcher`]。
/// `flexaudio_watch_devices` で作り `flexaudio_watcher_free` で解放する。
pub struct FlexWatcher {
    inner: DeviceWatcher,
}

/// デバイスの着脱・既定変更の監視を開始し、ウォッチャハンドルを返す。
///
/// Linux は PipeWire レジストリを永続監視する。PipeWire 不在・非対応 OS では no-op へ
/// 縮退して有効なハンドルを返す（着脱が来ないだけ・poll は常に 0）。失敗時のみ NULL +
/// last_error。返ったハンドルは `flexaudio_watcher_free` で解放する。
#[no_mangle]
pub extern "C" fn flexaudio_watch_devices() -> *mut FlexWatcher {
    guard_ptr(|| {
        clear_last_error();
        match flexaudio::watch_devices() {
            Ok(inner) => Box::into_raw(Box::new(FlexWatcher { inner })),
            Err(e) => {
                set_last_error(e.to_string());
                std::ptr::null_mut()
            }
        }
    })
}

/// デバイスイベントを 1 つ取り出して `out` を埋める（非ブロッキング）。
///
/// 戻り 1 = 取得して `out` を埋めた / 0 = 今は無し / 負 = エラー。埋めた `out` は使い
/// 終わったら `flexaudio_device_event_free` で解放する。
///
/// # Safety
/// `w` は有効なハンドル、`out` は有効な `FlexDeviceEvent` の書き込み先でなければならない。
#[no_mangle]
pub unsafe extern "C" fn flexaudio_watcher_poll(
    w: *mut FlexWatcher,
    out: *mut FlexDeviceEvent,
) -> i32 {
    guard_i32(|| {
        clear_last_error();
        let Some(watcher) = w.as_mut() else {
            set_last_error("flexaudio_watcher_poll: watcher pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        if out.is_null() {
            set_last_error("flexaudio_watcher_poll: out pointer is null");
            return code::FLEX_INVALID_ARG;
        }
        match watcher.inner.poll_event() {
            Some(ev) => {
                out.write(device_event_to_c(ev));
                1
            }
            None => 0,
        }
    })
}

/// `flexaudio_watcher_poll` が埋めた `id`/`name` を解放し、NULL にする。NULL・二重解放
/// とも安全。
///
/// # Safety
/// `ev` は `flexaudio_watcher_poll` が埋めた `FlexDeviceEvent`（または NULL）を指して
/// いなければならない。
#[no_mangle]
pub unsafe extern "C" fn flexaudio_device_event_free(ev: *mut FlexDeviceEvent) {
    guard_i32(|| {
        if let Some(ev) = ev.as_mut() {
            if !ev.id.is_null() {
                drop(CString::from_raw(ev.id));
                ev.id = std::ptr::null_mut();
            }
            if !ev.name.is_null() {
                drop(CString::from_raw(ev.name));
                ev.name = std::ptr::null_mut();
            }
        }
        code::FLEX_OK
    });
}

/// ウォッチャを停止して解放する。NULL 安全。
///
/// # Safety
/// `w` は `flexaudio_watch_devices` が返したハンドル（または NULL）でなければならない。
/// 解放後の `w` を使ってはならない。
#[no_mangle]
pub unsafe extern "C" fn flexaudio_watcher_free(w: *mut FlexWatcher) {
    guard_i32(|| {
        if !w.is_null() {
            // DeviceWatcher の Drop が stop() を呼ぶ。
            drop(Box::from_raw(w));
        }
        code::FLEX_OK
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio::{DeviceInfo, SourceKind};
    use std::ffi::CStr;

    /// watch → poll → free の一巡（PipeWire 不在でも縮退して安全）。
    #[test]
    fn watch_poll_free_smoke() {
        let w = flexaudio_watch_devices();
        assert!(
            !w.is_null(),
            "watch_devices は縮退して常にハンドルを返す設計"
        );
        let mut ev = std::mem::MaybeUninit::<FlexDeviceEvent>::uninit();
        // 縮退時は 0（今は無し）。取れても負にはならない。
        let rc = unsafe { flexaudio_watcher_poll(w, ev.as_mut_ptr()) };
        assert!(rc >= 0, "poll がエラーを返した: {rc}");
        if rc == 1 {
            // 取れた場合はイベントを解放する。
            unsafe { flexaudio_device_event_free(ev.as_mut_ptr()) };
        }
        unsafe { flexaudio_watcher_free(w) };
    }

    /// NULL ハンドル・NULL 出力先は InvalidArg。NULL free は安全。
    #[test]
    fn watcher_null_args() {
        let mut ev = std::mem::MaybeUninit::<FlexDeviceEvent>::uninit();
        assert_eq!(
            unsafe { flexaudio_watcher_poll(std::ptr::null_mut(), ev.as_mut_ptr()) },
            code::FLEX_INVALID_ARG
        );
        let w = flexaudio_watch_devices();
        assert_eq!(
            unsafe { flexaudio_watcher_poll(w, std::ptr::null_mut()) },
            code::FLEX_INVALID_ARG
        );
        unsafe { flexaudio_watcher_free(w) };
        unsafe { flexaudio_watcher_free(std::ptr::null_mut()) };
        unsafe { flexaudio_device_event_free(std::ptr::null_mut()) };
    }

    /// 各 DeviceEvent バリアントの C 変換と文字列解放が整合する。
    #[test]
    fn device_event_conversion_and_free() {
        // Added: 全フィールドが埋まる。
        let added = device_event_to_c(DeviceEvent::Added(DeviceInfo {
            id: "node-1".to_string(),
            name: "Mic A".to_string(),
            source_kind: SourceKind::Mic,
            sample_rate: 48_000,
            channels: 2,
            is_loopback: false,
            is_default: true,
        }));
        assert_eq!(added.kind, FlexDeviceEventKind::Added);
        assert_eq!(added.source_kind, FlexSourceKind::Mic);
        assert_eq!(added.sample_rate, 48_000);
        assert!(added.is_default);
        assert_eq!(
            unsafe { CStr::from_ptr(added.id) }.to_str().unwrap(),
            "node-1"
        );
        assert_eq!(
            unsafe { CStr::from_ptr(added.name) }.to_str().unwrap(),
            "Mic A"
        );
        let mut added = added;
        unsafe { flexaudio_device_event_free(&mut added) };
        assert!(added.id.is_null() && added.name.is_null());

        // Removed: id のみ・name は NULL。
        let mut removed = device_event_to_c(DeviceEvent::Removed {
            id: "node-2".to_string(),
        });
        assert_eq!(removed.kind, FlexDeviceEventKind::Removed);
        assert!(removed.name.is_null());
        assert_eq!(
            unsafe { CStr::from_ptr(removed.id) }.to_str().unwrap(),
            "node-2"
        );
        unsafe { flexaudio_device_event_free(&mut removed) };

        // DefaultChanged: id + source_kind。
        let mut def = device_event_to_c(DeviceEvent::DefaultChanged {
            kind: SourceKind::SystemLoopback,
            id: "sink-3".to_string(),
        });
        assert_eq!(def.kind, FlexDeviceEventKind::DefaultChanged);
        assert_eq!(def.source_kind, FlexSourceKind::System);
        assert_eq!(
            unsafe { CStr::from_ptr(def.id) }.to_str().unwrap(),
            "sink-3"
        );
        unsafe { flexaudio_device_event_free(&mut def) };
        // 二重解放は安全。
        unsafe { flexaudio_device_event_free(&mut def) };
    }
}
