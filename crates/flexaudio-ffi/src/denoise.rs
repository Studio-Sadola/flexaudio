//! ノイズ抑制（RNNoise）アドオンの独立ハンドルと C ABI。
//!
//! [`FlexDenoiser`] は [`flexaudio_denoise::Denoiser`] を包む不透明ハンドルで、ストリームとは
//! 独立して使える（手元の interleaved f32 をインプレースで処理する）。ストリームに組み込んだ
//! denoise（`FlexConfig::denoise`）とは別経路。
//!
//! # 48kHz 前提
//! RNNoise は 48kHz・±1.0 正規化の interleaved f32 を前提にする（内部で 480 サンプル/ch の
//! 固定フレームに切って処理する）。他のサンプルレートを渡しても弾かないが、意図した抑制には
//! ならない。呼び出し側が 48kHz を渡すこと。出力は入力を 480 サンプル/ch 遅らせた列で、
//! 先頭のその分は無音になる（ストリーミング遅延）。

use std::slice;

use flexaudio_denoise::Denoiser;

use crate::error::{clear_last_error, code, set_last_error};
use crate::{guard_i32, guard_ptr};

/// ノイズ抑制の不透明ハンドル。中身は [`flexaudio_denoise::Denoiser`]。
/// `flexaudio_denoise_new` で作り `flexaudio_denoise_free` で解放する。
pub struct FlexDenoiser {
    pub(crate) inner: Denoiser,
}

/// チャンネル数（1 = mono / 2 = stereo interleaved）を指定して denoiser を構築する。
///
/// `channels` が 1..=2 以外なら NULL を返し last_error をセット。返ったハンドルは
/// `flexaudio_denoise_free` で解放する。48kHz 前提はモジュールの説明を参照。
#[no_mangle]
pub extern "C" fn flexaudio_denoise_new(channels: u16) -> *mut FlexDenoiser {
    guard_ptr(|| {
        clear_last_error();
        match Denoiser::new(channels) {
            Ok(inner) => Box::into_raw(Box::new(FlexDenoiser { inner })),
            Err(e) => {
                set_last_error(e.to_string());
                std::ptr::null_mut()
            }
        }
    })
}

/// interleaved f32（48kHz・±1.0 正規化）を **インプレース**でノイズ抑制する。
///
/// `len` はチャンネル数の倍数であること（倍数でなければ InvalidArg）。`len=0` は no-op。
/// 出力は入力を 480 サンプル/ch 遅らせた列で、ストリーム先頭のその分は無音になる。
/// 戻り 0 = 成功 / 負 = エラー。
///
/// # Safety
/// `d` は有効なハンドル、`samples` は `len` 要素の有効な可変配列（`len=0` なら NULL 可）で
/// なければならない。
#[no_mangle]
pub unsafe extern "C" fn flexaudio_denoise_process(
    d: *mut FlexDenoiser,
    samples: *mut f32,
    len: usize,
) -> i32 {
    guard_i32(|| {
        clear_last_error();
        let Some(dn) = d.as_mut() else {
            set_last_error("flexaudio_denoise_process: denoiser pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        if len == 0 {
            // 空は no-op（NULL でも安全）。
            return code::FLEX_OK;
        }
        if samples.is_null() {
            set_last_error("flexaudio_denoise_process: samples pointer is null");
            return code::FLEX_INVALID_ARG;
        }
        let buf = slice::from_raw_parts_mut(samples, len);
        match dn.inner.process(buf) {
            Ok(()) => code::FLEX_OK,
            Err(e) => {
                // 長さがチャンネル数の倍数でない等は引数の問題として扱う。
                set_last_error(e.to_string());
                code::FLEX_INVALID_ARG
            }
        }
    })
}

/// RNN 状態・持ち越しバッファ・遅延線を初期化する（生成直後と同じ状態に戻す）。
///
/// # Safety
/// `d` は有効なハンドルでなければならない（NULL は InvalidArg）。
#[no_mangle]
pub unsafe extern "C" fn flexaudio_denoise_reset(d: *mut FlexDenoiser) -> i32 {
    guard_i32(|| {
        clear_last_error();
        let Some(dn) = d.as_mut() else {
            set_last_error("flexaudio_denoise_reset: denoiser pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        dn.inner.reset();
        code::FLEX_OK
    })
}

/// denoiser ハンドルを解放する。NULL 安全。
///
/// # Safety
/// `d` は `flexaudio_denoise_new` が返したハンドル（または NULL）でなければならない。
/// 解放後の `d` を使ってはならない。
#[no_mangle]
pub unsafe extern "C" fn flexaudio_denoise_free(d: *mut FlexDenoiser) {
    guard_i32(|| {
        if !d.is_null() {
            drop(Box::from_raw(d));
        }
        code::FLEX_OK
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// new → process（無音）→ reset → free の一巡が panic しないスモーク。
    #[test]
    fn denoise_new_process_free_smoke() {
        let d = unsafe { &mut *flexaudio_denoise_new(1) };
        let dptr = d as *mut FlexDenoiser;
        let mut buf = vec![0.0f32; 960]; // 48k/20ms/mono 相当
        let rc = unsafe { flexaudio_denoise_process(dptr, buf.as_mut_ptr(), buf.len()) };
        assert_eq!(rc, code::FLEX_OK);
        // 先頭 480 サンプルは遅延の無音（0.0）。
        assert!(buf[..480].iter().all(|&x| x == 0.0));
        assert_eq!(unsafe { flexaudio_denoise_reset(dptr) }, code::FLEX_OK);
        unsafe { flexaudio_denoise_free(dptr) };
    }

    /// 不正なチャンネル数は NULL、NULL ハンドル/長さ不整合は InvalidArg。
    #[test]
    fn denoise_invalid_inputs() {
        // channels=3 は非対応 → NULL。
        assert!(flexaudio_denoise_new(3).is_null());

        // 長さが 2ch の倍数でない → InvalidArg。
        let d = flexaudio_denoise_new(2);
        let mut buf = vec![0.0f32; 3];
        let rc = unsafe { flexaudio_denoise_process(d, buf.as_mut_ptr(), buf.len()) };
        assert_eq!(rc, code::FLEX_INVALID_ARG);
        unsafe { flexaudio_denoise_free(d) };

        // NULL ハンドル。
        assert_eq!(
            unsafe { flexaudio_denoise_process(std::ptr::null_mut(), std::ptr::null_mut(), 0) },
            code::FLEX_INVALID_ARG
        );
        assert_eq!(
            unsafe { flexaudio_denoise_reset(std::ptr::null_mut()) },
            code::FLEX_INVALID_ARG
        );
        unsafe { flexaudio_denoise_free(std::ptr::null_mut()) };
    }
}
