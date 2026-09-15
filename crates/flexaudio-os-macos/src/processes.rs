//! 録れるプロセスの列挙（[`list_processes`]）— Core Audio のプロセスオブジェクトから。
//!
//! system object の `kAudioHardwarePropertyProcessObjectList` で Core Audio が把握している
//! プロセスオブジェクトを取り、各オブジェクトの
//! - `kAudioProcessPropertyPID`（pid_t）
//! - `kAudioProcessPropertyBundleID`（CFString。無いこともある）
//! - `kAudioProcessPropertyIsRunningOutput`（UInt32。出力 IO が動いているか）
//!
//! を読む。実行ファイル名は `proc_pidpath`（libSystem）で取る。
//!
//! 載せる範囲は Linux / Windows と完全には揃わない。Core Audio のプロセスオブジェクト
//! 属性（`kAudioProcessPropertyDevices` は「今使っているデバイス」、
//! `IsRunningOutput` は「今出力 IO が動いているか」）では「出力を持ったことがない」
//! プロセスだけを、停止中・Idle の出力プロセスを落とさずに判別できない。そのため
//! 入力だけのプロセスも含め、Core Audio が把握しているプロセスをそのまま載せる。
//!
//! Process Tap と同じく macOS 14.4 以上が前提で、未満は [`Error::UnsupportedOsVersion`]
//! （[`MacProcessBackend`](crate::MacProcessBackend) の `start` と同じゲート）。
//! 列挙は読み取り専用で、tap を作らないので TCC（`kTCCServiceAudioCapture`）の
//! プロンプトは出ない。
//!
//! 返すのは生リストで、重複統合・自プロセス除外・並べ替えは facade が行う。

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2_core_audio::{
    kAudioHardwarePropertyProcessObjectList, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioProcessPropertyBundleID,
    kAudioProcessPropertyIsRunningOutput, kAudioProcessPropertyPID, AudioObjectGetPropertyData,
    AudioObjectID, AudioObjectPropertyAddress,
};

use flexaudio_core::process_list::executable_basename;
use flexaudio_core::types::{ProcessInfo, Result};

use crate::common::{map_os_status, read_cfstring_property, read_system_object_list, NO_ERR};

// libproc（libSystem に常在）。PID の実行ファイルの絶対パスを buffer へ書き、書いた
// バイト数（NUL 除く）を返す。失敗時は 0 以下。
extern "C" {
    fn proc_pidpath(pid: i32, buffer: *mut c_void, buffersize: u32) -> i32;
}

/// `proc_pidpath` のバッファ長（`PROC_PIDPATHINFO_MAXSIZE` = 4 * MAXPATHLEN）。
const PROC_PIDPATHINFO_MAXSIZE: usize = 4 * 1024;

/// Core Audio のプロセスオブジェクトを列挙する（生リスト）。
///
/// 14.4 未満は [`Error::UnsupportedOsVersion`](flexaudio_core::types::Error)。プロセス
/// オブジェクト一覧そのものを読めないときは `OSStatus` を型付きエラーへ写して返す
/// （[`map_os_status`]）。個々のオブジェクトの PID が読めないものは飛ばす。
/// 載せるのは Core Audio が把握しているプロセス（入力だけも含む）。出力を持ったことが
/// ないプロセスだけを落とす属性は無い（`Devices` は今使っているデバイス、
/// `IsRunningOutput` は今動いているか）。
pub fn list_processes() -> Result<Vec<ProcessInfo>> {
    crate::version::ensure_process_tap_supported()?;

    let objects = read_system_object_list(kAudioHardwarePropertyProcessObjectList)
        .map_err(|status| map_os_status("AudioObjectGetPropertyData(ProcessObjectList)", status))?;

    let mut out = Vec::with_capacity(objects.len());
    for object in objects {
        let Some(pid) = read_i32_property(object, kAudioProcessPropertyPID) else {
            continue;
        };
        if pid <= 0 {
            continue;
        }
        let bundle_id = read_cfstring_property(
            object,
            kAudioProcessPropertyBundleID,
            kAudioObjectPropertyScopeGlobal,
        );
        let is_output_active =
            read_u32_property(object, kAudioProcessPropertyIsRunningOutput).map(|v| v != 0);
        let executable = process_path(pid).and_then(|path| executable_basename(&path));
        out.push(ProcessInfo {
            pid: pid as u32,
            name: executable.clone().unwrap_or_default(),
            executable,
            bundle_id,
            is_output_active,
        });
    }
    Ok(out)
}

/// global scope / main element のプロパティアドレス。
fn global_address(selector: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// 4 バイトの数値プロパティを読む（`T` は `i32` か `u32`）。読めなければ `None`。
fn read_scalar_property<T: Copy + Default>(object: AudioObjectID, selector: u32) -> Option<T> {
    let addr = global_address(selector);
    let mut value: T = T::default();
    let mut size = core::mem::size_of::<T>() as u32;
    // SAFETY: addr/size/value は有効なローカル。value は size バイトの書き込み先。
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&addr),
            0,
            core::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new_unchecked((&mut value as *mut T).cast::<c_void>()),
        )
    };
    if status != NO_ERR || size as usize != core::mem::size_of::<T>() {
        return None;
    }
    Some(value)
}

/// `pid_t`（i32）プロパティを読む。
fn read_i32_property(object: AudioObjectID, selector: u32) -> Option<i32> {
    read_scalar_property::<i32>(object, selector)
}

/// `UInt32` プロパティを読む。
fn read_u32_property(object: AudioObjectID, selector: u32) -> Option<u32> {
    read_scalar_property::<u32>(object, selector)
}

/// PID の実行ファイルの絶対パス。読めなければ `None`。
fn process_path(pid: i32) -> Option<String> {
    let mut buffer = vec![0u8; PROC_PIDPATHINFO_MAXSIZE];
    // SAFETY: buffer は PROC_PIDPATHINFO_MAXSIZE バイトの書き込み可能領域。
    let written = unsafe {
        proc_pidpath(
            pid,
            buffer.as_mut_ptr().cast::<c_void>(),
            PROC_PIDPATHINFO_MAXSIZE as u32,
        )
    };
    if written <= 0 {
        return None;
    }
    let len = (written as usize).min(buffer.len());
    let path = String::from_utf8_lossy(&buffer[..len]).into_owned();
    if path.is_empty() {
        None
    } else {
        Some(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 14.4+ では `Ok`、未満では `UnsupportedOsVersion`。panic しないこと。
    #[test]
    fn list_processes_is_gated_and_well_formed() {
        match list_processes() {
            Ok(list) => {
                for p in &list {
                    assert_ne!(p.pid, 0);
                }
            }
            Err(flexaudio_core::types::Error::UnsupportedOsVersion) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    /// 自プロセスの実行ファイルパスを読めること（proc_pidpath の配線確認）。
    #[test]
    fn own_process_path_is_readable() {
        let path = process_path(std::process::id() as i32).expect("own path");
        assert!(path.starts_with('/'), "{path}");
    }
}
