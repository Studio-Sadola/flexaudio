//! Conversion helpers between C ABI types and flexaudio types.
//!
//! Split conversions into small functions: `FlexConfig` → [`StreamConfig`],
//! [`AudioChunk`] → `FlexChunk`, [`Event`] → `FlexEvent`, and [`DeviceInfo`] →
//! `FlexDeviceInfo`. Follow napi's `build_config` / `chunk_to_js` / `event_to_js` approach
//! (sentinel values select defaults, and ring_capacity_chunks is not exposed, using the
//! `StreamConfig::default()` value instead).

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

// Values used when mapping sentinel 0 to defaults (matching StreamConfig defaults).
pub(crate) const DEFAULT_OUTPUT_RATE: u32 = 48_000;
pub(crate) const DEFAULT_OUTPUT_CHANNELS: u16 = 2;
const DEFAULT_CHUNK_MS: u32 = 20;
const DEFAULT_GAIN: f32 = 1.0;
const MAX_EXCLUDE_PIDS: usize = 4096;

/// Validate and copy the caller's PID array without retaining the C pointer.
///
/// # Safety
/// For a nonempty list of at most 4096 entries, an aligned, non-NULL pointer must refer to
/// that many initialized `u32` values in one allocation, readable and unchanged during this call.
pub(crate) unsafe fn copy_exclude_pids(
    exclude_pids: *const u32,
    exclude_pids_len: usize,
) -> flexaudio::Result<Vec<u32>> {
    if exclude_pids_len > MAX_EXCLUDE_PIDS {
        return Err(flexaudio::Error::InvalidArg(
            "exclude_pids: too many entries (max 4096)".into(),
        ));
    }
    if exclude_pids_len == 0 {
        return Ok(Vec::new());
    }
    if exclude_pids.is_null() {
        return Err(flexaudio::Error::InvalidArg(
            "exclude_pids: pointer is null with nonzero length".into(),
        ));
    }
    if !exclude_pids.is_aligned() {
        return Err(flexaudio::Error::InvalidArg(
            "exclude_pids: pointer is not aligned for uint32_t".into(),
        ));
    }
    // The length cap also bounds the byte length below isize::MAX. The caller guarantees
    // readable memory; NULL and alignment checks alone cannot establish pointer validity.
    let owned = slice::from_raw_parts(exclude_pids, exclude_pids_len).to_vec();
    for (index, pid) in owned.iter().enumerate() {
        if *pid == 0 {
            return Err(flexaudio::Error::InvalidArg(format!(
                "exclude_pids[{index}] must be a positive integer in 1..=4294967295, got 0"
            )));
        }
    }
    Ok(owned)
}

/// Resolve output format in `FlexConfig`, including sentinel values (0 → default).
///
/// Small helper so `build_config` (StreamConfig construction) and `build_addons` (denoise
/// 48k validation and Denoiser channel count) share the same resolved values.
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

/// Validate a raw source code without constructing an invalid Rust enum.
pub(crate) fn source_kind_from_c(kind: i32) -> Result<SourceKind, ()> {
    match kind {
        0 => Ok(SourceKind::Mic),
        1 => Ok(SourceKind::SystemLoopback),
        2 => Ok(SourceKind::ProcessLoopback),
        3 => Ok(SourceKind::Mix),
        _ => invalid_config("source kind must be 0..=3"),
    }
}

/// Convert a trusted source kind to its fixed-width C code.
pub(crate) fn source_kind_to_c(kind: SourceKind) -> i32 {
    match kind {
        SourceKind::Mic => FlexSourceKind::Mic as i32,
        SourceKind::SystemLoopback => FlexSourceKind::System as i32,
        SourceKind::ProcessLoopback => FlexSourceKind::Process as i32,
        SourceKind::Mix => FlexSourceKind::Mix as i32,
    }
}

fn process_mode_from_c(mode: i32) -> Result<ProcessMode, ()> {
    match mode {
        value if value == FlexProcessMode::Include as i32 => Ok(ProcessMode::Include),
        value if value == FlexProcessMode::Exclude as i32 => Ok(ProcessMode::Exclude),
        _ => invalid_config("process mode must be 0 or 1"),
    }
}

fn invalid_config<T>(message: &str) -> Result<T, ()> {
    crate::error::set_audio_error(flexaudio::Error::InvalidArg(message.into()));
    Err(())
}

