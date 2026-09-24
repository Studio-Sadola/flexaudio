//! [`MacSystemBackend`] — Process Tap loopback of the whole system audio output.
//!
//! Creates a tap with `CATapDescription::initStereoGlobalTapButExcludeProcesses([...])` and
//! captures through a private aggregate device + IOProc. The counterpart of Windows's
//! [`WasapiSystemBackend`](../flexaudio_os_windows) / Linux's
//! [`PwSystemBackend`](../flexaudio_os_linux).
//!
//! # `exclude_self`
//! The `exclude_self` of [`MacSystemBackend::new`] switches the exclusion set.
//! - `exclude_self == false` (default) → no exclusion, `excludeProcesses([])`, all system
//!   audio.
//! - `exclude_self == true` → excludes the host process ([`std::process::id`]) with
//!   `excludeProcesses([self_object])`. Our own output is not captured (feedback
//!   prevention).
//!
//! `exclude_self` is specific to the system source and is not combined with the process
//! source's [`ProcessMode`](flexaudio_core::types::ProcessMode) (the system source does not
//! look at `mode`).
//!
//! # Output device selection (`device_id`)
//! The `device_id` of [`MacSystemBackend::new`] selects the target output device.
//! - `None` (default) → a global tap on the default output.
//! - `Some(name)` → creates a tap targeting the output device with that name via
//!   `initExcludingProcesses:andDeviceUID:withStream:` (name → UID is resolved with
//!   [`uid_for_device_name`](crate::devices::uid_for_device_name)). If no device matches,
//!   `start` returns [`Error::DeviceNotFound`]. Enumeration is
//!   [`list_output_devices`](crate::list_output_devices).
//!
//! When `exclude_self == true`, `device_id` is ignored and an own-process-excluding tap on
//! the default output is used (excluding our own audio takes priority).
//!
//! # Threads / Send
//! The `!Send` ObjC objects around tap/aggregate/ioproc ([`TapChain`]) are confined to the
//! dedicated thread, and [`MacSystemBackend`] holds only `Send` things (stop flag,
//! [`JoinHandle`], cached format) (same design as Windows).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use flexaudio_core::backend::{CaptureBackend, RawSink};
use flexaudio_core::types::{Error, Result};

use crate::common::{translate_pid_to_object, FALLBACK_FORMAT};
use crate::tap::{build_tap_chain, TapChain, TapKind};

/// [`CaptureBackend`] that captures the whole system audio output with a Process Tap.
///
/// Builds the tap chain (global tap → aggregate → IOProc) on a dedicated thread and feeds
/// interleaved f32 from the IOProc's RT block to [`RawSink::push`]. When tap creation fails
/// (e.g. TCC not approved), it does not panic; [`start`](CaptureBackend::start) returns an
/// [`Error`].
///
/// `exclude_self` switches whether the host process's output is excluded (feedback
/// prevention). `device_id` selects the target output device (`None` = default output).
///
/// `Send`. It holds only `exclude_self`, `device_id`, the stop flag, [`JoinHandle`], and the
/// cached format; `!Send` ObjC objects are confined to the dedicated thread.
pub struct MacSystemBackend {
    /// Host-process exclusion flag. `true` adds our own ([`std::process::id`]) output to the
    /// exclusion set (`excludeProcesses([self])`); `false` means all system audio with no
    /// exclusion.
    exclude_self: bool,
    /// Target output device name (= [`DeviceInfo::id`](flexaudio_core::types::DeviceInfo)).
    /// `None` for a global tap on the default output, `Some(name)` to target that output
    /// device. Ignored when `exclude_self == true`.
    device_id: Option<String>,
    /// Running flag (double-start guard / stop request / drop check). `Send`.
    stop_flag: Arc<AtomicBool>,
    /// Handle of the thread that owns the tap chain (`Some` after start).
    handle: Option<JoinHandle<()>>,
    /// Native format `(rate, channels)`. The actual value is determined through `start`
    /// after the tap is created, but `native_format` returns the pre-cached value (the
    /// fallback).
    native: (u32, u16),
}

