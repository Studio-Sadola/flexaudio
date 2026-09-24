//! [`WasapiSystemBackend`] — WASAPI loopback of the system audio output.
//!
//! `exclude_self == false` (default): classic loopback that captures the mix flowing into a
//! render endpoint with `AUDCLNT_STREAMFLAGS_LOOPBACK`. The counterpart of Linux's
//! [`PwSystemBackend`](../flexaudio_os_linux). `device_id` selects the output endpoint
//! (`None` for the default render endpoint, `Some(id)` for the eRender endpoint whose
//! FriendlyName matches). `id` is a FriendlyName returned by [`list_output_devices`].
//!
//! `exclude_self == true`: captures all system audio except the host process's own audio
//! (its tree) (feedback prevention). Instead of classic loopback, it calls the process
//! loopback mechanism of the `process` module with [`ProcessMode::Exclude`] + the own PID
//! (`std::process::id()`) ([`crate::process::setup_process_loopback`]). The native format of
//! this path is the process-loopback fixed `(48000, 2)`. The mechanism is not tied to an
//! endpoint, so `device_id` is ignored.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use flexaudio_core::backend::{CaptureBackend, RawSink};
use flexaudio_core::types::{DeviceInfo, Error, ProcessMode, Result, SourceKind};

use windows::core::Interface;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    DEVICE_STATE_ACTIVE, WAVEFORMATEX,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL, STGM_READ};

use crate::common::{capture_loop, init_loopback_capture, map_hr, parse_mix_format, ComThread};

/// Safe fallback `(48000, 2)` returned by [`native_format`] when the default render endpoint
/// cannot be obtained (does not panic). If obtaining it fails in the actual `start`, an
/// [`Error`] is returned.
const FALLBACK_FORMAT: (u32, u16) = (48_000, 2);

/// Fixed native format `(48000, 2)` of the `exclude_self == true` (process loopback EXCLUDE)
/// path. Process loopback cannot use `GetMixFormat` and Initializes with a fixed WAVEFORMATEX
/// (`fixed_process_format` in [`crate::process`]), so native is this fixed value as well.
const PROCESS_LOOPBACK_FORMAT: (u32, u16) = (48_000, 2);

/// [`CaptureBackend`] that captures the system audio output.
///
/// `exclude_self == false` (default): classic loopback that initializes COM on a dedicated
/// thread, obtains `MMDeviceEnumerator` → `GetDefaultAudioEndpoint(eRender, eConsole)` →
/// `IAudioClient`, and Initializes it with `AUDCLNT_STREAMFLAGS_LOOPBACK`. Packets are fed
/// to [`RawSink::push`] event-driven.
///
/// `exclude_self == true`: captures all system audio except the host's own PID (its tree)
/// (feedback prevention). Instead of classic loopback, it calls
/// [`crate::process::setup_process_loopback`] with [`ProcessMode::Exclude`] +
/// `std::process::id()` and runs the same [`capture_loop`].
///
/// This type is `Send` (it holds only the stop flag, [`JoinHandle`], `exclude_self`, and the
/// cached format; the `!Send` COM interfaces are confined to the dedicated thread).
pub struct WasapiSystemBackend {
    /// Host self-exclusion flag. `true` selects the process loopback EXCLUDE path, `false`
    /// the classic loopback path.
    exclude_self: bool,
    /// Output endpoint selection. `None` for the default render endpoint, `Some(id)` for the
    /// eRender endpoint whose FriendlyName matches `id`. Not used when
    /// `exclude_self == true`.
    device_id: Option<String>,
    /// Running flag (double-start guard / stop request / drop check). `Send`.
    stop_flag: Arc<AtomicBool>,
    /// Handle of the thread that owns COM/capture (`Some` after start).
    handle: Option<JoinHandle<()>>,
    /// Native format determined and cached at `new` time.
    native: (u32, u16),
}