/// Validate all raw discriminants, including fields ignored by a source kind.
pub(crate) fn validate_config(config: &FlexConfig) -> Result<(), ()> {
    source_kind_from_c(config.kind)?;
    process_mode_from_c(config.mode)?;
    for (field, value) in [
        ("exclude_self", config.exclude_self),
        ("denoise", config.denoise),
        ("has_vad", config.has_vad),
    ] {
        if value > 1 {
            return invalid_config(&format!("{field} must be 0 or 1"));
        }
    }
    if config.chunk_ms != 0 && config.chunk_ms != DEFAULT_CHUNK_MS {
        return invalid_config("chunk_ms must be 20 (or 0 for the default)");
    }
    Ok(())
}

/// Check structural pointer constraints before forming a Rust reference.
pub(crate) fn valid_pointer<T>(pointer: *const T) -> bool {
    !pointer.is_null() && pointer.is_aligned()
}

/// Check the slice size and structural pointer constraints before dereferencing.
pub(crate) fn valid_array<T>(pointer: *const T, len: usize) -> bool {
    len <= (isize::MAX as usize) / std::mem::size_of::<T>() && (len == 0 || valid_pointer(pointer))
}

/// Convert a NUL-terminated C string to `Option<String>`. NULL becomes `None`.
///
/// If the string is invalid UTF-8, set last_error to a message containing `field` (the field
/// name) and return `Err` (the caller treats this as InvalidArg). For safety, the caller must
/// provide a valid NUL-terminated pointer or NULL.
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
            crate::error::set_audio_error(flexaudio::Error::InvalidArg(format!(
                "{field} is not valid UTF-8"
            )));
            Err(())
        }
    }
}

/// Map sentinel 0.0 to default gain 1.0 (`gain` / `mix_*_gain` share this convention).
fn gain_or_default(gain: f32) -> f32 {
    if gain == 0.0 {
        DEFAULT_GAIN
    } else {
        gain
    }
}

/// Build [`StreamConfig`] from `FlexConfig`. As in napi's `build_config`, do not expose
/// `ring_capacity_chunks`; use its default. Map fields with sentinel 0 to their defaults.
/// Transfer the owned, boundary-validated exclusion list into the stream configuration.
///
/// If `device_id` / `mix_mic_device_id` / `mix_system_device_id` contains invalid UTF-8, set
/// last_error and return `Err`.
///
/// # Safety
/// `config` must point to a valid `FlexConfig`, and each string field must be NULL or point to
/// a valid NUL-terminated C string.
pub unsafe fn build_config(
    config: &FlexConfig,
    exclude_pids: Vec<u32>,
) -> Result<StreamConfig, ()> {
    validate_config(config)?;
    let device_id = opt_string_from_c(config.device_id, "device_id")?;
    let mix_mic_device_id = opt_string_from_c(config.mix_mic_device_id, "mix_mic_device_id")?;
    let mix_system_device_id =
        opt_string_from_c(config.mix_system_device_id, "mix_system_device_id")?;

    let output = resolve_output(config);

    Ok(StreamConfig {
        kind: source_kind_from_c(config.kind)?,
        device_id,
        // process_id 0 is a sentinel meaning "none".
        target_pid: if config.process_id == 0 {
            None
        } else {
            Some(config.process_id)
        },
        // mode is process-only; exclude_self is system-only. The facade handles them separately.
        mode: process_mode_from_c(config.mode)?,
        exclude_self: config.exclude_self != 0,
        exclude_pids,
        chunk_ms: if config.chunk_ms == 0 {
            DEFAULT_CHUNK_MS
        } else {
            config.chunk_ms
        },
        // gain 0.0 is a sentinel for default 1.0 (same convention as output_rate 0→48000).
        // To mute at runtime, use flexaudio_set_gain(s, 0.0). Mix side gains use the same
        // convention (0.0 sentinel → 1.0; muting one side before mixing is not currently supported).
        gain: gain_or_default(config.gain),
        mix_mic_device_id,
        mix_system_device_id,
        mix_mic_gain: gain_or_default(config.mix_mic_gain),
        mix_system_gain: gain_or_default(config.mix_system_gain),
        output,
        // Do not expose ring_capacity_chunks (use the StreamConfig default).
        ..Default::default()
    })
}

/// Convert [`AudioChunk`] to `FlexChunk`.
///
/// Retain PCM ownership and frame metadata in library storage until chunk_free.
pub fn chunk_to_c(chunk: AudioChunk) -> FlexChunk {
    let frames = chunk.frames as u32;
    let flags = chunk.flags.bits();
    let peak = chunk.peak;
    let rms = chunk.rms;
    let pts_ns = chunk.pts_ns;
    let seq = chunk.seq;
    let dropped_before = chunk.dropped_before;

    let (data, len) = crate::chunk_storage::store(chunk.data, chunk.frame_index);

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
        // The caller (poll_processed) adds VAD events later, only when enabled.
        // Default is "none".
        vad_events: ptr::null_mut(),
        vad_events_len: 0,
    }
}

