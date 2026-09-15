//! 録れるプロセスの列挙（[`list_processes`]）— WASAPI の音声セッションから。
//!
//! 有効な全 render エンドポイントについて
//! `IMMDevice::Activate(IAudioSessionManager2)` → `GetSessionEnumerator` →
//! 各 `IAudioSessionControl2` の `GetProcessId` / `GetState` を読み、音声セッションを
//! 持つプロセスを返す。システム音セッション（`IsSystemSoundsSession == S_OK`）・
//! 期限切れセッション・PID 0 は除く。
//!
//! 返すのは「見つけたまま」の生リスト（同じ PID が複数エンドポイント／複数セッションで
//! 重複し得る）で、重複統合・自プロセス除外・並べ替えは facade
//! （`flexaudio_core::process_list::normalize_process_list`）が行う。
//!
//! 読み取り専用で、音声を開かない（`IAudioClient` を Initialize しない）ので、
//! マイクのプライバシー設定などの権限プロンプトは出ない。プロセス名は
//! `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` + `QueryFullProcessImageNameW` で取り、
//! 開けないプロセス（保護プロセス等）は名前なしで返す（facade が `pid <N>` を補う）。

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

/// `QueryFullProcessImageNameW` のバッファ長（UTF-16 単位）。長いパス（`\\?\` 付き）でも
/// 収まる上限 32767 文字 + NUL。
const IMAGE_PATH_CAPACITY: usize = 32_768;

/// 1 セッションから読んだ生の値。
struct SessionRecord {
    pid: u32,
    /// `Some(true)`=Active / `Some(false)`=Inactive / `None`=状態を読めなかった。
    active: Option<bool>,
}

/// 音声セッションを持つプロセスを列挙する（生リスト）。
///
/// プロセスループバックに要る OS の版（build 20348 以上）を先に確かめ、未満なら
/// [`Error::UnsupportedOsVersion`]（録音側と同じ関数）。render エンドポイントが
/// 1 つも無ければ `Ok(空)`。エンドポイントはあるのにどれからもセッションマネージャを
/// 取れなかったときは、最後の失敗を型付き [`Error`] で返す
/// （例: アクセス拒否 → [`Error::PermissionDenied`]）。
pub fn list_processes() -> Result<Vec<ProcessInfo>> {
    crate::version::ensure_process_loopback_supported()?;
    let _com = ComThread::new();
    // SAFETY: この関数内で COM を初期化済み（ComThread）。COM インターフェイスはこの関数内
    // （同一スレッド）でだけ使い、スレッド境界を跨がない。
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

/// 全 render エンドポイントのセッションを集める。
///
/// # Safety
/// 呼び出しスレッドで COM が初期化済みであること。
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

/// 1 セッションを読む。システム音・期限切れ・PID 0・読めないセッションは `None`。
///
/// # Safety
/// 呼び出しスレッドで COM が初期化済みであること。`session_list` は有効。
unsafe fn read_session(
    session_list: &windows::Win32::Media::Audio::IAudioSessionEnumerator,
    session_index: i32,
) -> Option<SessionRecord> {
    let control = session_list.GetSession(session_index).ok()?;
    let control2: IAudioSessionControl2 = control.cast().ok()?;
    // windows 0.54 の `IsSystemSoundsSession` は `HRESULT` を返す（`Result<()>` ではない）。
    // S_OK = システム音セッション（通知音など・特定アプリではない）。S_FALSE (1) = 通常。
    // S_FALSE も成功扱い（HRESULT >= 0）なので `Result<()>` に包むと区別が消える。
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

/// PID のイメージパス（Win32 形式）。開けない／読めないときは `None`。
fn process_image_path(pid: u32, buffer: &mut [u16]) -> Option<String> {
    // SAFETY: OpenProcess の戻りハンドルは必ず CloseHandle する。buffer は呼び出し元が
    // 所有する書き込み可能領域で、size はその UTF-16 要素数（戻りで実長に更新される）。
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

/// 実行ファイル名から表示名を作る（末尾の `.exe` を大小無視で落とす）。
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

    /// 実機での列挙は panic せず、返ったエントリは pid 非 0。
    #[test]
    fn list_processes_does_not_panic() {
        if let Ok(list) = list_processes() {
            for p in &list {
                assert_ne!(p.pid, 0);
                assert_eq!(p.bundle_id, None);
            }
        }
    }

    /// 自プロセスの PID を読めること（名前解決経路の実機確認）。
    #[test]
    fn own_image_path_is_readable() {
        let mut buffer = vec![0u16; IMAGE_PATH_CAPACITY];
        let path = process_image_path(std::process::id(), &mut buffer)
            .expect("the current process image path should be readable");
        assert!(path.to_ascii_lowercase().ends_with(".exe"), "{path}");
    }
}
