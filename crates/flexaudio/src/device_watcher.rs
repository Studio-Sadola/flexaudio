//! Facade for device hotplug watching (hotplug notifications).
//!
//! [`DeviceWatcher`] delivers OS device hotplug and default-device changes as [`DeviceEvent`]s
//! in a pull model ([`poll_event`](DeviceWatcher::poll_event)). It is a separate channel from
//! the per-capture-stream [`Event`](crate::core::Event) and handles per-device occurrences.
//!
//! OS backend differences are absorbed by the private trait `DeviceWatchBackend`:
//! - Linux: `PwDeviceWatcher` (`flexaudio-os-linux`), which persistently watches the PipeWire
//!   registry.
//! - Other OSes / degraded: `NoopWatcher`, which always returns `None`.
//!
//! [`crate::watch_devices`] makes the cfg and degradation decisions, wraps the appropriate
//! implementation in a `Box`, and returns a [`DeviceWatcher`].

use flexaudio_core::types::DeviceEvent;

/// Hotplug watch interface that OS backends satisfy (private to the facade).
///
/// Requires `Send` so that a [`DeviceWatcher`] can be handed across threads. A `!Send`
/// implementation such as PipeWire confines itself to a dedicated thread internally, and the
/// main object only holds a `Send` handle (this is what `PwDeviceWatcher` does).
trait DeviceWatchBackend: Send {
    /// Takes the next hotplug event (`None` if there is none). Non-blocking.
    fn poll_event(&mut self) -> Option<DeviceEvent>;
    /// Stops watching (must be safe for a double stop / a stop without a prior start).
    fn stop(&mut self);
}

/// Watcher that delivers device hotplug and default-device changes in a pull model.
///
/// Created with [`crate::watch_devices`]. Call [`poll_event`](Self::poll_event) periodically
/// to take [`DeviceEvent`]s. Stops automatically on drop.
///
/// ```no_run
/// let mut watcher = flexaudio::watch_devices()?;
/// while let Some(ev) = watcher.poll_event() {
///     println!("device event: {ev:?}");
/// }
/// # Ok::<(), flexaudio::core::Error>(())
/// ```
pub struct DeviceWatcher {
    /// Per-OS watch implementation (Linux = persistent PipeWire watch / otherwise = Noop).
    inner: Box<dyn DeviceWatchBackend>,
}

impl DeviceWatcher {
    /// Takes the next hotplug event (`None` if there is none). Non-blocking.
    pub fn poll_event(&mut self) -> Option<DeviceEvent> {
        self.inner.poll_event()
    }

    /// Stops watching (afterwards [`poll_event`](Self::poll_event) returns `None`).
    /// Safe for a double stop / a stop with nothing delivered. Also called automatically on
    /// drop.
    pub fn stop(&mut self) {
        self.inner.stop();
    }
}

impl Drop for DeviceWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

/// No-op watcher used on non-Linux / when degraded (always `None`).
///
/// Even when `PwDeviceWatcher::start()` returns `Err` because PipeWire is absent,
/// `watch_devices()` degrades to this and returns `Ok` (if no hotplug arrives, nothing needs
/// to be delivered; the same treatment as `devices()` swallowing an absent daemon into an
/// empty list).
struct NoopWatcher;

impl DeviceWatchBackend for NoopWatcher {
    fn poll_event(&mut self) -> Option<DeviceEvent> {
        None
    }
    fn stop(&mut self) {}
}

// Linux: adapt the persistent PipeWire watch to DeviceWatchBackend.
// Because this crate owns the trait, it can be implemented without violating the orphan rule
// even though the type lives in flexaudio-os-linux. os-linux keeps depending only on core
// (it does not know the facade's trait), and the bridging happens here.
#[cfg(target_os = "linux")]
impl DeviceWatchBackend for flexaudio_os_linux::PwDeviceWatcher {
    fn poll_event(&mut self) -> Option<DeviceEvent> {
        flexaudio_os_linux::PwDeviceWatcher::poll_event(self)
    }
    fn stop(&mut self) {
        flexaudio_os_linux::PwDeviceWatcher::stop(self)
    }
}

/// Starts watching OS device hotplug and returns a [`DeviceWatcher`].
///
/// - Linux: tries `PwDeviceWatcher::start()` (persistent PipeWire watch) and wraps it on
///   success. On failure (PipeWire absent, etc.) degrades to [`NoopWatcher`] and returns `Ok`
///   (the same treatment as `devices()` swallowing an absent daemon into an empty list).
/// - Other OSes: always [`NoopWatcher`].
pub(crate) fn watch_devices() -> flexaudio_core::types::Result<DeviceWatcher> {
    #[cfg(target_os = "linux")]
    {
        let inner: Box<dyn DeviceWatchBackend> = match flexaudio_os_linux::PwDeviceWatcher::start()
        {
            Ok(w) => Box::new(w),
            // PipeWire absent / connection failure degrades to no-op (hotplug just never arrives).
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

    /// [`DeviceWatcher`] is `Send` (can be handed across threads).
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

    /// `watch_devices()` does not panic even in an environment without PipeWire and returns
    /// `Ok(DeviceWatcher)` (it just degrades to Noop). The returned watcher is safe to poll
    /// immediately and can go all the way through to stop.
    #[test]
    fn watch_devices_is_graceful_without_pipewire() {
        let mut w =
            watch_devices().expect("watch_devices is designed to degrade and always return Ok");
        // When degraded this is None; even with PipeWire the initial scan is suppressed, so it
        // may be None immediately.
        let _ = w.poll_event();
        w.stop();
    }
}
