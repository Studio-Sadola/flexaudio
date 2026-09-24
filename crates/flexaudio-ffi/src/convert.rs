//! Conversion helpers between C ABI types and flexaudio types.
//!
//! `FlexConfig` → [`StreamConfig`], [`AudioChunk`] → `FlexChunk`, [`Event`] →
//! `FlexEvent`, and [`DeviceInfo`] → `FlexDeviceInfo` are split into small functions. Same
//! policy as napi's `build_config` / `chunk_to_js` / `event_to_js` (sentinels fill in
//! defaults; ring_capacity_chunks is not exposed and the `StreamConfig::default()` value is
//! used).

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::ptr;
use std::slice;

use flexaudio::{
    AudioChunk, DeviceInfo, Event, OutputFormat, ProcessInfo, ProcessMode, SourceKind, StreamConfig,
};
use flexaudio_vad::{VadConfig, VadEvent};

use crate::error::set_last_error;
use crate::types::{
    FlexChunk, FlexConfig, FlexDeviceInfo, FlexEvent, FlexEventKind, FlexOutputActivity,
    FlexProcessInfo, FlexProcessMode, FlexSourceKind, FlexVadConfig, FlexVadEvent,
};

// Values used when mapping the sentinel 0 to the default (aligned with the StreamConfig
// defaults).
pub(crate) const DEFAULT_OUTPUT_RATE: u32 = 48_000;
pub(crate) const DEFAULT_OUTPUT_CHANNELS: u16 = 2;
const DEFAULT_CHUNK_MS: u32 = 20;
const DEFAULT_GAIN: f32 = 1.0;

/// Resolves the output format of a `FlexConfig`, including sentinels (0 → default).
///
/// Small helper so that `build_config` (StreamConfig construction) and `build_addons`
/// (denoise's 48k check and the Denoiser's channel count) share the same resolved result.
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

/// `FlexSourceKind` → [`SourceKind`].
fn source_kind_from_c(kind: FlexSourceKind) -> SourceKind {
    match kind {
        FlexSourceKind::Mic => SourceKind::Mic,
        FlexSourceKind::System => SourceKind::SystemLoopback,
        FlexSourceKind::Process => SourceKind::ProcessLoopback,
        FlexSourceKind::Mix => SourceKind::Mix,
    }
}

/// [`SourceKind`] → `FlexSourceKind`.
pub(crate) fn source_kind_to_c(kind: SourceKind) -> FlexSourceKind {
    match kind {
        SourceKind::Mic => FlexSourceKind::Mic,
        SourceKind::SystemLoopback => FlexSourceKind::System,
        SourceKind::ProcessLoopback => FlexSourceKind::Process,
        SourceKind::Mix => FlexSourceKind::Mix,
    }
}

/// `FlexProcessMode` → [`ProcessMode`].
fn process_mode_from_c(mode: FlexProcessMode) -> ProcessMode {
    match mode {
        FlexProcessMode::Include => ProcessMode::Include,
        FlexProcessMode::Exclude => ProcessMode::Exclude,
    }
}

/// Converts a NUL-terminated C string into an `Option<String>`. NULL becomes `None`.
///
/// If it is not valid UTF-8, sets last_error to a message containing `field` (the field name)
/// and returns `Err` (the caller treats it as InvalidArg). For safety, this assumes the caller
/// passes a valid NUL-terminated pointer (or NULL).
///
/// # Safety
/// `ptr` must be NULL or point to a valid NUL-terminated C string.
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

/// Gain conversion that maps the sentinel 0.0 to the default 1.0 (the convention shared by
/// `gain` / `mix_*_gain`).
fn gain_or_default(gain: f32) -> f32 {
    if gain == 0.0 {
        DEFAULT_GAIN
    } else {
        gain
    }
}

