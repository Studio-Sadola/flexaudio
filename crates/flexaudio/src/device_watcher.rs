//! Facade for device-change monitoring (hot-plug notifications).
//!
//! [`DeviceWatcher`] reports OS device changes and default-device changes as [`DeviceEvent`]
//! values through a pull interface ([`poll_event`](DeviceWatcher::poll_event)). It handles
//! device-level events, separate from capture-stream-level [`Event`](crate::core::Event).
//!
//! OS backend differences are hidden behind the private `DeviceWatchBackend` trait:
//! - Linux: `PwDeviceWatcher` persistently monitors the PipeWire registry (`flexaudio-os-linux`).
//! - Other OSes: `NoopWatcher` always returns `None`.
//!
//! [`crate::watch_devices`] handles platform selection, boxes the appropriate
//! implementation, and returns a [`DeviceWatcher`].

use flexaudio_core::types::DeviceEvent;

/// Device-change monitoring interface implemented by OS backends (private to the facade).
///
/// Requires `Send` so [`DeviceWatcher`] can be transferred between threads. A `!Send`
/// implementation such as PipeWire must keep its state on a dedicated thread and expose
/// only a `Send` handle (as `PwDeviceWatcher` does).
trait DeviceWatchBackend: Send {
    /// Take the next hot-plug event, or return `None` without blocking.
    fn poll_event(&mut self) -> Option<DeviceEvent>;
    /// Stop monitoring; calling stop twice or before start must be safe.
    fn stop(&mut self);
}

/// Watcher that reports device and default-device changes through a pull interface.
///
/// Create with [`crate::watch_devices`]. Call [`poll_event`](Self::poll_event) periodically
/// to take [`DeviceEvent`] values. Monitoring stops automatically on drop.
///
/// ```no_run
/// let mut watcher = flexaudio::watch_devices()?;
/// while let Some(ev) = watcher.poll_event() {
///     println!("device event: {ev:?}");
/// }
/// # Ok::<(), flexaudio::core::Error>(())
/// ```
pub struct DeviceWatcher {
    /// OS-specific watcher (persistent PipeWire monitoring on Linux; Noop elsewhere).
    inner: Box<dyn DeviceWatchBackend>,
}

impl DeviceWatcher {
    /// Take the next hot-plug event, or return `None` without blocking.
    pub fn poll_event(&mut self) -> Option<DeviceEvent> {
        self.inner.poll_event()
    }

    /// Stop producers and retain pending events for [`poll_event`](Self::poll_event) to drain.
    /// Safe to call twice or before any events have been delivered. Also called on drop.
    pub fn stop(&mut self) {
        self.inner.stop();
    }
}

impl Drop for DeviceWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Intentional no-op on unsupported platforms.
#[cfg(any(test, not(target_os = "linux")))]
struct NoopWatcher;

#[cfg(any(test, not(target_os = "linux")))]
impl DeviceWatchBackend for NoopWatcher {
    fn poll_event(&mut self) -> Option<DeviceEvent> {
        None
    }
    fn stop(&mut self) {}
}

// Adapt persistent PipeWire monitoring on Linux to DeviceWatchBackend.
// This crate owns the trait, so it can implement it for a type from flexaudio-os-linux
// without violating the orphan rule. This bridges the types here while os-linux continues
// to depend only on core and remains unaware of the facade trait.
#[cfg(target_os = "linux")]
impl DeviceWatchBackend for flexaudio_os_linux::PwDeviceWatcher {
    fn poll_event(&mut self) -> Option<DeviceEvent> {
        flexaudio_os_linux::PwDeviceWatcher::poll_event(self)
    }
    fn stop(&mut self) {
        flexaudio_os_linux::PwDeviceWatcher::stop(self)
    }
}

/// Linux startup failures are errors; unsupported platforms intentionally use no-op.
pub(crate) fn watch_devices() -> flexaudio_core::types::Result<DeviceWatcher> {
    #[cfg(target_os = "linux")]
    {
        watcher_from_start(
            flexaudio_os_linux::PwDeviceWatcher::start()
                .map(|watcher| Box::new(watcher) as Box<dyn DeviceWatchBackend>),
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        watcher_from_start(Ok(Box::new(NoopWatcher)))
    }
}

fn watcher_from_start(
    start: flexaudio_core::Result<Box<dyn DeviceWatchBackend>>,
) -> flexaudio_core::Result<DeviceWatcher> {
    start.map(|inner| DeviceWatcher { inner })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`DeviceWatcher`] is `Send` and can be transferred between threads.
    #[test]
    fn watcher_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<DeviceWatcher>();
    }

    /// [`NoopWatcher`] always returns `None`, and stop is safe (does not panic).
    #[test]
    fn noop_watcher_yields_nothing() {
        let mut w = DeviceWatcher {
            inner: Box::new(NoopWatcher),
        };
        assert!(w.poll_event().is_none());
        assert!(w.poll_event().is_none());
        w.stop();
        w.stop();
        assert!(w.poll_event().is_none());
    }

    /// Missing PipeWire is a startup error under the selected 0.5 contract.
    #[test]
    fn watcher_start_failure_returns_error() {
        let error = flexaudio_core::Error::Backend("unavailable watcher".into());
        assert!(
            matches!(watcher_from_start(Err(error.clone())), Err(observed) if observed == error)
        );
    }
}

#[cfg(test)]
mod repro_tests {
    use super::*;
    #[test]
    #[cfg(target_os = "linux")]
    fn repro_p2_watcher_start_error() {
        // Environment is restricted to a child process, so concurrently running
        // tests and the user's PipeWire session are untouched. No daemon is started.
        if std::env::var_os("FLEXAUDIO_REPRO_WATCHER_CHILD").is_none() {
            let directory = std::env::temp_dir()
                .join(format!("flexaudio-repro-watcher-{}", std::process::id()));
            std::fs::create_dir(&directory).unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "device_watcher::repro_tests::repro_p2_watcher_start_error",
                    "--exact",
                    "--include-ignored",
                    "--nocapture",
                ])
                .env("FLEXAUDIO_REPRO_WATCHER_CHILD", "1")
                .env("XDG_RUNTIME_DIR", &directory)
                .env("PIPEWIRE_RUNTIME_DIR", &directory)
                .env("PIPEWIRE_REMOTE", "absent-repro-server")
                .output()
                .unwrap();
            std::fs::remove_dir(directory).unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success(), "{stdout}\n{stderr}");
            return;
        }
        let original = flexaudio_os_linux::PwDeviceWatcher::start()
            .err()
            .expect("fixture must fail native watcher startup");
        let result = watch_devices();
        assert!(
            result.is_err(),
            "F26: native startup error={original}; facade returned Ok(NoopWatcher)"
        );
    }
    #[test]
    fn repro_p2_watcher_start_error_control() {
        // Intentional no-op is benign on unsupported platforms, unlike failed
        // Linux startup. Exercise the same facade wrapper and pull interface.
        let mut watcher = DeviceWatcher {
            inner: Box::new(NoopWatcher),
        };
        assert!(watcher.poll_event().is_none());
        watcher.stop();
    }
}