impl MacSystemBackend {
    /// Builds a system loopback backend (does not create the tap yet).
    ///
    /// When `exclude_self` is `true`, `start` adds the host process ([`std::process::id`]) to
    /// the exclusion set (feedback prevention). When `false`, it is a whole-system tap with no
    /// exclusion.
    ///
    /// When `device_id` is `Some(name)`, the tap targets the output device with that name
    /// (`start` resolves name → UID and returns [`Error::DeviceNotFound`] if none exists).
    /// `None` means the default output. `device_id` is ignored when `exclude_self == true`.
    ///
    /// The native format caches the fallback `(48000, 2)`. The real format is determined from
    /// the tap's ASBD when the tap is created (`start`), but `native_format` must return one
    /// value at construction time, so the fallback, which can be obtained safely without a
    /// tap, is used. The Normalizer works on a 20 ms output time base, so small errors in the
    /// native estimate are absorbed by the first-stage resample.
    pub fn new(exclude_self: bool, device_id: Option<String>) -> Self {
        Self {
            exclude_self,
            device_id,
            stop_flag: Arc::new(AtomicBool::new(false)),
            handle: None,
            native: FALLBACK_FORMAT,
        }
    }
}

impl Default for MacSystemBackend {
    fn default() -> Self {
        Self::new(false, None)
    }
}

impl CaptureBackend for MacSystemBackend {
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
        // bool is Copy, so it can be moved into the closure as is.
        let exclude_self = self.exclude_self;
        // device_id (String) is moved into the closure. Not used when exclude_self is set.
        let device_id = self.device_id.clone();

