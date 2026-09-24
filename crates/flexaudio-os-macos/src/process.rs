//! [`MacProcessBackend`] — per-process Process Tap loopback.
//!
//! Translates the target PID into an `AudioObjectID` and switches between INCLUDE / EXCLUDE
//! with [`ProcessMode`].
//! - [`ProcessMode::Include`] (default) → `initStereoMixdownOfProcesses([objectID])`
//!   (captures only the target PID).
//! - [`ProcessMode::Exclude`] → `initStereoGlobalTapButExcludeProcesses([objectID])`
//!   (all system audio except the target PID).
//!
//! `mode` is specific to the process source and is not combined with the system source's
//! `exclude_self` (the process source does not look at `exclude_self`).
//!
//! The counterpart of Windows's [`WasapiProcessBackend`](../flexaudio_os_windows) / Linux's
//! [`PwProcessBackend`](../flexaudio_os_linux).
//!
//! # Threads / Send
//! Same design as [`MacSystemBackend`](crate::MacSystemBackend). `!Send` ObjC objects
//! ([`TapChain`]) are confined to the dedicated thread, and the backend itself holds only
//! `Send` things.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use flexaudio_core::backend::{CaptureBackend, RawSink};
use flexaudio_core::types::{Error, ProcessMode, Result};

use crate::common::{translate_pid_to_object, FALLBACK_FORMAT};
use crate::system::run_tap_thread;
use crate::tap::TapKind;

/// [`CaptureBackend`] that captures the audio of a specific PID with a per-process Process
/// Tap.
///
/// Performs the PID → objectID translation and builds the tap chain on a dedicated thread,
/// and feeds interleaved f32 from the IOProc's RT block to [`RawSink::push`]. When the target
/// is silent/absent and no objectID is available, it does not panic;
/// [`start`](CaptureBackend::start) returns [`Error::DeviceNotFound`].
///
/// `Send`. It holds only `target_pid` / `mode` / the stop flag / [`JoinHandle`] / the cached
/// format; `!Send` ObjC objects are confined to the dedicated thread.
pub struct MacProcessBackend {
    /// PID of the process to capture.
    target_pid: u32,
    /// Capture mode. [`ProcessMode::Include`] means INCLUDE (only the target PID's audio);
    /// [`ProcessMode::Exclude`] means EXCLUDE (all system audio except the target PID).
    mode: ProcessMode,
    /// Running flag (double-start guard / stop request / drop check). `Send`.
    stop_flag: Arc<AtomicBool>,
    /// Handle of the thread that owns the tap chain (`Some` after start).
    handle: Option<JoinHandle<()>>,
    /// Native format `(rate, channels)`. Caches the fallback (the real format is determined
    /// when the tap is created; same policy as [`MacSystemBackend`]).
    native: (u32, u16),
}

impl MacProcessBackend {
    /// Builds the backend from the target PID and [`ProcessMode`] (does not connect yet).
    pub fn new(target_pid: u32, mode: ProcessMode) -> Self {
        Self {
            target_pid,
            mode,
            stop_flag: Arc::new(AtomicBool::new(false)),
            handle: None,
            native: FALLBACK_FORMAT,
        }
    }

    /// The PID to capture.
    pub fn target_pid(&self) -> u32 {
        self.target_pid
    }

    /// The stored capture mode.
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

        // Version gate. Process Taps require macOS 14.4 or later. Check the OS version before
        // proceeding to tap creation, and if it is not met, return the typed
        // Error::UnsupportedOsVersion instead of letting it turn into raw OSStatus → Backend.
        crate::version::ensure_process_tap_supported()?;

        self.stop_flag.store(false, Ordering::SeqCst);

        let stop_flag = self.stop_flag.clone();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let target_pid = self.target_pid;
        // ProcessMode is Copy, so it can be moved into the closure as is.
        let mode = self.mode;

        let handle = thread::Builder::new()
            .name("flexaudio-macos-process".into())
            .spawn(move || {
                // The PID → AudioObjectID translation calls CoreAudio, so do it on the owning
                // thread.
                let kind = match translate_pid_to_object(target_pid as i32) {
                    Ok(0) => {
                        // There is no audio object for the target process (silent/absent).
                        let _ = ready_tx.send(Err(Error::DeviceNotFound));
                        return;
                    }
                    Ok(object_id) => match mode {
                        // INCLUDE (default): a mixdown of only the target PID.
                        ProcessMode::Include => TapKind::IncludeProcesses(vec![object_id]),
                        // EXCLUDE: all system audio except the target PID.
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

    /// `new` + `native_format` do not panic and return sensible values.
    #[test]
    fn new_and_native_format_do_not_panic() {
        let backend = MacProcessBackend::new(1234, ProcessMode::Include);
        let (rate, channels) = backend.native_format();
        assert!(rate > 0);
        assert!(channels > 0);
        assert_eq!(backend.target_pid(), 1234);
        assert_eq!(backend.mode(), ProcessMode::Include);
    }

    /// `start` → `stop` does not panic whether or not the target PID exists.
    /// `Err` is allowed for an absent PID / unapproved TCC (only panics are not).
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
            Err(_e) => { /* absent PID / unapproved TCC is allowed */ }
        }
    }
}
