//! C ABI 型と flexaudio 型の間の変換ヘルパ。
//!
//! `FlexConfig` → [`StreamConfig`]、[`AudioChunk`] → `FlexChunk`、[`Event`] →
//! `FlexEvent`、[`DeviceInfo`] → `FlexDeviceInfo` を小さな関数に分ける。napi の
//! `build_config` / `chunk_to_js` / `event_to_js` と同じ方針（番兵で既定を入れ、
//! ring_capacity_chunks は公開せず `StreamConfig::default()` の値を使う）。

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::ptr;
use std::slice;

use flexaudio::{
    AudioChunk, DeviceInfo, Event, OutputFormat, ProcessMode, SourceKind, StreamConfig,
};
use flexaudio_vad::{VadConfig, VadEvent};

use crate::error::set_last_error;
use crate::types::{
    FlexChunk, FlexConfig, FlexDeviceInfo, FlexEvent, FlexEventKind, FlexProcessMode,
    FlexSourceKind, FlexVadConfig, FlexVadEvent,
};

// 番兵 0 を既定へ写すときの値（StreamConfig 既定と揃える）。
pub(crate) const DEFAULT_OUTPUT_RATE: u32 = 48_000;
pub(crate) const DEFAULT_OUTPUT_CHANNELS: u16 = 2;
const DEFAULT_CHUNK_MS: u32 = 20;
const DEFAULT_GAIN: f32 = 1.0;

/// `FlexConfig` の出力フォーマットを番兵込みで解決する（0 → 既定）。
///
/// `build_config`（StreamConfig 構築）と `build_addons`（denoise の 48k 検証・
/// Denoiser のチャンネル数）が同じ解決結果を共有するための小ヘルパ。
pub(crate) fn resolve_output(config: &FlexConfig) -> OutputFormat {
    OutputFormat {
        sample_rate: if config.output_rate == 0 {
            DEFAULT_OUTPUT_RATE
        } else {
            config.output_rate
        },
        channels: if config.output_channels == 0 {
            DEFAULT_OUTPUT_CHANNELS
        } else {
            config.output_channels
        },
    }
}

/// `FlexSourceKind` → [`SourceKind`]。
fn source_kind_from_c(kind: FlexSourceKind) -> SourceKind {
    match kind {
        FlexSourceKind::Mic => SourceKind::Mic,
        FlexSourceKind::System => SourceKind::SystemLoopback,
        FlexSourceKind::Process => SourceKind::ProcessLoopback,
        FlexSourceKind::Mix => SourceKind::Mix,
    }
}

/// [`SourceKind`] → `FlexSourceKind`。
pub(crate) fn source_kind_to_c(kind: SourceKind) -> FlexSourceKind {
    match kind {
        SourceKind::Mic => FlexSourceKind::Mic,
        SourceKind::SystemLoopback => FlexSourceKind::System,
        SourceKind::ProcessLoopback => FlexSourceKind::Process,
        SourceKind::Mix => FlexSourceKind::Mix,
    }
}

/// `FlexProcessMode` → [`ProcessMode`]。
fn process_mode_from_c(mode: FlexProcessMode) -> ProcessMode {
    match mode {
        FlexProcessMode::Include => ProcessMode::Include,
        FlexProcessMode::Exclude => ProcessMode::Exclude,
    }
}

/// NUL 終端 C 文字列を `Option<String>` にする。NULL は `None`。
///
/// UTF-8 として不正なら last_error に `field`（フィールド名）入りのメッセージを
/// セットして `Err` を返す（呼び出し元は InvalidArg として扱う）。安全のため、
/// 呼び出し側が有効な NUL 終端ポインタ（または NULL）を渡すことを前提にする。
///
/// # Safety
/// `ptr` は NULL か、有効な NUL 終端 C 文字列を指していなければならない。
unsafe fn opt_string_from_c(ptr: *const c_char, field: &str) -> Result<Option<String>, ()> {
    if ptr.is_null() {
        return Ok(None);
    }
    match CStr::from_ptr(ptr).to_str() {
        Ok(s) => Ok(Some(s.to_string())),
        Err(_) => {
            set_last_error(format!("{field} is not valid UTF-8"));
            Err(())
        }
    }
}

