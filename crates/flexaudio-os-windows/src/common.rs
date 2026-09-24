//! Shared helpers for the Windows backends: COM initialization guard, WAVEFORMATEX parsing,
//! HRESULT-to-[`Error`] conversion, and the WASAPI capture loop.
//!
//! [`WasapiSystemBackend`](crate::WasapiSystemBackend) and
//! [`WasapiProcessBackend`](crate::WasapiProcessBackend) run the same capture loop
//! ([`capture_loop`]) on a dedicated thread. The only differences between them are how the
//! `IAudioClient` is obtained (classic loopback vs. process loopback activation) and how the
//! format is chosen (`GetMixFormat` vs. a fixed WAVEFORMATEX); Initialize → GetService →
//! Start → capture → Stop is shared.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use flexaudio_core::backend::RawSink;
use flexaudio_core::clock::monotonic_now_ns;
use flexaudio_core::types::Error;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    IAudioCaptureClient, IAudioClient, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// Classifies access-denied and device-missing HRESULTs into typed [`Error`] variants.
///
/// Maps the common WASAPI/COM HRESULTs onto the cross-OS error type:
/// - Access denied → [`Error::PermissionDenied`]: `E_ACCESSDENIED` (returned when the
///   microphone/audio capture privacy setting denies access), `AUDCLNT_E_DEVICE_IN_USE`
///   (cannot open because it is in exclusive use), `AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED`
///   (exclusive mode not allowed).
/// - Device missing/invalidated → [`Error::DeviceNotFound`]: `AUDCLNT_E_DEVICE_INVALIDATED`
///   (the target endpoint/device disappeared or was invalidated), `E_NOTFOUND` (element/
///   endpoint not found).
///
/// Anything else cannot be classified, so this returns `None` and [`map_hr`] falls back to a
/// [`Error::Backend`] with context. Constant imports shift between `windows` crate versions,
/// so this compares against raw i32 values, which are stable (hex values noted alongside
/// below).
pub(crate) fn classify_hr(code: i32) -> Option<Error> {
    // Access denied → PermissionDenied
    const E_ACCESSDENIED: i32 = 0x80070005u32 as i32;
    const AUDCLNT_E_DEVICE_IN_USE: i32 = 0x8889000Au32 as i32;
    const AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED: i32 = 0x8889000Eu32 as i32;
    // Device missing/invalidated → DeviceNotFound
    const AUDCLNT_E_DEVICE_INVALIDATED: i32 = 0x88890004u32 as i32;
    // E_NOTFOUND (0x80070490, ERROR_NOT_FOUND as an HRESULT). The given endpoint/element is
    // not found.
    const E_NOTFOUND: i32 = 0x80070490u32 as i32;

    match code {
        E_ACCESSDENIED | AUDCLNT_E_DEVICE_IN_USE | AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED => {
            Some(Error::PermissionDenied)
        }
        AUDCLNT_E_DEVICE_INVALIDATED | E_NOTFOUND => Some(Error::DeviceNotFound),
        _ => None,
    }
}

/// Converts an HRESULT into an [`Error`] with a context string.
///
/// Access-denied and device-missing codes are mapped by [`classify_hr`] to typed variants
/// ([`Error::PermissionDenied`] / [`Error::DeviceNotFound`]); anything that cannot be
/// classified falls back to a [`Error::Backend`] with context.
pub(crate) fn map_hr(ctx: &str, e: windows::core::Error) -> Error {
    if let Some(mapped) = classify_hr(e.code().0) {
        return mapped;
    }
    Error::Backend(format!("{ctx}: {e}"))
}

/// Monotonic clock (ns). Uses the core [`monotonic_now_ns`] as is. The downstream
/// `ClockNormalizer` takes the origin on first use, so a monotonic approximation of the
/// arrival time is sufficient.
pub(crate) fn now_ns() -> i64 {
    monotonic_now_ns()
}

