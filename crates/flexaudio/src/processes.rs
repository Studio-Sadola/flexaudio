//! 録れるプロセスの列挙（[`processes`]）。
//!
//! OS 別バックエンドの生リストを、上限時間つきの専用スレッドで取り、
//! [`normalize_process_list`] で全 OS 共通の形（重複統合・自プロセス除外・表示名補完・
//! 安定ソート）に揃える。列挙の本体は OS ごとに 1 か所ずつで、この関数が唯一の入口。

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use flexaudio_core::process_list::normalize_process_list;
use flexaudio_core::types::{Error, ProcessInfo, Result};

/// 列挙全体の上限時間。OS の問い合わせ（PipeWire の往復・COM・coreaudiod への IPC）が
/// 応答しなくても、呼び出し側はこの時間で必ず戻る。Linux バックエンドは内部でさらに短い
/// 期限（2 秒）を持つので、通常はそちらが先に効く。
pub(crate) const PROCESS_ENUM_TIMEOUT: Duration = Duration::from_secs(3);

/// 今プロセス別キャプチャ（[`SourceKind::ProcessLoopback`](crate::SourceKind)）の対象に
/// できる、音声出力のセッション（ストリーム）を持つプロセスを列挙する。呼び出し元
/// プロセス自身は含めない。
///
/// 返った [`ProcessInfo::pid`] を [`StreamConfig::target_pid`](crate::StreamConfig) に
/// 渡せばそのプロセスを録れる。並びは「出力中が先頭 → 表示名 → PID」で、同じ PID は
/// 1 件にまとめてある。読み取り専用で、権限プロンプトを新たに出すことはない。
///
/// OS への問い合わせは同時に 1 本だけ（single-flight）。前の問い合わせがまだ終わって
/// いないときに呼ぶと、新しいスレッドは立てず [`Error::Backend`]（「まだ終わっていない」）
/// を即座に返す。
///
/// # OS ごとの挙動
/// - **Linux（PipeWire）**: レジストリの `Stream/Output/Audio` ノードを持つ Client を
///   列挙する（PID は Client の `pipewire.sec.pid`＝プロセス別キャプチャと同じ解決経路）。
///   表示名はノード／Client の `application.name`。実行ファイル名は `/proc/<pid>/exe`
///   のベース名で、読めなければ `/proc/<pid>/comm`。`is_output_active` はノードの状態が
///   Running かどうか。
/// - **Windows（WASAPI）**: 有効な全 render エンドポイントの音声セッション
///   （`IAudioSessionManager2` → `IAudioSessionEnumerator` → `IAudioSessionControl2`）を
///   列挙する。システム音セッションと期限切れセッションは除く。表示名はプロセスの
///   イメージ名（拡張子なし）、`is_output_active` はセッションが Active かどうか。
///   列挙も録音も Windows build 20348 or later (Windows 11 / Windows Server 2022) が
///   必要で、未満は [`Error::UnsupportedOsVersion`]。
/// - **macOS（Core Audio, 14.4+）**: `kAudioHardwarePropertyProcessObjectList` の
///   プロセスオブジェクトを列挙する（Core Audio が把握しているプロセス。入力だけの
///   プロセスも含む）。`bundle_id` と `is_output_active`
///   （`kAudioProcessPropertyIsRunningOutput`）が付く。14.4 未満は
///   [`Error::UnsupportedOsVersion`]（プロセス別キャプチャと同じ条件）。
///
/// # 戻り値の意味（能力の判定にも使える）
/// - `Ok(空でないリスト)`: プロセス別キャプチャが使え、音声出力のセッション（ストリーム）
///   を持つプロセスがある。停止中・Idle も載る。今鳴っているかは
///   [`ProcessInfo::is_output_active`] で見る。
/// - `Ok(空)`: プロセス別キャプチャは使えるが、そういうプロセスが今は無い
///   （「何も鳴っていない」ではない）。
/// - `Err(_)`: この環境ではプロセス別キャプチャができない（Linux: PipeWire に届かない
///   ＝[`Error::Backend`] / macOS 14.4 未満・Windows build 20348 未満＝
///   [`Error::UnsupportedOsVersion`] / 上記以外の OS＝[`Error::Unsupported`]）、
///   権限が無い（[`Error::PermissionDenied`]）、または OS が上限時間内に応答しなかった
///   ／前の問い合わせがまだ終わっていない（[`Error::Backend`]）。
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