impl WasapiSystemBackend {
    /// Builds a new system loopback backend (does not connect yet).
    ///
    /// `exclude_self == false` (default path): queries the MixFormat of the endpoint that
    /// `device_id` points to (`None` for the default render endpoint) once and caches it. On
    /// failure, caches [`FALLBACK_FORMAT`] (`(48000, 2)`) (does not panic). Matching of
    /// `device_id` is not checked at construction but in [`start`](CaptureBackend::start),
    /// which actually opens it.
    ///
    /// `exclude_self == true` (process loopback EXCLUDE path): native is the process-loopback
    /// fixed [`PROCESS_LOOPBACK_FORMAT`] (`(48000, 2)`). No MixFormat query is made and
    /// `device_id` is not used (the mechanism is not tied to an endpoint).
    pub fn new(exclude_self: bool, device_id: Option<String>) -> Self {
        let native = if exclude_self {
            PROCESS_LOOPBACK_FORMAT
        } else {
            query_native_format(device_id.as_deref()).unwrap_or(FALLBACK_FORMAT)
        };
        Self {
            exclude_self,
            device_id,
            stop_flag: Arc::new(AtomicBool::new(false)),
            handle: None,
            native,
        }
    }
}

impl Default for WasapiSystemBackend {
    /// The default is classic loopback of the default render endpoint
    /// (`exclude_self == false` / `device_id == None`).
    fn default() -> Self {
        Self::new(false, None)
    }
}

/// Gets `(rate, channels)` from the MixFormat of the render endpoint that `device_id` points
/// to (`None` for the default). Returns `None` if it cannot be obtained (does not panic).
/// Temporarily initializes COM for the query.
fn query_native_format(device_id: Option<&str>) -> Option<(u32, u16)> {
    let _com = ComThread::new();
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let device = resolve_render_endpoint(&enumerator, device_id).ok()?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).ok()?;
        let pwfx = client.GetMixFormat().ok()?;
        if pwfx.is_null() {
            return None;
        }
        // Read packed fields by value copy (same approach as parse_mix_format).
        let rate = core::ptr::addr_of!((*pwfx).nSamplesPerSec).read_unaligned();
        let channels = core::ptr::addr_of!((*pwfx).nChannels).read_unaligned();
        CoTaskMemFree(Some(pwfx as *const _ as *const _));
        Some((rate, channels))
    }
}

/// Resolves `device_id` to a render endpoint.
///
/// `None` uses `GetDefaultAudioEndpoint(eRender, eConsole)`. `Some(id)` enumerates the
/// ACTIVE eRender endpoints and returns the first whose FriendlyName matches `id`. Returns
/// [`Error::DeviceNotFound`] if nothing matches.
///
/// # Safety
/// COM must already be initialized on the calling thread. `enumerator` must be valid.
unsafe fn resolve_render_endpoint(
    enumerator: &IMMDeviceEnumerator,
    device_id: Option<&str>,
) -> Result<IMMDevice> {
    let Some(id) = device_id else {
        return enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(|_e| Error::DeviceNotFound);
    };

    let collection = enumerator
        .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
        .map_err(|e| map_hr("IMMDeviceEnumerator::EnumAudioEndpoints", e))?;
    let count = collection
        .GetCount()
        .map_err(|e| map_hr("IMMDeviceCollection::GetCount", e))?;

    for i in 0..count {
        let device = match collection.Item(i) {
            Ok(d) => d,
            Err(_e) => continue,
        };
        if endpoint_friendly_name(&device).as_deref() == Some(id) {
            return Ok(device);
        }
    }
    Err(Error::DeviceNotFound)
}

