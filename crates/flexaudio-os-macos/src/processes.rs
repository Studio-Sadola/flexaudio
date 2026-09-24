//! Enumeration of capturable processes ([`list_processes`]) — from Core Audio process objects.
//!
//! Gets the process objects known to Core Audio via the system object's
//! `kAudioHardwarePropertyProcessObjectList`, and reads, for each object,
//! - `kAudioProcessPropertyPID` (pid_t)
//! - `kAudioProcessPropertyBundleID` (CFString; may be absent)
//! - `kAudioProcessPropertyIsRunningOutput` (UInt32; whether output IO is running)
//!
//! The executable name is obtained with `proc_pidpath` (libSystem).
//!
//! The set of listed processes does not fully match Linux / Windows. With the Core Audio
//! process object attributes (`kAudioProcessPropertyDevices` is "the devices in use now",
//! `IsRunningOutput` is "whether output IO is running now"), it is impossible to single out
//! only the processes that "have never had output" without dropping stopped/idle output
//! processes. Therefore the processes known to Core Audio are listed as is, including
//! input-only processes.
//!
//! Like Process Taps, this requires macOS 14.4 or later; below that it returns
//! [`Error::UnsupportedOsVersion`] (the same gate as `start` of
//! [`MacProcessBackend`](crate::MacProcessBackend)). Enumeration is read-only and creates no
//! tap, so no TCC (`kTCCServiceAudioCapture`) prompt appears.
//!
//! What is returned is a raw list; deduplication, own-process exclusion, and sorting are
//! done by the facade.

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

// libproc (always present in libSystem). Writes the absolute path of the PID's executable
// into buffer and returns the number of bytes written (excluding NUL). 0 or less on
// failure.
extern "C" {
    fn proc_pidpath(pid: i32, buffer: *mut c_void, buffersize: u32) -> i32;
}

/// Buffer length for `proc_pidpath` (`PROC_PIDPATHINFO_MAXSIZE` = 4 * MAXPATHLEN).
const PROC_PIDPATHINFO_MAXSIZE: usize = 4 * 1024;

/// Enumerates Core Audio process objects (raw list).
///
/// Below 14.4, returns [`Error::UnsupportedOsVersion`](flexaudio_core::types::Error). When
/// the process object list itself cannot be read, maps the `OSStatus` to a typed error and
/// returns it ([`map_os_status`]). Objects whose PID cannot be read are skipped.
/// The processes known to Core Audio are listed (including input-only ones). There is no
/// attribute that drops only the processes that have never had output (`Devices` is the
/// devices in use now, `IsRunningOutput` is whether it is running now).
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

/// Property address for the global scope / main element.
fn global_address(selector: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// Reads a 4-byte numeric property (`T` is `i32` or `u32`). `None` if it cannot be read.
fn read_scalar_property<T: Copy + Default>(object: AudioObjectID, selector: u32) -> Option<T> {
    let addr = global_address(selector);
    let mut value: T = T::default();
    let mut size = core::mem::size_of::<T>() as u32;
    // SAFETY: addr/size/value are valid locals. value is the destination for size bytes.
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

/// Reads a `pid_t` (i32) property.
fn read_i32_property(object: AudioObjectID, selector: u32) -> Option<i32> {
    read_scalar_property::<i32>(object, selector)
}

/// Reads a `UInt32` property.
fn read_u32_property(object: AudioObjectID, selector: u32) -> Option<u32> {
    read_scalar_property::<u32>(object, selector)
}

/// Absolute path of the PID's executable. `None` if it cannot be read.
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

    /// `Ok` on 14.4+, `UnsupportedOsVersion` below. Must not panic.
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

    /// The own process's executable path can be read (checks the proc_pidpath wiring).
    #[test]
    fn own_process_path_is_readable() {
        let path = process_path(std::process::id() as i32).expect("own path");
        assert!(path.starts_with('/'), "{path}");
    }
}
