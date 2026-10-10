//! Enumerate processes available for capture ([`processes`]).
//!
//! Collect raw lists from OS-specific backends on a dedicated thread with a time limit, then
//! use [`normalize_process_list`] to produce a consistent cross-platform result (merge
//! duplicates, exclude this process, fill display names, and sort stably). Enumeration is
//! implemented once per OS, and this is the single entry point.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use flexaudio_core::process_list::normalize_process_list;
use flexaudio_core::types::{Error, ProcessInfo, Result};

/// Maximum duration for enumeration. The caller always returns within this time, even if an
/// OS query (PipeWire round trip, COM, or IPC to coreaudiod) stops responding. The Linux
/// backend has a shorter internal deadline (2 seconds), which normally takes effect first.
pub(crate) const PROCESS_ENUM_TIMEOUT: Duration = Duration::from_secs(3);

/// Enumerate processes with audio-output sessions (streams) that can currently be targeted
/// by per-process capture ([`SourceKind::ProcessLoopback`](crate::SourceKind)). The caller
/// process itself is excluded.
///
/// Pass a returned [`ProcessInfo::pid`] to [`StreamConfig::target_pid`](crate::StreamConfig)
/// to capture that process. Results are ordered by active output first, then display name,
/// then PID; duplicate PIDs are merged. This is read-only and does not trigger permission prompts.
///
/// Only one OS query can run at a time (single-flight). If a previous query has not finished,
/// return [`Error::Backend`] ("previous process enumeration is still in progress") immediately
/// without spawning another thread.
///
/// # OS-specific behavior
/// - **Linux (PipeWire)**: Enumerates clients with registry `Stream/Output/Audio` nodes (PID
///   comes from the client's `pipewire.sec.pid`, using the same resolution path as per-process
///   capture). Display name comes from the node/client `application.name`. The executable name
///   is the basename of `/proc/<pid>/exe`, falling back to `/proc/<pid>/comm`. `is_output_active`
///   reflects whether the node is Running.
/// - **Windows (WASAPI)**: Enumerates audio sessions on all active render endpoints
///   (`IAudioSessionManager2` → `IAudioSessionEnumerator` → `IAudioSessionControl2`). System
///   audio and expired sessions are excluded. The display name is the process image name
///   without its extension; `is_output_active` reflects whether the session is Active.
///   Enumeration and capture require Windows build 20348 or later (Windows 11 / Windows
///   Server 2022); earlier builds return [`Error::UnsupportedOsVersion`].
/// - **macOS (Core Audio, 14.4+)**: Enumerates process objects from
///   `kAudioHardwarePropertyProcessObjectList` (processes known to Core Audio, including
///   input-only processes). Results include `bundle_id` and `is_output_active`
///   (`kAudioProcessPropertyIsRunningOutput`). Earlier versions return
///   [`Error::UnsupportedOsVersion`], as with per-process capture.
///
/// # Return values (also useful for capability detection)
/// - `Ok(non-empty list)`: Per-process capture is available and one or more processes have
///   audio-output sessions (streams). Stopped/Idle sessions are also listed. Check
///   [`ProcessInfo::is_output_active`] to see whether a process is playing now.
/// - `Ok(empty)`: Per-process capture is available, but no matching processes are present
///   (this does not mean that nothing is playing).
/// - `Err(_)`: Per-process capture is unavailable in this environment (Linux cannot reach
///   PipeWire = [`Error::Backend`]; macOS before 14.4 or Windows below build 20348 =
///   [`Error::UnsupportedOsVersion`]; other OSes = [`Error::Unsupported`]), permission is
///   denied ([`Error::PermissionDenied`]), or the OS query timed out / a previous query is
///   still running ([`Error::Backend`]).
///
/// # Example
/// ```no_run
/// use flexaudio::{open, processes, SourceKind, StreamConfig};
///
/// let candidates = processes()?;
/// if let Some(target) = candidates.first() {
///     let mut stream = open(StreamConfig {
///         kind: SourceKind::ProcessLoopback,
///         target_pid: Some(target.pid),
///         ..Default::default()
///     })?;
///     stream.start()?;
///     stream.stop();
/// }
/// # Ok::<(), flexaudio::Error>(())
/// ```
pub fn processes() -> Result<Vec<ProcessInfo>> {
    let raw = run_bounded(PROCESS_ENUM_TIMEOUT, list_raw_processes)?;
    Ok(normalize_process_list(raw, Some(std::process::id())))
}

/// Raw list from an OS-specific backend (may contain duplicates or empty names).
fn list_raw_processes() -> Result<Vec<ProcessInfo>> {
    #[cfg(target_os = "linux")]
    {
        flexaudio_os_linux::list_processes()
    }
    #[cfg(target_os = "windows")]
    {
        flexaudio_os_windows::list_processes()
    }
    #[cfg(target_os = "macos")]
    {
        flexaudio_os_macos::list_processes()
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        Err(Error::Unsupported)
    }
}

