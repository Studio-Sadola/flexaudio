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
use std::sync::{mpsc, Arc};

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
use windows::Win32::System::Threading::{
    CreateEventW, WaitForMultipleObjects, WaitForSingleObject,
};

use crate::keepalive::SilentRender;
use crate::lifecycle::{check_wait_result, Session, StreamClient};
use flexaudio_core::{ErrorContext, NativeStatus, Operation, ShutdownReport};

/// Classify access-denied and device-unavailable HRESULTs as typed [`Error`] variants.
///
/// Map common WASAPI/COM HRESULTs to cross-platform error types:
/// - Access denied → [`Error::PermissionDenied`]: `E_ACCESSDENIED` for
///   system/process capture. Microphone consent is checked in `flexaudio-mic`.
/// - Exclusive-use/policy conflicts → [`Error::Backend`]; these do not establish
///   a recording-consent denial.
/// - Device invalidation → [`Error::DeviceLost`]; missing endpoint → [`Error::DeviceNotFound`]:
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
        AUDCLNT_E_DEVICE_INVALIDATED => Some(Error::DeviceLost),
        E_NOTFOUND => Some(Error::DeviceNotFound),
        _ => None,
    }
}

/// Convert an HRESULT to [`Error`] with a context string.
///
/// [`classify_hr`] maps access-denied and device-unavailable codes to typed
/// variants ([`Error::PermissionDenied`] / [`Error::DeviceNotFound`]); unknown
/// codes fall back to a contextual [`Error::Backend`].
pub(crate) fn map_hr(call: &'static str, e: windows::core::Error) -> Error {
    map_hr_at(Operation::Start, call, e)
}

pub(crate) fn map_hr_at(
    operation: Operation,
    call: &'static str,
    e: windows::core::Error,
) -> Error {
    let code = e.code();
    // Derive the explanation from the OS status, never an attached external message.
    classify_hr(code.0)
        .unwrap_or_else(|| Error::Backend(windows::core::Error::from(code).message().to_string()))
        .with_context(
            ErrorContext::new(operation).with_native_status(NativeStatus::HResult {
                call,
                bits: code.0 as u32,
            }),
        )
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
/// are unsupported and return [`Error::UnsupportedFormat`]. Shared-mode MixFormat is
/// typically float on real hardware.
///
/// `WAVEFORMATEX` / `WAVEFORMATEXTENSIBLE` use `#[repr(C, packed(1))]`, so
/// creating a reference to a packed field is UB. Copy values with `addr_of!` +
/// `read_unaligned` without taking references.
///
/// # Safety
/// `pwfx` must point to a valid `WAVEFORMATEX` (returned by `GetMixFormat`),
/// including its declared extension bytes when `cbSize >= 22`.
pub(crate) unsafe fn parse_mix_format(pwfx: *const WAVEFORMATEX) -> Result<(u32, u16), Error> {
    use core::ptr::addr_of;

    if pwfx.is_null() {
        return Err(
            Error::Backend("native format query returned no description".into())
                .with_context(ErrorContext::new(Operation::Start)),
        );
    }
    // Read packed fields by copying their values.
    let format_tag = addr_of!((*pwfx).wFormatTag).read_unaligned();
    let rate = addr_of!((*pwfx).nSamplesPerSec).read_unaligned();
    let channels = addr_of!((*pwfx).nChannels).read_unaligned();
    let bits = addr_of!((*pwfx).wBitsPerSample).read_unaligned();
    let cb_size = addr_of!((*pwfx).cbSize).read_unaligned();
    let alignment = addr_of!((*pwfx).nBlockAlign).read_unaligned();
    let byte_rate = addr_of!((*pwfx).nAvgBytesPerSec).read_unaligned();

    if rate == 0 || channels == 0 {
        return Err(Error::InvalidArg(
            "native rate and channels must be positive".into(),
        ));
    }
    if channels > 2 || bits != 32 || alignment != channels * 4 {
        return Err(Error::UnsupportedFormat(
            "native input must be mono or stereo f32 with complete frames".into(),
        ));
    }
    if rate.checked_mul(u32::from(alignment)) != Some(byte_rate) {
        return Err(Error::UnsupportedFormat(
            "native byte rate does not match frame alignment".into(),
        ));
    }

    // Constants are u32. Compare with `==` so identifiers in match patterns are not mistaken for bindings.
    let tag = format_tag as u32;
    let is_float = if tag == WAVE_FORMAT_IEEE_FLOAT {
        true
    } else if tag == WAVE_FORMAT_EXTENSIBLE {
        // Treat as EXTENSIBLE if cbSize is at least 22 bytes, the extension size.
        if (cb_size as usize) >= 22 {
            let extension = pwfx.cast::<WAVEFORMATEXTENSIBLE>();
            let valid_bits = addr_of!((*extension).Samples.wValidBitsPerSample).read_unaligned();
            let mask = addr_of!((*extension).dwChannelMask).read_unaligned();
            if valid_bits != 32 || (mask != 0 && mask.count_ones() != u32::from(channels)) {
                return Err(Error::UnsupportedFormat(
                    "native valid bits or speaker mask do not match the float format".into(),
                ));
            }
            let sub = addr_of!((*extension).SubFormat).read_unaligned();
            sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
        } else {
            false
        }
    } else {
        false
    };

    if !is_float {
        return Err(Error::UnsupportedFormat(format!(
            "unsupported mix format (not IEEE float): tag={tag} bits={bits}"
        )));
    }
    Ok((rate, channels))
}

/// Close a thread-owned event on every return path.
pub(crate) struct EventHandle(pub(crate) HANDLE);

impl Drop for EventHandle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Initialized capture resources; only classic loopback supplies a keepalive.
pub(crate) struct CaptureSetup {
    pub(crate) client: IAudioClient,
    pub(crate) capture: IAudioCaptureClient,
    pub(crate) event: EventHandle,
    pub(crate) channels: u16,
    pub(crate) keepalive: Option<SilentRender>,
}

impl CaptureSetup {
    pub(crate) fn process(
        (client, capture, event, channels): (IAudioClient, IAudioCaptureClient, HANDLE, u16),
    ) -> Self {
        Self {
            client,
            capture,
            event: EventHandle(event),
            channels,
            keepalive: None,
        }
    }
}

struct CaptureClient<'a>(&'a IAudioClient);

