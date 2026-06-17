//! [`MacProcessBackend`] — プロセス別 Process Tap loopback。
//!
//! 対象 PID を `AudioObjectID` へ変換し、[`ProcessMode`] で INCLUDE / EXCLUDE を切り替える。
//! - [`ProcessMode::Include`]（既定）→ `initStereoMixdownOfProcesses([objectID])`
//!   （対象 PID だけ録る）。
//! - [`ProcessMode::Exclude`] → `initStereoGlobalTapButExcludeProcesses([objectID])`
//!   （対象 PID を除く全システム音）。
//!
//! `mode` は process ソース専用で、system ソースの `exclude_self` とは合成しない
//! （process ソースは `exclude_self` を見ない）。
//!
//! Windows の [`WasapiProcessBackend`](../flexaudio_os_windows) / Linux の
//! [`PwProcessBackend`](../flexaudio_os_linux) に相当する。
//!
//! # スレッド / Send
//! [`MacSystemBackend`](crate::MacSystemBackend) と同じ作り。`!Send` な ObjC（[`TapChain`]）は
//! 専用スレッド内に閉じ込め、本体は `Send` なものだけ保持する。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use flexaudio_core::backend::{CaptureBackend, RawSink};
use flexaudio_core::types::{Error, ProcessMode, Result};

use crate::common::{translate_pid_to_object, FALLBACK_FORMAT};
use crate::system::run_tap_thread;
use crate::tap::TapKind;

/// プロセス別 Process Tap で特定 PID の音声をキャプチャする [`CaptureBackend`]。
///
/// 専用スレッド上で PID→objectID 変換と tap チェーン構築を行い、IOProc の RT block から
/// interleaved f32 を [`RawSink::push`] へ流す。対象が無音/不在で objectID が得られないときは
/// panic せず [`start`](CaptureBackend::start) が [`Error::DeviceNotFound`] を返す。
///
/// `Send`。保持するのは `target_pid` / `mode` / 停止フラグ / [`JoinHandle`] / キャッシュ済み
/// フォーマットだけで、`!Send` な ObjC は専用スレッド内に閉じ込める。
pub struct MacProcessBackend {
    /// キャプチャ対象プロセスの PID。
    target_pid: u32,
    /// 録音モード。[`ProcessMode::Include`] で INCLUDE（対象 PID の音だけ）、
    /// [`ProcessMode::Exclude`] で EXCLUDE（対象 PID を除く全システム音）。
    mode: ProcessMode,
    /// 起動中フラグ（二重 start ガード / 停止指示 / drop 判定）。`Send`。
    stop_flag: Arc<AtomicBool>,
    /// tap チェーンを所有するスレッドのハンドル（start 後に `Some`）。
    handle: Option<JoinHandle<()>>,
    /// ネイティブフォーマット `(rate, channels)`。フォールバックをキャッシュする
    /// （実フォーマットは tap 作成時に決まる。[`MacSystemBackend`] と同じ方針）。
    native: (u32, u16),
}

impl MacProcessBackend {
    /// 対象 PID と [`ProcessMode`] からバックエンドを構築する（この時点では接続しない）。
    pub fn new(target_pid: u32, mode: ProcessMode) -> Self {
        Self {
            target_pid,
            mode,
            stop_flag: Arc::new(AtomicBool::new(false)),
            handle: None,
            native: FALLBACK_FORMAT,
        }
    }

    /// キャプチャ対象の PID。
    pub fn target_pid(&self) -> u32 {
        self.target_pid
    }

    /// 保持している録音モード。
    pub fn mode(&self) -> ProcessMode {
        self.mode
    }
}

impl CaptureBackend for MacProcessBackend {
    fn native_format(&self) -> (u32, u16) {
        self.native
    }

    fn start(&mut self, sink: RawSink) -> Result<()> {
        if self.handle.is_some() {
            return Ok(());
        }

        // バージョンゲート。Process Tap は macOS 14.4 以上が必須。tap 生成へ進む前に OS バージョンを
        // 確認し、満たさなければ raw OSStatus→Backend に化けさせず型付きの
        // Error::UnsupportedOsVersion を返す。
        crate::version::ensure_process_tap_supported()?;

        self.stop_flag.store(false, Ordering::SeqCst);

        let stop_flag = self.stop_flag.clone();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let target_pid = self.target_pid;
        // ProcessMode は Copy なのでそのままクロージャへ move できる。
        let mode = self.mode;

        let handle = thread::Builder::new()
            .name("flexaudio-macos-process".into())
            .spawn(move || {
                // PID → AudioObjectID 変換は CoreAudio を叩くので所有スレッド内で行う。
                let kind = match translate_pid_to_object(target_pid as i32) {
                    Ok(0) => {
                        // 対象プロセスに対応するオーディオオブジェクトが無い（無音/不在）。
                        let _ = ready_tx.send(Err(Error::DeviceNotFound));
                        return;
                    }
                    Ok(object_id) => match mode {
                        // INCLUDE（既定）: 対象 PID だけの mixdown。
                        ProcessMode::Include => TapKind::IncludeProcesses(vec![object_id]),
                        // EXCLUDE: 対象 PID を除く全システム音。
                        ProcessMode::Exclude => TapKind::ExcludeProcesses(vec![object_id]),
                    },
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                run_tap_thread(kind, sink, stop_flag, ready_tx);
            })
            .map_err(|e| Error::Backend(format!("spawn macos process thread: {e}")))?;

        match ready_rx.recv() {
            Ok(Ok(())) => {
                self.handle = Some(handle);
                Ok(())
            }
            Ok(Err(e)) => {
                self.stop_flag.store(false, Ordering::SeqCst);
                let _ = handle.join();
                Err(e)
            }
            Err(_) => {
                self.stop_flag.store(false, Ordering::SeqCst);
                let _ = handle.join();
                Err(Error::Backend(
                    "macos process thread exited before reporting readiness".into(),
                ))
            }
        }
    }

    fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for MacProcessBackend {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio_core::raw_ring;

    /// `new` + `native_format` は panic せず妥当な値を返す。
    #[test]
    fn new_and_native_format_do_not_panic() {
        let backend = MacProcessBackend::new(1234, ProcessMode::Include);
        let (rate, channels) = backend.native_format();
        assert!(rate > 0);
        assert!(channels > 0);
        assert_eq!(backend.target_pid(), 1234);
        assert_eq!(backend.mode(), ProcessMode::Include);
    }

    /// `start` → `stop` が対象 PID 有無を問わず panic しないこと。
    /// 不在 PID / TCC 未承認では `Err` を許容（panic だけ不可）。
    #[test]
    fn start_then_stop_tolerates_missing_target() {
        let mut backend = MacProcessBackend::new(0xFFFF_FFFE, ProcessMode::Include);
        let (rate, channels) = backend.native_format();
        let cap = (rate as usize * channels as usize).max(1);
        let (prod, _cons) = raw_ring(cap);
        let sink = RawSink::new(prod, rate, channels);

        match backend.start(sink) {
            Ok(()) => {
                backend.stop();
                backend.stop();
            }
            Err(_e) => { /* 不在 PID / TCC 未承認は許容 */ }
        }
    }
}
