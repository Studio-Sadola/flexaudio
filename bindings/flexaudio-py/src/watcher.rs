//! Device hotplug watch [`DeviceWatcher`] and the entry point `watch_devices()`.
//!
//! Delivers OS device hotplug and default changes pull/poll-style. This is a separate channel
//! from the per-capture-stream [`StreamEvent`](crate::marshal::PyStreamEvent) and handles
//! per-device events. The napi version uses a bridge thread + callback, but Python is made
//! pull-style to match the other poll APIs (`poll_chunk` / `poll_event`) (the caller calls
//! `poll_event` periodically).

use pyo3::prelude::*;

use ::flexaudio as fa;

use crate::marshal::{device_event_to_py, PyDeviceEvent};
use crate::to_py_err;

/// Watcher that delivers device hotplug and default changes pull-style.
///
/// Created by [`watch_devices`]. Call [`poll_event`](DeviceWatcher::poll_event) periodically
/// to take out [`DeviceEvent`](PyDeviceEvent)s. `stop()` stops it (it also stops automatically
/// on drop). It also supports the context manager protocol (`with`).
///
/// The inner `flexaudio::DeviceWatcher` (`Box<dyn DeviceWatchBackend>`) is Send but !Sync, so
/// the pyclass default of Send+Sync cannot be met. Usage is assumed to be poll-style and
/// single-threaded, so the class is made unsendable and pinned to the thread that created it.
#[pyclass(module = "flexaudio", name = "DeviceWatcher", unsendable)]
pub struct DeviceWatcher {
    inner: fa::DeviceWatcher,
}

#[pymethods]
impl DeviceWatcher {
    /// Takes out the next hotplug event. `None` if there is none (non-blocking).
    fn poll_event(&mut self) -> Option<PyDeviceEvent> {
        self.inner.poll_event().map(device_event_to_py)
    }

    /// Stops watching (after this, `poll_event` returns `None`). Safe to call twice.
    fn stop(&mut self) {
        self.inner.stop();
    }

    /// Context manager support. Usable as `with flexaudio.watch_devices() as w:`.
    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    /// Stops when leaving the `with` block. Exceptions are not swallowed (returns False).
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

/// Starts watching device hotplug and default changes, and returns a [`DeviceWatcher`].
///
/// On Linux it persistently watches the PipeWire registry. When PipeWire is absent, and on
/// other OSes, it degrades to a watcher that always delivers `None` (hotplug events simply
/// never arrive; it neither panics nor raises).
#[pyfunction]
pub fn watch_devices() -> PyResult<DeviceWatcher> {
    let inner = fa::watch_devices().map_err(to_py_err)?;
    Ok(DeviceWatcher { inner })
}