/// Reads the FriendlyName from the endpoint's property store. `None` if it cannot be read.
///
/// # Safety
/// COM must already be initialized on the calling thread. `device` must be a valid
/// `IMMDevice`.
unsafe fn endpoint_friendly_name(device: &IMMDevice) -> Option<String> {
    let store = device.OpenPropertyStore(STGM_READ).ok()?;
    let value = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
    // PROPVARIANT's Display stringifies via PropVariantToBSTR (it also handles VT_LPWSTR).
    let name = value.to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// Enumerates the active render (output) endpoints and returns a list of [`DeviceInfo`].
///
/// For each `DeviceInfo`, `id` / `name` are the endpoint's FriendlyName, `sample_rate` /
/// `channels` come from the MixFormat, `source_kind` is [`SourceKind::SystemLoopback`], and
/// `is_loopback` is always `true`. `is_default` is set on the entry whose FriendlyName
/// matches the default render endpoint. Endpoints whose FriendlyName cannot be read are
/// skipped.
pub fn list_output_devices() -> Result<Vec<DeviceInfo>> {
    let _com = ComThread::new();
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| map_hr("CoCreateInstance(MMDeviceEnumerator)", e))?;

        // FriendlyName of the default render endpoint (for matching is_default). Enumeration
        // continues even without it.
        let default_name = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .ok()
            .and_then(|d| endpoint_friendly_name(&d));

        let collection = enumerator
            .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
            .map_err(|e| map_hr("IMMDeviceEnumerator::EnumAudioEndpoints", e))?;
        let count = collection
            .GetCount()
            .map_err(|e| map_hr("IMMDeviceCollection::GetCount", e))?;

        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            let device = match collection.Item(i) {
                Ok(d) => d,
                Err(_e) => continue,
            };
            // Endpoints without a FriendlyName cannot produce an id, so skip them.
            let Some(name) = endpoint_friendly_name(&device) else {
                continue;
            };
            // rate/channels from the MixFormat. If unavailable, the requested native
            // (48000/2).
            let (sample_rate, channels) = endpoint_mix_format(&device).unwrap_or(FALLBACK_FORMAT);
            let is_default = default_name.as_deref() == Some(name.as_str());
            out.push(DeviceInfo {
                id: name.clone(),
                name,
                source_kind: SourceKind::SystemLoopback,
                sample_rate,
                channels,
                is_loopback: true,
                is_default,
            });
        }
        Ok(out)
    }
}

/// Gets `(rate, channels)` from the endpoint's MixFormat. `None` if unavailable.
///
/// # Safety
/// COM must already be initialized on the calling thread. `device` must be a valid
/// `IMMDevice`.
unsafe fn endpoint_mix_format(device: &IMMDevice) -> Option<(u32, u16)> {
    let client: IAudioClient = device.Activate(CLSCTX_ALL, None).ok()?;
    let pwfx = client.GetMixFormat().ok()?;
    if pwfx.is_null() {
        return None;
    }
    let rate = core::ptr::addr_of!((*pwfx).nSamplesPerSec).read_unaligned();
    let channels = core::ptr::addr_of!((*pwfx).nChannels).read_unaligned();
    CoTaskMemFree(Some(pwfx as *const _ as *const _));
    Some((rate, channels))
}

impl CaptureBackend for WasapiSystemBackend {
    fn native_format(&self) -> (u32, u16) {
        self.native
    }

    fn start(&mut self, sink: RawSink) -> Result<()> {
        // Safe against double start: do nothing if the thread is already alive.
        if self.handle.is_some() {
            return Ok(());
        }
        // Reset the flag so it can be restarted even after a previous stop.
        self.stop_flag.store(false, Ordering::SeqCst);

        let stop_flag = self.stop_flag.clone();
        let exclude_self = self.exclude_self;
        let device_id = self.device_id.clone();
        // Channel that synchronously returns the success or failure of setup (COM init →
        // Initialize → just before Start).
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();

        let handle = thread::Builder::new()
            .name("flexaudio-wasapi-system".into())
            .spawn(move || {
                run_system_thread(exclude_self, device_id, sink, stop_flag, ready_tx);
            })
            .map_err(|e| Error::Backend(format!("spawn wasapi system thread: {e}")))?;

        match ready_rx.recv() {
            Ok(Ok(())) => {
                self.handle = Some(handle);
                Ok(())
            }
            Ok(Err(e)) => {
                // Setup failed. The thread exits right after sending ready, so join.
                self.stop_flag.store(false, Ordering::SeqCst);
                let _ = handle.join();
                Err(e)
            }
            Err(_) => {
                self.stop_flag.store(false, Ordering::SeqCst);
                let _ = handle.join();
                Err(Error::Backend(
                    "wasapi system thread exited before reporting readiness".into(),
                ))
            }
        }
    }