/// Implementation of `flexaudio_chunk_free`. Reconstruct and drop the boxed slice from
/// `data`/`len`, then clear the fields to prevent double-free.
///
/// # Safety
/// `chunk` must point to a valid `FlexChunk`. `data` must be allocated by `chunk_to_c` (or NULL).
pub unsafe fn free_chunk_data(chunk: &mut FlexChunk) {
    if !chunk.data.is_null() {
        crate::chunk_storage::release(chunk.data, chunk.len);
        chunk.data = ptr::null_mut();
        chunk.len = 0;
    }
    // The VAD event array is also owned by this chunk, so free it too.
    free_vad_events(chunk.vad_events, chunk.vad_events_len);
    chunk.vad_events = ptr::null_mut();
    chunk.vad_events_len = 0;
}

/// Convert `FlexVadConfig` to [`VadConfig`] (sentinel 0 selects the default).
///
/// Defaults come from [`VadConfig::default`]; only nonzero fields override them.
/// For `max_speech_ms`, 0 means "unlimited" and is not treated as a sentinel.
/// For `neg_threshold`, 0 maps to `None` (automatic Silero-style selection).
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
        // 0 means "unlimited" (the default), so pass it through.
        max_speech_ms: c.max_speech_ms,
        sample_rate: if c.sample_rate == 0 {
            d.sample_rate
        } else {
            c.sample_rate
        },
    }
}

/// Preserve the signed v1 range without wrapping exact u64 values negative.
fn signed_position(value: u64) -> i64 {
    i64::try_from(value).unwrap_or_else(|_| {
        set_last_error("v1 counter exceeds INT64_MAX; value saturated");
        i64::MAX
    })
}

/// Convert [`VadEvent`] to `FlexVadEvent` (start = 0 / end = 1).
pub fn vad_event_to_c(ev: VadEvent) -> FlexVadEvent {
    match ev {
        VadEvent::SpeechStart { at_sample } => FlexVadEvent {
            kind: 0,
            at_sample: signed_position(at_sample),
        },
        VadEvent::SpeechEnd { at_sample } => FlexVadEvent {
            kind: 1,
            at_sample: signed_position(at_sample),
        },
    }
}

/// Convert a sequence of [`VadEvent`] values to a C array (`*mut FlexVadEvent` + element count).
///
/// If empty, allocate nothing and return `(NULL, 0)` (C recognizes either NULL or len==0 as
/// absent). Otherwise, allocate exactly the element count with `into_boxed_slice` so
/// `free_vad_events` can reconstruct and free it with the same count (`shrink_to_fit` is not used).
pub fn vad_events_to_c(events: Vec<VadEvent>) -> (*mut FlexVadEvent, usize) {
    if events.is_empty() {
        return (ptr::null_mut(), 0);
    }
    let boxed: Box<[FlexVadEvent]> = events.into_iter().map(vad_event_to_c).collect();
    let len = boxed.len();
    let data = Box::into_raw(boxed) as *mut FlexVadEvent;
    (data, len)
}

/// Reconstruct and drop the array allocated by `vad_events_to_c`. NULL / 0 is a no-op.
///
/// # Safety
/// `ptr`/`len` must be values returned by `vad_events_to_c` (or NULL/0).
pub unsafe fn free_vad_events(ptr: *mut FlexVadEvent, len: usize) {
    if ptr.is_null() && len == 0 {
        return;
    }
    if !valid_array(ptr, len) || ptr.is_null() || len == 0 {
        crate::error::set_audio_error(flexaudio::Error::InvalidArg(
            "invalid VAD event array".into(),
        ));
        return;
    }
    // FlexVadEvent is Copy and owns no heap data, so reconstructing and dropping the boxed
    // slice is sufficient (no per-element cleanup is needed).
    let slice = slice::from_raw_parts_mut(ptr, len);
    drop(Box::from_raw(slice as *mut [FlexVadEvent]));
}