/// 番兵 0.0 を既定 1.0 に写すゲイン変換（`gain` / `mix_*_gain` 共通の流儀）。
fn gain_or_default(gain: f32) -> f32 {
    if gain == 0.0 {
        DEFAULT_GAIN
    } else {
        gain
    }
}

/// `FlexConfig` から [`StreamConfig`] を組み立てる。napi の `build_config` と同じ方針で、
/// `ring_capacity_chunks` は公開せず既定値を使う。番兵 0 のフィールドは既定へ写す。
///
/// `device_id` / `mix_mic_device_id` / `mix_system_device_id` が不正な UTF-8 なら
/// last_error をセットして `Err`。
///
/// # Safety
/// `config` は有効な `FlexConfig` を指し、その文字列フィールドはいずれも NULL か
/// 有効な NUL 終端 C 文字列でなければならない。
pub unsafe fn build_config(config: &FlexConfig) -> Result<StreamConfig, ()> {
    let device_id = opt_string_from_c(config.device_id, "device_id")?;
    let mix_mic_device_id = opt_string_from_c(config.mix_mic_device_id, "mix_mic_device_id")?;
    let mix_system_device_id =
        opt_string_from_c(config.mix_system_device_id, "mix_system_device_id")?;

    let output = resolve_output(config);

    Ok(StreamConfig {
        kind: source_kind_from_c(config.kind),
        device_id,
        // process_id 0 は「なし」を表す番兵。
        target_pid: if config.process_id == 0 {
            None
        } else {
            Some(config.process_id)
        },
        // mode は process 専用 / exclude_self は system 専用。混ぜないのは facade 側が見る。
        mode: process_mode_from_c(config.mode),
        exclude_self: config.exclude_self,
        chunk_ms: if config.chunk_ms == 0 {
            DEFAULT_CHUNK_MS
        } else {
            config.chunk_ms
        },
        // gain 0.0 は番兵＝既定 1.0（output_rate 0→48000 と同じ流儀）。実行時に無音へ
        // したいときは flexaudio_set_gain(s, 0.0) を使う。mix の側別ゲインも同じ流儀
        // （0.0 番兵 → 1.0。合成前に側だけ無音にする用途は現状想定しない）。
        gain: gain_or_default(config.gain),
        mix_mic_device_id,
        mix_system_device_id,
        mix_mic_gain: gain_or_default(config.mix_mic_gain),
        mix_system_gain: gain_or_default(config.mix_system_gain),
        output,
        // ring_capacity_chunks は公開しない（StreamConfig 既定の値を使う）。
        ..Default::default()
    })
}

/// [`AudioChunk`] を `FlexChunk` に写す。
///
/// `data`（`Vec<f32>`）は `into_boxed_slice` → `Box::into_raw` で C へ所有権を渡す。
/// ポインタと `len` を slice 由来で必ず一致させ、`flexaudio_chunk_free` が同じ `len`
/// で `Box::from_raw` できるようにする。
pub fn chunk_to_c(chunk: AudioChunk) -> FlexChunk {
    let frames = chunk.frames as u32;
    let flags = chunk.flags.bits();
    let peak = chunk.peak;
    let rms = chunk.rms;
    let pts_ns = chunk.pts_ns;
    let seq = chunk.seq;
    let dropped_before = chunk.dropped_before;

    // Vec → boxed slice にして、ポインタと長さを取り出す。空でも null は返さず
    // （Box::into_raw は dangling 非 null を返す）、len=0 と整合する。
    let boxed: Box<[f32]> = chunk.data.into_boxed_slice();
    let len = boxed.len();
    let data = Box::into_raw(boxed) as *mut f32;

    FlexChunk {
        data,
        len,
        frames,
        pts_ns,
        seq,
        flags,
        dropped_before,
        peak,
        rms,
        // VAD イベントは呼び出し側（poll_processed）が有効時だけ後から差し込む。
        // 既定は「無し」。
        vad_events: ptr::null_mut(),
        vad_events_len: 0,
    }
}

