//! VAD（発話区間検出）アドオンの独立ハンドルと C ABI。
//!
//! [`FlexVad`] は [`flexaudio_vad::Vad`] を包む不透明ハンドルで、ストリームとは独立して
//! 使える（すでに手元にある任意フォーマットの f32 サンプルを流し込める）。ストリームに
//! 組み込んだ VAD（`FlexConfig::has_vad`）とは別経路。
//!
//! 流儀はクレート全体と同じ（guard で panic を吸収・NULL 検査・失敗は last_error）。
//! 出力イベント配列は `into_boxed_slice` で確保し、[`flexaudio_vad_events_free`] で解放する。

use std::slice;

use flexaudio_vad::Vad;

use crate::convert::{vad_config_from_c, vad_events_to_c};
use crate::error::{clear_last_error, code, set_last_error};
use crate::types::{FlexVadConfig, FlexVadEvent};
use crate::{guard_i32, guard_ptr};

/// VAD の不透明ハンドル。中身は [`flexaudio_vad::Vad`]（ONNX セッションを 1 つ持つ）。
/// `flexaudio_vad_new` で作り `flexaudio_vad_free` で解放する。
pub struct FlexVad {
    pub(crate) inner: Vad,
}

/// 設定から VAD を構築する。`config` が NULL なら既定設定（silero 準拠）。
///
/// 失敗（モデルのロード失敗・不正な sample_rate 等）で NULL を返し last_error をセット。
/// 返ったハンドルは `flexaudio_vad_free` で解放する。
///
/// # Safety
/// `config` は NULL か、有効な `FlexVadConfig` を指していなければならない。
#[no_mangle]
pub unsafe extern "C" fn flexaudio_vad_new(config: *const FlexVadConfig) -> *mut FlexVad {
    guard_ptr(|| {
        clear_last_error();
        // NULL は「全既定」。それ以外は番兵込みで VadConfig へ写す。
        let vad_config = match config.as_ref() {
            Some(c) => vad_config_from_c(c),
            None => Default::default(),
        };
        match Vad::new(vad_config) {
            Ok(inner) => Box::into_raw(Box::new(FlexVad { inner })),
            Err(e) => {
                set_last_error(e.to_string());
                std::ptr::null_mut()
            }
        }
    })
}