/// Convert [`Event`] to `FlexEvent`. Put `Error` messages in last_error (`FlexEvent` itself
/// carries only the kind and count).
pub fn event_to_c(ev: Event) -> FlexEvent {
    match ev {
        Event::TerminalError { error }
        | Event::RecoverableError { error }
        | Event::ShutdownError { error } => {
            crate::error::set_audio_error(error);
            FlexEvent {
                kind: FlexEventKind::Error as i32,
                count: 0,
            }
        }
        Event::ChunkDropped { count } => FlexEvent {
            kind: FlexEventKind::ChunkDropped as i32,
            count: signed_position(count),
        },
        Event::StreamStalled => FlexEvent {
            kind: FlexEventKind::Stalled as i32,
            count: 0,
        },
        Event::StreamRecovered => FlexEvent {
            kind: FlexEventKind::Recovered as i32,
            count: 0,
        },
        Event::PermissionPending { permission, .. } => {
            set_last_error(format!(
                "Recording permission is pending. {}",
                permission.guidance()
            ));
            FlexEvent {
                kind: FlexEventKind::PermissionPending as i32,
                count: 0,
            }
        }
        Event::PermissionDenied { permission, detail } => {
            crate::error::set_audio_error(flexaudio::Error::PermissionDenied {
                permission,
                detail,
            });
            FlexEvent {
                kind: FlexEventKind::PermissionDenied as i32,
                count: 0,
            }
        }
        Event::SilenceWhileSourceActive { .. } => {
            set_last_error("Capture is silent while the source is active; check recording permissions and source output");
            FlexEvent {
                kind: FlexEventKind::SilenceWhileSourceActive as i32,
                count: 0,
            }
        }
        Event::DeviceLost => FlexEvent {
            kind: FlexEventKind::DeviceLost as i32,
            count: 0,
        },
        Event::Error(_) => {
            crate::error::set_audio_error(flexaudio::Error::Backend(
                "legacy capture failure".into(),
            ));
            FlexEvent {
                kind: FlexEventKind::Error as i32,
                count: 0,
            }
        }
        Event::AudioLoss { .. } | Event::Clipped | Event::PermissionGranted => {
            // New advisory events have no representable v1 payload.
            FlexEvent {
                kind: FlexEventKind::Unknown as i32,
                count: 0,
            }
        }
        _ => {
            set_last_error("unknown event".to_string());
            FlexEvent {
                kind: FlexEventKind::Unknown as i32,
                count: 0,
            }
        }
    }
}

/// Convert a `String` to `*mut c_char` for C. Replace embedded NULs with an empty string
/// (ownership transfers to C and the corresponding free function releases it).
pub(crate) fn string_to_c(s: String) -> *mut c_char {
    CString::new(s)
        .unwrap_or_else(|_| CString::new("").unwrap())
        .into_raw()
}

/// Convert [`DeviceInfo`] to `FlexDeviceInfo`. Transfer `id`/`name` to C as CStrings.
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