/// `flexaudio_chunk_free` の本体。`data`/`len` から boxed slice を再構成して drop し、
/// 二重解放を防ぐためにフィールドをクリアする。
///
/// # Safety
/// `chunk` は有効な `FlexChunk` を指していなければならない。`data` は `chunk_to_c` が
/// 確保したもの（または NULL）。
pub unsafe fn free_chunk_data(chunk: &mut FlexChunk) {
    if !chunk.data.is_null() {
        // chunk_to_c が確保したのと同じ len で boxed slice を復元して drop する。
        let slice = slice::from_raw_parts_mut(chunk.data, chunk.len);
        drop(Box::from_raw(slice as *mut [f32]));
        chunk.data = ptr::null_mut();
        chunk.len = 0;
    }
    // VAD イベント配列も同じチャンクの所有物なので一緒に解放する。
    free_vad_events(chunk.vad_events, chunk.vad_events_len);
    chunk.vad_events = ptr::null_mut();
    chunk.vad_events_len = 0;
}

/// `FlexVadConfig` を [`VadConfig`] に写す（番兵 0 は既定へ）。
///
/// 既定値は [`VadConfig::default`] から採り、非 0 のフィールドだけ上書きする。
/// `max_speech_ms` は 0 がそのまま「無制限」を意味するので番兵扱いしない。
/// `neg_threshold` は 0 のとき `None`（silero 式で自動決定）にする。
pub fn vad_config_from_c(c: &FlexVadConfig) -> VadConfig {
    let d = VadConfig::default();
    VadConfig {
        threshold: if c.threshold == 0.0 {
            d.threshold
        } else {
            c.threshold
        },
        neg_threshold: if c.neg_threshold == 0.0 {
            d.neg_threshold
        } else {
            Some(c.neg_threshold)
        },
        min_speech_ms: if c.min_speech_ms == 0 {
            d.min_speech_ms
        } else {
            c.min_speech_ms
        },
        min_silence_ms: if c.min_silence_ms == 0 {
            d.min_silence_ms
        } else {
            c.min_silence_ms
        },
        speech_pad_ms: if c.speech_pad_ms == 0 {
            d.speech_pad_ms
        } else {
            c.speech_pad_ms
        },
        // 0 は「無制限」（既定）なのでそのまま通す。
        max_speech_ms: c.max_speech_ms,
        sample_rate: if c.sample_rate == 0 {
            d.sample_rate
        } else {
            c.sample_rate
        },
    }
}

/// [`VadEvent`] を `FlexVadEvent` に写す（開始 = 0 / 終了 = 1）。
pub fn vad_event_to_c(ev: VadEvent) -> FlexVadEvent {
    match ev {
        VadEvent::SpeechStart { at_sample } => FlexVadEvent {
            kind: 0,
            at_sample: at_sample as i64,
        },
        VadEvent::SpeechEnd { at_sample } => FlexVadEvent {
            kind: 1,
            at_sample: at_sample as i64,
        },
    }
}

/// [`VadEvent`] の列を C へ渡す配列（`*mut FlexVadEvent` + 要素数）にする。
///
/// 空のときは確保せず `(NULL, 0)` を返す（C 側は NULL か len==0 で「無し」と判る）。
/// 非空のときは `into_boxed_slice` で要素数ぴったりに確保し、`free_vad_events` が
/// 同じ要素数で復元・解放できるようにする（`shrink_to_fit` は使わない）。
pub fn vad_events_to_c(events: Vec<VadEvent>) -> (*mut FlexVadEvent, usize) {
    if events.is_empty() {
        return (ptr::null_mut(), 0);
    }
    let boxed: Box<[FlexVadEvent]> = events.into_iter().map(vad_event_to_c).collect();
    let len = boxed.len();
    let data = Box::into_raw(boxed) as *mut FlexVadEvent;
    (data, len)
}

/// `vad_events_to_c` が確保した配列を復元して drop する。NULL / 0 は no-op。
///
/// # Safety
/// `ptr`/`len` は `vad_events_to_c` が返したもの（または NULL/0）でなければならない。
pub unsafe fn free_vad_events(ptr: *mut FlexVadEvent, len: usize) {
    if ptr.is_null() {
        return;
    }
    // FlexVadEvent は Copy でヒープ所有物を持たないので、boxed slice を復元して
    // drop するだけでよい（要素ごとの後始末は不要）。
    let slice = slice::from_raw_parts_mut(ptr, len);
    drop(Box::from_raw(slice as *mut [FlexVadEvent]));
}