/// COM initialization guard. `new` calls `CoInitializeEx(MULTITHREADED)` and `Drop` calls
/// `CoUninitialize` (called symmetrically on the same thread).
///
/// Already being initialized in a different mode (`RPC_E_CHANGED_MODE`) is not treated as a
/// failure, because WASAPI calls still work even if someone else initialized the thread as
/// STA. In that case `uninit_on_drop=false`, so `CoUninitialize` is not called against
/// someone else's initialization.
pub(crate) struct ComThread {
    uninit_on_drop: bool,
}

impl ComThread {
    /// Initializes COM on this thread. Does not panic.
    pub(crate) fn new() -> Self {
        // In 0.54, CoInitializeEx returns an HRESULT (not a Result).
        // S_OK / S_FALSE mean success; RPC_E_CHANGED_MODE means "already initialized in a
        // different mode".
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        // On success (S_OK=0 / S_FALSE=1) we initialized it, so uninit on drop.
        // RPC_E_CHANGED_MODE etc. means someone else initialized it → do not uninit.
        let uninit_on_drop = hr.is_ok();
        Self { uninit_on_drop }
    }
}

impl Drop for ComThread {
    fn drop(&mut self) {
        if self.uninit_on_drop {
            unsafe { CoUninitialize() };
        }
    }
}

/// Parses a `WAVEFORMATEX` (or `WAVEFORMATEXTENSIBLE` when applicable) and returns
/// `Ok((rate, channels))` if the subformat is IEEE float. PCM (int) formats are unsupported
/// and return [`Error::Backend`]. On real hardware the shared-mode MixFormat is normally
/// float.
///
/// `WAVEFORMATEX` / `WAVEFORMATEXTENSIBLE` are `#[repr(C, packed(1))]`, so creating a
/// reference to a packed field is UB. Values are copied with `addr_of!` + `read_unaligned`
/// without taking references.
///
/// # Safety
/// `pwfx` must point to a valid `WAVEFORMATEX` (the return value of `GetMixFormat`).
pub(crate) unsafe fn parse_mix_format(pwfx: *const WAVEFORMATEX) -> Result<(u32, u16), Error> {
    use core::ptr::addr_of;

    if pwfx.is_null() {
        return Err(Error::Backend("GetMixFormat returned null format".into()));
    }
    // Read packed fields by value copy.
    let format_tag = addr_of!((*pwfx).wFormatTag).read_unaligned();
    let rate = addr_of!((*pwfx).nSamplesPerSec).read_unaligned();
    let channels = addr_of!((*pwfx).nChannels).read_unaligned();
    let bits = addr_of!((*pwfx).wBitsPerSample).read_unaligned();
    let cb_size = addr_of!((*pwfx).cbSize).read_unaligned();

    // The constants are u32. An identifier in a match pattern would be taken as a binding,
    // so compare with `==`.
    let tag = format_tag as u32;
    let is_float = if tag == WAVE_FORMAT_IEEE_FLOAT {
        true
    } else if tag == WAVE_FORMAT_EXTENSIBLE {
        // If cbSize covers at least the EXTENSIBLE extension (22), read it as EXTENSIBLE.
        if (cb_size as usize) >= 22 {
            let sub = addr_of!((*(pwfx as *const WAVEFORMATEXTENSIBLE)).SubFormat).read_unaligned();
            sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
        } else {
            false
        }
    } else {
        false
    };

    if !is_float {
        return Err(Error::Backend(format!(
            "unsupported mix format (not IEEE float): tag={tag} bits={bits}"
        )));
    }
    Ok((rate, channels))
}

