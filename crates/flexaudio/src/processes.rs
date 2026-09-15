//! 録れるプロセスの列挙（[`processes`]）。
//!
//! OS 別バックエンドの生リストを、上限時間つきの専用スレッドで取り、
//! [`normalize_process_list`] で全 OS 共通の形（重複統合・自プロセス除外・表示名補完・
//! 安定ソート）に揃える。列挙の本体は OS ごとに 1 か所ずつで、この関数が唯一の入口。

use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use flexaudio_core::process_list::normalize_process_list;
use flexaudio_core::types::{Error, ProcessInfo, Result};

/// 列挙全体の上限時間。OS の問い合わせ（PipeWire の往復・COM・coreaudiod への IPC）が
/// 応答しなくても、呼び出し側はこの時間で必ず戻る。Linux バックエンドは内部でさらに短い
/// 期限（2 秒）を持つので、通常はそちらが先に効く。
pub(crate) const PROCESS_ENUM_TIMEOUT: Duration = Duration::from_secs(3);

/// 今プロセス別キャプチャ（[`SourceKind::ProcessLoopback`](crate::SourceKind)）の対象に
/// できる、音声出力を持つプロセスを列挙する。呼び出し元プロセス自身は含めない。
///
/// 返った [`ProcessInfo::pid`] を [`StreamConfig::target_pid`](crate::StreamConfig) に
/// 渡せばそのプロセスを録れる。並びは「出力中が先頭 → 表示名 → PID」で、同じ PID は
/// 1 件にまとめてある。読み取り専用で、権限プロンプトを新たに出すことはない。
///
/// # OS ごとの挙動
/// - **Linux（PipeWire）**: レジストリの `Stream/Output/Audio` ノードを持つ Client を
///   列挙する（PID は Client の `pipewire.sec.pid`＝プロセス別キャプチャと同じ解決経路）。
///   表示名はノード／Client の `application.name`、実行ファイル名は `/proc/<pid>/exe`。
///   `is_output_active` はノードの状態が Running かどうか。
/// - **Windows（WASAPI）**: 有効な全 render エンドポイントの音声セッション
///   （`IAudioSessionManager2` → `IAudioSessionEnumerator` → `IAudioSessionControl2`）を
///   列挙する。システム音セッションと期限切れセッションは除く。表示名はプロセスの
///   イメージ名（拡張子なし）、`is_output_active` はセッションが Active かどうか。
///   列挙自体はどの Windows でも動くが、返った PID を録るプロセスループバックは
///   Windows 11（build 20348 以降）が必要で、それ未満では `start` が
///   [`Error::UnsupportedOsVersion`] になる（列挙は Ok のまま＝他 OS と非対称な点に注意）。
/// - **macOS（Core Audio, 14.4+）**: `kAudioHardwarePropertyProcessObjectList` の
///   プロセスオブジェクトを列挙する。`bundle_id` と `is_output_active`
///   （`kAudioProcessPropertyIsRunningOutput`）が付く。14.4 未満は
///   [`Error::UnsupportedOsVersion`]（プロセス別キャプチャと同じ条件）。
///
/// # 戻り値の意味（能力の判定にも使える）
/// - `Ok(空でないリスト)`: プロセス別キャプチャが使え、候補がある。
/// - `Ok(空)`: プロセス別キャプチャは使えるが、今は音声出力を持つプロセスが無い。
/// - `Err(_)`: この環境ではプロセス別キャプチャ自体が使えない見込み
///   （Linux: PipeWire に接続できない＝[`Error::Backend`] / macOS 14.4 未満＝
///   [`Error::UnsupportedOsVersion`] / 上記以外の OS＝[`Error::Unsupported`]）、または
///   OS が上限時間内に応答しなかった（[`Error::Backend`]）。
///
/// # 例
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

/// OS 別バックエンドの生リスト（重複・空名を含み得る）。
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

/// `job` を専用スレッドで走らせ、`timeout` 以内に結果が来なければ [`Error::Backend`] を返す。
///
/// OS の問い合わせは呼び出し側から中断できないので、期限切れのスレッドは切り離して
/// （detach して）そのまま終わらせる。そのスレッドが後で結果を送っても受け手は居ないので
/// 捨てられる。`job` が panic した場合も送信側が落ちるので [`Error::Backend`] になる。
pub(crate) fn run_bounded<T, F>(timeout: Duration, job: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    // 容量 1 の同期チャネル。受け手が期限切れで居なくなっても送信側は詰まらない
    // （容量ぶんは受け手無しでも積める）。
    let (tx, rx) = mpsc::sync_channel::<Result<T>>(1);
    thread::Builder::new()
        .name("flexaudio-processes".into())
        .spawn(move || {
            let _ = tx.send(job());
        })
        .map_err(|e| Error::Backend(format!("spawn process enumeration thread: {e}")))?;

    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => Err(Error::Backend(format!(
            "process enumeration timed out after {} ms",
            timeout.as_millis()
        ))),
        Err(RecvTimeoutError::Disconnected) => Err(Error::Backend(
            "process enumeration thread exited without a result".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_bounded_returns_the_job_result() {
        let got = run_bounded(Duration::from_secs(2), || Ok(7u32)).expect("fast job succeeds");
        assert_eq!(got, 7);
        let err = run_bounded(Duration::from_secs(2), || -> Result<u32> {
            Err(Error::UnsupportedOsVersion)
        });
        assert!(matches!(err, Err(Error::UnsupportedOsVersion)));
    }

    #[test]
    fn run_bounded_times_out_instead_of_blocking() {
        let started = std::time::Instant::now();
        let err = run_bounded(Duration::from_millis(50), || {
            thread::sleep(Duration::from_millis(500));
            Ok(())
        });
        match err {
            Err(Error::Backend(msg)) => assert!(msg.contains("timed out"), "{msg}"),
            other => panic!("expected a timeout error, got {other:?}"),
        }
        assert!(
            started.elapsed() < Duration::from_millis(400),
            "the caller must not wait for the stuck job"
        );
    }

    #[test]
    fn run_bounded_maps_a_panicking_job_to_backend_error() {
        let err = run_bounded(Duration::from_secs(2), || -> Result<()> {
            panic!("simulated backend panic");
        });
        assert!(matches!(err, Err(Error::Backend(_))));
    }

    /// 実 OS での列挙は環境次第（PipeWire 無し等）で `Err` もあり得るが、panic せず、
    /// 返ったリストは契約（自プロセス無し・pid 非 0・表示名非空・PID 重複無し）を満たす。
    #[test]
    fn processes_is_well_formed_on_this_host() {
        match processes() {
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
            Err(Error::Backend(_) | Error::Unsupported | Error::UnsupportedOsVersion) => {}
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }
}