/// [`Event`] を `FlexEvent` に写す。`Error` のメッセージは last_error に入れる
/// （`FlexEvent` 自体は種別と count だけを運ぶ）。
pub fn event_to_c(ev: Event) -> FlexEvent {
    match ev {
        Event::ChunkDropped { count } => FlexEvent {
            kind: FlexEventKind::ChunkDropped,
            count: count as i64,
        },
        Event::StreamStalled => FlexEvent {
            kind: FlexEventKind::Stalled,
            count: 0,
        },
        Event::StreamRecovered => FlexEvent {
            kind: FlexEventKind::Recovered,
            count: 0,
        },
        Event::PermissionDenied => FlexEvent {
            kind: FlexEventKind::PermissionDenied,
            count: 0,
        },
        Event::DeviceLost => FlexEvent {
            kind: FlexEventKind::DeviceLost,
            count: 0,
        },
        Event::Error(msg) => {
            set_last_error(msg);
            FlexEvent {
                kind: FlexEventKind::Error,
                count: 0,
            }
        }
        // Event は #[non_exhaustive]。未知のバリアントは Unknown にし、デバッグ表現を
        // last_error に残して握り潰さない。
        other => {
            set_last_error(format!("unknown event: {other:?}"));
            FlexEvent {
                kind: FlexEventKind::Unknown,
                count: 0,
            }
        }
    }
}

/// `String` を C へ渡す `*mut c_char` にする。内部 NUL があれば空文字列に差し替える
/// （所有権は C 側へ渡り、対応する free 関数が解放する）。
pub(crate) fn string_to_c(s: String) -> *mut c_char {
    CString::new(s)
        .unwrap_or_else(|_| CString::new("").unwrap())
        .into_raw()
}

/// [`DeviceInfo`] を `FlexDeviceInfo` に写す。`id`/`name` は CString として C へ渡す。
pub fn device_info_to_c(info: DeviceInfo) -> FlexDeviceInfo {
    FlexDeviceInfo {
        id: string_to_c(info.id),
        name: string_to_c(info.name),
        source_kind: source_kind_to_c(info.source_kind),
        sample_rate: info.sample_rate,
        channels: info.channels,
        is_loopback: info.is_loopback,
        is_default: info.is_default,
    }
}

