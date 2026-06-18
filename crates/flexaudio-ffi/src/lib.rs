//! flexaudio-ffi — C ABI 公開層（cbindgen → flexaudio.h）。プル型 API。
//!
//! C アプリが flexaudio をインプロセスで使うための第三の経路（第一は CLI パイプ、
//! 第二は N-API addon）。napi のようにブリッジスレッド + コールバックは使わず、
//! 呼び出し側が `flexaudio_poll_chunk` / `flexaudio_poll_event` を周期的に呼んで
//! チャンク・イベントを取り出すプル型にする。
//!
//! 設計（config 構築・チャンク/イベント/デバイスの変換・エラー処理）は
//! `flexaudio-napi` を手本にする。違いはコールバックでなくポーリングである点だけ。
//!
//! 約束:
//! - 全関数は FFI 境界で panic を巻き上げない（`catch_unwind` で包み、panic 時は
//!   エラーコード / NULL / false を返す）。
//! - ポインタ引数は NULL をチェックする。
//! - 失敗時は `i32` を負にして thread-local の last_error にメッセージを入れ、
//!   `flexaudio_last_error` で取れるようにする。
//! - C へ渡す確保物（チャンクの `data`・デバイス文字列と配列）は対応する free 関数で
//!   必ず Rust 側が解放する（C の free は使わせない）。
//!
//! ヘッダ `include/flexaudio.h` は cbindgen で再生成する（`cbindgen.toml` を使用）。

mod convert;
mod error;
mod types;

use std::os::raw::c_char;
use std::panic::{catch_unwind, AssertUnwindSafe};

use error::{clear_last_error, code, last_error_ptr, set_last_error};
use types::{FlexChunk, FlexConfig, FlexDeviceInfo, FlexEvent, FlexStream};

// ---------------------------------------------------------------------------
// panic ガード
//
// FFI 境界で Rust の panic を巻き上げると未定義動作になる。各関数本体を
// catch_unwind で包み、panic を捕まえたら呼び出し側に値で返す。種類ごとに
// 「失敗を表す値」が違う（i32 は PANIC コード / ポインタは NULL / bool は false）。
// ---------------------------------------------------------------------------

/// `i32` を返す関数を panic ガードで包む。panic 時は last_error をセットして PANIC。
fn guard_i32(f: impl FnOnce() -> i32) -> i32 {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(_) => {
            set_last_error("panic caught at FFI boundary");
            code::FLEX_PANIC
        }
    }
}

/// ポインタを返す関数を panic ガードで包む。panic 時は last_error をセットして NULL。
fn guard_ptr<T>(f: impl FnOnce() -> *mut T) -> *mut T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(_) => {
            set_last_error("panic caught at FFI boundary");
            std::ptr::null_mut()
        }
    }
}

/// `bool` を返す関数を panic ガードで包む。panic 時は false。
fn guard_bool(f: impl FnOnce() -> bool) -> bool {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(false)
}

/// `flexaudio::Error` を last_error に記録して FAILURE コードを返す小ヘルパ。
fn fail(err: flexaudio::Error) -> i32 {
    set_last_error(err.to_string());
    code::FLEX_FAILURE
}

// ---------------------------------------------------------------------------
// ストリームのライフサイクル
// ---------------------------------------------------------------------------

/// 構成からストリームを開く（まだ start しない）。失敗で NULL を返し last_error をセット。
///
/// 返ったハンドルは `flexaudio_free` で解放する。
///
/// # Safety
/// `config` は有効な `FlexConfig` を指していなければならない（NULL は失敗扱い）。
#[no_mangle]
pub unsafe extern "C" fn flexaudio_open(config: *const FlexConfig) -> *mut FlexStream {
    guard_ptr(|| {
        clear_last_error();
        let Some(config) = config.as_ref() else {
            set_last_error("flexaudio_open: config pointer is null");
            return std::ptr::null_mut();
        };
        let stream_config = match convert::build_config(config) {
            Ok(c) => c,
            // build_config が last_error を既にセットしている。
            Err(()) => return std::ptr::null_mut(),
        };
        match flexaudio::open(stream_config) {
            Ok(inner) => Box::into_raw(Box::new(FlexStream { inner })),
            Err(e) => {
                set_last_error(e.to_string());
                std::ptr::null_mut()
            }
        }
    })
}

/// ストリームを停止してから解放する。NULL 安全。
///
/// # Safety
/// `s` は `flexaudio_open` が返したハンドル（または NULL）でなければならない。
/// 解放後の `s` を使ってはならない。
#[no_mangle]
pub unsafe extern "C" fn flexaudio_free(s: *mut FlexStream) {
    // 戻り値を捨てる i32 ガードに乗せて panic を吸収する。
    guard_i32(|| {
        if s.is_null() {
            return code::FLEX_OK;
        }
        let mut stream = Box::from_raw(s);
        stream.inner.stop();
        drop(stream);
        code::FLEX_OK
    });
}