/// OS 問い合わせの同時実行スロット。期限切れ後もワーカーが終わるまで占有し、
/// 新しい呼び出しはスレッドを足さず [`Error::Backend`] を即座に返す。
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

/// ワーカーが終わるまで flight を占有し、panic でも必ず印を下ろす。
/// `send` より前に drop して、呼び出し側が結果を受け取った直後の次の `processes()` が
/// 偽の「まだ終わっていない」にならないようにする。
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

/// テストが「時間切れのあとスレッドが増えない」ことを数えるための spawn 回数。
#[cfg(test)]
static ENUM_SPAWN_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// `job` を専用スレッドで走らせ、`timeout` 以内に結果が来なければ [`Error::Backend`] を返す。
///
/// OS の問い合わせは呼び出し側から中断できない。同時に走る問い合わせは 1 本だけ
/// （single-flight）。期限切れのスレッドは切り離して終わらせるが、スロットはワーカーが
/// 終わるまで占有する。そのあいだの新しい呼び出しは待たず、
/// 「前の問い合わせがまだ終わっていない」という [`Error::Backend`] を即座に返す
/// （ハングした COM / PipeWire / coreaudiod 待ちを 1 回につき 1 本足さないため。
/// 待たせると 2 人目も上限時間ぶんブロックするので、即エラーの方が fail-closed）。
/// `job` が panic した場合も [`Error::Backend`] になり、スロットは必ず空ける。
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

    // 容量 1 の同期チャネル。受け手が期限切れで居なくなっても送信側は詰まらない
    // （容量ぶんは受け手無しでも積める）。
    let (tx, rx) = mpsc::sync_channel::<Result<T>>(1);
    let spawn = thread::Builder::new()
        .name("flexaudio-processes".into())
        .spawn(move || {
            let guard = FlightGuard { flight };
            let result = match catch_unwind(AssertUnwindSafe(job)) {
                Ok(r) => r,
                Err(_) => Err(Error::Backend("process enumeration thread panicked".into())),
            };
            // 印を下ろしてから結果を送る。recv 側が戻った直後の次の呼び出しが
            // まだ busy に見えないようにする。
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
        // ワーカーの FlightGuard が既に印を下ろしている。ここで end() すると、
        // そのあいだに始まった別の flight の印を消してしまう。
        Err(RecvTimeoutError::Disconnected) => Err(Error::Backend(
            "process enumeration thread exited without a result".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 単体テスト同士が同じ single-flight スロットを奪い合わないように直列化する。
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
            let started = std::time::Instant::now();
            let err = run_bounded(Duration::from_millis(50), || {
                thread::sleep(Duration::from_millis(200));
                Ok(())
            });
            match err {
                Err(Error::Backend(msg)) => assert!(msg.contains("timed out"), "{msg}"),
                other => panic!("expected a timeout error, got {other:?}"),
            }
            assert!(
                started.elapsed() < Duration::from_millis(180),
                "the caller must not wait for the stuck job"
            );
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
            let before = ENUM_SPAWN_COUNT.load(Ordering::SeqCst);
            let err = run_bounded(Duration::from_millis(40), || {
                thread::sleep(Duration::from_millis(250));
                Ok(())
            });
            match err {
                Err(Error::Backend(msg)) => assert!(msg.contains("timed out"), "{msg}"),
                other => panic!("expected a timeout error, got {other:?}"),
            }
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
                    started.elapsed() < Duration::from_millis(20),
                    "in-progress callers must return immediately"
                );
            }
            assert_eq!(
                ENUM_SPAWN_COUNT.load(Ordering::SeqCst),
                before + 1,
                "timed-out callers must not spawn another OS-query thread"
            );
        });
    }

    /// 実 OS での列挙は環境次第（PipeWire 無し等）で `Err` もあり得るが、panic せず、
    /// 返ったリストは契約（自プロセス無し・pid 非 0・表示名非空・PID 重複無し）を満たす。
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
