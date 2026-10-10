//! Device hotplug monitoring with [`DeviceWatcher`] and the `watch_devices()` entry point.
//!
//! Delivers OS device hotplug and default-device changes through pull/poll. These are device-level
//! events, separate from capture-stream [`StreamEvent`](crate::marshal::PyStreamEvent). The napi
//! version uses a bridge thread and callbacks; Python uses pull to match its other polling APIs
//! (`poll_chunk` / `poll_event`), so callers periodically invoke `poll_event`.

use pyo3::prelude::*;

use ::flexaudio as fa;

use crate::marshal::{device_event_to_py, PyDeviceEvent};
use crate::to_py_err;

/// A watcher that delivers device hotplug and default-device changes through pull polling.
///
/// Create it with [`watch_devices`]. Call [`poll_event`](DeviceWatcher::poll_event) periodically to
/// retrieve [`DeviceEvent`](PyDeviceEvent). Call `stop()` to stop it (drop also stops it).
/// Supports the context manager protocol (`with`).
///
/// The internal `flexaudio::DeviceWatcher` (`Box<dyn DeviceWatchBackend>`) is Send but not Sync,
/// so it cannot meet pyclass's default Send+Sync requirements. Polling is single-threaded, so mark
/// it unsendable and bind it to the thread that created it.
#[pyclass(module = "flexaudio", name = "DeviceWatcher", unsendable)]
pub struct DeviceWatcher {
    inner: fa::DeviceWatcher,
}

#[pymethods]
impl DeviceWatcher {
    /// Retrieve the next hotplug event, or `None` if there is none (non-blocking).
    fn poll_event(&mut self) -> Option<PyDeviceEvent> {
        self.inner.poll_event().map(device_event_to_py)
    }

    /// Stop monitoring and retain queued events for draining. Safe to call more than once.
    fn stop(&mut self) {
        self.inner.stop();
    }

    /// Support the context manager protocol; use as `with flexaudio.watch_devices() as w:`.
    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    /// Stop when leaving the `with` block. Does not swallow exceptions (returns False).
    fn __exit__(
        &mut self,
        _exc_type: Option<Bound<'_, PyAny>>,
        _exc_value: Option<Bound<'_, PyAny>>,
        _traceback: Option<Bound<'_, PyAny>>,
    ) -> bool {
        self.inner.stop();
        false
    }
}

/// Start monitoring device hotplug and default-device changes, and return a [`DeviceWatcher`].
///
/// On Linux, monitor the PipeWire registry; startup failure raises a typed exception.
/// Other operating systems retain the intentional no-op watcher.
#[pyfunction]
pub fn watch_devices() -> PyResult<DeviceWatcher> {
    let inner = fa::watch_devices().map_err(to_py_err)?;
    Ok(DeviceWatcher { inner })
}
