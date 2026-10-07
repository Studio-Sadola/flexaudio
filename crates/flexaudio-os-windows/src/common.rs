//! Shared Windows backend helpers: COM initialization guard, WAVEFORMATEX
//! parsing, HRESULT-to-[`Error`] mapping, and the WASAPI capture loop.
//!
//! [`WasapiSystemBackend`](crate::WasapiSystemBackend) and
//! [`WasapiProcessBackend`](crate::WasapiProcessBackend) run the same capture
//! loop ([`capture_loop`]) on dedicated threads. They differ only in how they
//! obtain `IAudioClient` (classic loopback or process loopback activation) and
//! choose the format (`GetMixFormat` or a fixed WAVEFORMATEX). The sequence from
//! Initialize through GetService, Start, capture, and Stop is shared.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use flexaudio_core::backend::RawSink;
use flexaudio_core::clock::monotonic_now_ns;
use flexaudio_core::types::{Error, Permission};

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

/// Classify access-denied and device-unavailable HRESULTs as typed [`Error`] variants.
///
/// Map common WASAPI/COM HRESULTs to cross-platform error types:
/// - Access denied → [`Error::PermissionDenied`]: `E_ACCESSDENIED` for
///   system/process capture. Microphone consent is checked in `flexaudio-mic`.
/// - Exclusive-use/policy conflicts → [`Error::Backend`]; these do not establish
///   a recording-consent denial.
/// - Device unavailable or invalidated → [`Error::DeviceNotFound`]:
///   `AUDCLNT_E_DEVICE_INVALIDATED` (the endpoint/device disappeared or was
///   invalidated) and `E_NOTFOUND` (the item/endpoint does not exist).
///
/// Return `None` for other codes; [`map_hr`] falls back to a contextual
/// [`Error::Backend`]. Constant imports vary across `windows` crate versions, so
/// compare stable raw i32 values here (hex values are noted in comments below).
pub(crate) fn classify_hr(code: i32) -> Option<Error> {
    // Access denied → PermissionDenied
    const E_ACCESSDENIED: i32 = 0x80070005u32 as i32;
    const AUDCLNT_E_DEVICE_IN_USE: i32 = 0x8889000Au32 as i32;
    const AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED: i32 = 0x8889000Eu32 as i32;
    // Device unavailable/invalidated → DeviceNotFound
    const AUDCLNT_E_DEVICE_INVALIDATED: i32 = 0x88890004u32 as i32;
    // E_NOTFOUND (ERROR_NOT_FOUND converted to HRESULT 0x80070490): specified endpoint/item missing.
    const E_NOTFOUND: i32 = 0x80070490u32 as i32;

    match code {
        E_ACCESSDENIED => Some(Error::PermissionDenied {
            permission: Permission::SystemAudio,
            detail: "Windows denied access to system/process audio capture (E_ACCESSDENIED)".into(),
        }),
        AUDCLNT_E_DEVICE_IN_USE => Some(Error::Backend("audio device is in exclusive use".into())),
        AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED => {
            Some(Error::Backend("exclusive audio mode is disallowed".into()))
        }
        AUDCLNT_E_DEVICE_INVALIDATED | E_NOTFOUND => Some(Error::DeviceNotFound),
        _ => None,
    }
}

/// Convert an HRESULT to [`Error`] with a context string.
///
/// [`classify_hr`] maps access-denied and device-unavailable codes to typed
/// variants ([`Error::PermissionDenied`] / [`Error::DeviceNotFound`]); unknown
/// codes fall back to a contextual [`Error::Backend`].
pub(crate) fn map_hr(ctx: &str, e: windows::core::Error) -> Error {
    if let Some(mapped) = classify_hr(e.code().0) {
        return mapped;
    }
    Error::Backend(format!("{ctx}: {e}"))
}

/// Monotonic clock in ns. Uses the core [`monotonic_now_ns`] directly. The
/// downstream `ClockNormalizer` sets the initial origin, so a monotonic
/// approximation of arrival time is sufficient.
pub(crate) fn now_ns() -> i64 {
    monotonic_now_ns()
}

