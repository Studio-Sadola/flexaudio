//! Facade for device-change monitoring (hot-plug notifications).
//!
//! [`DeviceWatcher`] reports OS device changes and default-device changes as [`DeviceEvent`]
//! values through a pull interface ([`poll_event`](DeviceWatcher::poll_event)). It handles
//! device-level events, separate from capture-stream-level [`Event`](crate::core::Event).
//!
//! OS backend differences are hidden behind the private `DeviceWatchBackend` trait:
//! - Linux: `PwDeviceWatcher` persistently monitors the PipeWire registry (`flexaudio-os-linux`).
//! - Other OSes / degraded mode: `NoopWatcher` always returns `None`.
//!
//! [`crate::watch_devices`] handles cfg and degraded-mode decisions, boxes the appropriate
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

    /// Stop monitoring; [`poll_event`](Self::poll_event) returns `None` afterward.
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

/// No-op watcher used on non-Linux systems or in degraded mode (always returns `None`).
///
/// If `PwDeviceWatcher::start()` returns `Err` because PipeWire is unavailable,
/// `watch_devices()` degrades to this watcher and returns `Ok`. Hotplug events will never arrive,
/// consistent with `devices()` returning an empty list while the daemon is down.
struct NoopWatcher;

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

/// Start OS device-change monitoring and return a [`DeviceWatcher`].
///
/// - Linux: Try `PwDeviceWatcher::start()` for persistent PipeWire monitoring and wrap it on
///   success. On failure (such as missing PipeWire), degrade to [`NoopWatcher`] and return
///   `Ok`, as `devices()` does for an unavailable daemon.
/// - Other OSes: Always use [`NoopWatcher`].
pub(crate) fn watch_devices() -> flexaudio_core::types::Result<DeviceWatcher> {
    #[cfg(target_os = "linux")]
    {
        let inner: Box<dyn DeviceWatchBackend> = match flexaudio_os_linux::PwDeviceWatcher::start()
        {
            Ok(w) => Box::new(w),
            // Missing PipeWire or connection failure degrades to no-op (no change events).
            Err(_) => Box::new(NoopWatcher),
        };
        Ok(DeviceWatcher { inner })
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(DeviceWatcher {
            inner: Box::new(NoopWatcher),
        })
    }
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

    /// `watch_devices()` returns `Ok(DeviceWatcher)` without panicking when PipeWire is absent
    /// (it simply degrades to Noop). The returned watcher is safe to poll immediately and can
    /// be stopped normally.
    #[test]
    fn watch_devices_is_graceful_without_pipewire() {
        let mut w = watch_devices().expect("watch_devices always returns Ok in degraded mode");
        // Returns None in degraded mode; with PipeWire, initial-scan events may also be suppressed.
        let _ = w.poll_event();
        w.stop();
    }
}
