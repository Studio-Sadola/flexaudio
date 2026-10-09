//! [`MacProcessBackend`] — Process Tap loopback for an individual process.
//!
//! Converts the target PID to an `AudioObjectID` and uses [`ProcessMode`] to select INCLUDE or EXCLUDE.
//! - [`ProcessMode::Include`] (default) → `initStereoMixdownOfProcesses([objectID])`
//!   (captures only the target PID).
//! - [`ProcessMode::Exclude`] → `initStereoGlobalTapButExcludeProcesses([objectID])`
//!   (captures all system audio except the target PID).
//!
//! `mode` applies only to process sources and is not combined with `exclude_self` on system sources
//! (process sources ignore `exclude_self`).
//!
//! Equivalent to Windows [`WasapiProcessBackend`](../flexaudio_os_windows) and Linux
//! [`PwProcessBackend`](../flexaudio_os_linux).
//!
//! # Threading / Send
//! Uses the same design as [`MacSystemBackend`](crate::MacSystemBackend). Confine the `!Send` ObjC
//! value ([`TapChain`]) to a dedicated thread; the backend itself holds only `Send` values.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use flexaudio_core::backend::{CaptureBackend, RawSink};
use flexaudio_core::types::{Error, Event, ProcessMode, Result};

use crate::common::{translate_pid_to_object, FALLBACK_FORMAT};
use crate::probe::PublicationGate;
use crate::system::run_tap_thread;
use crate::tap::TapKind;
use crate::terminal::{StartAction, TerminalFailure};

/// A [`CaptureBackend`] that captures audio for a specific PID using a process-specific Process Tap.
///
/// Converts the PID to an objectID and builds the tap chain on a dedicated thread, then sends
/// interleaved f32 from the IOProc real-time block to [`RawSink::push`]. If the target is silent or
/// absent and no objectID is available, [`start`](CaptureBackend::start) returns
/// [`Error::DeviceNotFound`] instead of panicking.
///
/// This type is `Send`. It holds only `target_pid`, `mode`, a stop flag, [`JoinHandle`], and a cached
/// format; the `!Send` ObjC values stay on the dedicated thread.
pub struct MacProcessBackend {
    /// PID of the process to capture.
    target_pid: u32,
    /// Capture mode. [`ProcessMode::Include`] captures only the target PID;
    /// [`ProcessMode::Exclude`] captures all system audio except the target PID.
    mode: ProcessMode,
    /// Running flag (guards duplicate starts, signals stop, and tracks drop state). `Send`.
    stop_flag: Arc<AtomicBool>,
    /// Probe publication and stop are ordered; the lock is released before joining.
    publication: Arc<PublicationGate>,
    /// Owner-reported terminal failure, retained after stop and mailbox consumption.
    terminal: Arc<TerminalFailure>,
    /// Handle for the thread that owns the tap chain (`Some` after start).
    handle: Option<JoinHandle<()>>,
    /// Owner-thread notifications for the current capture generation.
    events: Option<mpsc::Receiver<Event>>,
    /// Native format `(rate, channels)`. Cache the fallback because the actual format is determined
    /// when the tap is created, following [`MacSystemBackend`].
    native: (u32, u16),
}

impl MacProcessBackend {
    /// Build the backend from a target PID and [`ProcessMode`] (does not connect yet).
    pub fn new(target_pid: u32, mode: ProcessMode) -> Self {
        Self {
            target_pid,
            mode,
            stop_flag: Arc::new(AtomicBool::new(false)),
            publication: Arc::new(PublicationGate::default()),
            terminal: Arc::new(TerminalFailure::default()),
            handle: None,
            events: None,
            native: FALLBACK_FORMAT,
        }
    }

    /// PID of the process to capture.
    pub fn target_pid(&self) -> u32 {
        self.target_pid
    }

    /// Capture mode held by this backend.
    pub fn mode(&self) -> ProcessMode {
        self.mode
    }
}

impl CaptureBackend for MacProcessBackend {
    fn native_format(&self) -> (u32, u16) {
        self.native
    }

