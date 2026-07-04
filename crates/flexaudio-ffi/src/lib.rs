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
mod denoise;
mod error;
mod flac;
mod integration;
mod types;
mod vad;
mod watch;

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
pub(crate) fn guard_i32(f: impl FnOnce() -> i32) -> i32 {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(_) => {
            set_last_error("panic caught at FFI boundary");
            code::FLEX_PANIC
        }
    }
}

/// ポインタを返す関数を panic ガードで包む。panic 時は last_error をセットして NULL。
pub(crate) fn guard_ptr<T>(f: impl FnOnce() -> *mut T) -> *mut T {
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
/// `config.denoise` / `config.has_vad` が有効なら、対応するアドオン（ノイズ抑制 / VAD）を
/// ここで構築してストリームに同居させる（`poll_chunk` が denoise → VAD の順で通す）。
/// denoise 有効時は出力レートが 48000 でなければ失敗する（NULL + last_error。RNNoise は
/// 48kHz 固定）。返ったハンドルは `flexaudio_free` で解放する。
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
        // アドオン（denoise / VAD）を先に組み立てる。48k 制約違反やモデルロード失敗は
        // ここで弾く（build_addons が last_error をセット済み）。デバイスに触る前に返す。
        let (denoiser, vad) = match integration::build_addons(config) {
            Ok(pair) => pair,
            Err(()) => return std::ptr::null_mut(),
        };
        match flexaudio::open(stream_config) {
            Ok(inner) => Box::into_raw(Box::new(FlexStream {
                inner,
                denoiser,
                vad,
            })),
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

/// 入力ゲイン（線形倍率）を変更する。1.0 でそのまま、2.0 で約 +6dB、0.0 で無音。
/// 録音中いつでも呼べて、次のチャンクから効く（20ms 粒度）。乗算後のサンプルは
/// ±1.0 にクランプされる。有限かつ 0 以上でなければ FLEX_INVALID_ARG。
///
/// # Safety
/// `s` は有効なハンドルでなければならない（NULL は InvalidArg）。
#[no_mangle]
pub unsafe extern "C" fn flexaudio_set_gain(s: *mut FlexStream, gain: f32) -> i32 {
    guard_i32(|| {
        clear_last_error();
        let Some(stream) = s.as_mut() else {
            set_last_error("flexaudio_set_gain: stream pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        match stream.inner.set_gain(gain) {
            Ok(()) => code::FLEX_OK,
            Err(e) => {
                set_last_error(e.to_string());
                code::FLEX_INVALID_ARG
            }
        }
    })
}

/// 現在の入力ゲイン（線形倍率）を返す。NULL や panic では 1.0。
///
/// # Safety
/// `s` は有効なハンドル（または NULL）でなければならない。
#[no_mangle]
pub unsafe extern "C" fn flexaudio_gain(s: *const FlexStream) -> f32 {
    catch_unwind(AssertUnwindSafe(|| match s.as_ref() {
        Some(stream) => stream.inner.gain(),
        None => 1.0,
    }))
    .unwrap_or(1.0)
}

/// 現在の backend のネイティブフォーマット `(sample_rate, channels)` を `sr`/`ch` に書く。
///
/// open 時に backend から取得した値で、`flexaudio_switch_source` で更新される。表示・診断用
/// （出力フォーマットは `config` で指定した値）。戻り 0 = 成功 / 負 = エラー。
///
/// # Safety
/// `s` は有効なハンドル、`sr`/`ch` は有効な書き込み先でなければならない（NULL は InvalidArg）。
#[no_mangle]
pub unsafe extern "C" fn flexaudio_native_format(
    s: *const FlexStream,
    sr: *mut u32,
    ch: *mut u16,
) -> i32 {
    guard_i32(|| {
        clear_last_error();
        let Some(stream) = s.as_ref() else {
            set_last_error("flexaudio_native_format: stream pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        if sr.is_null() || ch.is_null() {
            set_last_error("flexaudio_native_format: output pointer is null");
            return code::FLEX_INVALID_ARG;
        }
        let (rate, channels) = stream.inner.native_format();
        sr.write(rate);
        ch.write(channels);
        code::FLEX_OK
    })
}

/// これまでにチャンクリングが DROP_OLDEST で捨てた累計チャンク数を返す。NULL や panic では 0。
///
/// # Safety
/// `s` は有効なハンドル（または NULL）でなければならない。
#[no_mangle]
pub unsafe extern "C" fn flexaudio_dropped_chunks(s: *const FlexStream) -> u64 {
    catch_unwind(AssertUnwindSafe(|| match s.as_ref() {
        Some(stream) => stream.inner.dropped_chunks(),
        None => 0,
    }))
    .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// ポーリング（プル型 API の中心）
// ---------------------------------------------------------------------------

/// チャンクを 1 つ取り出して `out` を埋める。
///
/// 戻り 1 = 取得して `out` を埋めた / 0 = 今は無し / 負 = エラー。`out.data` は
/// flexaudio 所有で、使い終わったら `flexaudio_chunk_free` で解放する。
///
/// アドオンが有効なら、返す前にチャンクを denoise → VAD の順で通す。VAD 有効時は
/// 確定したイベントが `out.vad_events`（要素数 `out.vad_events_len`）に入り、これも
/// `flexaudio_chunk_free` が `data` と一緒に解放する（無効時・イベント無しは NULL/0）。
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
        // アドオン（denoise → VAD）を通した結果を書き込む。無効なら素通し。
        match stream.poll_processed() {
            Some(chunk) => {
                out.write(chunk);
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

/// 録音を止めずに入力ソースをホットスワップする。`config.gain` は無視される
/// （ゲインはストリームの状態。変更は `flexaudio_set_gain`）。同様に `config.denoise` /
/// `config.has_vad` / `config.vad` も無視される（アドオンは open 時に確定したものを保つ。
/// 出力フォーマットは switch_source で変えられないので、48k 制約や VAD 設定は不変）。
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
