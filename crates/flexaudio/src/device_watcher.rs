//! デバイス着脱監視（ホットプラグ通知）の facade。
//!
//! [`DeviceWatcher`] は OS のデバイス着脱・既定変更を [`DeviceEvent`] として
//! pull 型（[`poll_event`](DeviceWatcher::poll_event)）で配信する。capture
//! stream 単位の [`Event`](crate::core::Event) とは別系統で、デバイス単位の事象を扱う。
//!
//! OS バックエンドの差異は private trait `DeviceWatchBackend` で吸収する:
//! - Linux: PipeWire レジストリを永続監視する `PwDeviceWatcher`（`flexaudio-os-linux`）。
//! - その他 OS / 縮退: 常に `None` を返す `NoopWatcher`。
//!
//! [`crate::watch_devices`] が cfg と縮退判断を行い、適切な実装を `Box` で包んで
//! [`DeviceWatcher`] を返す。

use flexaudio_core::types::DeviceEvent;

/// OS バックエンドが満たす着脱監視インターフェース（facade 内 private）。
///
/// [`DeviceWatcher`] をスレッド間で渡せるように `Send` を要求する。PipeWire のような
/// `!Send` 実装は内部で専用スレッドへ閉じ込め、本体は `Send` なハンドルだけ持つ
/// （`PwDeviceWatcher` がそうしている）。
trait DeviceWatchBackend: Send {
    /// 次のホットプラグイベントを 1 つ取り出す（無ければ `None`）。非ブロッキング。
    fn poll_event(&mut self) -> Option<DeviceEvent>;
    /// 監視を停止する（二重 stop / 未 start 後の stop に安全であること）。
    fn stop(&mut self);
}

/// デバイスの着脱・既定変更を pull 型で配信するウォッチャ。
///
/// [`crate::watch_devices`] で生成する。[`poll_event`](Self::poll_event) を周期的に
/// 呼んで [`DeviceEvent`] を取り出す。drop 時に自動で停止する。
///
/// ```no_run
/// let mut watcher = flexaudio::watch_devices()?;
/// while let Some(ev) = watcher.poll_event() {
///     println!("device event: {ev:?}");
/// }
/// # Ok::<(), flexaudio::core::Error>(())
/// ```
pub struct DeviceWatcher {
    /// OS 別の監視実装（Linux=PipeWire 永続監視 / それ以外=Noop）。
    inner: Box<dyn DeviceWatchBackend>,
}

impl DeviceWatcher {
    /// 次のホットプラグイベントを 1 つ取り出す（無ければ `None`）。非ブロッキング。
    pub fn poll_event(&mut self) -> Option<DeviceEvent> {
        self.inner.poll_event()
    }

    /// 監視を停止する（以後 [`poll_event`](Self::poll_event) は `None`）。
    /// 二重 stop / 未配信での stop に安全。drop でも自動的に呼ばれる。
    pub fn stop(&mut self) {
        self.inner.stop();
    }
}

impl Drop for DeviceWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 非 Linux / 縮退時に使う何もしないウォッチャ（常に `None`）。
///
/// PipeWire 不在で `PwDeviceWatcher::start()` が `Err` のときも、`watch_devices()`
/// はこれへ縮退して `Ok` を返す（着脱が来なければ何も配信しなくてよい。`devices()`
/// がデーモン不在を空リストに握るのと同じ扱い）。
struct NoopWatcher;

impl DeviceWatchBackend for NoopWatcher {
    fn poll_event(&mut self) -> Option<DeviceEvent> {
        None
    }
    fn stop(&mut self) {}
}

// Linux: PipeWire 永続監視を DeviceWatchBackend に適合させる。
// trait はこのクレート所有なので、型が flexaudio-os-linux 側でも孤児ルールに触れず
// impl できる。os-linux は core にしか依存しない（facade の trait を知らない）まま、
// ここで橋渡しする。
#[cfg(target_os = "linux")]
impl DeviceWatchBackend for flexaudio_os_linux::PwDeviceWatcher {
    fn poll_event(&mut self) -> Option<DeviceEvent> {
        flexaudio_os_linux::PwDeviceWatcher::poll_event(self)
    }
    fn stop(&mut self) {
        flexaudio_os_linux::PwDeviceWatcher::stop(self)
    }
}

/// OS のデバイス着脱監視を開始し、[`DeviceWatcher`] を返す。
///
/// - Linux: `PwDeviceWatcher::start()`（PipeWire 永続監視）を試み、成功すればそれを
///   包む。失敗（PipeWire 不在等）なら [`NoopWatcher`] へ縮退して `Ok` を返す
///   （`devices()` がデーモン不在を空に握るのと同じ扱い）。
/// - その他 OS: 常に [`NoopWatcher`]。
pub(crate) fn watch_devices() -> flexaudio_core::types::Result<DeviceWatcher> {
    #[cfg(target_os = "linux")]
    {
        let inner: Box<dyn DeviceWatchBackend> = match flexaudio_os_linux::PwDeviceWatcher::start()
        {
            Ok(w) => Box::new(w),
            // PipeWire 不在/接続失敗は no-op 縮退（着脱が来ないだけ）。
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

    /// [`DeviceWatcher`] が `Send` であること（スレッド間で渡せる）。
    #[test]
    fn watcher_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<DeviceWatcher>();
    }

    /// [`NoopWatcher`] は常に `None` を返し、stop は安全（panic しない）。
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

    /// `watch_devices()` は PipeWire 不在の環境でも panic せず
    /// `Ok(DeviceWatcher)` を返す（縮退して Noop になるだけ）。返ったウォッチャは
    /// 即 poll しても安全で、stop まで一巡できる。
    #[test]
    fn watch_devices_is_graceful_without_pipewire() {
        let mut w = watch_devices().expect("watch_devices は縮退して常に Ok を返す設計");
        // 縮退時は None、PipeWire ありでも初期スキャン抑制済みで即 None になり得る。
        let _ = w.poll_event();
        w.stop();
    }
}