/// Single-flight slot for OS queries. Remains occupied until the worker exits, even after a
/// timeout. New calls immediately return [`Error::Backend`] without spawning another thread.
struct EnumFlight {
    busy: AtomicBool,
}

impl EnumFlight {
    const fn new() -> Self {
        Self {
            busy: AtomicBool::new(false),
        }
    }

    fn try_begin(&self) -> bool {
        self.busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    fn end(&self) {
        self.busy.store(false, Ordering::SeqCst);
    }

    #[cfg(test)]
    fn in_flight(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }
}

/// Holds the flight until the worker exits and always clears the marker, including on panic.
/// Drop before `send` so that the next `processes()` call can start as soon as the caller
/// receives the result instead of incorrectly seeing a stale "still in progress" state.
struct FlightGuard {
    flight: &'static EnumFlight,
}

impl Drop for FlightGuard {
    fn drop(&mut self) {
        self.flight.end();
    }
}

fn enum_flight() -> &'static EnumFlight {
    static FLIGHT: OnceLock<EnumFlight> = OnceLock::new();
    FLIGHT.get_or_init(EnumFlight::new)
}

/// Spawn count used by tests to verify that timeouts do not create more threads.
#[cfg(test)]
static ENUM_SPAWN_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Run `job` on a dedicated thread and return [`Error::Backend`] if no result arrives within `timeout`.
///
/// OS queries cannot be canceled by the caller. Only one can run at a time (single-flight).
/// A timed-out thread is detached, but the slot remains occupied until the worker exits. New
/// calls during that time do not wait; they immediately return [`Error::Backend`] with
/// "previous process enumeration is still in progress" (to avoid adding another blocked COM,
/// PipeWire, or coreaudiod query each time this hangs). Waiting would block the next caller for
/// the full timeout too, so failing immediately is fail-closed. If `job` panics, return
/// [`Error::Backend`] and always release the slot.
pub(crate) fn run_bounded<T, F>(timeout: Duration, job: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    let flight = enum_flight();
    if !flight.try_begin() {
        return Err(Error::Backend(
            "previous process enumeration is still in progress".into(),
        ));
    }

    // Capacity-one synchronous channel. The sender will not block if the receiver has timed
    // out and gone away; one value fits in the buffer without a receiver.
    let (tx, rx) = mpsc::sync_channel::<Result<T>>(1);
    let spawn = thread::Builder::new()
        .name("flexaudio-processes".into())
        .spawn(move || {
            let guard = FlightGuard { flight };
            let result = match catch_unwind(AssertUnwindSafe(job)) {
                Ok(r) => r,
                Err(_) => Err(Error::Backend("process enumeration thread panicked".into())),
            };
            // Clear the marker before sending the result so the next call can start as soon as
            // recv returns instead of still appearing busy.
            drop(guard);
            let _ = tx.send(result);
        });
    if let Err(e) = spawn {
        flight.end();
        return Err(Error::Backend(format!(
            "spawn process enumeration thread: {e}"
        )));
    }
    #[cfg(test)]
    ENUM_SPAWN_COUNT.fetch_add(1, Ordering::SeqCst);

    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => Err(Error::Backend(format!(
            "process enumeration timed out after {} ms",
            timeout.as_millis()
        ))),
        // The worker's FlightGuard has already cleared the marker. Calling end() here could
        // clear the marker for a different flight that started in the meantime.
        Err(RecvTimeoutError::Disconnected) => Err(Error::Backend(
            "process enumeration thread exited without a result".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Serialize unit tests so they do not contend for the same single-flight slot.
    static TEST_SERIAL: Mutex<()> = Mutex::new(());

    fn wait_until_idle() {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while enum_flight().in_flight() {
            if std::time::Instant::now() >= deadline {
                panic!("process enumeration flight did not become idle");
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn with_enum_lock<R>(f: impl FnOnce() -> R) -> R {
        let _guard = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        wait_until_idle();
        let result = f();
        wait_until_idle();
        result
    }

    /// A job that never completes until signaled. Even if the caller accidentally waits for
    /// completion, a 2-second watchdog and Drop release it so the test cannot hang forever.
    struct StuckJob {
        finished: Arc<AtomicBool>,
        release_tx: mpsc::Sender<()>,
    }

    impl StuckJob {
        fn park() -> (Self, impl FnOnce() -> Result<()> + Send + 'static) {
            let finished = Arc::new(AtomicBool::new(false));
            let (release_tx, release_rx) = mpsc::channel::<()>();
            let watchdog_tx = release_tx.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_secs(2));
                let _ = watchdog_tx.send(());
            });
            let finished_for_job = Arc::clone(&finished);
            let job = move || {
                let _ = release_rx.recv();
                finished_for_job.store(true, Ordering::SeqCst);
                Ok(())
            };
            (
                Self {
                    finished,
                    release_tx,
                },
                job,
            )
        }

        fn is_finished(&self) -> bool {
            self.finished.load(Ordering::SeqCst)
        }

        fn release(&self) {
            let _ = self.release_tx.send(());
        }
    }

    impl Drop for StuckJob {
        fn drop(&mut self) {
            let _ = self.release_tx.send(());
        }
    }

    #[test]
    fn run_bounded_returns_the_job_result() {
        with_enum_lock(|| {
            let got = run_bounded(Duration::from_secs(2), || Ok(7u32)).expect("fast job succeeds");
            assert_eq!(got, 7);
            let err = run_bounded(Duration::from_secs(2), || -> Result<u32> {
                Err(Error::UnsupportedOsVersion)
            });
            assert!(matches!(err, Err(Error::UnsupportedOsVersion)));
        });
    }

    #[test]
    fn run_bounded_second_call_succeeds_immediately_after_result() {
        with_enum_lock(|| {
            for i in 0..200u32 {
                let first = run_bounded(Duration::from_secs(2), move || Ok(i))
                    .unwrap_or_else(|e| panic!("iteration {i} first call: {e:?}"));
                assert_eq!(first, i);
                let second = run_bounded(Duration::from_secs(2), move || Ok(i + 1_000))
                    .unwrap_or_else(|e| {
                        panic!(
                            "iteration {i} second call must succeed right after the first result, got {e:?}"
                        )
                    });
                assert_eq!(second, i + 1_000);
            }
        });
    }

    #[test]
    fn run_bounded_times_out_instead_of_blocking() {
        with_enum_lock(|| {
            let (stuck, job) = StuckJob::park();
            let started = std::time::Instant::now();
            let err = run_bounded(Duration::from_millis(50), job);
            match err {
                Err(Error::Backend(msg)) => assert!(msg.contains("timed out"), "{msg}"),
                other => panic!("expected a timeout error, got {other:?}"),
            }
            assert!(
                !stuck.is_finished(),
                "the caller must not wait for the stuck job"
            );
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "timeout must return well before the watchdog releases the job"
            );
            stuck.release();
        });
    }

    #[test]
    fn run_bounded_maps_a_panicking_job_to_backend_error() {
        with_enum_lock(|| {
            let err = run_bounded(Duration::from_secs(2), || -> Result<()> {
                panic!("simulated backend panic");
            });
            match err {
                Err(Error::Backend(msg)) => assert!(msg.contains("panicked"), "{msg}"),
                other => panic!("expected a panic mapped to Backend, got {other:?}"),
            }
        });
    }

    #[test]
    fn run_bounded_does_not_spawn_another_thread_after_timeout() {
        with_enum_lock(|| {
            let (stuck, job) = StuckJob::park();
            let before = ENUM_SPAWN_COUNT.load(Ordering::SeqCst);
            let err = run_bounded(Duration::from_millis(40), job);
            match err {
                Err(Error::Backend(msg)) => assert!(msg.contains("timed out"), "{msg}"),
                other => panic!("expected a timeout error, got {other:?}"),
            }
            assert!(
                !stuck.is_finished(),
                "the timed-out job must still be in flight"
            );
            assert_eq!(ENUM_SPAWN_COUNT.load(Ordering::SeqCst), before + 1);
            for _ in 0..8 {
                let started = std::time::Instant::now();
                let err = run_bounded(Duration::from_millis(30), || Ok(()));
                match err {
                    Err(Error::Backend(msg)) => {
                        assert!(
                            msg.contains("still in progress"),
                            "expected in-progress error, got {msg}"
                        );
                    }
                    other => panic!("expected in-progress error, got {other:?}"),
                }
                assert!(
                    started.elapsed() < Duration::from_secs(1),
                    "in-progress callers must return well before the watchdog releases the job"
                );
            }
            assert_eq!(
                ENUM_SPAWN_COUNT.load(Ordering::SeqCst),
                before + 1,
                "timed-out callers must not spawn another OS-query thread"
            );
            stuck.release();
        });
    }

    /// Enumeration may return `Err` depending on the environment (for example, missing
    /// PipeWire), but must not panic. Any returned list must satisfy the contract: no current
    /// process, nonzero PIDs, nonempty display names, and no duplicate PIDs.
    #[test]
    fn processes_is_well_formed_on_this_host() {
        with_enum_lock(|| match processes() {
            Ok(list) => {
                let me = std::process::id();
                let mut seen = std::collections::HashSet::new();
                for p in &list {
                    assert_ne!(p.pid, 0);
                    assert_ne!(p.pid, me, "the calling process is excluded");
                    assert!(!p.name.trim().is_empty());
                    assert!(seen.insert(p.pid), "pid {} listed twice", p.pid);
                }
            }
            Err(error) => match error.root() {
                Error::Backend(_)
                | Error::Unsupported
                | Error::UnsupportedOsVersion
                | Error::PermissionDenied { .. } => {}
                other => panic!("unexpected error variant: {other:?}"),
            },
        });
    }
}
