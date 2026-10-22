//! Bounded, single-flight PipeWire connection and server-sync ownership.

use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::{Duration, Instant};

use flexaudio_core::{Availability, Error, UnavailableReason};
use pipewire as pw;

use crate::processes::LIST_DEADLINE;

pub(crate) fn probe() -> Availability {
    static FLIGHT: OnceLock<Arc<AtomicBool>> = OnceLock::new();
    bounded(
        FLIGHT
            .get_or_init(|| Arc::new(AtomicBool::new(false)))
            .clone(),
        LIST_DEADLINE,
        connect_and_sync,
    )
}

struct FlightGuard(Arc<AtomicBool>);

impl Drop for FlightGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn unknown(message: &str) -> Availability {
    Availability::Unknown {
        error: Error::Backend(message.into()),
    }
}

fn timeout() -> Availability {
    unknown("PipeWire capability probe timed out; check the audio session and retry")
}

fn bounded(
    flight: Arc<AtomicBool>,
    bound: Duration,
    job: impl FnOnce() -> Availability + Send + 'static,
) -> Availability {
    if flight
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return unknown("previous PipeWire capability probe is still in progress");
    }
    let (tx, rx) = mpsc::sync_channel(1);
    let guard = FlightGuard(flight);
    let worker = std::thread::Builder::new()
        .name("flexaudio-capabilities".into())
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(job))
                .unwrap_or_else(|_| unknown("PipeWire capability probe worker panicked"));
            // Release the slot before publishing a completed result.
            drop(guard);
            #[allow(clippy::let_underscore_must_use, reason = "A timed-out capability caller no longer needs the completed probe notification.")]
            let _ = tx.send(result);
        });
    if let Err(error) = worker {
        return Availability::Unknown {
            error: flexaudio_core::spawn_error("PipeWire capability probe", error),
        };
    }
    match rx.recv_timeout(bound) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => timeout(),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            unknown("PipeWire capability probe worker exited without a result")
        }
    }
}

// Match module-protocol-native/local-socket.c in libpipewire: empty remotes
// select the default, but empty runtime variables are still used. USERPROFILE
// is the last environment fallback; HOME is not consulted. Named remotes also
// try the system runtime directory after the session directory.
fn socket_paths(
    get_env: impl Fn(&str) -> Option<OsString>,
    configured_remote: Option<&str>,
    system_runtime: &Path,
) -> flexaudio_core::Result<Option<Vec<PathBuf>>> {
    let remote = get_env("PIPEWIRE_REMOTE")
        .filter(|value| !value.is_empty())
        .or_else(|| {
            configured_remote
                .filter(|value| !value.is_empty())
                .map(OsString::from)
        })
        .unwrap_or_else(|| OsString::from("pipewire-0"));
    // Remote lists use SPA-JSON, and "internal" does not use a socket. Leave
    // these to libpipewire rather than treating their syntax as a missing path.
    if remote == "internal"
        || remote
            .as_encoded_bytes()
            .iter()
            .find(|byte| !byte.is_ascii_whitespace())
            == Some(&b'[')
    {
        return Ok(None);
    }
    let remote = Path::new(&remote);
    if remote.is_absolute() {
        return Ok(Some(vec![remote.to_path_buf()]));
    }
    let runtime = get_env("PIPEWIRE_RUNTIME_DIR")
        .or_else(|| get_env("XDG_RUNTIME_DIR"))
        .or_else(|| get_env("USERPROFILE"))
        .ok_or_else(|| {
            Error::Backend(
                "PipeWire capability runtime directory is unknown; set PIPEWIRE_RUNTIME_DIR, XDG_RUNTIME_DIR or USERPROFILE".into(),
            )
        })?;
    // Native code inserts '/' between runtime and name, even for an empty
    // runtime value; Path::join on an empty path alone would be relative.
    let runtime = if runtime.is_empty() {
        Path::new("/")
    } else {
        Path::new(&runtime)
    };
    Ok(Some(vec![
        runtime.join(remote),
        system_runtime.join(remote),
    ]))
}

fn socket_preflight(paths: &[PathBuf]) -> flexaudio_core::Result<Option<Availability>> {
    for path in paths {
        // Do not follow the final symlink: an existing but broken socket path
        // must still be connected and reported as unknown on failure.
        match std::fs::symlink_metadata(path) {
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return Err(Error::Backend(
                    "PipeWire capability socket inspection failed; check audio-session access and retry".into(),
                ));
            }
        }
    }
    Ok(Some(Availability::Unavailable {
        reason: UnavailableReason::PipeWireUnavailable,
    }))
}

fn connect_and_sync() -> Availability {
    match try_connect_and_sync() {
        Ok(availability) => availability,
        Err(error) => Availability::Unknown { error },
    }
}

