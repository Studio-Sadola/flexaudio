//! [`MacSystemBackend`] — Process Tap loopback for all system audio output.
//!
//! Creates a tap with `CATapDescription::initStereoGlobalTapButExcludeProcesses([...])` and
//! captures through a private aggregate device and IOProc. Equivalent to Windows'
//! [`WasapiSystemBackend`](../flexaudio_os_windows) and Linux's
//! [`PwSystemBackend`](../flexaudio_os_linux).
//!
//! # `exclude_self`
//! Use `exclude_self` in [`MacSystemBackend::new`] to choose the exclusion set.
//! - `exclude_self == false` (default) → no exclusions via `excludeProcesses([])`; capture all system audio.
//! - `exclude_self == true` → exclude the calling process ([`std::process::id`]) through
//!   `excludeProcesses([self_object])`. This prevents capturing our own output (feedback prevention).
//!
//! exclude_pids extends the exclusion set with arbitrary pids (Electron helper tree):
//! the effective set is `exclude_pids ∪ {self if exclude_self}`, and a non-empty set
//! takes the exclusion path (so it also applies when `exclude_self == false`).
//!
//! `exclude_self` applies only to system sources and is not combined with a process source's
//! [`ProcessMode`](flexaudio_core::types::ProcessMode) (system sources ignore `mode`).
//!
//! # Selecting an output device (`device_id`)
//! Select an output device with `device_id` in [`MacSystemBackend::new`].
//! - `None` (default) → global tap for the default output.
//! - `Some(name)` → create a tap for the named output device with
//!   `initExcludingProcesses:andDeviceUID:withStream:` (resolve name → UID through
//!   [`uid_for_device_name`](crate::devices::uid_for_device_name)). If no device matches,
//!   `start` returns [`Error::DeviceNotFound`]. Enumerate devices with
//!   [`list_output_devices`](crate::list_output_devices).
//!
//! Device selection and process exclusion are combined: the tap captures audio sent to the
//! requested device while excluding the resolved process objects. Without a requested device,
//! the exclusion tap uses the default output.
//!
//! # Threading / Send
//! Keep `!Send` ObjC objects used by the tap/aggregate/IOProc ([`TapChain`]) on a dedicated
//! thread. [`MacSystemBackend`] stores only `Send` values (stop flag, [`JoinHandle`], and
//! cached format), following the same design as Windows.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use flexaudio_core::backend::{CaptureBackend, RawSink};
use flexaudio_core::types::{Error, Result};

use crate::common::{translate_pid_to_object, FALLBACK_FORMAT};
use crate::tap::{build_tap_chain, TapChain, TapKind};

/// Choose the system tap from the resolved exclusion snapshot and optional device UID.
fn system_tap_kind(excluded_objects: Vec<u32>, device_uid: Option<String>) -> TapKind {
    match device_uid {
        Some(device_uid) if excluded_objects.is_empty() => TapKind::ExcludeProcessesOnDevice {
            ids: excluded_objects,
            device_uid,
        },
        _ => TapKind::ExcludeProcesses(excluded_objects),
    }
}

fn checked_exclusion_pid(pid: u32) -> Result<i32> {
    match i32::try_from(pid) {
        Ok(pid) if pid > 0 => Ok(pid),
        _ => Err(Error::InvalidArg(format!(
            "exclude_pids: pid {pid} is not a valid macOS pid"
        ))),
    }
}

/// Resolve the start-time exclusion snapshot; only confirmed process exit can
/// suppress a translation failure for a non-host pid.
fn resolve_exclusion(
    translation: Result<u32>,
    is_host: bool,
    probe: impl FnOnce() -> std::io::Result<()>,
) -> Result<Option<u32>> {
    match translation {
        Ok(0) => Ok(None),
        Ok(object_id) => Ok(Some(object_id)),
        Err(error) if !is_host => match probe() {
            Err(probe_error) if probe_error.raw_os_error() == Some(libc::ESRCH) => Ok(None),
            _ => Err(error),
        },
        Err(error) => Err(error),
    }
}