/// 任意フォーマット（`in_rate` / `in_ch` の interleaved f32）のサンプルを VAD に通し、
/// 確定したイベント配列を確保して `out` / `out_len` にセットする。
///
/// 内部で mono 化・VAD レートへのリサンプルをしてから処理する（[`flexaudio_vad::Vad::process_pcm`]）。
/// イベントが無ければ `out=NULL` / `out_len=0`。確保した配列は
/// `flexaudio_vad_events_free` で解放する。戻り 0 = 成功 / 負 = エラー。
///
/// # Safety
/// `v` は有効なハンドル、`samples` は `len` 要素の有効な配列（`len=0` なら NULL 可）、
/// `out` / `out_len` は有効な書き込み先でなければならない。
#[no_mangle]
pub unsafe extern "C" fn flexaudio_vad_process(
    v: *mut FlexVad,
    samples: *const f32,
    len: usize,
    in_rate: u32,
    in_ch: u16,
    out: *mut *mut FlexVadEvent,
    out_len: *mut usize,
) -> i32 {
    guard_i32(|| {
        clear_last_error();
        let Some(vad) = v.as_mut() else {
            set_last_error("flexaudio_vad_process: vad pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        if out.is_null() || out_len.is_null() {
            set_last_error("flexaudio_vad_process: output pointer is null");
            return code::FLEX_INVALID_ARG;
        }
        // len=0 は空スライスとして扱う（samples が NULL でも安全）。
        let input: &[f32] = if len == 0 {
            &[]
        } else if samples.is_null() {
            set_last_error("flexaudio_vad_process: samples pointer is null");
            return code::FLEX_INVALID_ARG;
        } else {
            slice::from_raw_parts(samples, len)
        };

        let events = vad.inner.process_pcm(input, in_rate, in_ch);
        let (ptr, ev_len) = vad_events_to_c(events);
        out.write(ptr);
        out_len.write(ev_len);
        code::FLEX_OK
    })
}

/// `flexaudio_vad_process` が確保したイベント配列を解放する。NULL / 0 は安全。
///
/// # Safety
/// `events`/`len` は `flexaudio_vad_process` が返したもの（または NULL/0）でなければならない。
#[no_mangle]
pub unsafe extern "C" fn flexaudio_vad_events_free(events: *mut FlexVadEvent, len: usize) {
    guard_i32(|| {
        crate::convert::free_vad_events(events, len);
        code::FLEX_OK
    });
}

/// VAD の状態（内部 state / context / 端数バッファ / リサンプラ）を初期化する。
///
/// # Safety
/// `v` は有効なハンドルでなければならない（NULL は InvalidArg）。
#[no_mangle]
pub unsafe extern "C" fn flexaudio_vad_reset(v: *mut FlexVad) -> i32 {
    guard_i32(|| {
        clear_last_error();
        let Some(vad) = v.as_mut() else {
            set_last_error("flexaudio_vad_reset: vad pointer is null");
            return code::FLEX_INVALID_ARG;
        };
        vad.inner.reset();
        code::FLEX_OK
    })
}

/// VAD ハンドルを解放する。NULL 安全。
///
/// # Safety
/// `v` は `flexaudio_vad_new` が返したハンドル（または NULL）でなければならない。
/// 解放後の `v` を使ってはならない。
#[no_mangle]
pub unsafe extern "C" fn flexaudio_vad_free(v: *mut FlexVad) {
    guard_i32(|| {
        if !v.is_null() {
            drop(Box::from_raw(v));
        }
        code::FLEX_OK
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FlexVadEvent;

    /// new → process（無音）→ reset → free の一巡が panic せず、イベント配列の確保・解放も
    /// 整合すること（into_boxed_slice の解放整合スモーク）。
    #[test]
    fn vad_new_process_free_smoke() {
        // NULL config = 全既定。
        let v = unsafe { flexaudio_vad_new(std::ptr::null()) };
        assert!(!v.is_null(), "vad_new failed: model load?");

        // 48k/stereo の無音を流す（既定では発話イベントは出ないので out=NULL 想定）。
        let samples = vec![0.0f32; 48_000 * 2];
        let mut out: *mut FlexVadEvent = std::ptr::null_mut();
        let mut out_len: usize = 123; // 上書きされること
        let rc = unsafe {
            flexaudio_vad_process(
                v,
                samples.as_ptr(),
                samples.len(),
                48_000,
                2,
                &mut out,
                &mut out_len,
            )
        };
        assert_eq!(rc, code::FLEX_OK);
        // 無音なのでイベントは無い（NULL/0）。
        assert!(out.is_null());
        assert_eq!(out_len, 0);
        unsafe { flexaudio_vad_events_free(out, out_len) };

        // 空入力でも安全。
        let rc2 = unsafe {
            flexaudio_vad_process(v, std::ptr::null(), 0, 48_000, 2, &mut out, &mut out_len)
        };
        assert_eq!(rc2, code::FLEX_OK);
        assert!(out.is_null());
        assert_eq!(out_len, 0);

        assert_eq!(unsafe { flexaudio_vad_reset(v) }, code::FLEX_OK);
        unsafe { flexaudio_vad_free(v) };
    }

    /// NULL ハンドル・NULL 出力先は InvalidArg（panic しない）。
    #[test]
    fn vad_null_args_are_invalid() {
        let mut out: *mut FlexVadEvent = std::ptr::null_mut();
        let mut out_len: usize = 0;
        let rc = unsafe {
            flexaudio_vad_process(
                std::ptr::null_mut(),
                std::ptr::null(),
                0,
                16_000,
                1,
                &mut out,
                &mut out_len,
            )
        };
        assert_eq!(rc, code::FLEX_INVALID_ARG);
        assert_eq!(
            unsafe { flexaudio_vad_reset(std::ptr::null_mut()) },
            code::FLEX_INVALID_ARG
        );
        // NULL free は安全。
        unsafe { flexaudio_vad_free(std::ptr::null_mut()) };
    }
}
