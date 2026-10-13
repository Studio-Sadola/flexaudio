//! Enumerate recordable processes ([`list_processes`]) from WASAPI audio sessions.
//!
//! For every active render endpoint,
//! `IMMDevice::Activate(IAudioSessionManager2)` → `GetSessionEnumerator` →
//! read `GetProcessId` / `GetState` from each `IAudioSessionControl2` and return processes with
//! audio sessions. Exclude system sound sessions (`IsSystemSoundsSession == S_OK`), expired sessions,
//! and PID 0.
//!
//! Return the raw list as found (a PID may appear across endpoints or sessions). The facade
//! (`flexaudio_core::process_list::normalize_process_list`) merges duplicates, excludes the current
//! process, and sorts the results.
//!
//! This is read-only and does not open audio (it does not initialize `IAudioClient`), so it does not
//! trigger permission prompts such as microphone privacy prompts. Read process names with
//! `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` and `QueryFullProcessImageNameW`. Return no name
//! when optional metadata is unreadable; vanished processes may be omitted.

use flexaudio_core::process_list::executable_basename;
use flexaudio_core::types::{Error, ProcessInfo, Result};

use windows::core::{Interface, PWSTR};
use windows::Win32::Foundation::{CloseHandle, S_OK};
use windows::Win32::Media::Audio::{
    eRender, AudioSessionStateActive, AudioSessionStateExpired, AudioSessionStateInactive,
    IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator, MMDeviceEnumerator,
    DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};

use crate::common::{map_hr_at, ComThread};
use flexaudio_core::Operation;

/// `QueryFullProcessImageNameW` buffer length in UTF-16 units: the 32767-character limit plus NUL,
/// enough for long paths with the `\\?\` prefix.
const IMAGE_PATH_CAPACITY: usize = 32_768;

/// Raw values read from one session.
struct SessionRecord {
    pid: u32,
    /// A successfully queried session state; unreadable activity stays unknown.
    active: Option<bool>,
}

/// Enumerate processes with audio sessions (raw list).
///
/// Check first that the OS supports process loopback (build 20348 or later); otherwise return
/// [`Error::UnsupportedOsVersion`] using the same function as capture. Return `Ok(empty)` if there
/// are no render endpoints. Collection/count failures retain their typed cause.
/// Unreadable optional metadata stays unknown; vanished processes may be omitted.
pub fn list_processes() -> Result<Vec<ProcessInfo>> {
    crate::version::ensure_process_loopback_supported()?;
    let _com = ComThread::new()?;
    // SAFETY: COM is initialized in this function (ComThread). COM interfaces are used only here,
    // on the same thread, and never cross a thread boundary.
    let sessions = unsafe { collect_sessions()? };

    let mut image_buffer = vec![0u16; IMAGE_PATH_CAPACITY];
    let mut out = Vec::with_capacity(sessions.len());
    for session in sessions {
        let Some(path) = flexaudio_core::inventory::process_metadata(
            process_image_path(session.pid, &mut image_buffer),
            flexaudio_core::inventory::windows_error_failure,
        ) else {
            continue;
        };
        let executable = path.and_then(|path| executable_basename(&path));
        out.push(ProcessInfo {
            pid: session.pid,
            name: executable.as_deref().map(display_stem).unwrap_or_default(),
            executable,
            bundle_id: None,
            is_output_active: session.active,
        });
    }
    Ok(out)
}

/// Collect sessions from all render endpoints.
///
/// # Safety
/// COM must be initialized on the calling thread.
unsafe fn collect_sessions() -> Result<Vec<SessionRecord>> {
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
        .map_err(|e| {
        map_hr_at(
            Operation::Enumerate,
            "CoCreateInstance(MMDeviceEnumerator)",
            e,
        )
    })?;
    let collection = enumerator
        .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
        .map_err(|e| {
            map_hr_at(
                Operation::Enumerate,
                "IMMDeviceEnumerator::EnumAudioEndpoints",
                e,
            )
        })?;
    let endpoint_count = collection
        .GetCount()
        .map_err(|e| map_hr_at(Operation::Enumerate, "IMMDeviceCollection::GetCount", e))?;

    let by_endpoint = crate::inventory::collect_complete(0..endpoint_count, |index| {
        let Ok(device) = collection.Item(index) else {
            return Ok(None);
        };
        let manager: IAudioSessionManager2 =
            device.Activate(CLSCTX_ALL, None).map_err(|error| {
                map_hr_at(
                    Operation::Enumerate,
                    "IMMDevice::Activate(IAudioSessionManager2)",
                    error,
                )
            })?;
        let session_list = manager.GetSessionEnumerator().map_err(|error| {
            map_hr_at(
                Operation::Enumerate,
                "IAudioSessionManager2::GetSessionEnumerator",
                error,
            )
        })?;
        let session_count = session_list.GetCount().map_err(|error| {
            map_hr_at(
                Operation::Enumerate,
                "IAudioSessionEnumerator::GetCount",
                error,
            )
        })?;
        let sessions = crate::inventory::collect_complete(0..session_count, |session_index| {
            read_session(&session_list, session_index)
        })?;
        Ok(Some(sessions))
    })?;
    Ok(by_endpoint.into_iter().flatten().collect())
}

