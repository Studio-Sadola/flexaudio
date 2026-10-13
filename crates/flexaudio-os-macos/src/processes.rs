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
use flexaudio_core::types::{Error, ProcessInfo, Result};

use crate::common::{map_os_status_at, query_cfstring_property, read_system_object_list, NO_ERR};

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
/// ([`map_os_status_at`]). A failed property query rejects the entire snapshot.
/// Includes all processes known to Core Audio, including input-only processes. No property can
/// exclude only processes that have never had output (`Devices` lists devices in use now, and
/// `IsRunningOutput` indicates whether output is active now).
pub fn list_processes() -> Result<Vec<ProcessInfo>> {
    crate::version::ensure_process_tap_supported()?;

    let objects =
        read_system_object_list(kAudioHardwarePropertyProcessObjectList).map_err(|status| {
            map_os_status_at(
                flexaudio_core::Operation::Enumerate,
                "AudioObjectGetPropertyData(ProcessObjectList)",
                status,
            )
        })?;

    crate::inventory::collect_complete(objects, |object| {
        let pid = read_scalar_property::<i32>(object, kAudioProcessPropertyPID)?;
        if pid <= 0 {
            return Ok(None);
        }
        let bundle_id = query_cfstring_property(
            object,
            kAudioProcessPropertyBundleID,
            kAudioObjectPropertyScopeGlobal,
        )?;
        let is_output_active =
            Some(read_scalar_property::<u32>(object, kAudioProcessPropertyIsRunningOutput)? != 0);
        let executable = process_path(pid)?.and_then(|path| executable_basename(&path));
        Ok(Some(ProcessInfo {
            pid: pid as u32,
            name: executable.clone().unwrap_or_default(),
            executable,
            bundle_id,
            is_output_active,
        }))
    })
}

/// Property address for the global scope and main element.
fn global_address(selector: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// Query a 4-byte numeric property (`T` is `i32` or `u32`) with its native cause.
fn read_scalar_property<T: Copy + Default>(object: AudioObjectID, selector: u32) -> Result<T> {
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
    if status != NO_ERR {
        return Err(map_os_status_at(
            flexaudio_core::Operation::Enumerate,
            "AudioObjectGetPropertyData(process scalar)",
            status,
        ));
    }
    if size as usize != core::mem::size_of::<T>() {
        return Err(Error::Backend(
            "process property returned an invalid size; retry discovery".into(),
        )
        .with_context(flexaudio_core::ErrorContext::new(
            flexaudio_core::Operation::Enumerate,
        )));
    }
    Ok(value)
}

/// Read a `pid_t` (`i32`) property.
pub(crate) fn read_i32_property(object: AudioObjectID, selector: u32) -> Option<i32> {
    read_scalar_property::<i32>(object, selector).ok()
}

/// Read a `UInt32` property.
pub(crate) fn read_u32_property(object: AudioObjectID, selector: u32) -> Option<u32> {
    read_scalar_property::<u32>(object, selector).ok()
}

/// Query an executable path; only a disappeared process is explicitly absent.
fn process_path(pid: i32) -> Result<Option<String>> {
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
        let code = std::io::Error::last_os_error().raw_os_error();
        if code == Some(libc::ESRCH) {
            return Ok(None);
        }
        return Err(Error::Backend(format!("process image query failed (OS code {code:?}); check process access and retry discovery"))
            .with_context(flexaudio_core::ErrorContext::new(flexaudio_core::Operation::Enumerate)));
    }
    let path = buffer
        .get(..written as usize)
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .ok_or_else(|| {
            Error::Backend("process image query returned invalid text; retry discovery".into())
        })?;
    // proc_pidpath may include its final NUL in the returned byte count.
    let path = path.trim_end_matches('\0').to_owned();
    Ok((!path.is_empty()).then_some(path))
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
        let path = process_path(std::process::id() as i32)
            .expect("own path query")
            .expect("own path");
        assert!(path.starts_with('/'), "{path}");
    }
}
