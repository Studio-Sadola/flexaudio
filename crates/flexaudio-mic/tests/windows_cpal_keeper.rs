#![cfg(windows)]

//! Windows 固有の cpal WASAPI lifetime 回帰テスト。

use std::thread;

/// 短命な呼出側スレッドが終了した後も、別スレッドから cpal を利用できる。
///
/// この integration test binary には本テストしかないため、修正前は最初の worker が
/// `list_devices()` 内で cpal の process-wide enumerator を初期化して終了する。その後の
/// worker の最初の cpal 呼出しは access violation でプロセスを終了する。修正後は各呼出し
/// が keeper を先に通るので、両 worker は安全に終了する。音声端点が無い GitHub runner でも
/// `list_devices()` は空 Vec を返す契約なので、実マイクを必要としない。
#[test]
fn cpal_survives_after_calling_thread_exits() {
    let first = thread::spawn(flexaudio_mic::list_devices)
        .join()
        .expect("first cpal caller must not panic");
    first.expect("list_devices must return Ok even when no input endpoint exists");

    let second = thread::spawn(flexaudio_mic::list_devices)
        .join()
        .expect("second cpal caller must not panic after the first thread exits");
    second.expect("list_devices must return Ok even when no input endpoint exists");
}