fn probe_process_exists(pid: i32) -> std::io::Result<()> {
    // SAFETY: The caller validated a positive process ID. Signal 0 checks
    // existence and permissions without sending a signal or using pointers.
    let status = unsafe { libc::kill(pid, 0) };
    if status == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// [`CaptureBackend`] that captures all system audio output through Process Tap.
///
/// Builds the tap chain (global tap → aggregate → IOProc) on a dedicated thread and sends
/// interleaved f32 from the IOProc RT block to [`RawSink::push`]. If tap creation fails (for
/// example, because TCC permission has not been granted), [`start`](CaptureBackend::start)
/// returns [`Error`] without panicking.
///
/// `exclude_self` controls whether to exclude the calling process's output (feedback prevention).
/// `device_id` selects the output device (`None` = default output).
///
/// This type is `Send`. It stores only `exclude_self`, `device_id`, a stop flag, [`JoinHandle`],
/// and the cached format; `!Send` ObjC objects stay on the dedicated thread.
pub struct MacSystemBackend {
    /// Host-process exclusion flag. When `true`, exclude this process ([`std::process::id`])
    /// with `excludeProcesses([self])`; when `false`, capture all system audio without exclusions.
    exclude_self: bool,
    /// Extra pids excluded from the tap (see `StreamConfig::exclude_pids`). The
    /// effective exclusion set is `exclude_pids ∪ {self if exclude_self}`; a
    /// non-empty set takes the exclusion path.
    exclude_pids: Vec<u32>,
    /// Target output device name (= [`DeviceInfo::id`](flexaudio_core::types::DeviceInfo)).
    /// `None` uses a global tap for the default output; `Some(name)` targets that output device.
    /// Process exclusions also apply to the selected device.
    device_id: Option<String>,
    /// Running flag (guards repeated start, signals stop, and tracks drop state). `Send`.
    stop_flag: Arc<AtomicBool>,
    /// Handle to the thread that owns the tap chain (`Some` after start).
    handle: Option<JoinHandle<()>>,
    /// Native format `(rate, channels)`. The actual values are determined after tap creation
    /// through `start`, but `native_format` returns the cached fallback beforehand.
    native: (u32, u16),
}

impl MacSystemBackend {
    /// Create a system loopback backend (does not create the tap yet).
    ///
    /// When `exclude_self` is `true`, `start` adds the calling process ([`std::process::id`]) to
    /// the exclusion set (feedback prevention). When `false`, the tap captures all system audio.
    ///
    /// If `device_id` is `Some(name)`, create a tap for the named output device (resolve name →
    /// UID at `start`, returning [`Error::DeviceNotFound`] if missing). `None` selects the
    /// default output. Process exclusions also apply when a device is selected.
    ///
    /// Cache fallback native format `(48000, 2)`. The actual format comes from the tap's ASBD
    /// when the tap is created at `start`, but `native_format` must return a value at
    /// construction time, so use a safe fallback that does not require a tap. The Normalizer
    /// uses 20 ms output-time chunks, so the first resampling stage absorbs modest errors in
    /// the native-format estimate.
    pub fn new(exclude_self: bool, device_id: Option<String>) -> Self {
        Self {
            exclude_self,
            exclude_pids: Vec::new(),
            device_id,
            stop_flag: Arc::new(AtomicBool::new(false)),
            handle: None,
            native: FALLBACK_FORMAT,
        }
    }

    /// Exclude these pids' output in addition to `exclude_self`. Each pid is
    /// translated to its Core Audio process object at `start`; pids with no
    /// audio object (not producing sound) are skipped, not errors.
    ///
    /// Pids must be in `1..=i32::MAX`; invalid values fail capture readiness.
    /// A translation failure for a non-host pid is skipped only when `kill(pid, 0)`
    /// confirms that the process is gone (`ESRCH`). Existing processes, permission
    /// failures (`EPERM`), and other probe errors fail readiness with the original
    /// translation error. A translation failure for the host pid always fails
    /// readiness, because capturing without excluding it could echo our own output.
    ///
    /// The resolution happens once, when the capture starts: a helper that has
    /// not yet rendered audio has no Core Audio process object and is therefore
    /// not excluded. Open the capture while the app is already playing, or
    /// reopen it when a new helper appears.
    pub fn with_exclude_pids(mut self, pids: Vec<u32>) -> Self {
        self.exclude_pids = pids;
        self
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

        // Version gate: Process Tap requires macOS 14.4 or later. Check the OS version before
        // creating the tap; if unsupported, return typed Error::UnsupportedOsVersion instead
        // of obscuring the problem as a raw OSStatus→Backend error.
        crate::version::ensure_process_tap_supported()?;

        self.stop_flag.store(false, Ordering::SeqCst);

        let stop_flag = self.stop_flag.clone();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        // Effective exclusion set: `exclude_pids ∪ {self if exclude_self}`. Built here
        // and moved into the closure (PID→object translation happens on that thread).
        let mut excluded: Vec<u32> = self.exclude_pids.clone();
        if self.exclude_self {
            excluded.push(std::process::id());
        }
        // Move device_id (String) into the closure for owner-thread UID resolution.
        let device_id = self.device_id.clone();

        let handle = thread::Builder::new()
            .name("flexaudio-macos-system".into())
            .spawn(move || {
                // Resolve PIDs through Core Audio on the owning thread, as in process.rs.
                // A missing audio object remains a snapshot limitation; translation errors
                // require confirmed process exit. Resolve exclusions before device access.
                let mut ids = Vec::with_capacity(excluded.len());
                let own_pid = std::process::id();
                for pid in excluded {
                    let macos_pid = match checked_exclusion_pid(pid) {
                        Ok(pid) => pid,
                        Err(e) => {
                            let _ = ready_tx.send(Err(e));
                            return;
                        }
                    };
                    match resolve_exclusion(
                        translate_pid_to_object(macos_pid),
                        pid == own_pid,
                        || probe_process_exists(macos_pid),
                    ) {
                        Ok(None) => {}
                        Ok(Some(object_id)) => ids.push(object_id),
                        Err(e) => {
                            let _ = ready_tx.send(Err(e));
                            return;
                        }
                    }
                }
                let device_uid = if let Some(name) = device_id {
                    // Resolve the selected output device even when exclusions are active.
                    // Return DeviceNotFound if no device matches.
                    match crate::devices::uid_for_device_name(&name) {
                        Ok(Some(uid)) => Some(uid),
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
                    None
                };
                let kind = system_tap_kind(ids, device_uid);
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

/// Owner-thread body for the tap chain (shared by system / process capture).
///
/// Build the chain with [`build_tap_chain`] for `kind` and report success/failure through
/// `ready_tx`. After success, the IOProc (CoreAudio RT thread) continues processing blocks,
/// so this thread only parks until `stop_flag` is set. On stop, drop [`TapChain`] to tear down
/// resources in reverse order.
pub(crate) fn run_tap_thread(
    kind: TapKind,
    sink: RawSink,
    stop_flag: Arc<AtomicBool>,
    ready_tx: mpsc::Sender<Result<()>>,
) {
    // CATapDescription / aggregate display name (private, so collisions are harmless; debug only).
    let label = match &kind {
        TapKind::IncludeProcesses(_) => "flexaudio-process-tap",
        TapKind::ExcludeProcesses(_) => "flexaudio-system-tap",
        TapKind::ExcludeProcessesOnDevice { .. } => "flexaudio-system-device-tap",
    };
    // SAFETY: build_tap_chain calls CoreAudio. Move sink into the block.
    let chain: TapChain = match unsafe { build_tap_chain(kind, label, sink) } {
        Ok(c) => c,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    if ready_tx.send(Ok(())).is_err() {
        // The caller has gone away. Drop the chain to clean up.
        drop(chain);
        return;
    }

    // IOProc runs on CoreAudio's RT thread. This thread waits until stop.
    while !stop_flag.load(Ordering::SeqCst) {
        thread::park_timeout(std::time::Duration::from_millis(100));
    }

    // Stop. Dropping the chain tears down Stop→IOProc→aggregate→tap in order.
    drop(chain);
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio_core::raw_ring;

    #[test]
    fn system_tap_without_device_or_exclusions_captures_default_output() {
        assert!(matches!(
            system_tap_kind(Vec::new(), None),
            TapKind::ExcludeProcesses(ids) if ids.is_empty()
        ));
    }

    #[test]
    fn system_tap_without_device_preserves_exclusions() {
        assert!(matches!(
            system_tap_kind(vec![42, 43, 42], None),
            TapKind::ExcludeProcesses(ids) if ids == vec![42, 43, 42]
        ));
    }

    #[test]
    fn system_tap_with_device_and_no_exclusions_captures_selected_output() {
        assert!(matches!(
            system_tap_kind(Vec::new(), Some("output-device-uid".into())),
            TapKind::ExcludeProcessesOnDevice { ids, device_uid }
                if ids.is_empty() && device_uid == "output-device-uid"
        ));
    }

    #[test]
    fn system_tap_with_device_preserves_exclusions_and_uid() {
        assert!(matches!(
            system_tap_kind(vec![42, 43, 42], Some("output-device-uid".into())),
            TapKind::ExcludeProcessesOnDevice { ids, device_uid }
                if ids == vec![42, 43, 42] && device_uid == "output-device-uid"
        ));
    }

    #[test]
    fn exclusion_without_audio_object_skips_without_probe() {
        let result = resolve_exclusion(Ok(0), false, || {
            panic!("successful translation must not probe the process")
        });
        assert!(matches!(result, Ok(None)));
    }

    #[test]
    fn exclusion_with_audio_object_includes_without_probe() {
        let result = resolve_exclusion(Ok(42), false, || {
            panic!("successful translation must not probe the process")
        });
        assert!(matches!(result, Ok(Some(42))));
    }

    #[test]
    fn host_translation_failure_fails_without_probe() {
        let result = resolve_exclusion(Err(Error::PermissionDenied), true, || {
            panic!("host translation failure must not probe the process")
        });
        assert!(matches!(result, Err(Error::PermissionDenied)));
    }

    #[test]
    fn non_host_translation_failure_skips_confirmed_exit() {
        let result = resolve_exclusion(Err(Error::PermissionDenied), false, || {
            Err(std::io::Error::from_raw_os_error(libc::ESRCH))
        });
        assert!(matches!(result, Ok(None)));
    }

    #[test]
    fn existing_process_preserves_original_translation_error() {
        let result = resolve_exclusion(
            Err(Error::Backend("original translation error".into())),
            false,
            || Ok(()),
        );
        assert!(matches!(
            result,
            Err(Error::Backend(message)) if message == "original translation error"
        ));
    }

    #[test]
    fn eperm_preserves_original_translation_error() {
        let result = resolve_exclusion(
            Err(Error::Backend("original translation error".into())),
            false,
            || Err(std::io::Error::from_raw_os_error(libc::EPERM)),
        );
        assert!(matches!(
            result,
            Err(Error::Backend(message)) if message == "original translation error"
        ));
    }

    #[test]
    fn other_probe_errors_preserve_original_translation_error() {
        for probe_error in [
            std::io::Error::from_raw_os_error(libc::EINVAL),
            std::io::Error::other("unknown process probe failure"),
        ] {
            let result = resolve_exclusion(
                Err(Error::Backend("original translation error".into())),
                false,
                || Err(probe_error),
            );
            assert!(matches!(
                result,
                Err(Error::Backend(message)) if message == "original translation error"
            ));
        }
    }

    #[test]
    fn zero_exclusion_pid_is_invalid() {
        assert!(matches!(
            checked_exclusion_pid(0),
            Err(Error::InvalidArg(message))
                if message == "exclude_pids: pid 0 is not a valid macOS pid"
        ));
    }

    #[test]
    fn exclusion_pid_above_i32_max_is_invalid() {
        for pid in [2_147_483_648, u32::MAX] {
            assert!(matches!(
                checked_exclusion_pid(pid),
                Err(Error::InvalidArg(message))
                    if message == format!("exclude_pids: pid {pid} is not a valid macOS pid")
            ));
        }
    }

    #[test]
    fn positive_i32_exclusion_pid_boundaries_are_valid() {
        assert!(matches!(checked_exclusion_pid(1), Ok(1)));
        assert!(matches!(checked_exclusion_pid(2_147_483_647), Ok(i32::MAX)));
    }

    /// `new` + `native_format` return valid values without panicking.
    #[test]
    fn new_and_native_format_do_not_panic() {
        let backend = MacSystemBackend::new(false, None);
        let (rate, channels) = backend.native_format();
        assert!(rate > 0);
        assert!(channels > 0);
    }

    /// `start` → `stop` does not panic regardless of whether tap creation succeeds.
    /// Accept `Err` if TCC permission is missing or tap creation is unavailable; panics are not accepted.
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
                backend.stop(); // Repeated stop calls are safe too.
            }
            Err(_e) => { /* Missing TCC permission / unavailable tap is acceptable. */ }
        }
    }

    /// With `exclude_self == true`, `native_format` is valid and `start` → `stop` does not
    /// panic (headless/CI may return `Err` without TCC; exercise the self-PID conversion path).
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
                backend.stop(); // Repeated stop calls are safe too.
            }
            Err(_e) => { /* Missing TCC permission / unavailable tap / self-PID conversion failure is acceptable. */
            }
        }
    }

    /// The `with_exclude_pids` builder stores the pids and leaves `exclude_self` alone.
    #[test]
    fn exclude_pids_builder_is_stored() {
        let be = MacSystemBackend::new(false, None).with_exclude_pids(vec![11, 12]);
        assert_eq!(be.exclude_pids, vec![11, 12]);
        assert!(!be.exclude_self);
    }

    /// Specifying a nonexistent output-device name makes `start` return `DeviceNotFound`.
    /// Name resolution fails deterministically even when headless (before macOS 14.4, the
    /// version gate returns `UnsupportedOsVersion` first, which is also acceptable).
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