/// Read one session. Omit vanished/unidentifiable sessions, system sounds and expired sessions.
/// An unreadable activity state stays unknown once the PID is readable.
///
/// # Safety
/// COM must be initialized on the calling thread. `session_list` must be valid.
unsafe fn read_session(
    session_list: &windows::Win32::Media::Audio::IAudioSessionEnumerator,
    session_index: i32,
) -> Result<Option<SessionRecord>> {
    let Ok(control) = session_list.GetSession(session_index) else {
        return Ok(None);
    };
    let Ok(control2) = control.cast::<IAudioSessionControl2>() else {
        return Ok(None);
    };
    let system_sounds = control2.IsSystemSoundsSession();
    if system_sounds == S_OK {
        return Ok(None);
    }
    let Ok(pid) = control2.GetProcessId() else {
        return Ok(None);
    };
    if pid == 0 {
        return Ok(None);
    }
    let state = control2.GetState().ok();
    if state == Some(AudioSessionStateExpired) {
        return Ok(None);
    }
    let active = state.and_then(|state| {
        if state == AudioSessionStateActive {
            Some(true)
        } else if state == AudioSessionStateInactive {
            Some(false)
        } else {
            None
        }
    });
    Ok(Some(SessionRecord { pid, active }))
}

/// Retain a metadata failure without misclassifying process access as recording consent.
fn metadata_error(call: &'static str, error: windows::core::Error) -> Error {
    let bits = error.code().0 as u32;
    Error::Backend(format!("process metadata query failed (HRESULT 0x{bits:08X}); check process access and retry discovery"))
        .with_context(flexaudio_core::ErrorContext::new(Operation::Enumerate)
            .with_native_status(flexaudio_core::NativeStatus::HResult { call, bits }))
}

/// Get the PID's image path. Only a disappeared process or empty successful query is absent.
fn process_image_path(pid: u32, buffer: &mut [u16]) -> Result<Option<String>> {
    // SAFETY: close the owned handle; buffer is writable for the supplied UTF-16 capacity.
    unsafe {
        let handle = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(handle) => handle,
            Err(error) => return Err(metadata_error("OpenProcess", error)),
        };
        let mut size = buffer.len() as u32;
        let queried = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut size,
        )
        .map_err(|error| metadata_error("QueryFullProcessImageNameW", error));
        let closed = CloseHandle(handle).map_err(|error| metadata_error("CloseHandle", error));
        match (queried, closed) {
            (Err(primary), Err(secondary)) => {
                return Err(Error::Multiple(flexaudio_core::ErrorGroup::new(
                    primary,
                    secondary,
                    Vec::new(),
                )))
            }
            (Err(error), _) | (_, Err(error)) => return Err(error),
            (Ok(()), Ok(())) => {}
        }
        let path = buffer
            .get(..size as usize)
            .and_then(|units| String::from_utf16(units).ok())
            .ok_or_else(|| {
                Error::Backend("process image query returned invalid text; retry discovery".into())
                    .with_context(flexaudio_core::ErrorContext::new(Operation::Enumerate))
            })?;
        Ok((!path.is_empty()).then_some(path))
    }
}

/// Create a display name from the executable name, ignoring a trailing `.exe` case-insensitively.
fn display_stem(executable: &str) -> String {
    let lower = executable.to_ascii_lowercase();
    match lower.strip_suffix(".exe") {
        Some(stem) if !stem.is_empty() => executable[..stem.len()].to_string(),
        _ => executable.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_stem_strips_exe_case_insensitively() {
        assert_eq!(display_stem("chrome.exe"), "chrome");
        assert_eq!(display_stem("Spotify.EXE"), "Spotify");
        assert_eq!(display_stem("noext"), "noext");
        assert_eq!(display_stem(".exe"), ".exe");
    }

    /// Enumeration on real hardware does not panic, and returned entries have nonzero PIDs.
    #[test]
    fn list_processes_does_not_panic() {
        if let Ok(list) = list_processes() {
            for p in &list {
                assert_ne!(p.pid, 0);
                assert_eq!(p.bundle_id, None);
            }
        }
    }

    /// The current process PID can be read (real-hardware check of the name-resolution path).
    #[test]
    fn own_image_path_is_readable() {
        let mut buffer = vec![0u16; IMAGE_PATH_CAPACITY];
        let path = process_image_path(std::process::id(), &mut buffer)
            .expect("the current process image query should succeed")
            .expect("the current process image path should exist");
        assert!(path.to_ascii_lowercase().ends_with(".exe"), "{path}");
    }
}