/// `flexaudio_devices_free` の本体。各 `id`/`name` の CString を復元して drop し、
/// 配列自体も `Vec` として復元して drop する。
///
/// # Safety
/// `arr`/`count` は `flexaudio_devices` が返したもの（または NULL/0）でなければならない。
pub unsafe fn free_device_array(arr: *mut FlexDeviceInfo, count: usize) {
    if arr.is_null() {
        return;
    }
    // into_boxed_slice で要素数ぴったりに確保したものを Vec として復元する
    // （確保サイズが count ぴったりなので capacity = count で健全）。
    let infos = Vec::from_raw_parts(arr, count, count);
    for info in &infos {
        if !info.id.is_null() {
            drop(CString::from_raw(info.id));
        }
        if !info.name.is_null() {
            drop(CString::from_raw(info.name));
        }
    }
    drop(infos);
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio::ChunkFlags;

    // 全フィールド 0 = すべて既定を意味する FlexVadConfig。
    fn zero_vad_config() -> FlexVadConfig {
        FlexVadConfig {
            threshold: 0.0,
            neg_threshold: 0.0,
            min_speech_ms: 0,
            min_silence_ms: 0,
            speech_pad_ms: 0,
            max_speech_ms: 0,
            sample_rate: 0,
        }
    }

    // FlexConfig をテスト用に組み立てる（文字列は NULL = 既定、数値は 0 番兵）。
    fn make_config(kind: FlexSourceKind) -> FlexConfig {
        FlexConfig {
            kind,
            device_id: ptr::null(),
            process_id: 0,
            mode: FlexProcessMode::Include,
            exclude_self: false,
            output_rate: 0,
            output_channels: 0,
            chunk_ms: 0,
            gain: 0.0,
            mix_mic_device_id: ptr::null(),
            mix_system_device_id: ptr::null(),
            mix_mic_gain: 0.0,
            mix_system_gain: 0.0,
            denoise: false,
            has_vad: false,
            vad: zero_vad_config(),
        }
    }

    #[test]
    fn build_config_applies_defaults_for_sentinels() {
        let c = make_config(FlexSourceKind::Mic);
        let cfg = unsafe { build_config(&c) }.unwrap();
        assert_eq!(cfg.kind, SourceKind::Mic);
        // 0 番兵は既定へ。
        assert_eq!(cfg.output.sample_rate, 48_000);
        assert_eq!(cfg.output.channels, 2);
        assert_eq!(cfg.chunk_ms, 20);
        assert_eq!(cfg.target_pid, None);
        assert_eq!(cfg.device_id, None);
        assert_eq!(cfg.mode, ProcessMode::Include);
        assert!(!cfg.exclude_self);
        // gain も 0 番兵 → 既定 1.0。
        assert_eq!(cfg.gain, 1.0);
        // mix 専用フィールドも番兵から既定へ（NULL → None / 0.0 → 1.0）。
        assert_eq!(cfg.mix_mic_device_id, None);
        assert_eq!(cfg.mix_system_device_id, None);
        assert_eq!(cfg.mix_mic_gain, 1.0);
        assert_eq!(cfg.mix_system_gain, 1.0);
        // 公開しない ring_capacity_chunks は StreamConfig 既定（50）。
        assert_eq!(cfg.ring_capacity_chunks, 50);
    }

    #[test]
    fn build_config_reflects_explicit_values() {
        let mut c = make_config(FlexSourceKind::Process);
        c.process_id = 4321;
        c.mode = FlexProcessMode::Exclude;
        c.exclude_self = true;
        c.output_rate = 16_000;
        c.output_channels = 1;
        c.chunk_ms = 20;
        c.gain = 2.5;
        let cfg = unsafe { build_config(&c) }.unwrap();
        assert_eq!(cfg.kind, SourceKind::ProcessLoopback);
        assert_eq!(cfg.target_pid, Some(4321));
        assert_eq!(cfg.mode, ProcessMode::Exclude);
        assert!(cfg.exclude_self);
        assert_eq!(cfg.output.sample_rate, 16_000);
        assert_eq!(cfg.output.channels, 1);
        assert_eq!(cfg.chunk_ms, 20);
        assert_eq!(cfg.gain, 2.5);
    }

    #[test]
    fn build_config_maps_gain_sentinel_and_explicit() {
        // 0.0 は番兵＝既定 1.0（output_rate 0→48000 と同じ流儀）。
        let c = make_config(FlexSourceKind::Mic);
        let cfg = unsafe { build_config(&c) }.unwrap();
        assert_eq!(cfg.gain, 1.0);
        // 明示値はそのまま通る。
        let mut c2 = make_config(FlexSourceKind::Mic);
        c2.gain = 0.5;
        let cfg2 = unsafe { build_config(&c2) }.unwrap();
        assert_eq!(cfg2.gain, 0.5);
    }

    #[test]
    fn build_config_reads_device_id() {
        let id = CString::new("dev-x").unwrap();
        let mut c = make_config(FlexSourceKind::Mic);
        c.device_id = id.as_ptr();
        let cfg = unsafe { build_config(&c) }.unwrap();
        assert_eq!(cfg.device_id.as_deref(), Some("dev-x"));
    }

    #[test]
    fn build_config_reflects_mix_fields() {
        let mic_id = CString::new("mic-a").unwrap();
        let sys_id = CString::new("sink-b").unwrap();
        let mut c = make_config(FlexSourceKind::Mix);
        c.mix_mic_device_id = mic_id.as_ptr();
        c.mix_system_device_id = sys_id.as_ptr();
        c.mix_mic_gain = 0.5;
        c.mix_system_gain = 2.0;
        let cfg = unsafe { build_config(&c) }.unwrap();
        assert_eq!(cfg.kind, SourceKind::Mix);
        assert_eq!(cfg.mix_mic_device_id.as_deref(), Some("mic-a"));
        assert_eq!(cfg.mix_system_device_id.as_deref(), Some("sink-b"));
        assert_eq!(cfg.mix_mic_gain, 0.5);
        assert_eq!(cfg.mix_system_gain, 2.0);
    }

    #[test]
    fn source_kind_roundtrips() {
        for (c, k) in [
            (FlexSourceKind::Mic, SourceKind::Mic),
            (FlexSourceKind::System, SourceKind::SystemLoopback),
            (FlexSourceKind::Process, SourceKind::ProcessLoopback),
            (FlexSourceKind::Mix, SourceKind::Mix),
        ] {
            assert_eq!(source_kind_from_c(c), k);
            assert_eq!(source_kind_to_c(k), c);
        }
    }

    #[test]
    fn chunk_to_c_keeps_ptr_and_len_consistent() {
        let chunk = AudioChunk {
            data: vec![0.1, -0.2, 0.3, -0.4],
            frames: 2,
            pts_ns: 123,
            seq: 9,
            flags: ChunkFlags::DISCONTINUITY,
            dropped_before: 1,
            peak: 0.4,
            rms: 0.25,
        };
        let mut fc = chunk_to_c(chunk);
        assert_eq!(fc.len, 4);
        assert_eq!(fc.frames, 2);
        assert_eq!(fc.flags, ChunkFlags::DISCONTINUITY.bits());
        assert_eq!(fc.dropped_before, 1);
        assert!(!fc.data.is_null());
        // ポインタと len が一致しているので読み戻せる。
        let view = unsafe { slice::from_raw_parts(fc.data, fc.len) };
        assert_eq!(view, &[0.1, -0.2, 0.3, -0.4]);
        // 解放後は NULL/0 になり二重解放安全。
        unsafe { free_chunk_data(&mut fc) };
        assert!(fc.data.is_null());
        assert_eq!(fc.len, 0);
        unsafe { free_chunk_data(&mut fc) };
    }

    #[test]
    fn event_to_c_maps_each_variant() {
        assert_eq!(
            event_to_c(Event::ChunkDropped { count: 5 }).kind,
            FlexEventKind::ChunkDropped
        );
        assert_eq!(event_to_c(Event::ChunkDropped { count: 5 }).count, 5);
        assert_eq!(
            event_to_c(Event::StreamStalled).kind,
            FlexEventKind::Stalled
        );
        assert_eq!(
            event_to_c(Event::StreamRecovered).kind,
            FlexEventKind::Recovered
        );
        assert_eq!(
            event_to_c(Event::PermissionDenied).kind,
            FlexEventKind::PermissionDenied
        );
        assert_eq!(
            event_to_c(Event::DeviceLost).kind,
            FlexEventKind::DeviceLost
        );
        let err = event_to_c(Event::Error("boom".to_string()));
        assert_eq!(err.kind, FlexEventKind::Error);
        assert_eq!(err.count, 0);
    }

    #[test]
    fn device_info_to_c_and_free_roundtrip() {
        let infos = vec![DeviceInfo {
            id: "id-1".to_string(),
            name: "Mic A".to_string(),
            source_kind: SourceKind::Mic,
            sample_rate: 48_000,
            channels: 2,
            is_loopback: false,
            is_default: true,
        }];
        // 本番の flexaudio_devices と同じく Box<[T]> へ集約して ptr/count を作る。
        let boxed: Box<[FlexDeviceInfo]> = infos.into_iter().map(device_info_to_c).collect();
        let count = boxed.len();
        let first = &boxed[0];
        assert_eq!(first.source_kind, FlexSourceKind::Mic);
        assert!(first.is_default);
        let id = unsafe { CStr::from_ptr(first.id) }.to_str().unwrap();
        assert_eq!(id, "id-1");
        // free が CString と配列を解放する（leak/二重解放しない）。
        let ptr = Box::into_raw(boxed) as *mut FlexDeviceInfo;
        unsafe { free_device_array(ptr, count) };
    }

    #[test]
    fn resolve_output_applies_sentinels() {
        // 0 番兵は 48000 / 2 に写り、明示値はそのまま通る。
        let c = make_config(FlexSourceKind::Mic);
        let out = resolve_output(&c);
        assert_eq!(out.sample_rate, 48_000);
        assert_eq!(out.channels, 2);

        let mut c2 = make_config(FlexSourceKind::Mic);
        c2.output_rate = 16_000;
        c2.output_channels = 1;
        let out2 = resolve_output(&c2);
        assert_eq!(out2.sample_rate, 16_000);
        assert_eq!(out2.channels, 1);
    }

    #[test]
    fn vad_config_all_zero_is_default() {
        // 全 0 の FlexVadConfig は VadConfig::default() と一致する。
        let cfg = vad_config_from_c(&zero_vad_config());
        assert_eq!(cfg, VadConfig::default());
    }

    #[test]
    fn vad_config_reflects_explicit_values() {
        let c = FlexVadConfig {
            threshold: 0.4,
            neg_threshold: 0.2,
            min_speech_ms: 300,
            min_silence_ms: 200,
            speech_pad_ms: 40,
            max_speech_ms: 5000,
            sample_rate: 8000,
        };
        let cfg = vad_config_from_c(&c);
        assert_eq!(cfg.threshold, 0.4);
        assert_eq!(cfg.neg_threshold, Some(0.2));
        assert_eq!(cfg.min_speech_ms, 300);
        assert_eq!(cfg.min_silence_ms, 200);
        assert_eq!(cfg.speech_pad_ms, 40);
        assert_eq!(cfg.max_speech_ms, 5000);
        assert_eq!(cfg.sample_rate, 8000);
    }

    #[test]
    fn vad_config_max_speech_zero_stays_unlimited() {
        // max_speech_ms は 0 が「無制限」なので番兵扱いしない（0 → 0）。
        let mut c = zero_vad_config();
        c.max_speech_ms = 0;
        assert_eq!(vad_config_from_c(&c).max_speech_ms, 0);
    }

    #[test]
    fn vad_event_maps_start_and_end() {
        assert_eq!(
            vad_event_to_c(VadEvent::SpeechStart { at_sample: 512 }),
            FlexVadEvent {
                kind: 0,
                at_sample: 512,
            }
        );
        assert_eq!(
            vad_event_to_c(VadEvent::SpeechEnd { at_sample: 1024 }),
            FlexVadEvent {
                kind: 1,
                at_sample: 1024,
            }
        );
    }

    #[test]
    fn vad_events_empty_is_null() {
        // 空イベント列は確保せず (NULL, 0)。free は no-op。
        let (ptr, len) = vad_events_to_c(Vec::new());
        assert!(ptr.is_null());
        assert_eq!(len, 0);
        unsafe { free_vad_events(ptr, len) };
    }

    #[test]
    fn vad_events_roundtrip_and_free() {
        // 手組みのイベント列を into_boxed_slice で確保 → 読み戻し → free まで一巡
        // （shrink_to_fit を使わず要素数ぴったりの確保・解放が整合することの検証）。
        let events = vec![
            VadEvent::SpeechStart { at_sample: 0 },
            VadEvent::SpeechEnd { at_sample: 512 },
            VadEvent::SpeechStart { at_sample: 2048 },
        ];
        let (ptr, len) = vad_events_to_c(events);
        assert_eq!(len, 3);
        assert!(!ptr.is_null());
        let view = unsafe { slice::from_raw_parts(ptr, len) };
        assert_eq!(view[0].kind, 0);
        assert_eq!(view[0].at_sample, 0);
        assert_eq!(view[1].kind, 1);
        assert_eq!(view[1].at_sample, 512);
        assert_eq!(view[2].kind, 0);
        assert_eq!(view[2].at_sample, 2048);
        unsafe { free_vad_events(ptr, len) };
    }

    #[test]
    fn free_chunk_data_also_frees_vad_events() {
        // chunk_to_c 由来の data と、後付けの vad_events を両方確保し、free_chunk_data が
        // どちらも解放して NULL/0 に戻すことを確認（二重解放も安全）。
        let chunk = AudioChunk {
            data: vec![0.0, 0.1, 0.2, 0.3],
            frames: 2,
            pts_ns: 0,
            seq: 0,
            flags: ChunkFlags::empty(),
            dropped_before: 0,
            peak: 0.3,
            rms: 0.1,
        };
        let mut fc = chunk_to_c(chunk);
        let (ptr, len) = vad_events_to_c(vec![VadEvent::SpeechStart { at_sample: 0 }]);
        fc.vad_events = ptr;
        fc.vad_events_len = len;

        unsafe { free_chunk_data(&mut fc) };
        assert!(fc.data.is_null());
        assert_eq!(fc.len, 0);
        assert!(fc.vad_events.is_null());
        assert_eq!(fc.vad_events_len, 0);
        // 二重解放しても安全。
        unsafe { free_chunk_data(&mut fc) };
    }
}
