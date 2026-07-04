//! デバイス着脱監視 [`DeviceWatcher`] と入口 `watch_devices()`。
//!
//! OS のデバイス着脱・既定変更（ホットプラグ）を pull/poll 型で配信する。capture stream
//! 単位の [`StreamEvent`](crate::marshal::PyStreamEvent) とは別系統で、デバイス単位の事象を
//! 扱う。napi 版は bridge スレッド + コールバックだが、Python は他の poll 系（`poll_chunk` /
//! `poll_event`）と揃えて pull 型にする（利用側が `poll_event` を周期的に呼ぶ）。

use pyo3::prelude::*;

use ::flexaudio as fa;

use crate::marshal::{device_event_to_py, PyDeviceEvent};
use crate::to_py_err;

/// デバイス着脱・既定変更を pull 型で配信するウォッチャ。
///
/// [`watch_devices`] で生成する。[`poll_event`](DeviceWatcher::poll_event) を周期的に呼んで
/// [`DeviceEvent`](PyDeviceEvent) を取り出す。`stop()` で停止（drop でも自動停止）。
/// context manager（`with`）にも対応する。
///
/// 内部の `flexaudio::DeviceWatcher`（`Box<dyn DeviceWatchBackend>`）が Send だが !Sync な
/// ので、pyclass の Send+Sync 既定を満たせない。poll 型の単一スレッド利用が前提なので
/// unsendable にして生成スレッドに固定する。
#[pyclass(module = "flexaudio", name = "DeviceWatcher", unsendable)]
pub struct DeviceWatcher {
    inner: fa::DeviceWatcher,
}

#[pymethods]
impl DeviceWatcher {
    /// 次のホットプラグイベントを 1 つ取り出す。無ければ `None`（非ブロッキング）。
    fn poll_event(&mut self) -> Option<PyDeviceEvent> {
        self.inner.poll_event().map(device_event_to_py)
    }

    /// 監視を停止する（以後 `poll_event` は `None`）。二重呼び出し安全。
    fn stop(&mut self) {
        self.inner.stop();
    }

    /// context manager 対応。`with flexaudio.watch_devices() as w:` で使える。
    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    /// `with` ブロックを抜けるとき stop する。例外は握り潰さない（False を返す）。
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

/// デバイスの着脱・既定変更（ホットプラグ）の監視を開始し、[`DeviceWatcher`] を返す。
///
/// Linux は PipeWire レジストリを永続監視する。PipeWire 不在・その他 OS では縮退して常に
/// `None` を配信するウォッチャを返す（着脱が来ないだけで、panic も例外もしない）。
#[pyfunction]
pub fn watch_devices() -> PyResult<DeviceWatcher> {
    let inner = fa::watch_devices().map_err(to_py_err)?;
    Ok(DeviceWatcher { inner })
}