fn try_connect_and_sync() -> flexaudio_core::Result<Availability> {
    let started = Instant::now();
    crate::pw_init_once();
    let main_loop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|_| Error::Backend("PipeWire capability main loop creation failed".into()))?;
    let context = pw::context::ContextRc::new(&main_loop, None)
        .map_err(|_| Error::Backend("PipeWire capability context creation failed".into()))?;
    if let Some(paths) = socket_paths(
        |name| std::env::var_os(name),
        context.properties().get("remote.name"),
        Path::new("/run/pipewire"),
    )? {
        if let Some(availability) = socket_preflight(&paths)? {
            return Ok(availability);
        }
    }
    let core = context.connect_rc(None).map_err(|_| {
        Error::Backend(
            "PipeWire capability connection failed; check audio-session access and retry".into(),
        )
    })?;
    if started.elapsed() >= LIST_DEADLINE {
        return Ok(timeout());
    }

    let pending = core
        .sync(0)
        .map_err(|_| Error::Backend("PipeWire capability server sync failed".into()))?;
    let done = Rc::new(Cell::new(false));
    let failure = Rc::new(RefCell::new(None));
    let done_for_callback = done.clone();
    let failure_for_callback = failure.clone();
    let _listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pw::core::PW_ID_CORE && seq.seq() == pending.seq() {
                done_for_callback.set(true);
            }
        })
        .error(move |id, _seq, code, message| {
            failure_for_callback
                .borrow_mut()
                .get_or_insert_with(|| crate::discovery::core_error(id, code, message));
        })
        .register();

    // No registry or stream objects are created. A matching server sync is
    // sufficient to distinguish a live server from an unanswered connection.
    while !done.get() && failure.borrow().is_none() {
        let remaining = LIST_DEADLINE.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Ok(timeout());
        }
        if main_loop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(remaining))
            < 0
        {
            return Ok(unknown("PipeWire capability loop iteration failed"));
        }
    }
    if let Some(error) = failure.take() {
        return Err(error);
    }
    Ok(Availability::Supported)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "capability-probe-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).expect("create temporary directory");
            Self(path)
        }

        fn paths(&self, vars: &[(&str, &std::ffi::OsStr)]) -> Vec<PathBuf> {
            socket_paths(
                |name| {
                    vars.iter()
                        .find(|(key, _)| *key == name)
                        .map(|(_, value)| (*value).to_os_string())
                },
                None,
                &self.0.join("system"),
            )
            .expect("resolve socket paths")
            .expect("single socket remote")
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).expect("remove temporary directory");
        }
    }

    #[test]
    fn socket_resolution_uses_native_runtime_precedence_and_default_remote() {
        let dir = TestDirectory::new();
        let runtime = dir.0.join("runtime");
        let xdg = dir.0.join("xdg");
        let profile = dir.0.join("profile");
        let vars = [
            ("PIPEWIRE_RUNTIME_DIR", runtime.as_os_str()),
            ("XDG_RUNTIME_DIR", xdg.as_os_str()),
            ("USERPROFILE", profile.as_os_str()),
        ];
        for (start, expected) in [(0, &runtime), (1, &xdg), (2, &profile)] {
            assert_eq!(
                dir.paths(&vars[start..]),
                [
                    expected.join("pipewire-0"),
                    dir.0.join("system").join("pipewire-0")
                ]
            );
        }
    }

    #[test]
    fn socket_resolution_accepts_named_and_absolute_remotes() {
        let dir = TestDirectory::new();
        let remote = dir.0.join("custom-socket");
        assert_eq!(
            dir.paths(&[("PIPEWIRE_REMOTE", remote.as_os_str())]),
            [remote]
        );
        for (name, expected) in [("custom-socket", "custom-socket"), ("", "pipewire-0")] {
            assert_eq!(
                dir.paths(&[
                    ("PIPEWIRE_REMOTE", name.as_ref()),
                    ("XDG_RUNTIME_DIR", dir.0.as_os_str()),
                ]),
                [dir.0.join(expected), dir.0.join("system").join(expected)]
            );
        }
    }

    #[test]
    fn socket_resolution_honors_configured_remote_unless_environment_overrides_it() {
        let dir = TestDirectory::new();
        let configured = dir.0.join("configured");
        let override_path = dir.0.join("override");
        for remote in [None, Some(""), Some(override_path.to_str().unwrap())] {
            let paths = socket_paths(
                |name| {
                    (name == "PIPEWIRE_REMOTE")
                        .then(|| remote.map(OsString::from))
                        .flatten()
                },
                configured.to_str(),
                &dir.0,
            )
            .unwrap()
            .unwrap();
            assert_eq!(
                paths,
                [if remote.is_some_and(|value| !value.is_empty()) {
                    override_path.clone()
                } else {
                    configured.clone()
                }]
            );
        }
    }

    #[test]
    fn socket_resolution_keeps_empty_runtime_and_non_unicode_paths() {
        use std::os::unix::ffi::OsStringExt;

        let dir = TestDirectory::new();
        assert_eq!(
            dir.paths(&[
                ("PIPEWIRE_RUNTIME_DIR", "".as_ref()),
                ("XDG_RUNTIME_DIR", dir.0.as_os_str()),
            ])[0],
            Path::new("/").join("pipewire-0")
        );
        let remote = OsString::from_vec(b"socket-\xff".to_vec());
        assert_eq!(
            dir.paths(&[
                ("PIPEWIRE_REMOTE", &remote),
                ("XDG_RUNTIME_DIR", dir.0.as_os_str()),
            ])[0],
            dir.0.join(remote)
        );
    }

    #[test]
    fn missing_runtime_is_unknown_even_when_home_is_set() {
        let dir = TestDirectory::new();
        let result = socket_paths(
            |name| (name == "HOME").then(|| dir.0.clone().into_os_string()),
            None,
            &dir.0,
        );
        assert!(
            matches!(result, Err(Error::Backend(message)) if message.contains("runtime directory is unknown"))
        );
    }

    #[test]
    fn socket_lists_and_internal_remotes_are_left_to_native_connection() {
        for remote in ["internal", " [ \"first\", \"second\" ]"] {
            assert_eq!(
                socket_paths(
                    |name| (name == "PIPEWIRE_REMOTE").then(|| remote.into()),
                    None,
                    Path::new("unused"),
                )
                .unwrap(),
                None
            );
        }
    }

    #[test]
    fn absent_socket_is_unavailable_but_existing_paths_require_connection() {
        let dir = TestDirectory::new();
        let paths = dir.paths(&[("XDG_RUNTIME_DIR", dir.0.as_os_str())]);
        assert_eq!(
            socket_preflight(&paths).unwrap(),
            Some(Availability::Unavailable {
                reason: UnavailableReason::PipeWireUnavailable,
            })
        );
        // A regular file cannot accept connections, but its presence alone
        // cannot establish absence of the session.
        std::fs::write(&paths[0], []).unwrap();
        assert_eq!(socket_preflight(&paths).unwrap(), None);
    }

    #[test]
    fn system_socket_fallback_and_broken_symlinks_are_not_absent() {
        let dir = TestDirectory::new();
        let paths = dir.paths(&[("XDG_RUNTIME_DIR", dir.0.as_os_str())]);
        std::fs::create_dir(dir.0.join("system")).unwrap();
        std::os::unix::fs::symlink(dir.0.join("missing-target"), &paths[1]).unwrap();
        assert_eq!(socket_preflight(&paths).unwrap(), None);
    }

    #[test]
    fn socket_inspection_errors_are_not_absence() {
        let dir = TestDirectory::new();
        let file = dir.0.join("not-a-directory");
        std::fs::write(&file, []).unwrap();
        assert!(matches!(
            socket_preflight(&[file.join("socket")]),
            Err(Error::Backend(_))
        ));
    }

    #[test]
    fn timeout_does_not_spawn_more_probes_and_slot_releases_on_completion() {
        let flight = Arc::new(AtomicBool::new(false));
        let (release_tx, release_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let guard_flight = flight.clone();
        let result = bounded(flight.clone(), Duration::from_millis(10), move || {
            release_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("release probe");
            Availability::Supported
        });
        assert_eq!(result, timeout());
        assert!(flight.load(Ordering::SeqCst));
        assert!(matches!(
            bounded(flight.clone(), Duration::from_secs(1), || panic!(
                "must not spawn"
            )),
            Availability::Unknown { .. }
        ));
        release_tx.send(()).unwrap();
        // Wait on a finite observer rather than leaving a polling thread behind.
        let observer = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(1);
            while guard_flight.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::yield_now();
            }
            finished_tx.send(()).unwrap();
        });
        finished_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        observer.join().unwrap();
        assert!(!flight.load(Ordering::SeqCst));
        assert_eq!(
            bounded(flight, Duration::from_secs(1), || Availability::Supported),
            Availability::Supported
        );
    }

    #[test]
    fn worker_panic_is_unknown_and_releases_slot() {
        let flight = Arc::new(AtomicBool::new(false));
        assert!(matches!(
            bounded(flight.clone(), Duration::from_secs(1), || panic!(
                "injected probe failure"
            )),
            Availability::Unknown { .. }
        ));
        assert!(!flight.load(Ordering::SeqCst));
    }
}
