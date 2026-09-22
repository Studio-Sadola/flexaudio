//! Windows で cpal のプロセス共有 WASAPI enumerator を生かし続ける。

use std::any::Any;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc;
use std::sync::OnceLock;
use std::thread;

use cpal::traits::HostTrait;

use flexaudio_core::types::{Error, Result};

/// cpal の `OnceLock<Enumerator>` を初期化済みにした長命 keeper の結果。
///
/// `OnceLock` の初期化クロージャが返るまで、他の呼出側は待つ。従ってこの値が `Ok`
/// なら、keeper が WASAPI enumerator を作った後であることが保証される。
static KEEPER_READY: OnceLock<std::result::Result<(), String>> = OnceLock::new();

/// cpal の WASAPI enumerator を keeper スレッド上で初期化済みにする。
///
/// cpal 0.16 は `IMMDeviceEnumerator` をプロセス共有の `OnceLock` に持つ一方、最初に
/// 作ったスレッドの STA 初期化を thread-local RAII で保持する。そのスレッドが終了すると
/// enumerator は解放済み COM apartment にひも付き、以後の利用が access violation になる。
/// この関数を cpal の全入口より先に呼び、最初の初期化スレッドをプロセス終了まで生かす。
pub(super) fn ensure() -> Result<()> {
    match KEEPER_READY.get_or_init(start_keeper) {
        Ok(()) => Ok(()),
        Err(message) => Err(Error::Backend(message.clone())),
    }
}

/// keeper を起動して WASAPI enumerator の初期化完了を待つ。
fn start_keeper() -> std::result::Result<(), String> {
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let handle = thread::Builder::new()
        .name("flexaudio-cpal-wasapi-keeper".into())
        .spawn(move || {
            // cpal は COM 初期化失敗を panic で表す。この境界で型付きエラーへ変換して
            // 呼出側へ返す。失敗時はスレッドを終了し、成功時だけ生存し続ける。
            let initialized = catch_unwind(AssertUnwindSafe(initialize_enumerator))
                .map_err(panic_message)
                .and_then(|result| result);
            let keep_alive = initialized.is_ok();
            let _ = ready_tx.send(initialized);

            if keep_alive {
                // cpal の COM guard は thread-local で、スレッド終了時にだけ
                // CoUninitialize を呼ぶ。keeper 自身は COM 呼出しを受け付けず、cpal が
                // process-wide に保持する enumerator の生成元を生かすだけなので、Windows
                // message queue を処理する必要はない。park は guard を生かしたまま、CPU を
                // 消費せずプロセス終了まで待機する。
                loop {
                    thread::park();
                }
            }
        })
        .map_err(|error| format!("spawn cpal WASAPI keeper thread: {error}"))?;

    // JoinHandle を drop して keeper を detach する。成功時の keeper は意図的に process
    // lifetime まで終了しないので、呼出側に join 責務を持たせない。
    drop(handle);

    ready_rx
        .recv()
        .map_err(|_| "cpal WASAPI keeper exited before initialization completed".to_owned())?
}

/// cpal の process-wide `ENUMERATOR` をこの長命スレッドで初期化する。
fn initialize_enumerator() -> std::result::Result<(), String> {
    let host = cpal::default_host();
    // Windows/WASAPI の `default_input_device()` は、入力端点が無い場合も先に
    // `get_enumerator()` を通る。よって戻り値が None でも keeper が `ENUMERATOR` を
    // 初期化済みであるという目的は満たす。
    let _ = host.default_input_device();
    Ok(())
}

/// `catch_unwind` の payload を診断可能なエラー文字列にする。
fn panic_message(payload: Box<dyn Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        format!("cpal WASAPI keeper initialization panicked: {message}")
    } else if let Some(message) = payload.downcast_ref::<String>() {
        format!("cpal WASAPI keeper initialization panicked: {message}")
    } else {
        "cpal WASAPI keeper initialization panicked with a non-string payload".to_owned()
    }
}