/// Event-driven WASAPI capture loop (runs on the dedicated thread).
///
/// Takes an already-Initialized `client` (initialized in shared mode with
/// LOOPBACK|EVENTCALLBACK), its capture service, the event handle, and the channel count,
/// and pulls packets and feeds them to [`RawSink::push`] until `stop_flag` is set. On exit
/// it calls `client.Stop()` and closes the event handle.
///
/// Packets are read as interleaved f32 (`channels` ch). WASAPI buffers are allocated on at
/// least an 8-byte boundary, so the `*const f32` cast is safe. When the silent flag
/// (`AUDCLNT_BUFFERFLAGS_SILENT`) is set, `frames*channels` zeros are pushed (to avoid a DC
/// offset).
///
/// # Safety
/// `client` / `capture` / `event` must be valid COM objects/handles Initialized on the same
/// thread. `channels >= 1`.
pub(crate) unsafe fn capture_loop(
    client: &IAudioClient,
    capture: &IAudioCaptureClient,
    event: HANDLE,
    channels: u16,
    mut sink: RawSink,
    stop_flag: &Arc<AtomicBool>,
) {
    let channels = channels.max(1) as usize;
    // Reusable buffer for pushing zeros when the silent flag is set. It is allocated at the
    // maximum expected length before entering the RT loop (before Start) so the loop does not
    // allocate. The maximum frame count of one packet is bounded by the engine buffer size
    // (`GetBufferSize`), so reserving `buffer_frames * channels` makes the in-loop `resize` an
    // in-capacity no-op. Only when `GetBufferSize` fails does it stay empty, in which case it
    // falls back to a resize on the first iteration of the loop.
    let mut silence: Vec<f32> = Vec::new();
    if let Ok(buffer_frames) = client.GetBufferSize() {
        let max_silence = (buffer_frames as usize).saturating_mul(channels);
        silence.resize(max_silence, 0.0);
    }

    if client.Start().is_err() {
        // If Start fails, return without doing anything (normally unreachable because the
        // setup side has already started it).
        let _ = CloseHandle(event);
        return;
    }

    while !stop_flag.load(Ordering::SeqCst) {
        // Wakes after 100 ms or when the event fires. The timeout ensures a stop request is
        // never missed.
        let _ = WaitForSingleObject(event, 100);
        if stop_flag.load(Ordering::SeqCst) {
            break;
        }

        loop {
            let packet = match capture.GetNextPacketSize() {
                Ok(p) => p,
                Err(_e) => {
                    // Can become DEVICE_INVALIDATED, e.g. when the target PID exits. Leave
                    // the loop and stop.
                    stop_flag.store(true, Ordering::SeqCst);
                    break;
                }
            };
            if packet == 0 {
                break;
            }

            let mut pdata: *mut u8 = std::ptr::null_mut();
            let mut frames: u32 = 0;
            let mut flags: u32 = 0;
            if capture
                .GetBuffer(&mut pdata, &mut frames, &mut flags, None, None)
                .is_err()
            {
                stop_flag.store(true, Ordering::SeqCst);
                break;
            }

            let n = frames as usize * channels;
            // Wrap push in catch_unwind. This is our own thread, not an FFI boundary, but if
            // `RawSink::push` or similar ever panics, the `ReleaseBuffer` / `client.Stop()` /
            // `CloseHandle` cleanup below must not be skipped. Processing continues after a
            // caught panic.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                if (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 {
                    // Silence: push n zeros (for downstream gap detection / to avoid a DC
                    // offset).
                    if silence.len() < n {
                        silence.resize(n, 0.0);
                    }
                    if n > 0 {
                        sink.push(&silence[..n], now_ns());
                    }
                } else if !pdata.is_null() && n > 0 {
                    let slice = std::slice::from_raw_parts(pdata as *const f32, n);
                    sink.push(slice, now_ns());
                }
            }));

            // Always release the acquired frames (pass frames regardless of success or
            // failure).
            let _ = capture.ReleaseBuffer(frames);
        }
    }

    let _ = client.Stop();
    let _ = CloseHandle(event);
}