/// Implementation of `flexaudio_devices_free`. Reconstruct and drop each `id`/`name` CString,
/// then reconstruct and drop the array as a `Vec`.
///
/// # Safety
/// `arr`/`count` must be returned by `flexaudio_devices` (or NULL/0).
pub unsafe fn free_device_array(arr: *mut FlexDeviceInfo, count: usize) {
    if arr.is_null() && count == 0 {
        return;
    }
    if !valid_array(arr, count) || arr.is_null() || count == 0 {
        crate::error::set_audio_error(flexaudio::Error::InvalidArg("invalid device array".into()));
        return;
    }
    // Reconstruct as a Vec after allocation of exactly the element count with into_boxed_slice
    // (capacity = count, so this is sound).
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

/// Convert `Option<bool>` (active output or unknown) to the C three-state value.
pub(crate) fn output_activity_to_c(active: Option<bool>) -> i32 {
    match active {
        None => FlexOutputActivity::Unknown as i32,
        Some(false) => FlexOutputActivity::Inactive as i32,
        Some(true) => FlexOutputActivity::Active as i32,
    }
}

/// Pass `Option<String>` to C. `None` becomes NULL.
fn optional_string_to_c(s: Option<String>) -> *mut c_char {
    s.map(string_to_c).unwrap_or(std::ptr::null_mut())
}

/// Reclaim and free a string passed to C (no-op for NULL).
///
/// # Safety
/// `p` must be returned by [`string_to_c`] (or NULL) and not already freed.
unsafe fn reclaim_c_string(p: *mut c_char) {
    if !p.is_null() {
        drop(CString::from_raw(p));
    }
}

/// Convert [`ProcessInfo`] to `FlexProcessInfo`. Pass strings to C as CStrings.
pub fn process_info_to_c(info: ProcessInfo) -> FlexProcessInfo {
    FlexProcessInfo {
        pid: info.pid,
        name: string_to_c(info.name),
        executable: optional_string_to_c(info.executable),
        bundle_id: optional_string_to_c(info.bundle_id),
        output_activity: output_activity_to_c(info.is_output_active),
    }
}

/// Implementation of `flexaudio_processes_free`. Reconstruct and drop each CString, then
/// reconstruct and drop the array as a `Vec`. Call **once only** for a given pointer.
///
/// # Safety
/// `arr`/`count` must be returned by `flexaudio_processes` (or NULL/0). Call this function
/// only once for a given `arr`.
pub unsafe fn free_process_array(arr: *mut FlexProcessInfo, count: usize) {
    if arr.is_null() && count == 0 {
        return;
    }
    if !valid_array(arr, count) || arr.is_null() || count == 0 {
        crate::error::set_audio_error(flexaudio::Error::InvalidArg("invalid process array".into()));
        return;
    }
    // Reconstruct as a Vec; Box<[T]> was allocated for exactly the element count (capacity = count).
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

    #[test]
    fn terminal_backend_events_keep_error_code_and_cause() {
        let error = flexaudio::Error::Backend("authorization query failed".into());
        let event = event_to_c(Event::TerminalError {
            error: error.clone(),
        });
        assert_eq!(event.kind, FlexEventKind::Error as i32);
        // SAFETY: event_to_c just stored a live thread-local C string; no call has replaced it.
        let message = unsafe { CStr::from_ptr(crate::error::last_error_ptr()) }
            .to_str()
            .unwrap();
        assert_eq!(message, error.to_string());
    }

    #[test]
    fn v1_projects_new_events_and_saturates_signed_counters() {
        use crate::error::{clear_last_error, last_audio_error};
        let error = flexaudio::Error::DeviceLost
            .with_context(flexaudio::ErrorContext::new(flexaudio::Operation::Reopen));
        for event in [
            Event::TerminalError {
                error: error.clone(),
            },
            Event::RecoverableError {
                error: error.clone(),
            },
            Event::ShutdownError {
                error: error.clone(),
            },
        ] {
            clear_last_error();
            let projected = event_to_c(event);
            assert_eq!(projected.kind, FlexEventKind::Error as i32);
            assert_eq!(projected.count, 0);
            assert_eq!(last_audio_error(), Some(error.clone()));
        }
        for event in [
            Event::PermissionGranted,
            Event::Clipped,
            Event::AudioLoss {
                loss: flexaudio::AudioLoss::raw_overflow(None, None, 48000, 2).unwrap(),
            },
        ] {
            clear_last_error();
            let projected = event_to_c(event);
            assert_eq!(projected.kind, FlexEventKind::Unknown as i32);
            assert_eq!(projected.count, 0);
            assert!(crate::error::last_error_ptr().is_null());
        }
        for count in [i64::MAX as u64 + 1, u64::MAX] {
            clear_last_error();
            assert_eq!(event_to_c(Event::ChunkDropped { count }).count, i64::MAX);
            assert!(!crate::error::last_error_ptr().is_null());
            clear_last_error();
            for event in [
                VadEvent::SpeechStart { at_sample: count },
                VadEvent::SpeechEnd { at_sample: count },
            ] {
                assert_eq!(vad_event_to_c(event).at_sample, i64::MAX);
                assert!(!crate::error::last_error_ptr().is_null());
            }
        }
        clear_last_error();
        let legacy = event_to_c(Event::Error("secret diagnostic must stay private".into()));
        assert_eq!(legacy.kind, FlexEventKind::Error as i32);
        let message = unsafe { CStr::from_ptr(crate::error::last_error_ptr()) }
            .to_str()
            .unwrap();
        assert_eq!(message, "backend error: legacy capture failure");
    }

    // All-zero fields in FlexVadConfig mean all defaults.
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

    // Build FlexConfig for tests (NULL strings = defaults, numeric 0 = sentinel).
    fn make_config(kind: i32) -> FlexConfig {
        FlexConfig {
            kind,
            device_id: ptr::null(),
            process_id: 0,
            mode: FlexProcessMode::Include as i32,
            exclude_self: 0,
            output_rate: 0,
            output_channels: 0,
            chunk_ms: 0,
            gain: 0.0,
            mix_mic_device_id: ptr::null(),
            mix_system_device_id: ptr::null(),
            mix_mic_gain: 0.0,
            mix_system_gain: 0.0,
            denoise: 0,
            has_vad: 0,
            vad: zero_vad_config(),
        }
    }

    #[test]
    fn build_config_applies_defaults_for_sentinels() {
        let c = make_config(FlexSourceKind::Mic as i32);
        let cfg = unsafe { build_config(&c, Vec::new()) }.unwrap();
        assert_eq!(cfg.kind, SourceKind::Mic);
        // Sentinel 0 selects the default.
        assert_eq!(cfg.output.sample_rate, 48_000);
        assert_eq!(cfg.output.channels, 2);
        assert_eq!(cfg.chunk_ms, 20);
        assert_eq!(cfg.target_pid, None);
        assert_eq!(cfg.device_id, None);
        assert_eq!(cfg.mode, ProcessMode::Include);
        assert!(!cfg.exclude_self);
        // gain sentinel 0 also selects default 1.0.
        assert_eq!(cfg.gain, 1.0);
        // Mix-specific fields use defaults for sentinels too (NULL → None / 0.0 → 1.0).
        assert_eq!(cfg.mix_mic_device_id, None);
        assert_eq!(cfg.mix_system_device_id, None);
        assert_eq!(cfg.mix_mic_gain, 1.0);
        assert_eq!(cfg.mix_system_gain, 1.0);
        // Unexposed ring_capacity_chunks uses the StreamConfig default (50).
        assert_eq!(cfg.ring_capacity_chunks, 50);
    }

    #[test]
    fn build_config_reflects_explicit_values() {
        let mut c = make_config(FlexSourceKind::Process as i32);
        c.process_id = 4321;
        c.mode = FlexProcessMode::Exclude as i32;
        c.exclude_self = 1;
        c.output_rate = 16_000;
        c.output_channels = 1;
        c.chunk_ms = 20;
        c.gain = 2.5;
        let cfg = unsafe { build_config(&c, Vec::new()) }.unwrap();
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
        // 0.0 is a sentinel for default 1.0 (same convention as output_rate 0→48000).
        let c = make_config(FlexSourceKind::Mic as i32);
        let cfg = unsafe { build_config(&c, Vec::new()) }.unwrap();
        assert_eq!(cfg.gain, 1.0);
        // Explicit values pass through unchanged.
        let mut c2 = make_config(FlexSourceKind::Mic as i32);
        c2.gain = 0.5;
        let cfg2 = unsafe { build_config(&c2, Vec::new()) }.unwrap();
        assert_eq!(cfg2.gain, 0.5);
    }

    #[test]
    fn build_config_reads_device_id() {
        let id = CString::new("dev-x").unwrap();
        let mut c = make_config(FlexSourceKind::Mic as i32);
        c.device_id = id.as_ptr();
        let cfg = unsafe { build_config(&c, Vec::new()) }.unwrap();
        assert_eq!(cfg.device_id.as_deref(), Some("dev-x"));
    }

    #[test]
    fn build_config_reflects_mix_fields() {
        let mic_id = CString::new("mic-a").unwrap();
        let sys_id = CString::new("sink-b").unwrap();
        let mut c = make_config(FlexSourceKind::Mix as i32);
        c.mix_mic_device_id = mic_id.as_ptr();
        c.mix_system_device_id = sys_id.as_ptr();
        c.mix_mic_gain = 0.5;
        c.mix_system_gain = 2.0;
        let cfg = unsafe { build_config(&c, Vec::new()) }.unwrap();
        assert_eq!(cfg.kind, SourceKind::Mix);
        assert_eq!(cfg.mix_mic_device_id.as_deref(), Some("mic-a"));
        assert_eq!(cfg.mix_system_device_id.as_deref(), Some("sink-b"));
        assert_eq!(cfg.mix_mic_gain, 0.5);
        assert_eq!(cfg.mix_system_gain, 2.0);
    }

    #[test]
    fn source_kind_roundtrips() {
        for (c, k) in [
            (FlexSourceKind::Mic as i32, SourceKind::Mic),
            (FlexSourceKind::System as i32, SourceKind::SystemLoopback),
            (FlexSourceKind::Process as i32, SourceKind::ProcessLoopback),
            (FlexSourceKind::Mix as i32, SourceKind::Mix),
        ] {
            assert_eq!(source_kind_from_c(c).unwrap(), k);
            assert_eq!(source_kind_to_c(k), c);
        }
    }

    #[test]
    fn chunk_to_c_keeps_ptr_and_len_consistent() {
        let chunk = AudioChunk {
            frame_index: 9_007_199_254_740_993,
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
        unsafe {
            assert_eq!(
                crate::flexaudio_chunk_frame_index(&fc),
                9_007_199_254_740_993
            );
        }
        assert_eq!(fc.len, 4);
        assert_eq!(fc.frames, 2);
        assert_eq!(fc.flags, ChunkFlags::DISCONTINUITY.bits());
        assert_eq!(fc.dropped_before, 1);
        assert!(!fc.data.is_null());
        // Can read back because the pointer and len match.
        let view = unsafe { slice::from_raw_parts(fc.data, fc.len) };
        assert_eq!(view, &[0.1, -0.2, 0.3, -0.4]);
        // Becomes NULL/0 after freeing, making double-free safe.
        unsafe { free_chunk_data(&mut fc) };
        assert!(fc.data.is_null());
        assert_eq!(fc.len, 0);
        unsafe { free_chunk_data(&mut fc) };
    }

    #[test]
    fn caller_built_chunk_frame_index_fails_closed() {
        // The preceding words are caller data, never library metadata. A zero
        // offset data pointer must also be safe: the accessor may not read PCM.
        let mut caller_data = [f32::from_bits(123), f32::from_bits(456), 0.25];
        let mut chunk: FlexChunk = unsafe { std::mem::zeroed() };
        chunk.data = unsafe { caller_data.as_mut_ptr().add(2) };
        chunk.len = 1;
        assert_eq!(unsafe { crate::flexaudio_chunk_frame_index(&chunk) }, 0);
        chunk.data = caller_data.as_mut_ptr();
        assert_eq!(unsafe { crate::flexaudio_chunk_frame_index(&chunk) }, 0);
    }

    #[test]
    fn empty_chunk_allocations_have_distinct_indices_and_free_invalidates_them() {
        fn empty(index: u64) -> FlexChunk {
            chunk_to_c(AudioChunk {
                data: Vec::new(),
                frames: 0,
                frame_index: index,
                pts_ns: 0,
                seq: 0,
                flags: flexaudio::ChunkFlags::empty(),
                dropped_before: 0,
                peak: 0.0,
                rms: 0.0,
            })
        }
        let mut first = empty(123);
        let mut second = empty(456);
        assert_ne!(first.data, second.data);
        unsafe {
            assert_eq!(crate::flexaudio_chunk_frame_index(&first), 123);
            assert_eq!(crate::flexaudio_chunk_frame_index(&second), 456);
            free_chunk_data(&mut first);
            assert_eq!(crate::flexaudio_chunk_frame_index(&first), 0);
            free_chunk_data(&mut second);
            assert_eq!(crate::flexaudio_chunk_frame_index(&second), 0);
        }
    }

    #[test]
    fn permission_pending_preserves_new_code_and_advisory_message() {
        for permission in [
            flexaudio::Permission::Microphone,
            flexaudio::Permission::SystemAudio,
        ] {
            let event = event_to_c(Event::PermissionPending {
                permission,
                detail: "Permission is pending; capture may remain silent until granted".into(),
            });
            assert_eq!(event.kind, 8);
            assert_eq!(event.count, 0);
            // SAFETY: event_to_c stored a live thread-local C string; no call has replaced it.
            let message = unsafe { CStr::from_ptr(crate::error::last_error_ptr()) }
                .to_str()
                .unwrap();
            assert_eq!(
                message,
                format!("Recording permission is pending. {}", permission.guidance())
            );
        }
        assert_eq!(FlexEventKind::PermissionDenied as i32, 3);
        assert_eq!(FlexEventKind::SilenceWhileSourceActive as i32, 7);
    }

    #[test]
    fn permission_and_advisory_messages_preserve_c_codes() {
        use crate::error::last_error_ptr;
        for permission in [
            flexaudio::Permission::Microphone,
            flexaudio::Permission::SystemAudio,
        ] {
            let expected = flexaudio::Error::PermissionDenied {
                permission,
                detail: "denied by user".into(),
            }
            .to_string();
            let event = event_to_c(Event::PermissionDenied {
                permission,
                detail: "denied by user".into(),
            });
            assert_eq!(event.kind, 3);
            // SAFETY: last_error_ptr points to a live thread-local C string.
            let message = unsafe { CStr::from_ptr(last_error_ptr()) }
                .to_str()
                .unwrap();
            assert_eq!(message, expected);
        }
        let advisory = event_to_c(Event::SilenceWhileSourceActive {
            detail: "check recording privacy settings".into(),
        });
        assert_eq!(advisory.kind, 7);
        // SAFETY: No intervening call has changed the thread-local C string.
        assert_eq!(
            unsafe { CStr::from_ptr(last_error_ptr()) }
                .to_str()
                .unwrap(),
            "Capture is silent while the source is active; check recording permissions and source output"
        );
        assert_eq!(FlexEventKind::Unknown as i32, 6);
        assert_eq!(crate::error::code::FLEX_FAILURE, -2);
    }

    #[test]
    fn event_to_c_maps_each_variant() {
        assert_eq!(
            event_to_c(Event::ChunkDropped { count: 5 }).kind,
            FlexEventKind::ChunkDropped as i32
        );
        assert_eq!(event_to_c(Event::ChunkDropped { count: 5 }).count, 5);
        assert_eq!(
            event_to_c(Event::StreamStalled).kind,
            FlexEventKind::Stalled as i32
        );
        assert_eq!(
            event_to_c(Event::StreamRecovered).kind,
            FlexEventKind::Recovered as i32
        );
        assert_eq!(
            event_to_c(Event::PermissionDenied {
                permission: flexaudio::Permission::Microphone,
                detail: "denied by user".into()
            })
            .kind,
            FlexEventKind::PermissionDenied as i32
        );
        assert_eq!(
            event_to_c(Event::DeviceLost).kind,
            FlexEventKind::DeviceLost as i32
        );
        let err = event_to_c(Event::Error("boom".to_string()));
        assert_eq!(err.kind, FlexEventKind::Error as i32);
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
        // As in production flexaudio_devices, collect into Box<[T]> and create ptr/count.
        let boxed: Box<[FlexDeviceInfo]> = infos.into_iter().map(device_info_to_c).collect();
        let count = boxed.len();
        let first = &boxed[0];
        assert_eq!(first.source_kind, FlexSourceKind::Mic as i32);
        assert!(first.is_default);
        let id = unsafe { CStr::from_ptr(first.id) }.to_str().unwrap();
        assert_eq!(id, "id-1");
        // free releases the CStrings and array (no leaks or double-free).
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
        assert_eq!(first.output_activity, FlexOutputActivity::Active as i32);
        let bundle = unsafe { CStr::from_ptr(first.bundle_id) }.to_str().unwrap();
        assert_eq!(bundle, "com.apple.Music");
        let second = &boxed[1];
        assert!(second.executable.is_null(), "None becomes NULL");
        assert!(second.bundle_id.is_null());
        assert_eq!(second.output_activity, FlexOutputActivity::Unknown as i32);
        let name = unsafe { CStr::from_ptr(second.name) }.to_str().unwrap();
        assert_eq!(name, "pid 99");
        let ptr = Box::into_raw(boxed) as *mut FlexProcessInfo;
        unsafe { free_process_array(ptr, count) };
        // NULL is a no-op.
        unsafe { free_process_array(std::ptr::null_mut(), 0) };
    }

    #[test]
    fn output_activity_maps_all_three_states() {
        assert_eq!(
            output_activity_to_c(None),
            FlexOutputActivity::Unknown as i32
        );
        assert_eq!(
            output_activity_to_c(Some(false)),
            FlexOutputActivity::Inactive as i32
        );
        assert_eq!(
            output_activity_to_c(Some(true)),
            FlexOutputActivity::Active as i32
        );
    }

    #[test]
    fn resolve_output_applies_sentinels() {
        // Sentinel 0 maps to 48000 / 2; explicit values pass through unchanged.
        let c = make_config(FlexSourceKind::Mic as i32);
        let out = resolve_output(&c);
        assert_eq!(out.sample_rate, 48_000);
        assert_eq!(out.channels, 2);

        let mut c2 = make_config(FlexSourceKind::Mic as i32);
        c2.output_rate = 16_000;
        c2.output_channels = 1;
        let out2 = resolve_output(&c2);
        assert_eq!(out2.sample_rate, 16_000);
        assert_eq!(out2.channels, 1);
    }

    #[test]
    fn vad_config_all_zero_is_default() {
        // All-zero FlexVadConfig matches VadConfig::default().
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
        // max_speech_ms 0 means "unlimited", so it is not a sentinel (0 → 0).
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
        // Empty event lists allocate nothing and return (NULL, 0); free is a no-op.
        let (ptr, len) = vad_events_to_c(Vec::new());
        assert!(ptr.is_null());
        assert_eq!(len, 0);
        unsafe { free_vad_events(ptr, len) };
    }

    #[test]
    fn vad_events_roundtrip_and_free() {
        // Round-trip a hand-built event list through into_boxed_slice, readback, and free
        // (verify exact-size allocation/free consistency without shrink_to_fit).
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
        // Allocate both data from chunk_to_c and appended vad_events; verify free_chunk_data
        // releases both and resets them to NULL/0 (double-free is safe too).
        let chunk = AudioChunk {
            frame_index: 0,
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
        // Safe to free twice.
        unsafe { free_chunk_data(&mut fc) };
    }
}
