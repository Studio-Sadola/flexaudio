//! Enumerate capturable processes ([`list_processes`]) from Core Audio process objects.
//!
//! Use the system object's `kAudioHardwarePropertyProcessObjectList` to get the process objects
//! known to Core Audio, then read each object's:
//! - `kAudioProcessPropertyPID` (pid_t)
//! - `kAudioProcessPropertyBundleID` (CFString, optional)
//! - `kAudioProcessPropertyIsRunningOutput` (UInt32; whether output I/O is active)
//!
//! The executable name comes from `proc_pidpath` (libSystem).
//!
//! The process set does not exactly match Linux or Windows. Core Audio's process object
//! properties (`kAudioProcessPropertyDevices` means "devices in use now" and `IsRunningOutput`
//! means "output I/O is active now") cannot identify only processes that have never had output
//! without also excluding stopped or idle output processes. Therefore, this includes every
//! process known to Core Audio, including input-only processes.
//!
//! Like Process Tap, this requires macOS 14.4 or later. Earlier versions return
//! [`Error::UnsupportedOsVersion`] (the same gate as `start` on
//! [`MacProcessBackend`](crate::MacProcessBackend)). Enumeration is read-only and creates no tap,
//! so it does not trigger the TCC (`kTCCServiceAudioCapture`) prompt.
//!
//! This returns the raw list; the facade merges duplicates, excludes the current process, and sorts.

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

// libproc (always present in libSystem). Writes the PID's executable path to the buffer and
// returns the number of bytes written, excluding NUL. Returns 0 or less on failure.
extern "C" {
    fn proc_pidpath(pid: i32, buffer: *mut c_void, buffersize: u32) -> i32;
}

/// Buffer size for `proc_pidpath` (`PROC_PIDPATHINFO_MAXSIZE` = 4 * MAXPATHLEN).
const PROC_PIDPATHINFO_MAXSIZE: usize = 4 * 1024;

/// Enumerate Core Audio process objects (raw list).
///
/// Versions earlier than 14.4 return [`Error::UnsupportedOsVersion`](flexaudio_core::types::Error).
/// If the process object list itself cannot be read, map the `OSStatus` to a typed error
/// ([`map_os_status`]). Skip objects whose PID cannot be read.
/// Includes all processes known to Core Audio, including input-only processes. No property can
/// exclude only processes that have never had output (`Devices` lists devices in use now, and
/// `IsRunningOutput` indicates whether output is active now).
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

/// Property address for the global scope and main element.
fn global_address(selector: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// Read a 4-byte numeric property (`T` is `i32` or `u32`), or return `None` on failure.
fn read_scalar_property<T: Copy + Default>(object: AudioObjectID, selector: u32) -> Option<T> {
    let addr = global_address(selector);
    let mut value: T = T::default();
    let mut size = core::mem::size_of::<T>() as u32;
    // SAFETY: addr, size, and value are valid locals. value is a writable target of `size` bytes.
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

/// Read a `pid_t` (`i32`) property.
fn read_i32_property(object: AudioObjectID, selector: u32) -> Option<i32> {
    read_scalar_property::<i32>(object, selector)
}

/// Read a `UInt32` property.
fn read_u32_property(object: AudioObjectID, selector: u32) -> Option<u32> {
    read_scalar_property::<u32>(object, selector)
}

/// Get the PID's absolute executable path, or `None` if unavailable.
fn process_path(pid: i32) -> Option<String> {
    let mut buffer = vec![0u8; PROC_PIDPATHINFO_MAXSIZE];
    // SAFETY: buffer is a writable region of PROC_PIDPATHINFO_MAXSIZE bytes.
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

    /// Returns `Ok` on 14.4+ and `UnsupportedOsVersion` earlier; must not panic.
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

    /// Verify that the current process executable path can be read (checks `proc_pidpath` wiring).
    #[test]
    fn own_process_path_is_readable() {
        let path = process_path(std::process::id() as i32).expect("own path");
        assert!(path.starts_with('/'), "{path}");
    }
}
