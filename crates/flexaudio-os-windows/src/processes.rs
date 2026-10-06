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
//! for processes that cannot be opened (such as protected processes); the facade supplies `pid <N>`.

use flexaudio_core::process_list::executable_basename;
use flexaudio_core::types::{Error, ProcessInfo, Result};

use windows::core::{Interface, PWSTR};
use windows::Win32::Foundation::{CloseHandle, S_OK};
use windows::Win32::Media::Audio::{
    eRender, AudioSessionStateActive, AudioSessionStateExpired, IAudioSessionControl2,
    IAudioSessionManager2, IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};

use crate::common::{map_hr, ComThread};

/// `QueryFullProcessImageNameW` buffer length in UTF-16 units: the 32767-character limit plus NUL,
/// enough for long paths with the `\\?\` prefix.
const IMAGE_PATH_CAPACITY: usize = 32_768;

/// Raw values read from one session.
struct SessionRecord {
    pid: u32,
    /// `Some(true)`=Active / `Some(false)`=Inactive / `None`=state could not be read.
    active: Option<bool>,
}

/// Enumerate processes with audio sessions (raw list).
///
/// Check first that the OS supports process loopback (build 20348 or later); otherwise return
/// [`Error::UnsupportedOsVersion`] using the same function as capture. Return `Ok(empty)` if there
/// are no render endpoints. If endpoints exist but none provide a session manager, return the last
/// failure as a typed [`Error`] (for example, access denied becomes [`Error::PermissionDenied`]).
pub fn list_processes() -> Result<Vec<ProcessInfo>> {
    crate::version::ensure_process_loopback_supported()?;
    let _com = ComThread::new();
    // SAFETY: COM is initialized in this function (ComThread). COM interfaces are used only here,
    // on the same thread, and never cross a thread boundary.
    let sessions = unsafe { collect_sessions()? };

    let mut image_buffer = vec![0u16; IMAGE_PATH_CAPACITY];
    let mut out = Vec::with_capacity(sessions.len());
    for session in sessions {
        let executable = process_image_path(session.pid, &mut image_buffer)
            .and_then(|path| executable_basename(&path));
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
    let enumerator: IMMDeviceEnumerator =
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .map_err(|e| map_hr("CoCreateInstance(MMDeviceEnumerator)", e))?;
    let collection = enumerator
        .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
        .map_err(|e| map_hr("IMMDeviceEnumerator::EnumAudioEndpoints", e))?;
    let endpoint_count = collection
        .GetCount()
        .map_err(|e| map_hr("IMMDeviceCollection::GetCount", e))?;

    let mut sessions = Vec::new();
    let mut any_endpoint_read = false;
    let mut last_error: Option<Error> = None;
    for index in 0..endpoint_count {
        let device = match collection.Item(index) {
            Ok(d) => d,
            Err(e) => {
                last_error = Some(map_hr("IMMDeviceCollection::Item", e));
                continue;
            }
        };
        let manager: IAudioSessionManager2 = match device.Activate(CLSCTX_ALL, None) {
            Ok(m) => m,
            Err(e) => {
                last_error = Some(map_hr("IMMDevice::Activate(IAudioSessionManager2)", e));
                continue;
            }
        };
        let session_list = match manager.GetSessionEnumerator() {
            Ok(list) => list,
            Err(e) => {
                last_error = Some(map_hr("IAudioSessionManager2::GetSessionEnumerator", e));
                continue;
            }
        };
        any_endpoint_read = true;
        let session_count = session_list.GetCount().unwrap_or(0);
        for session_index in 0..session_count {
            if let Some(record) = read_session(&session_list, session_index) {
                sessions.push(record);
            }
        }
    }

    if endpoint_count > 0 && !any_endpoint_read {
        if let Some(e) = last_error {
            return Err(e);
        }
    }
    Ok(sessions)
}

/// Read one session. Return `None` for system sounds, expired sessions, PID 0, or unreadable sessions.
///
/// # Safety
/// COM must be initialized on the calling thread. `session_list` must be valid.
unsafe fn read_session(
    session_list: &windows::Win32::Media::Audio::IAudioSessionEnumerator,
    session_index: i32,
) -> Option<SessionRecord> {
    let control = session_list.GetSession(session_index).ok()?;
    let control2: IAudioSessionControl2 = control.cast().ok()?;
    // In windows 0.54, `IsSystemSoundsSession` returns `HRESULT` (not `Result<()>`).
    // S_OK means a system sound session (such as a notification), not a specific app; S_FALSE (1) is normal.
    // S_FALSE is also considered success (HRESULT >= 0), so wrapping it in `Result<()>` loses the distinction.
    if control2.IsSystemSoundsSession() == S_OK {
        return None;
    }
    let pid = control2.GetProcessId().ok()?;
    if pid == 0 {
        return None;
    }
    let state = control2.GetState().ok();
    if state == Some(AudioSessionStateExpired) {
        return None;
    }
    Some(SessionRecord {
        pid,
        active: state.map(|s| s == AudioSessionStateActive),
    })
}

/// Get the PID's image path in Win32 format. Return `None` if it cannot be opened or read.
fn process_image_path(pid: u32, buffer: &mut [u16]) -> Option<String> {
    // SAFETY: Always close the handle returned by OpenProcess. The caller owns `buffer`, which is
    // writable; `size` is its length in UTF-16 units and is updated with the actual length on return.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut size = buffer.len() as u32;
        let queried = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(handle);
        queried.ok()?;
        let len = (size as usize).min(buffer.len());
        let path = String::from_utf16_lossy(&buffer[..len]);
        if path.is_empty() {
            None
        } else {
            Some(path)
        }
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
            .expect("the current process image path should be readable");
        assert!(path.to_ascii_lowercase().ends_with(".exe"), "{path}");
    }
}