/// Builds a [`StreamConfig`] from a `FlexConfig`. Same policy as napi's `build_config`:
/// `ring_capacity_chunks` is not exposed and the default is used. Fields with the sentinel 0
/// are mapped to the default.
///
/// If `device_id` / `mix_mic_device_id` / `mix_system_device_id` is invalid UTF-8, sets
/// last_error and returns `Err`.
///
/// # Safety
/// `config` must point to a valid `FlexConfig`, and each of its string fields must be NULL or
/// a valid NUL-terminated C string.
pub unsafe fn build_config(config: &FlexConfig) -> Result<StreamConfig, ()> {
    let device_id = opt_string_from_c(config.device_id, "device_id")?;
    let mix_mic_device_id = opt_string_from_c(config.mix_mic_device_id, "mix_mic_device_id")?;
    let mix_system_device_id =
        opt_string_from_c(config.mix_system_device_id, "mix_system_device_id")?;

    let output = resolve_output(config);

    Ok(StreamConfig {
        kind: source_kind_from_c(config.kind),
        device_id,
        // process_id 0 is the sentinel for "none".
        target_pid: if config.process_id == 0 {
            None
        } else {
            Some(config.process_id)
        },
        // mode is process-only / exclude_self is system-only. The facade checks that they are
        // not mixed.
        mode: process_mode_from_c(config.mode),
        exclude_self: config.exclude_self,
        chunk_ms: if config.chunk_ms == 0 {
            DEFAULT_CHUNK_MS
        } else {
            config.chunk_ms
        },
        // gain 0.0 is the sentinel = default 1.0 (same convention as output_rate 0→48000). To
        // silence at runtime, use flexaudio_set_gain(s, 0.0). The per-side mix gains follow the
        // same convention (0.0 sentinel → 1.0; silencing only one side before mixing is not
        // currently an intended use).
        gain: gain_or_default(config.gain),
        mix_mic_device_id,
        mix_system_device_id,
        mix_mic_gain: gain_or_default(config.mix_mic_gain),
        mix_system_gain: gain_or_default(config.mix_system_gain),
        output,
        // ring_capacity_chunks is not exposed (the StreamConfig default value is used).
        ..Default::default()
    })
}

/// Maps an [`AudioChunk`] to a `FlexChunk`.
///
/// Ownership of `data` (`Vec<f32>`) is passed to C via `into_boxed_slice` → `Box::into_raw`.
/// The pointer and `len` always come from the same slice so they match, and
/// `flexaudio_chunk_free` can `Box::from_raw` with the same `len`.
pub fn chunk_to_c(chunk: AudioChunk) -> FlexChunk {
    let frames = chunk.frames as u32;
    let flags = chunk.flags.bits();
    let peak = chunk.peak;
    let rms = chunk.rms;
    let pts_ns = chunk.pts_ns;
    let seq = chunk.seq;
    let dropped_before = chunk.dropped_before;

    // Turn the Vec into a boxed slice and take out the pointer and length. Even when empty it
    // does not return null (Box::into_raw returns a dangling non-null pointer), consistent
    // with len=0.
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
        // VAD events are inserted later by the caller (poll_processed), only when enabled.
        // The default is "none".
        vad_events: ptr::null_mut(),
        vad_events_len: 0,
    }
}

/// Body of `flexaudio_chunk_free`. Reconstructs the boxed slice from `data`/`len` and drops
/// it, then clears the fields to prevent a double free.
///
/// # Safety
/// `chunk` must point to a valid `FlexChunk`. `data` must be what `chunk_to_c` allocated
/// (or NULL).
pub unsafe fn free_chunk_data(chunk: &mut FlexChunk) {
    if !chunk.data.is_null() {
        // Restore the boxed slice with the same len chunk_to_c allocated, and drop it.
        let slice = slice::from_raw_parts_mut(chunk.data, chunk.len);
        drop(Box::from_raw(slice as *mut [f32]));
        chunk.data = ptr::null_mut();
        chunk.len = 0;
    }
    // The VAD event array is also owned by the same chunk, so free it together.
    free_vad_events(chunk.vad_events, chunk.vad_events_len);
    chunk.vad_events = ptr::null_mut();
    chunk.vad_events_len = 0;
}