impl StreamClient for CaptureClient<'_> {
    fn start(&mut self) -> Result<(), Error> {
        unsafe { self.0.Start() }.map_err(|e| map_hr("IAudioClient::Start(capture)", e))
    }
    fn stop(&mut self) -> Result<(), Error> {
        unsafe { self.0.Stop() }
            .map_err(|e| map_hr_at(Operation::Stop, "IAudioClient::Stop(capture)", e))
    }
    fn fill_silence(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

/// Start capture before reporting readiness, then drain device-clocked packets.
/// Render and capture events share one owner; all runtime failures stop production
/// and are returned to that owner for the existing watchdog/reopen error path.
///
/// # Safety
/// The setup must have been initialized on this thread with a valid float format.
pub(crate) unsafe fn capture_loop(
    setup: CaptureSetup,
    mut sink: RawSink,
    stop_flag: &Arc<AtomicBool>,
    ready: mpsc::Sender<Result<(), Error>>,
) -> Result<(), Error> {
    let CaptureSetup {
        client,
        capture,
        event,
        channels,
        keepalive,
    } = setup;
    let channels = usize::from(channels);
    // Allocate before either client starts; never resize in the packet loop.
    let buffer_frames = match client.GetBufferSize() {
        Ok(frames) => frames,
        Err(e) => {
            let _ = ready.send(Err(map_hr("IAudioClient::GetBufferSize(capture)", e)));
            return Ok(());
        }
    };
    let silence = vec![0.0f32; buffer_frames as usize * channels];
    let mut events = [event.0, event.0];
    let event_count = if let Some(render) = &keepalive {
        events[1] = render.event.0;
        2
    } else {
        1
    };
    let Some(mut session) =
        Session::start_and_report(CaptureClient(&client), keepalive, |status| {
            ready.send(status).is_ok()
        })
    else {
        return Ok(());
    };

    let result = (|| {
        while !stop_flag.load(Ordering::SeqCst) {
            // Either client event services both clients; timeout bounds stop latency.
            let wait = WaitForMultipleObjects(&events[..event_count], false, 100);
            if wait.0 == u32::MAX {
                return Err(map_hr(
                    "WASAPI event wait (WAIT_FAILED)",
                    windows::core::Error::from_win32(),
                ));
            }
            check_wait_result(wait.0, event_count as u32)?;
            if stop_flag.load(Ordering::SeqCst) {
                break;
            }
            session.service_keepalive()?;

            loop {
                if stop_flag.load(Ordering::SeqCst) {
                    break;
                }
                let packet = capture
                    .GetNextPacketSize()
                    .map_err(|e| map_hr("IAudioCaptureClient::GetNextPacketSize", e))?;
                if packet == 0 {
                    break;
                }

                let mut pdata: *mut u8 = std::ptr::null_mut();
                let mut frames = 0;
                let mut flags = 0;
                capture
                    .GetBuffer(&mut pdata, &mut frames, &mut flags, None, None)
                    .map_err(|e| map_hr("IAudioCaptureClient::GetBuffer", e))?;
                let n = frames as usize * channels;
                // Release every acquired packet, even if validation or the sink fails.
                let pushed = catch_unwind(AssertUnwindSafe(|| -> Result<(), Error> {
                    if n > silence.len() {
                        return Err(Error::Backend(
                            "WASAPI packet exceeds capture buffer size".into(),
                        ));
                    }
                    if (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 {
                        sink.push(&silence[..n], now_ns());
                    } else if n > 0 {
                        if pdata.is_null() {
                            return Err(Error::Backend(
                                "WASAPI returned a null non-silent packet".into(),
                            ));
                        }
                        let slice = std::slice::from_raw_parts(pdata as *const f32, n);
                        sink.push(slice, now_ns());
                    }
                    Ok(())
                }));
                capture
                    .ReleaseBuffer(frames)
                    .map_err(|e| map_hr("IAudioCaptureClient::ReleaseBuffer", e))?;
                pushed.map_err(|_| Error::Backend("WASAPI capture sink panicked".into()))??;
                // Service render during long capture drains as well as on event wakes.
                session.service_keepalive()?;
            }
        }
        Ok(())
    })();
    let stopped = session.stop();
    // Session drops before capture/event/COM, and render stops after capture.
    ShutdownReport::new(result.err(), stopped.err().into_iter().collect()).result()
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

    /// Missing endpoints and invalidated devices retain distinct error kinds.
    #[test]
    fn classify_hr_maps_device_codes_to_device_not_found() {
        // AUDCLNT_E_DEVICE_INVALIDATED
        assert!(matches!(
            classify_hr(0x88890004u32 as i32),
            Some(Error::DeviceLost)
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

#[cfg(test)]
mod repro_tests {
    use super::*;

    #[test]
    fn repro_p6_device_invalidation_is_device_lost() {
        assert!(matches!(
            classify_hr(0x88890004u32 as i32),
            Some(Error::DeviceLost)
        ));
    }

    #[test]
    fn repro_p6_classified_hresult_retains_operation() {
        let error = map_hr(
            "IAudioClient::Initialize",
            windows::core::Error::from(windows::core::HRESULT(0x8889000Au32 as i32)),
        );
        assert_eq!(error.kind(), flexaudio_core::ErrorKind::Backend);
        let Error::Context { context, .. } = &error else {
            panic!("missing native context")
        };
        assert_eq!(context.operation(), Operation::Start);
        assert_eq!(
            context.native_status(),
            Some(NativeStatus::HResult {
                call: "IAudioClient::Initialize",
                bits: 0x8889000A
            })
        );
        assert!(!error.to_string().contains("IAudioClient::Initialize"));
        assert!(error.to_string().contains("exclusive use"));
    }

    fn float_format() -> WAVEFORMATEX {
        WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16,
            nChannels: 2,
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 384_000,
            nBlockAlign: 8,
            wBitsPerSample: 32,
            cbSize: 0,
        }
    }

    #[test]
    fn supported_float_formats_remain_accepted() {
        let mut format = float_format();
        // SAFETY: a complete, initialized native description with no extension.
        assert_eq!(unsafe { parse_mix_format(&format) }, Ok((48_000, 2)));
        format.nChannels = 1;
        format.nBlockAlign = 4;
        format.nAvgBytesPerSec = 192_000;
        // SAFETY: complete mono native description.
        assert_eq!(unsafe { parse_mix_format(&format) }, Ok((48_000, 1)));
        let mut format = WAVEFORMATEXTENSIBLE {
            Format: float_format(),
            Samples: windows::Win32::Media::Audio::WAVEFORMATEXTENSIBLE_0 {
                wValidBitsPerSample: 32,
            },
            dwChannelMask: 3,
            SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        };
        format.Format.wFormatTag = WAVE_FORMAT_EXTENSIBLE as u16;
        format.Format.cbSize = 22;
        for mask in [3, 0] {
            format.dwChannelMask = mask; // An unspecified speaker mask is allowed.
                                         // SAFETY: the complete initialized extension matches cbSize.
            assert_eq!(
                unsafe { parse_mix_format(std::ptr::addr_of!(format.Format)) },
                Ok((48_000, 2))
            );
        }
    }

    #[test]
    fn repro_p6_float_sample_width_must_be_32() {
        let mut format = float_format();
        format.wBitsPerSample = 64;
        format.nBlockAlign = 16;
        format.nAvgBytesPerSec = 768_000;
        // SAFETY: a complete, initialized WAVEFORMATEX; no native calls or sample reads.
        assert!(unsafe { parse_mix_format(&format) }.is_err());
    }

    #[test]
    fn repro_p6_inconsistent_frame_alignment_is_rejected() {
        let mut format = float_format();
        format.nBlockAlign = 4; // Stereo f32 requires eight bytes per frame.
                                // SAFETY: a complete, initialized WAVEFORMATEX.
        assert!(unsafe { parse_mix_format(&format) }.is_err());
    }

    #[test]
    fn repro_p6_inconsistent_byte_rate_is_rejected() {
        let mut format = float_format();
        format.nAvgBytesPerSec = 1;
        // SAFETY: a complete, initialized WAVEFORMATEX.
        assert!(unsafe { parse_mix_format(&format) }.is_err());
    }

    #[test]
    fn repro_p6_extensible_channel_mask_matches_channel_count() {
        let mut format = WAVEFORMATEXTENSIBLE {
            Format: float_format(),
            Samples: windows::Win32::Media::Audio::WAVEFORMATEXTENSIBLE_0 {
                wValidBitsPerSample: 32,
            },
            dwChannelMask: 1, // One speaker bit cannot describe two channels.
            SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        };
        format.Format.wFormatTag = WAVE_FORMAT_EXTENSIBLE as u16;
        format.Format.cbSize = 22;
        // SAFETY: the complete extension is present and initialized, including cbSize.
        assert!(unsafe { parse_mix_format(std::ptr::addr_of!(format.Format)) }.is_err());
    }
}
