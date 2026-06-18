//! エラーコードと thread-local の直近エラーメッセージ。
//!
//! 関数の戻り値（`i32`）でエラーの種別だけを返し、人間向けメッセージは呼び出し元の
//! スレッドごとに保持する。C 側は失敗を見たら [`flexaudio_last_error`] で文字列を取る。
//!
//! [`flexaudio_last_error`]: crate::flexaudio_last_error

use std::cell::RefCell;
use std::ffi::CString;
use std::os::raw::c_char;
use std::ptr;

/// FFI 関数の戻りコード。0 が成功、負がエラー。
///
/// `poll_*` だけは正の 1 を「取得あり」、0 を「なし」に使う（エラーは負のまま）。
/// 名前は C のヘッダで `FLEX_OK` 等になり、C 側の名前空間と衝突しないようにする。
pub mod code {
    /// 成功。
    pub const FLEX_OK: i32 = 0;
    /// 引数が無効（NULL ポインタ・不正な UTF-8・未知の列挙値など）。
    pub const FLEX_INVALID_ARG: i32 = -1;
    /// flexaudio の操作が失敗した（メッセージは last_error に入る）。
    pub const FLEX_FAILURE: i32 = -2;
    /// FFI 境界で panic を捕捉した（メッセージは last_error に入る）。
    pub const FLEX_PANIC: i32 = -3;
}

thread_local! {
    // 直近のエラーメッセージ。同一スレッドで次に last_error を更新する FFI 呼び出しまで
    // 有効。`flexaudio_last_error` が返すポインタはこの中身を指す。
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

/// 直近のエラーメッセージを現在のスレッドに記録する。
///
/// メッセージ中の NUL は CString が拒否するので、その場合は固定文言に差し替える
/// （メッセージを失っても last_error 自体は必ずセットする）。
pub fn set_last_error(msg: impl Into<String>) {
    let cstring = CString::new(msg.into())
        .unwrap_or_else(|_| CString::new("error message contained a NUL byte").unwrap());
    LAST_ERROR.with(|slot| *slot.borrow_mut() = Some(cstring));
}

/// 直近のエラーを消す（成功した操作の前後で呼び、古いメッセージを残さない）。
pub fn clear_last_error() {
    LAST_ERROR.with(|slot| *slot.borrow_mut() = None);
}

/// 現在のスレッドの直近エラーメッセージへのポインタを返す。
///
/// 返るポインタは thread-local の中身を指し、同一スレッドで次に last_error を更新する
/// 呼び出しまで有効。エラーが無ければ NULL。C 側で free してはならない。
pub fn last_error_ptr() -> *const c_char {
    LAST_ERROR.with(|slot| match &*slot.borrow() {
        Some(cstring) => cstring.as_ptr(),
        None => ptr::null(),
    })
}