/// キャプチャを開始する。
///
/// # Safety
/// `s` は有効なハンドルでなければならない（NULL は InvalidArg）。
#[no_mangle]
pub unsafe extern "C" fn flexaudio_start(s: *mut FlexStream) -> i32 {
    guard_i32(|| {
        clear_last_error();
        let Some(stream) = s.as_mut() else {
            set_last_error("flexaudio_start: stream pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        match stream.inner.start() {
            Ok(()) => code::FLEX_OK,
            Err(e) => fail(e),
        }
    })
}

/// キャプチャを停止する。
///
/// # Safety
/// `s` は有効なハンドルでなければならない（NULL は InvalidArg）。
#[no_mangle]
pub unsafe extern "C" fn flexaudio_stop(s: *mut FlexStream) -> i32 {
    guard_i32(|| {
        clear_last_error();
        let Some(stream) = s.as_mut() else {
            set_last_error("flexaudio_stop: stream pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        stream.inner.stop();
        code::FLEX_OK
    })
}

/// 配信を一時停止する（デバイスは動かしたまま）。
///
/// # Safety
/// `s` は有効なハンドルでなければならない（NULL は InvalidArg）。
#[no_mangle]
pub unsafe extern "C" fn flexaudio_pause(s: *mut FlexStream) -> i32 {
    guard_i32(|| {
        clear_last_error();
        let Some(stream) = s.as_mut() else {
            set_last_error("flexaudio_pause: stream pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        stream.inner.pause();
        code::FLEX_OK
    })
}

/// 一時停止を解除して配信を再開する。
///
/// # Safety
/// `s` は有効なハンドルでなければならない（NULL は InvalidArg）。
#[no_mangle]
pub unsafe extern "C" fn flexaudio_resume(s: *mut FlexStream) -> i32 {
    guard_i32(|| {
        clear_last_error();
        let Some(stream) = s.as_mut() else {
            set_last_error("flexaudio_resume: stream pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        stream.inner.resume();
        code::FLEX_OK
    })
}

/// 一時停止中なら true を返す。NULL や panic では false。
///
/// # Safety
/// `s` は有効なハンドル（または NULL）でなければならない。
#[no_mangle]
pub unsafe extern "C" fn flexaudio_is_paused(s: *const FlexStream) -> bool {
    guard_bool(|| match s.as_ref() {
        Some(stream) => stream.inner.is_paused(),
        None => false,
    })
}

// ---------------------------------------------------------------------------
// ポーリング（プル型 API の中心）
// ---------------------------------------------------------------------------

/// チャンクを 1 つ取り出して `out` を埋める。
///
/// 戻り 1 = 取得して `out` を埋めた / 0 = 今は無し / 負 = エラー。`out.data` は
/// flexaudio 所有で、使い終わったら `flexaudio_chunk_free` で解放する。
///
/// # Safety
/// `s` は有効なハンドル、`out` は有効な `FlexChunk` の書き込み先でなければならない。
#[no_mangle]
pub unsafe extern "C" fn flexaudio_poll_chunk(s: *mut FlexStream, out: *mut FlexChunk) -> i32 {
    guard_i32(|| {
        clear_last_error();
        let Some(stream) = s.as_mut() else {
            set_last_error("flexaudio_poll_chunk: stream pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        if out.is_null() {
            set_last_error("flexaudio_poll_chunk: out pointer is null");
            return code::FLEX_INVALID_ARG;
        }
        match stream.inner.poll_chunk() {
            Some(chunk) => {
                out.write(convert::chunk_to_c(chunk));
                1
            }
            None => 0,
        }
    })
}

/// `flexaudio_poll_chunk` が埋めた `data` を解放し、`data=NULL` / `len=0` にする。
/// NULL・二重解放とも安全。
///
/// # Safety
/// `chunk` は `flexaudio_poll_chunk` が埋めた `FlexChunk`（または NULL）を指して
/// いなければならない。
#[no_mangle]
pub unsafe extern "C" fn flexaudio_chunk_free(chunk: *mut FlexChunk) {
    guard_i32(|| {
        if let Some(chunk) = chunk.as_mut() {
            convert::free_chunk_data(chunk);
        }
        code::FLEX_OK
    });
}

/// イベントを 1 つ取り出して `out` を埋める。
///
/// 戻り 1 = 取得 / 0 = 今は無し / 負 = エラー。`Error` イベントのときは
/// `out.kind = Error` にし、メッセージを last_error に入れる。
///
/// # Safety
/// `s` は有効なハンドル、`out` は有効な `FlexEvent` の書き込み先でなければならない。
#[no_mangle]
pub unsafe extern "C" fn flexaudio_poll_event(s: *mut FlexStream, out: *mut FlexEvent) -> i32 {
    guard_i32(|| {
        clear_last_error();
        let Some(stream) = s.as_mut() else {
            set_last_error("flexaudio_poll_event: stream pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        if out.is_null() {
            set_last_error("flexaudio_poll_event: out pointer is null");
            return code::FLEX_INVALID_ARG;
        }
        match stream.inner.poll_event() {
            // event_to_c が Error/Unknown のメッセージを last_error に入れる。
            Some(ev) => {
                out.write(convert::event_to_c(ev));
                1
            }
            None => 0,
        }
    })
}

/// 録音を止めずに入力ソースをホットスワップする。
///
/// # Safety
/// `s` は有効なハンドル、`config` は有効な `FlexConfig` を指していなければならない。
#[no_mangle]
pub unsafe extern "C" fn flexaudio_switch_source(
    s: *mut FlexStream,
    config: *const FlexConfig,
) -> i32 {
    guard_i32(|| {
        clear_last_error();
        let Some(stream) = s.as_mut() else {
            set_last_error("flexaudio_switch_source: stream pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        let Some(config) = config.as_ref() else {
            set_last_error("flexaudio_switch_source: config pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        let stream_config = match convert::build_config(config) {
            Ok(c) => c,
            Err(()) => return code::FLEX_INVALID_ARG,
        };
        match stream.inner.switch_source(stream_config) {
            Ok(()) => code::FLEX_OK,
            Err(e) => fail(e),
        }
    })
}

// ---------------------------------------------------------------------------
// デバイス列挙
// ---------------------------------------------------------------------------

/// 利用可能なデバイスを列挙し、配列を確保して `out_array` / `out_count` にセットする。
///
/// 成功で 0。確保した配列は `flexaudio_devices_free` で解放する。ヘッドレス環境では
/// 0 件（`out_array=NULL` / `out_count=0`）でも成功扱い。
///
/// # Safety
/// `out_array` / `out_count` は有効な書き込み先でなければならない（NULL は InvalidArg）。
#[no_mangle]
pub unsafe extern "C" fn flexaudio_devices(
    out_array: *mut *mut FlexDeviceInfo,
    out_count: *mut usize,
) -> i32 {
    guard_i32(|| {
        clear_last_error();
        if out_array.is_null() || out_count.is_null() {
            set_last_error("flexaudio_devices: output pointer is null");
            return code::FLEX_INVALID_ARG;
        }
        let list = match flexaudio::devices() {
            Ok(list) => list,
            Err(e) => return fail(e),
        };
        if list.is_empty() {
            // 空でも成功。配列は確保しない。
            out_array.write(std::ptr::null_mut());
            out_count.write(0);
            return code::FLEX_OK;
        }
        // Box<[T]> へ集約すると確保サイズが要素数ぴったり（capacity == len）になり、
        // free 側の Vec::from_raw_parts(ptr, count, count) と整合する。
        let boxed: Box<[FlexDeviceInfo]> =
            list.into_iter().map(convert::device_info_to_c).collect();
        let count = boxed.len();
        let ptr = Box::into_raw(boxed) as *mut FlexDeviceInfo;
        out_array.write(ptr);
        out_count.write(count);
        code::FLEX_OK
    })
}

/// `flexaudio_devices` が確保した配列と各 `id`/`name` を解放する。NULL 安全。
///
/// # Safety
/// `arr`/`count` は `flexaudio_devices` が返したもの（または NULL/0）でなければならない。
#[no_mangle]
pub unsafe extern "C" fn flexaudio_devices_free(arr: *mut FlexDeviceInfo, count: usize) {
    guard_i32(|| {
        convert::free_device_array(arr, count);
        code::FLEX_OK
    });
}

// ---------------------------------------------------------------------------
// エラー取得
// ---------------------------------------------------------------------------

/// 現在のスレッドの直近エラーメッセージを返す。
///
/// 同一スレッドで次に last_error を更新する FFI 呼び出しまで有効。エラーが無ければ
/// NULL。返るポインタは flexaudio 所有で、C 側で free してはならない。
#[no_mangle]
pub extern "C" fn flexaudio_last_error() -> *const c_char {
    // last_error_ptr 自体は panic しないが、念のためガードして NULL を返す。
    match catch_unwind(last_error_ptr) {
        Ok(p) => p,
        Err(_) => std::ptr::null(),
    }
}
