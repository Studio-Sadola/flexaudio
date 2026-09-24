//! Enumeration of capturable processes ([`processes`]).
//!
//! Takes each OS backend's raw list on a dedicated thread with a time limit, and brings it
//! into a shape common to all OSes with [`normalize_process_list`] (duplicate merging, own
//! process exclusion, display-name completion, stable sort). There is exactly one enumeration
//! implementation per OS, and this function is the only entry point.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use flexaudio_core::process_list::normalize_process_list;
use flexaudio_core::types::{Error, ProcessInfo, Result};

/// Time limit for the whole enumeration. Even if the OS query (PipeWire round trip, COM, IPC
/// to coreaudiod) does not respond, the caller always returns within this time. The Linux
/// backend internally has an even shorter deadline (2 seconds), so that one normally kicks in
/// first.
pub(crate) const PROCESS_ENUM_TIMEOUT: Duration = Duration::from_secs(3);

/// Enumerates the processes that have an audio output session (stream) and can currently be
/// the target of per-process capture ([`SourceKind::ProcessLoopback`](crate::SourceKind)).
/// The calling process itself is not included.
///
/// Passing a returned [`ProcessInfo::pid`] to [`StreamConfig::target_pid`](crate::StreamConfig)
/// captures that process. The order is "outputting first -> display name -> PID", and entries
/// with the same PID are merged into one. It is read-only and never raises a new permission
/// prompt.
///
/// Only one OS query runs at a time (single-flight). Calling while the previous query has not
/// finished yet does not start a new thread and immediately returns [`Error::Backend`] ("still
/// not finished").
///
/// # Per-OS behavior
/// - **Linux (PipeWire)**: enumerates Clients that own a `Stream/Output/Audio` node in the
///   registry (the PID is the Client's `pipewire.sec.pid` = the same resolution path as
///   per-process capture). The display name is the node's / Client's `application.name`. The
///   executable name is the base name of `/proc/<pid>/exe`, or `/proc/<pid>/comm` if that is
///   unreadable. `is_output_active` is whether the node's state is Running.
/// - **Windows (WASAPI)**: enumerates the audio sessions of all active render endpoints
///   (`IAudioSessionManager2` -> `IAudioSessionEnumerator` -> `IAudioSessionControl2`).
///   System sound sessions and expired sessions are excluded. The display name is the
///   process's image name (without extension), and `is_output_active` is whether the session
///   is Active. Both enumeration and capture require Windows build 20348 or later (Windows 11 /
///   Windows Server 2022); older builds get [`Error::UnsupportedOsVersion`].
/// - **macOS (Core Audio, 14.4+)**: enumerates the process objects of
///   `kAudioHardwarePropertyProcessObjectList` (the processes Core Audio knows about;
///   input-only processes are included too). `bundle_id` and `is_output_active`
///   (`kAudioProcessPropertyIsRunningOutput`) are filled in. Below 14.4 it returns
///   [`Error::UnsupportedOsVersion`] (the same condition as per-process capture).
///
/// # Meaning of the return value (also usable as a capability check)
/// - `Ok(non-empty list)`: per-process capture is available and there are processes with an
///   audio output session (stream). Stopped and Idle ones are listed too. Whether one is
///   playing right now is seen via [`ProcessInfo::is_output_active`].
/// - `Ok(empty)`: per-process capture is available, but no such process exists right now
///   (this is not "nothing is playing").
/// - `Err(_)`: per-process capture is not possible in this environment (Linux: PipeWire is
///   unreachable = [`Error::Backend`] / macOS below 14.4, Windows below build 20348 =
///   [`Error::UnsupportedOsVersion`] / any other OS = [`Error::Unsupported`]), permission is
///   missing ([`Error::PermissionDenied`]), or the OS did not respond within the time limit /
///   the previous query has not finished yet ([`Error::Backend`]).
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

/// Raw list from the OS backend (may contain duplicates and empty names).
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

/// Concurrency slot for OS queries. It stays occupied until the worker finishes, even after
/// the deadline; new calls do not add a thread and immediately return [`Error::Backend`].
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

/// Occupies the flight until the worker finishes, and always lowers the mark even on panic.
/// Dropped before `send` so that the next `processes()` right after the caller receives the
/// result does not get a spurious "still not finished".
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

/// Spawn count used by tests to verify that "no threads are added after a timeout".
#[cfg(test)]
static ENUM_SPAWN_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Runs `job` on a dedicated thread and returns [`Error::Backend`] if no result arrives within
/// `timeout`.
///
/// An OS query cannot be interrupted from the caller side. Only one query runs at a time
/// (single-flight). A timed-out thread is detached and left to finish, but the slot stays
/// occupied until the worker finishes. New calls in the meantime do not wait and immediately
/// return an [`Error::Backend`] saying "the previous query is still in progress" (so that
/// each call does not add another thread waiting on a hung COM / PipeWire / coreaudiod.
/// Making it wait would block the second caller for the full time limit too, so an immediate
/// error is the fail-closed choice). If `job` panics it also becomes [`Error::Backend`], and
/// the slot is always released.
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

    // Sync channel with capacity 1. Even if the receiver is gone after the deadline, the
    // sender does not get stuck (up to the capacity can be queued without a receiver).
    let (tx, rx) = mpsc::sync_channel::<Result<T>>(1);
    let spawn = thread::Builder::new()
        .name("flexaudio-processes".into())
        .spawn(move || {
            let guard = FlightGuard { flight };
            let result = match catch_unwind(AssertUnwindSafe(job)) {
                Ok(r) => r,
                Err(_) => Err(Error::Backend("process enumeration thread panicked".into())),
            };
            // Lower the mark before sending the result, so that the next call right after
            // the recv side returns does not still see busy.
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
        // The worker's FlightGuard has already lowered the mark. Calling end() here would
        // erase the mark of another flight that started in the meantime.
        Err(RecvTimeoutError::Disconnected) => Err(Error::Backend(
            "process enumeration thread exited without a result".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Serializes the unit tests so they do not compete for the same single-flight slot.
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

    /// A job that does not finish until signaled. Even with a bug where the caller waits for
    /// completion, the 2-second watchdog and Drop release it, so the test never hangs forever.
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

    /// Enumeration on the real OS may return `Err` depending on the environment (no PipeWire,
    /// etc.), but it does not panic, and a returned list satisfies the contract (no own
    /// process, non-zero pid, non-empty display name, no duplicate PIDs).
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
            Err(
                Error::Backend(_)
                | Error::Unsupported
                | Error::UnsupportedOsVersion
                | Error::PermissionDenied,
            ) => {}
            Err(other) => panic!("unexpected error variant: {other:?}"),
        });
    }
}
