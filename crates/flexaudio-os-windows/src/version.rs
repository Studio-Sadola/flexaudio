//! Windows のビルド番号ゲート。プロセスループバックは build 20348 以上が必須。
//!
//! 判定の本体（ビルド番号 → 可否）は OS 呼び出しから切り離した純粋関数にしてあり、
//! 非 Windows でも単体テストできる。実際の版の取得は互換モードで偽られない
//! `RtlGetVersion`（ntdll）を使う。

#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

/// プロセスループバック（`ActivateAudioInterfaceAsync` +
/// `AUDIOCLIENT_ACTIVATION_PARAMS`）が使える最小 Windows ビルド。
/// Windows 11 と Windows Server 2022 がこの番号を満たす。
pub(crate) const MIN_PROCESS_LOOPBACK_BUILD: u32 = 20_348;

/// `build` がプロセスループバックの最小要件（20348）を満たすか。
///
/// テストできるよう OS 呼び出しから切り離した純粋関数。
pub(crate) fn process_loopback_supported(build: u32) -> bool {
    build >= MIN_PROCESS_LOOPBACK_BUILD
}

#[cfg(target_os = "windows")]
mod query {
    use flexaudio_core::types::{Error, Result};
    use windows::Wdk::System::SystemServices::RtlGetVersion;
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;

    use super::process_loopback_supported;

    /// 実行中の Windows ビルド番号を `RtlGetVersion` で取る。
    ///
    /// `GetVersionEx` は互換モードのマニフェストで偽られることがあるので使わない。
    /// 取得できなければ [`Error::Backend`]（呼び出し側はプロセスループバック不可として
    /// fail-closed する）。
    fn current_os_build() -> Result<u32> {
        let mut info = OSVERSIONINFOW {
            dwOSVersionInfoSize: core::mem::size_of::<OSVERSIONINFOW>() as u32,
            ..Default::default()
        };
        // SAFETY: `info` は有効な OSVERSIONINFOW。dwOSVersionInfoSize は構造体サイズ。
        // RtlGetVersion は ntdll の安定 API で、互換レイヤーを迂回して実ビルドを返す。
        let status = unsafe { RtlGetVersion(&mut info) };
        if status.is_err() {
            return Err(Error::Backend(format!(
                "RtlGetVersion failed (NTSTATUS {})",
                status.0
            )));
        }
        Ok(info.dwBuildNumber)
    }

    /// プロセスループバックがこの OS で使えるか確認する。build 20348 未満、または
    /// ビルド番号を取れないときは [`Error::UnsupportedOsVersion`] /
    /// [`Error::Backend`]。列挙（[`list_processes`](crate::list_processes)）と録音
    /// （[`WasapiProcessBackend`](crate::WasapiProcessBackend)）が同じ関数を使う。
    pub(crate) fn ensure_process_loopback_supported() -> Result<()> {
        let build = current_os_build()?;
        if process_loopback_supported(build) {
            Ok(())
        } else {
            Err(Error::UnsupportedOsVersion)
        }
    }
}

#[cfg(target_os = "windows")]
pub(crate) use query::ensure_process_loopback_supported;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_below_20348_is_unsupported() {
        assert!(!process_loopback_supported(0));
        assert!(!process_loopback_supported(19_045));
        assert!(!process_loopback_supported(20_347));
    }

    #[test]
    fn build_20348_is_the_boundary() {
        assert!(process_loopback_supported(20_348));
    }

    #[test]
    fn newer_builds_are_supported() {
        assert!(process_loopback_supported(22_000));
        assert!(process_loopback_supported(26_000));
        assert!(process_loopback_supported(u32::MAX));
    }
}