/// Maps a `FlexVadConfig` to a [`VadConfig`] (the sentinel 0 becomes the default).
///
/// Defaults are taken from [`VadConfig::default`], and only non-zero fields override them.
/// For `max_speech_ms`, 0 itself means "unlimited", so it is not treated as a sentinel.
/// `neg_threshold` becomes `None` (determined automatically by the silero formula) when 0.
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
        // 0 means "unlimited" (the default), so pass it through as is.
        max_speech_ms: c.max_speech_ms,
        sample_rate: if c.sample_rate == 0 {
            d.sample_rate
        } else {
            c.sample_rate
        },
    }
}

/// Maps a [`VadEvent`] to a `FlexVadEvent` (start = 0 / end = 1).
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

/// Turns a sequence of [`VadEvent`]s into an array to pass to C (`*mut FlexVadEvent` +
/// element count).
///
/// When empty, nothing is allocated and `(NULL, 0)` is returned (the C side sees "none" from
/// NULL or len==0). When non-empty, it is allocated with exactly the element count via
/// `into_boxed_slice`, so `free_vad_events` can restore and free it with the same element
/// count (`shrink_to_fit` is not used).
pub fn vad_events_to_c(events: Vec<VadEvent>) -> (*mut FlexVadEvent, usize) {
    if events.is_empty() {
        return (ptr::null_mut(), 0);
    }
    let boxed: Box<[FlexVadEvent]> = events.into_iter().map(vad_event_to_c).collect();
    let len = boxed.len();
    let data = Box::into_raw(boxed) as *mut FlexVadEvent;
    (data, len)
}

/// Restores and drops the array allocated by `vad_events_to_c`. NULL / 0 is a no-op.
///
/// # Safety
/// `ptr`/`len` must be what `vad_events_to_c` returned (or NULL/0).
pub unsafe fn free_vad_events(ptr: *mut FlexVadEvent, len: usize) {
    if ptr.is_null() {
        return;
    }
    // FlexVadEvent is Copy and owns no heap data, so restoring the boxed slice and dropping
    // it is enough (no per-element cleanup is needed).
    let slice = slice::from_raw_parts_mut(ptr, len);
    drop(Box::from_raw(slice as *mut [FlexVadEvent]));
}

/// Maps an [`Event`] to a `FlexEvent`. The message of `Error` goes into last_error
/// (`FlexEvent` itself carries only the kind and count).
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
        // Event is #[non_exhaustive]. An unknown variant becomes Unknown, and its debug
        // representation is left in last_error rather than being swallowed.
        other => {
            set_last_error(format!("unknown event: {other:?}"));
            FlexEvent {
                kind: FlexEventKind::Unknown,
                count: 0,
            }
        }
    }
}

/// Turns a `String` into a `*mut c_char` to pass to C. If it contains an interior NUL, it is
/// replaced with an empty string (ownership passes to the C side, and the matching free
/// function frees it).
pub(crate) fn string_to_c(s: String) -> *mut c_char {
    CString::new(s)
        .unwrap_or_else(|_| CString::new("").unwrap())
        .into_raw()
}

/// Maps a [`DeviceInfo`] to a `FlexDeviceInfo`. `id`/`name` are passed to C as CStrings.
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

