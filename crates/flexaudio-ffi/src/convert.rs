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

use crate::error::set_last_error;
use crate::types::{
    FlexChunk, FlexConfig, FlexDeviceInfo, FlexEvent, FlexEventKind, FlexProcessMode,
    FlexSourceKind,
};

// 番兵 0 を既定へ写すときの値（StreamConfig 既定と揃える）。
const DEFAULT_OUTPUT_RATE: u32 = 48_000;
const DEFAULT_OUTPUT_CHANNELS: u16 = 2;
const DEFAULT_CHUNK_MS: u32 = 20;

/// `FlexSourceKind` → [`SourceKind`]。
fn source_kind_from_c(kind: FlexSourceKind) -> SourceKind {
    match kind {
        FlexSourceKind::Mic => SourceKind::Mic,
        FlexSourceKind::System => SourceKind::SystemLoopback,
        FlexSourceKind::Process => SourceKind::ProcessLoopback,
    }
}

/// [`SourceKind`] → `FlexSourceKind`。
fn source_kind_to_c(kind: SourceKind) -> FlexSourceKind {
    match kind {
        SourceKind::Mic => FlexSourceKind::Mic,
        SourceKind::SystemLoopback => FlexSourceKind::System,
        SourceKind::ProcessLoopback => FlexSourceKind::Process,
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
/// UTF-8 として不正なら last_error をセットして `Err` を返す（呼び出し元は InvalidArg
/// として扱う）。安全のため、呼び出し側が有効な NUL 終端ポインタ（または NULL）を渡す
/// ことを前提にする。
///
/// # Safety
/// `ptr` は NULL か、有効な NUL 終端 C 文字列を指していなければならない。
unsafe fn opt_string_from_c(ptr: *const c_char) -> Result<Option<String>, ()> {
    if ptr.is_null() {
        return Ok(None);
    }
    match CStr::from_ptr(ptr).to_str() {
        Ok(s) => Ok(Some(s.to_string())),
        Err(_) => {
            set_last_error("device_id is not valid UTF-8");
            Err(())
        }
    }
}

/// `FlexConfig` から [`StreamConfig`] を組み立てる。napi の `build_config` と同じ方針で、
/// `ring_capacity_chunks` は公開せず既定値を使う。番兵 0 のフィールドは既定へ写す。
///
/// `device_id` が不正な UTF-8 なら last_error をセットして `Err`。
///
/// # Safety
/// `config` は有効な `FlexConfig` を指し、その `device_id` は NULL か有効な NUL 終端
/// C 文字列でなければならない。
pub unsafe fn build_config(config: &FlexConfig) -> Result<StreamConfig, ()> {
    let device_id = opt_string_from_c(config.device_id)?;

    let output = OutputFormat {
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
    };

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
/// （所有権は C 側へ渡り、`flexaudio_devices_free` が解放する）。
fn string_to_c(s: String) -> *mut c_char {
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

    // FlexConfig をテスト用に組み立てる（device_id は NULL = 既定）。
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
        let cfg = unsafe { build_config(&c) }.unwrap();
        assert_eq!(cfg.kind, SourceKind::ProcessLoopback);
        assert_eq!(cfg.target_pid, Some(4321));
        assert_eq!(cfg.mode, ProcessMode::Exclude);
        assert!(cfg.exclude_self);
        assert_eq!(cfg.output.sample_rate, 16_000);
        assert_eq!(cfg.output.channels, 1);
        assert_eq!(cfg.chunk_ms, 20);
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
    fn source_kind_roundtrips() {
        for (c, k) in [
            (FlexSourceKind::Mic, SourceKind::Mic),
            (FlexSourceKind::System, SourceKind::SystemLoopback),
            (FlexSourceKind::Process, SourceKind::ProcessLoopback),
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
}