    fn stop(&mut self) {
        // Safe against reentry and double stop.
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for WasapiSystemBackend {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Body of the owning thread. Initializes COM, configures loopback according to
/// `exclude_self`, and runs the capture loop. Reports setup success or failure to
/// [`WasapiSystemBackend::start`] through `ready_tx`. The COM guard (`_com`) is confined to
/// this function's scope and `CoUninitialize` is called when the thread ends (declaration
/// order ensures it is dropped after the COM objects).
///
/// - `exclude_self == false`: classic loopback of the render endpoint that `device_id`
///   points to (`None` for the default) ([`setup_system_loopback`]).
/// - `exclude_self == true`: process loopback passing the host's own PID with
///   [`ProcessMode::Exclude`] ([`crate::process::setup_process_loopback`]). `device_id` is
///   not used.
///
/// Both return the same 4-tuple `(IAudioClient, IAudioCaptureClient, HANDLE, u16)`, so
/// they converge on the shared [`capture_loop`] from there on.
fn run_system_thread(
    exclude_self: bool,
    device_id: Option<String>,
    sink: RawSink,
    stop_flag: Arc<AtomicBool>,
    ready_tx: mpsc::Sender<Result<()>>,
) {
    // Initialize COM on this thread (uninit on drop). Declared first = dropped last.
    let _com = ComThread::new();

    // Run setup and obtain the Initialized client / capture / event / channels.
    // The path branches on exclude_self, but the return type is the same for both.
    let setup = if exclude_self {
        // EXCLUDE the host's own PID (its tree) and capture all system audio (feedback
        // prevention). Reuses the process loopback mechanism of the `process` module as is.
        unsafe { crate::process::setup_process_loopback(std::process::id(), ProcessMode::Exclude) }
    } else {
        // Classic loopback of the render endpoint that device_id points to (None for the
        // default).
        unsafe { setup_system_loopback(device_id.as_deref(), &sink) }
    };
    let (client, capture, event, channels) = match setup {
        Ok(t) => t,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    // Report setup success. From here on, the capture loop (calls client.Start()
    // internally).
    if ready_tx.send(Ok(())).is_err() {
        // The caller is gone. Return without Start (COM is cleaned up on drop).
        return;
    }

    unsafe { capture_loop(&client, &capture, event, channels, sink, &stop_flag) };
    // capture_loop calls client.Stop() and CloseHandle(event).
    // Leaving here drops capture → client → _com in that order (reverse declaration
    // order).
}

/// Sets up classic loopback of the render endpoint that `device_id` points to (`None` for
/// the default) and returns the Initialized `IAudioClient` / `IAudioCaptureClient` / event
/// handle / channel count. Returns [`Error::DeviceNotFound`] if `device_id` does not match.
///
/// `sink` is not used for the channel-count check (verifying it matches native); only a
/// reference is taken (ownership stays with the caller, which hands it to the loop).
///
/// # Safety
/// COM must already be initialized on the calling thread.
#[allow(clippy::type_complexity)]
unsafe fn setup_system_loopback(
    device_id: Option<&str>,
    _sink: &RawSink,
) -> Result<(
    IAudioClient,
    windows::Win32::Media::Audio::IAudioCaptureClient,
    windows::Win32::Foundation::HANDLE,
    u16,
)> {
    let enumerator: IMMDeviceEnumerator =
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .map_err(|e| map_hr("CoCreateInstance(MMDeviceEnumerator)", e))?;

    // The render endpoint that device_id points to (None for the default). DeviceNotFound
    // if nothing matches.
    let device: IMMDevice = resolve_render_endpoint(&enumerator, device_id)?;

    let client: IAudioClient = device
        .Activate(CLSCTX_ALL, None)
        .map_err(|e| map_hr("IMMDevice::Activate(IAudioClient)", e))?;

    // Shared MixFormat (must be freed with CoTaskMemFree after use).
    let pwfx: *mut WAVEFORMATEX = client
        .GetMixFormat()
        .map_err(|e| map_hr("IAudioClient::GetMixFormat", e))?;

    // Check the format (only IEEE float is passed through directly). Note rate/channels
    // here.
    let parsed = parse_mix_format(pwfx as *const WAVEFORMATEX);
    let (_rate, channels) = match parsed {
        Ok(v) => v,
        Err(e) => {
            CoTaskMemFree(Some(pwfx as *const _ as *const _));
            return Err(e);
        }
    };

    // Initialize (LOOPBACK|EVENTCALLBACK) → event → capture service.
    let init = init_loopback_capture(&client, pwfx as *const WAVEFORMATEX, 0);
    // Initialize copies the format, so pwfx can be freed here.
    CoTaskMemFree(Some(pwfx as *const _ as *const _));
    let (capture, event) = init?;

    // Ensure the Interface is alive just in case (unused, but makes the drop order
    // explicit).
    let _ = client.as_raw();

    Ok((client, capture, event, channels))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio_core::raw_ring;

    /// `new` + `native_format` do not panic (whether or not a render endpoint exists).
    #[test]
    fn new_and_native_format_do_not_panic() {
        let backend = WasapiSystemBackend::new(false, None);
        let (rate, channels) = backend.native_format();
        assert!(rate > 0);
        assert!(channels > 0);
    }

    /// `new` does not panic even when given a device_id (matching is deferred until start).
    /// It can be built even with a nonexistent id, and native becomes a sensible fallback.
    #[test]
    fn new_with_device_id_does_not_panic() {
        let backend = WasapiSystemBackend::new(false, Some("no-such-endpoint".into()));
        let (rate, channels) = backend.native_format();
        assert!(rate > 0);
        assert!(channels > 0);
    }

    /// With `exclude_self == true`, native is always the process-loopback fixed
    /// `(48000, 2)`. No MixFormat is queried, so it is determined regardless of whether a
    /// render endpoint exists or of device_id.
    #[test]
    fn new_exclude_self_native_is_fixed() {
        let backend = WasapiSystemBackend::new(true, Some("ignored".into()));
        assert_eq!(backend.native_format(), (48_000, 2));
    }

    /// `list_output_devices` does not panic. Returned entries are treated as loopback.
    /// In environments without a render endpoint, an empty list or `Err` is allowed (only
    /// panics are not).
    #[test]
    fn list_output_devices_does_not_panic() {
        if let Ok(devices) = list_output_devices() {
            for d in &devices {
                assert!(d.is_loopback);
                assert_eq!(d.source_kind, SourceKind::SystemLoopback);
            }
        }
    }

    /// `start` → `stop` does not panic whether or not a device exists (classic loopback
    /// path). In environments where the render endpoint is missing / cannot be opened, `Err`
    /// is allowed (only panics are not).
    #[test]
    fn start_then_stop_tolerates_missing_endpoint() {
        let mut backend = WasapiSystemBackend::new(false, None);
        let (rate, channels) = backend.native_format();
        let cap = (rate as usize * channels as usize).max(1);
        let (prod, _cons) = raw_ring(cap);
        let sink = RawSink::new(prod, rate, channels);

        match backend.start(sink) {
            Ok(()) => {
                backend.stop();
                backend.stop(); // Double stop is safe too.
            }
            Err(_e) => { /* no render endpoint / non-float etc. is allowed */ }
        }
    }

    /// `start` with a nonexistent device_id yields `DeviceNotFound` (does not panic).
    #[test]
    fn start_with_unknown_device_id_is_device_not_found() {
        let mut backend = WasapiSystemBackend::new(false, Some("no-such-endpoint".into()));
        let (rate, channels) = backend.native_format();
        let cap = (rate as usize * channels as usize).max(1);
        let (prod, _cons) = raw_ring(cap);
        let sink = RawSink::new(prod, rate, channels);

        match backend.start(sink) {
            Ok(()) => {
                // In the unlikely environment where it does match, success is allowed too
                // (only check that it stops).
                backend.stop();
            }
            Err(e) => assert!(matches!(e, Error::DeviceNotFound)),
        }
    }

    /// `start` → `stop` does not panic with `exclude_self == true` (process loopback
    /// EXCLUDE path) either. `Err` is allowed for an unsupported OS / activation failure.
    #[test]
    fn start_then_stop_exclude_self_tolerates_failure() {
        let mut backend = WasapiSystemBackend::new(true, None);
        let (rate, channels) = backend.native_format();
        let cap = (rate as usize * channels as usize).max(1);
        let (prod, _cons) = raw_ring(cap);
        let sink = RawSink::new(prod, rate, channels);

        match backend.start(sink) {
            Ok(()) => {
                backend.stop();
                backend.stop(); // Double stop is safe too.
            }
            Err(_e) => { /* unsupported OS / process loopback activation failure is allowed */ }
        }
    }
}