        let handle = thread::Builder::new()
            .name("flexaudio-macos-system".into())
            .spawn(move || {
                let kind = if exclude_self {
                    // exclude_self takes priority over device_id. Exclude our own process on
                    // the default output. The PID → AudioObjectID translation calls CoreAudio,
                    // so do it on the owning thread (same as process.rs). Put the host
                    // process's object into the exclusion set.
                    match translate_pid_to_object(std::process::id() as i32) {
                        // There is no audio object for our own process (e.g. it is silent and
                        // not outputting right now). There is none of our own audio to exclude,
                        // so instead of an error, fall back to a whole-system tap with no
                        // exclusion (so nothing is missed).
                        Ok(0) => TapKind::ExcludeProcesses(Vec::new()),
                        // Add our own process's object to the exclusion set (feedback
                        // prevention).
                        Ok(self_object_id) => TapKind::ExcludeProcesses(vec![self_object_id]),
                        // The translation itself failed (TCC etc.). Report Err as readiness
                        // and exit.
                        Err(e) => {
                            let _ = ready_tx.send(Err(e));
                            return;
                        }
                    }
                } else if let Some(name) = device_id {
                    // A specific output device. Resolve name → UID and tap all audio going to
                    // that device (no exclusion). DeviceNotFound if no device matches.
                    match crate::devices::uid_for_device_name(&name) {
                        Ok(Some(uid)) => TapKind::ExcludeProcessesOnDevice {
                            ids: Vec::new(),
                            device_uid: uid,
                        },
                        Ok(None) => {
                            let _ = ready_tx.send(Err(Error::DeviceNotFound));
                            return;
                        }
                        Err(e) => {
                            let _ = ready_tx.send(Err(e));
                            return;
                        }
                    }
                } else {
                    // Default output. All system audio with no exclusion. No PID translation
                    // needed.
                    TapKind::ExcludeProcesses(Vec::new())
                };
                run_tap_thread(kind, sink, stop_flag, ready_tx);
            })
            .map_err(|e| Error::Backend(format!("spawn macos system thread: {e}")))?;

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
                    "macos system thread exited before reporting readiness".into(),
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

impl Drop for MacSystemBackend {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Body of the thread that owns the tap chain (shared by system / process).
///
/// Builds the chain with [`build_tap_chain`] according to `kind` and reports success or
/// failure through `ready_tx`. After success, the IOProc (CoreAudio's RT thread) keeps
/// running the block in the background, so this thread just parks until `stop_flag` is set.
/// On stop, it drops the [`TapChain`], tearing it down in reverse order.
pub(crate) fn run_tap_thread(
    kind: TapKind,
    sink: RawSink,
    stop_flag: Arc<AtomicBool>,
    ready_tx: mpsc::Sender<Result<()>>,
) {
    // Display name of the CATapDescription / aggregate (private, so collisions are harmless;
    // for debugging).
    let label = match &kind {
        TapKind::IncludeProcesses(_) => "flexaudio-process-tap",
        TapKind::ExcludeProcesses(_) => "flexaudio-system-tap",
        TapKind::ExcludeProcessesOnDevice { .. } => "flexaudio-system-device-tap",
    };
    // SAFETY: build_tap_chain calls CoreAudio. sink is moved into the block.
    let chain: TapChain = match unsafe { build_tap_chain(kind, label, sink) } {
        Ok(c) => c,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    if ready_tx.send(Ok(())).is_err() {
        // The caller is gone. Drop the chain to clean up.
        drop(chain);
        return;
    }

    // The IOProc runs on CoreAudio's RT thread. This thread waits until stop.
    while !stop_flag.load(Ordering::SeqCst) {
        thread::park_timeout(std::time::Duration::from_millis(100));
    }

    // Stop. Dropping the chain tears down Stop → IOProc → aggregate → tap in that order.
    drop(chain);
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio_core::raw_ring;

    /// `new` + `native_format` do not panic and return sensible values.
    #[test]
    fn new_and_native_format_do_not_panic() {
        let backend = MacSystemBackend::new(false, None);
        let (rate, channels) = backend.native_format();
        assert!(rate > 0);
        assert!(channels > 0);
    }

    /// `start` → `stop` does not panic whether or not the tap can be created.
    /// `Err` is allowed in environments with TCC not approved / no tap support (only panics
    /// are not).
    #[test]
    fn start_then_stop_tolerates_failure() {
        let mut backend = MacSystemBackend::new(false, None);
        let (rate, channels) = backend.native_format();
        let cap = (rate as usize * channels as usize).max(1);
        let (prod, _cons) = raw_ring(cap);
        let sink = RawSink::new(prod, rate, channels);

        match backend.start(sink) {
            Ok(()) => {
                backend.stop();
                backend.stop(); // Double stop is safe too.
            }
            Err(_e) => { /* TCC not approved / tap unavailable is allowed */ }
        }
    }

    /// With `exclude_self == true`, `native_format` is still sensible and `start` → `stop`
    /// does not panic (headless/CI has no TCC, so `Err` is allowed; exercises the own-PID
    /// translation path).
    #[test]
    fn new_exclude_self_start_then_stop_tolerates_failure() {
        let mut backend = MacSystemBackend::new(true, None);
        let (rate, channels) = backend.native_format();
        assert!(rate > 0);
        assert!(channels > 0);
        let cap = (rate as usize * channels as usize).max(1);
        let (prod, _cons) = raw_ring(cap);
        let sink = RawSink::new(prod, rate, channels);

        match backend.start(sink) {
            Ok(()) => {
                backend.stop();
                backend.stop(); // Double stop is safe too.
            }
            Err(_e) => { /* unapproved TCC / no tap / own-PID translation failure is allowed */ }
        }
    }

    /// Specifying a nonexistent output device name makes `start` return `DeviceNotFound`.
    /// It is rejected at name resolution, so it is deterministically Err even headless (below
    /// 14.4 the version gate returns `UnsupportedOsVersion` first, so that is allowed too).
    #[test]
    fn new_with_unknown_device_id_returns_device_not_found() {
        let mut backend =
            MacSystemBackend::new(false, Some("flexaudio-no-such-output-device-xyzzy".into()));
        let (rate, channels) = backend.native_format();
        let cap = (rate as usize * channels as usize).max(1);
        let (prod, _cons) = raw_ring(cap);
        let sink = RawSink::new(prod, rate, channels);

        match backend.start(sink) {
            Err(Error::DeviceNotFound) | Err(Error::UnsupportedOsVersion) => {}
            other => panic!("expected DeviceNotFound or UnsupportedOsVersion, got {other:?}"),
        }
    }
}