    fn start(&mut self, sink: RawSink) -> Result<()> {
        if self.terminal.check_start(self.handle.is_some())? == StartAction::AlreadyRunning {
            return Ok(());
        }

        // Version gate: Process Tap requires macOS 14.4 or later. Check the OS version before
        // creating the tap; return the typed Error::UnsupportedOsVersion instead of converting a
        // raw OSStatus into a Backend error.
        crate::version::ensure_process_tap_supported()?;

        self.stop_flag.store(false, Ordering::SeqCst);

        let stop_flag = self.stop_flag.clone();
        let publication = self.publication.clone();
        let terminal = self.terminal.clone();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let (event_tx, event_rx) = mpsc::channel();
        let target_pid = self.target_pid;
        // ProcessMode is Copy, so it can be moved directly into the closure.
        let mode = self.mode;

        let handle = thread::Builder::new()
            .name("flexaudio-macos-process".into())
            .spawn(move || {
                // PID-to-AudioObjectID conversion calls CoreAudio, so do it on the owner thread.
                let kind = match translate_pid_to_object(target_pid as i32) {
                    Ok(0) => {
                        // No audio object for the target process (silent or absent).
                        let _ = ready_tx.send(Err(Error::DeviceNotFound));
                        return;
                    }
                    Ok(object_id) => match mode {
                        // INCLUDE (default): mix down only the target PID.
                        ProcessMode::Include => TapKind::IncludeProcesses(vec![object_id]),
                        // EXCLUDE: all system audio except the target PID.
                        ProcessMode::Exclude => TapKind::ExcludeProcesses(vec![object_id]),
                    },
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                run_tap_thread(
                    kind,
                    sink,
                    stop_flag,
                    publication,
                    terminal,
                    ready_tx,
                    event_tx,
                );
            })
            .map_err(|e| Error::Backend(format!("spawn macos process thread: {e}")))?;

        match ready_rx.recv() {
            Ok(Ok(())) => {
                self.handle = Some(handle);
                self.events = Some(event_rx);
                Ok(())
            }
            Ok(Err(e)) => {
                if matches!(e, Error::PermissionDenied { .. }) {
                    self.terminal.record(e.clone());
                }
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
        // Poison still sets cancellation, so shutdown remains fail-closed.
        let _ = self.publication.cancel(&self.stop_flag);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }

    fn poll_event(&mut self) -> Option<Event> {
        self.events.as_ref()?.try_recv().ok()
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

    #[test]
    fn repeated_start_returns_terminal_cause_before_running_noop_and_after_stop() {
        for cause in [
            Error::PermissionDenied {
                permission: flexaudio_core::types::Permission::SystemAudio,
                detail: "controlled diagnostic capture stayed zero".into(),
            },
            Error::Backend("self-probe publication gate poisoned".into()),
        ] {
            let mut backend = MacProcessBackend::new(1234, ProcessMode::Include);
            // Only a completed Rust thread and in-memory ring are needed to
            // exercise the native-free early start and stop paths.
            backend.handle = Some(thread::spawn(|| {}));
            let sink = || {
                let (producer, _consumer) = raw_ring(16);
                RawSink::new(producer, 48_000, 2)
            };
            assert_eq!(backend.start(sink()), Ok(()));
            backend.terminal.record(cause.clone());
            assert_eq!(backend.start(sink()), Err(cause.clone()));
            assert_eq!(backend.start(sink()), Err(cause.clone()));
            backend.stop();
            assert_eq!(backend.start(sink()), Err(cause));
        }
    }

    /// `new` and `native_format` return valid values without panicking.
    #[test]
    fn new_and_native_format_do_not_panic() {
        let backend = MacProcessBackend::new(1234, ProcessMode::Include);
        let (rate, channels) = backend.native_format();
        assert!(rate > 0);
        assert!(channels > 0);
        assert_eq!(backend.target_pid(), 1234);
        assert_eq!(backend.mode(), ProcessMode::Include);
    }

    /// `start` → `stop` does not panic, whether or not the target PID exists.
    /// An `Err` is allowed for a missing PID or unapproved TCC; a panic is not.
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
            Err(_e) => { /* A missing PID or unapproved TCC is allowed. */ }
        }
    }
}

#[cfg(test)]
mod repro_tests {
    use super::*;
    use flexaudio_core::raw_ring;

    #[test]
    #[ignore = "repro: D L4 unchecked target PID"]
    fn repro_p7mac_oversized_target_pid_is_invalid_arg() {
        // On macOS 14.4+, this must fail before any native PID translation or tap creation.
        let mut backend = MacProcessBackend::new(u32::MAX, ProcessMode::Include);
        let (producer, _consumer) = raw_ring(16);
        let result = backend.start(RawSink::new(producer, 48_000, 2));
        backend.stop();
        assert!(
            matches!(result, Err(Error::InvalidArg(_))),
            "got {result:?}"
        );
    }

    #[test]
    #[ignore = "repro: C F37 / D L13 owner panic"]
    fn repro_p7mac_stop_reports_owner_panic() {
        let mut backend = MacProcessBackend::new(1, ProcessMode::Include);
        backend.handle = Some(thread::spawn(|| panic!("injected owner failure")));
        backend.stop();
        assert!(
            backend.poll_event().is_some(),
            "explicit stop must expose the owner panic"
        );
    }
}