/// Shared sequence that Initializes `client` in shared mode with LOOPBACK|EVENTCALLBACK,
/// attaches an event handle, and retrieves the capture service. Returns
/// `(IAudioCaptureClient, event_handle)` on success.
///
/// `pwfx` is the format passed to Initialize (for System, the raw pointer from
/// `GetMixFormat`; for Process, a pointer to our own fixed WAVEFORMATEX).
/// `AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK` is always set.
/// `extra_streamflags` are flags added on top (for process loopback,
/// `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`, as in the official ApplicationLoopback sample; for
/// classic loopback, `0`).
///
/// # Safety
/// `client` must be a valid COM object Activated on the same thread. `pwfx` must point to a
/// valid `WAVEFORMATEX`.
pub(crate) unsafe fn init_loopback_capture(
    client: &IAudioClient,
    pwfx: *const WAVEFORMATEX,
    extra_streamflags: u32,
) -> Result<(IAudioCaptureClient, HANDLE), Error> {
    client
        .Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK | extra_streamflags,
            0, // hnsBufferDuration: 0 = engine default
            0, // hnsPeriodicity: 0 in shared mode
            pwfx,
            None,
        )
        .map_err(|e| map_hr("IAudioClient::Initialize", e))?;

    // Manual reset = false / initially non-signaled / unnamed event.
    let event =
        CreateEventW(None, false, false, PCWSTR::null()).map_err(|e| map_hr("CreateEventW", e))?;

    if let Err(e) = client.SetEventHandle(event) {
        let _ = CloseHandle(event);
        return Err(map_hr("IAudioClient::SetEventHandle", e));
    }

    let capture: IAudioCaptureClient = match client.GetService() {
        Ok(c) => c,
        Err(e) => {
            let _ = CloseHandle(event);
            return Err(map_hr("IAudioClient::GetService(IAudioCaptureClient)", e));
        }
    };

    Ok((capture, event))
}

/// Whether the return value of `WaitForSingleObject` is signaled (`WAIT_OBJECT_0`).
/// Used by the process backend to wait for activation to complete.
pub(crate) fn wait_event_signaled(handle: HANDLE, timeout_ms: u32) -> bool {
    let r = unsafe { WaitForSingleObject(handle, timeout_ms) };
    r == WAIT_OBJECT_0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Access-denied HRESULTs are classified as PermissionDenied (audit P1-2).
    #[test]
    fn classify_hr_maps_access_denied_to_permission_denied() {
        // E_ACCESSDENIED
        assert!(matches!(
            classify_hr(0x80070005u32 as i32),
            Some(Error::PermissionDenied)
        ));
        // AUDCLNT_E_DEVICE_IN_USE
        assert!(matches!(
            classify_hr(0x8889000Au32 as i32),
            Some(Error::PermissionDenied)
        ));
        // AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED
        assert!(matches!(
            classify_hr(0x8889000Eu32 as i32),
            Some(Error::PermissionDenied)
        ));
    }

    /// Device-missing/invalidated HRESULTs are classified as DeviceNotFound (audit P1-4).
    #[test]
    fn classify_hr_maps_device_codes_to_device_not_found() {
        // AUDCLNT_E_DEVICE_INVALIDATED
        assert!(matches!(
            classify_hr(0x88890004u32 as i32),
            Some(Error::DeviceNotFound)
        ));
        // E_NOTFOUND
        assert!(matches!(
            classify_hr(0x80070490u32 as i32),
            Some(Error::DeviceNotFound)
        ));
    }

    /// HRESULTs that cannot be classified yield None (map_hr falls back to Backend).
    #[test]
    fn classify_hr_unknown_is_none() {
        // E_FAIL (generic failure) is not classified.
        assert!(classify_hr(0x80004005u32 as i32).is_none());
        // S_OK is not even a failure, so naturally None.
        assert!(classify_hr(0).is_none());
        // AUDCLNT_E_UNSUPPORTED_FORMAT is inherently Backend (unsupported format).
        assert!(classify_hr(0x88890008u32 as i32).is_none());
    }
}