/// COM initialization guard. Calls `CoInitializeEx(MULTITHREADED)` in `new` and
/// `CoUninitialize` in `Drop` (both on the same thread).
///
/// Do not treat `RPC_E_CHANGED_MODE` (already initialized in another mode) as a
/// failure. WASAPI calls still work if another component initialized STA; in
/// that case set `uninit_on_drop=false` so this guard does not call
/// `CoUninitialize` for another component's initialization.
pub(crate) struct ComThread {
    uninit_on_drop: bool,
}

impl ComThread {
    /// Initialize COM on this thread. Does not panic.
    pub(crate) fn new() -> Self {
        // In version 0.54, CoInitializeEx returns HRESULT, not Result.
        // S_OK / S_FALSE indicate success; RPC_E_CHANGED_MODE means another mode is already initialized.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        // On success (S_OK=0 / S_FALSE=1), this call initialized COM, so uninitialize on drop.
        // For RPC_E_CHANGED_MODE and similar cases, another component initialized it; do not uninitialize.
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

/// Parse `WAVEFORMATEX` (or `WAVEFORMATEXTENSIBLE` if needed) and return
/// `Ok((rate, channels))` if the subformat is IEEE float. PCM integer formats
/// are unsupported and return [`Error::Backend`]. Shared-mode MixFormat is
/// typically float on real hardware.
///
/// `WAVEFORMATEX` / `WAVEFORMATEXTENSIBLE` use `#[repr(C, packed(1))]`, so
/// creating a reference to a packed field is UB. Copy values with `addr_of!` +
/// `read_unaligned` without taking references.
///
/// # Safety
/// `pwfx` must point to a valid `WAVEFORMATEX` (returned by `GetMixFormat`).
pub(crate) unsafe fn parse_mix_format(pwfx: *const WAVEFORMATEX) -> Result<(u32, u16), Error> {
    use core::ptr::addr_of;

    if pwfx.is_null() {
        return Err(Error::Backend("GetMixFormat returned null format".into()));
    }
    // Read packed fields by copying their values.
    let format_tag = addr_of!((*pwfx).wFormatTag).read_unaligned();
    let rate = addr_of!((*pwfx).nSamplesPerSec).read_unaligned();
    let channels = addr_of!((*pwfx).nChannels).read_unaligned();
    let bits = addr_of!((*pwfx).wBitsPerSample).read_unaligned();
    let cb_size = addr_of!((*pwfx).cbSize).read_unaligned();

    // Constants are u32. Compare with `==` so identifiers in match patterns are not mistaken for bindings.
    let tag = format_tag as u32;
    let is_float = if tag == WAVE_FORMAT_IEEE_FLOAT {
        true
    } else if tag == WAVE_FORMAT_EXTENSIBLE {
        // Treat as EXTENSIBLE if cbSize is at least 22 bytes, the extension size.
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

/// Event-driven WASAPI capture loop, run on a dedicated thread.
///
/// Receive an initialized `client` (shared mode, LOOPBACK|EVENTCALLBACK), its
/// capture service, event handle, and channel count. Read packets and send them
/// to [`RawSink::push`] until `stop_flag` is set. On exit, call `client.Stop()`
/// and close the event handle.
///
/// Read packets as interleaved f32 (`channels` channels). WASAPI buffers are
/// allocated on at least 8-byte boundaries, so casting to `*const f32` is safe.
/// When `AUDCLNT_BUFFERFLAGS_SILENT` is set, push `frames*channels` zeros to
/// prevent DC offset.
///
/// # Safety
/// `client`, `capture`, and `event` must be valid COM objects/handles initialized
/// on the same thread. `channels >= 1`.
pub(crate) unsafe fn capture_loop(
    client: &IAudioClient,
    capture: &IAudioCaptureClient,
    event: HANDLE,
    channels: u16,
    mut sink: RawSink,
    stop_flag: &Arc<AtomicBool>,
) {
    let channels = channels.max(1) as usize;
    // Reusable buffer for silence packets. Allocate the maximum expected size
    // before entering the RT loop (before Start) to avoid allocations in the
    // loop. The engine buffer size (`GetBufferSize`) limits the frames in one
    // packet, so allocating `buffer_frames * channels` makes loop `resize` calls
    // no-ops within capacity. If `GetBufferSize` fails, leave it empty and fall
    // back to resizing on the first loop iteration.
    let mut silence: Vec<f32> = Vec::new();
    if let Ok(buffer_frames) = client.GetBufferSize() {
        let max_silence = (buffer_frames as usize).saturating_mul(channels);
        silence.resize(max_silence, 0.0);
    }

    if client.Start().is_err() {
        // Return if Start fails. This normally cannot happen because setup already called Start.
        let _ = CloseHandle(event);
        return;
    }

    while !stop_flag.load(Ordering::SeqCst) {
        // Wake on event or after 100ms. The timeout ensures a stop request is not missed.
        let _ = WaitForSingleObject(event, 100);
        if stop_flag.load(Ordering::SeqCst) {
            break;
        }

        loop {
            let packet = match capture.GetNextPacketSize() {
                Ok(p) => p,
                Err(_e) => {
                    // The target PID may have exited, invalidating the device. Exit the loop and stop.
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
            // Wrap push in catch_unwind. This is our own thread, not an FFI
            // boundary, but catching a panic from `RawSink::push` ensures the
            // `ReleaseBuffer`, `client.Stop()`, and `CloseHandle` cleanup below
            // still runs. Continue processing after catching it.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                if (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 {
                    // Silence: push n zeros for downstream gap detection and to prevent DC offset.
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

            // Always release the acquired frames, passing frames on success or failure.
            let _ = capture.ReleaseBuffer(frames);
        }
    }

    let _ = client.Stop();
    let _ = CloseHandle(event);
}

/// Shared sequence that initializes `client` in shared mode with
/// LOOPBACK|EVENTCALLBACK, attaches an event handle, and gets the capture
/// service. On success, returns `(IAudioCaptureClient, event_handle)`.
///
/// `pwfx` is the format passed to Initialize (System passes the raw pointer from
/// `GetMixFormat`; Process passes a pointer to its fixed WAVEFORMATEX).
/// Always set `AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK`.
/// `extra_streamflags` are added to those flags (process loopback uses
/// `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`, as in the official ApplicationLoopback
/// sample; classic loopback uses `0`).
///
/// # Safety
/// `client` must be a valid COM object activated on the same thread. `pwfx` must
/// point to a valid `WAVEFORMATEX`.
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

    // Auto-reset, initially non-signaled, unnamed event.
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

/// Check whether `WaitForSingleObject` returned the signaled value (`WAIT_OBJECT_0`).
/// Used by the process backend while waiting for activation to complete.
pub(crate) fn wait_event_signaled(handle: HANDLE, timeout_ms: u32) -> bool {
    let r = unsafe { WaitForSingleObject(handle, timeout_ms) };
    r == WAIT_OBJECT_0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Access-denied HRESULTs are classified as PermissionDenied.
    #[test]
    fn classify_hr_maps_access_denied_to_permission_denied() {
        // E_ACCESSDENIED
        assert!(matches!(
            classify_hr(0x80070005u32 as i32),
            Some(Error::PermissionDenied {
                permission: Permission::SystemAudio,
                ..
            })
        ));
        // AUDCLNT_E_DEVICE_IN_USE
        assert!(matches!(
            classify_hr(0x8889000Au32 as i32),
            Some(Error::Backend(_))
        ));
        // AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED
        assert!(matches!(
            classify_hr(0x8889000Eu32 as i32),
            Some(Error::Backend(_))
        ));
    }

    /// Device-unavailable/invalidated HRESULTs are classified as DeviceNotFound.
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

    /// Unclassified HRESULTs return None (map_hr falls back to Backend).
    #[test]
    fn classify_hr_unknown_is_none() {
        // E_FAIL (generic failure) is not classified.
        assert!(classify_hr(0x80004005u32 as i32).is_none());
        // S_OK is not a failure, so it naturally returns None.
        assert!(classify_hr(0).is_none());
        // AUDCLNT_E_UNSUPPORTED_FORMAT is inherently a Backend error (unsupported format).
        assert!(classify_hr(0x88890008u32 as i32).is_none());
    }
}