/// Body of `flexaudio_devices_free`. Restores and drops the CString of each `id`/`name`, and
/// also restores the array itself as a `Vec` and drops it.
///
/// # Safety
/// `arr`/`count` must be what `flexaudio_devices` returned (or NULL/0).
pub unsafe fn free_device_array(arr: *mut FlexDeviceInfo, count: usize) {
    if arr.is_null() {
        return;
    }
    // Restore what was allocated with exactly the element count via into_boxed_slice as a
    // Vec (the allocation size is exactly count, so capacity = count is sound).
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

/// Maps an `Option<bool>` (outputting or not / unknown) to the C three-state value.
pub(crate) fn output_activity_to_c(active: Option<bool>) -> FlexOutputActivity {
    match active {
        None => FlexOutputActivity::Unknown,
        Some(false) => FlexOutputActivity::Inactive,
        Some(true) => FlexOutputActivity::Active,
    }
}

/// Passes an `Option<String>` to C. `None` becomes NULL.
fn optional_string_to_c(s: Option<String>) -> *mut c_char {
    s.map(string_to_c).unwrap_or(std::ptr::null_mut())
}

/// Reclaims and frees a string passed to C (does nothing if NULL).
///
/// # Safety
/// `p` must be what [`string_to_c`] returned (or NULL) and must not have been freed yet.
unsafe fn reclaim_c_string(p: *mut c_char) {
    if !p.is_null() {
        drop(CString::from_raw(p));
    }
}

/// Maps a [`ProcessInfo`] to a `FlexProcessInfo`. Strings are passed to C as CStrings.
pub fn process_info_to_c(info: ProcessInfo) -> FlexProcessInfo {
    FlexProcessInfo {
        pid: info.pid,
        name: string_to_c(info.name),
        executable: optional_string_to_c(info.executable),
        bundle_id: optional_string_to_c(info.bundle_id),
        output_activity: output_activity_to_c(info.is_output_active),
    }
}

/// Body of `flexaudio_processes_free`. Restores and drops the CString of each string, and also
/// restores the array itself as a `Vec` and drops it. Call it **exactly once** for the same
/// pointer.
///
/// # Safety
/// `arr`/`count` must be what `flexaudio_processes` returned (or NULL/0), and this function
/// is called exactly once for the same `arr`.
pub unsafe fn free_process_array(arr: *mut FlexProcessInfo, count: usize) {
    if arr.is_null() {
        return;
    }
    // Restore what was allocated with exactly the element count via Box<[T]> as a Vec
    // (capacity = count).
    let infos = Vec::from_raw_parts(arr, count, count);
    for info in &infos {
        reclaim_c_string(info.name);
        reclaim_c_string(info.executable);
        reclaim_c_string(info.bundle_id);
    }
    drop(infos);
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio::ChunkFlags;

    // A FlexVadConfig with all fields 0 = everything at the default.
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

    // Builds a FlexConfig for tests (strings are NULL = default, numbers are the 0 sentinel).
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
        // The 0 sentinel becomes the default.
        assert_eq!(cfg.output.sample_rate, 48_000);
        assert_eq!(cfg.output.channels, 2);
        assert_eq!(cfg.chunk_ms, 20);
        assert_eq!(cfg.target_pid, None);
        assert_eq!(cfg.device_id, None);
        assert_eq!(cfg.mode, ProcessMode::Include);
        assert!(!cfg.exclude_self);
        // gain too: 0 sentinel → default 1.0.
        assert_eq!(cfg.gain, 1.0);
        // The mix-only fields also go from sentinel to default (NULL → None / 0.0 → 1.0).
        assert_eq!(cfg.mix_mic_device_id, None);
        assert_eq!(cfg.mix_system_device_id, None);
        assert_eq!(cfg.mix_mic_gain, 1.0);
        assert_eq!(cfg.mix_system_gain, 1.0);
        // The unexposed ring_capacity_chunks is the StreamConfig default (50).
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
        // 0.0 is the sentinel = default 1.0 (same convention as output_rate 0→48000).
        let c = make_config(FlexSourceKind::Mic);
        let cfg = unsafe { build_config(&c) }.unwrap();
        assert_eq!(cfg.gain, 1.0);
        // An explicit value passes through as is.
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
        // The pointer and len match, so the data can be read back.
        let view = unsafe { slice::from_raw_parts(fc.data, fc.len) };
        assert_eq!(view, &[0.1, -0.2, 0.3, -0.4]);
        // After freeing it becomes NULL/0 and a double free is safe.
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
        // Collect into Box<[T]> to make ptr/count, just like the real flexaudio_devices.
        let boxed: Box<[FlexDeviceInfo]> = infos.into_iter().map(device_info_to_c).collect();
        let count = boxed.len();
        let first = &boxed[0];
        assert_eq!(first.source_kind, FlexSourceKind::Mic);
        assert!(first.is_default);
        let id = unsafe { CStr::from_ptr(first.id) }.to_str().unwrap();
        assert_eq!(id, "id-1");
        // free releases the CStrings and the array (no leak / double free).
        let ptr = Box::into_raw(boxed) as *mut FlexDeviceInfo;
        unsafe { free_device_array(ptr, count) };
    }

    #[test]
    fn process_info_to_c_and_free_roundtrip() {
        let infos = vec![
            ProcessInfo {
                pid: 4321,
                name: "Music".to_string(),
                executable: Some("Music".to_string()),
                bundle_id: Some("com.apple.Music".to_string()),
                is_output_active: Some(true),
            },
            ProcessInfo {
                pid: 99,
                name: "pid 99".to_string(),
                executable: None,
                bundle_id: None,
                is_output_active: None,
            },
        ];
        let boxed: Box<[FlexProcessInfo]> = infos.into_iter().map(process_info_to_c).collect();
        let count = boxed.len();
        let first = &boxed[0];
        assert_eq!(first.pid, 4321);
        assert_eq!(first.output_activity, FlexOutputActivity::Active);
        let bundle = unsafe { CStr::from_ptr(first.bundle_id) }.to_str().unwrap();
        assert_eq!(bundle, "com.apple.Music");
        let second = &boxed[1];
        assert!(second.executable.is_null(), "None becomes NULL");
        assert!(second.bundle_id.is_null());
        assert_eq!(second.output_activity, FlexOutputActivity::Unknown);
        let name = unsafe { CStr::from_ptr(second.name) }.to_str().unwrap();
        assert_eq!(name, "pid 99");
        let ptr = Box::into_raw(boxed) as *mut FlexProcessInfo;
        unsafe { free_process_array(ptr, count) };
        // NULL does nothing.
        unsafe { free_process_array(std::ptr::null_mut(), 0) };
    }

    #[test]
    fn output_activity_maps_all_three_states() {
        assert_eq!(output_activity_to_c(None), FlexOutputActivity::Unknown);
        assert_eq!(
            output_activity_to_c(Some(false)),
            FlexOutputActivity::Inactive
        );
        assert_eq!(output_activity_to_c(Some(true)), FlexOutputActivity::Active);
    }

    #[test]
    fn resolve_output_applies_sentinels() {
        // The 0 sentinel maps to 48000 / 2, and explicit values pass through as is.
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
        // An all-zero FlexVadConfig equals VadConfig::default().
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
        // For max_speech_ms, 0 means "unlimited", so it is not treated as a sentinel (0 → 0).
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
        // An empty event sequence allocates nothing: (NULL, 0). free is a no-op.
        let (ptr, len) = vad_events_to_c(Vec::new());
        assert!(ptr.is_null());
        assert_eq!(len, 0);
        unsafe { free_vad_events(ptr, len) };
    }

    #[test]
    fn vad_events_roundtrip_and_free() {
        // One full cycle for a hand-built event sequence: allocate via into_boxed_slice → read
        // back → free (verifies that allocation and free with exactly the element count, without
        // shrink_to_fit, are consistent).
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
        // Allocate both the data from chunk_to_c and vad_events attached afterwards, and check
        // that free_chunk_data frees both and resets them to NULL/0 (a double free is also
        // safe).
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
        // A double free is also safe.
        unsafe { free_chunk_data(&mut fc) };
    }
}
