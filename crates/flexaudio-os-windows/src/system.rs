//! [`WasapiSystemBackend`] — WASAPI loopback for system audio output.
//!
//! `exclude_self == false` (default): Capture the mix sent to the render endpoint
//! with `AUDCLNT_STREAMFLAGS_LOOPBACK` (classic loopback), equivalent to Linux's
//! [`PwSystemBackend`](../flexaudio_os_linux). Select an output endpoint with
//! `device_id`: `None` uses the default render endpoint; `Some(id)` selects the
//! eRender endpoint whose FriendlyName matches. `id` is the FriendlyName returned
//! by [`list_output_devices`].
//!
//! `exclude_self == true`: Capture all system audio except this process and
//! its process tree (to prevent feedback). This uses the process loopback
//! mechanism in the `process` module with [`ProcessMode::Exclude`] and this
//! process's PID (`std::process::id()`), not classic loopback
//! ([`crate::process::setup_process_loopback`]). This path uses the fixed process
//! loopback native format `(48000, 2)`. Since it is not tied to an endpoint,
//! `device_id` is ignored.
//!
//! With a nonempty `exclude_pids` list, the EXCLUDE path is also taken when
//! `exclude_self == false`. WASAPI excludes one process tree: `exclude_self`
//! selects this process as the root; otherwise the first pid selects the root.
//! Every listed pid must equal that root (duplicates are accepted). A different
//! pid or pid 0 is rejected with [`Error::InvalidArg`] before capture starts
//! (see [`WasapiSystemBackend::with_exclude_pids`]).

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use flexaudio_core::backend::{CaptureBackend, RawSink};
use flexaudio_core::types::{DeviceInfo, Error, ProcessMode, Result, SourceKind};

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    DEVICE_STATE_ACTIVE, WAVEFORMATEX,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL, STGM_READ};

use crate::common::{
    capture_loop, init_loopback_capture, map_hr, parse_mix_format, CaptureSetup, ComThread,
    EventHandle,
};
use crate::format::{native_format_from_source, verify_format};
use crate::keepalive::SilentRender;
use crate::lifecycle::keepalive_error;

/// Safe fallback `(48000, 2)` returned by [`native_format`] when the default
/// render endpoint cannot be obtained. This does not panic. If retrieval fails
/// during `start`, it returns [`Error`].
const FALLBACK_FORMAT: (u32, u16) = (48_000, 2);

/// Fixed native format `(48000, 2)` for the `exclude_self == true` process
/// loopback EXCLUDE path. Process loopback cannot use `GetMixFormat` and is
/// initialized with a fixed WAVEFORMATEX (`fixed_process_format` in
/// [`crate::process`]), so `native` uses this fixed value too.
const PROCESS_LOOPBACK_FORMAT: (u32, u16) = (48_000, 2);

/// A [`CaptureBackend`] that captures system audio output.
///
/// `exclude_self == false` (default): Initialize COM on a dedicated thread,
/// `MMDeviceEnumerator` → `GetDefaultAudioEndpoint(eRender, eConsole)` →
/// obtain `IAudioClient`, and initialize classic loopback with
/// `AUDCLNT_STREAMFLAGS_LOOPBACK`. Forward packets to [`RawSink::push`] using
/// event-driven capture.
///
/// `exclude_self == true`: Capture all system audio except the calling process PID and its
/// process tree (to prevent feedback). Instead of classic loopback, call
/// [`crate::process::setup_process_loopback`] with [`ProcessMode::Exclude`] and
/// `std::process::id()`, then run the same [`capture_loop`].
///
/// This type is `Send`: it only holds a stop flag, a [`JoinHandle`],
/// `exclude_self`, and a cached format. `!Send` COM interfaces stay on the
/// dedicated thread.
pub struct WasapiSystemBackend {
    /// Host-exclusion flag. `true` selects process loopback EXCLUDE; `false`
    /// selects classic loopback.
    exclude_self: bool,
    /// Requested exclusion pids (see `StreamConfig::exclude_pids`). Every pid must
    /// equal the root validated by [`exclude_root`](WasapiSystemBackend::exclude_root).
    exclude_pids: Vec<u32>,
    /// Output endpoint selection. `None` uses the default render endpoint;
    /// `Some(id)` selects the eRender endpoint whose FriendlyName matches `id`.
    /// Unused when `exclude_self == true`.
    device_id: Option<String>,
    /// Running flag (guards duplicate starts, signals stop, and supports drop checks). `Send`.
    stop_flag: Arc<AtomicBool>,
    /// Handle for the thread that owns COM and capture (`Some` after start).
    handle: Option<JoinHandle<Result<()>>>,
    /// Runtime failure reported once by the next start (watchdog reopen).
    pending_error: Option<Error>,
    /// Last queried format, refreshed before constructing each classic-loopback sink.
    /// While running, retain the format with which that sink was configured.
    native: Cell<(u32, u16)>,
}

impl WasapiSystemBackend {
    /// Create a system loopback backend without connecting yet.
    ///
    /// `exclude_self == false` (default path): Query and cache the MixFormat of
    /// the endpoint selected by `device_id` (`None` uses the default render
    /// endpoint). On failure, cache [`FALLBACK_FORMAT`] (`(48000, 2)`) without
    /// panicking. Do not check whether `device_id` matches until the endpoint is
    /// opened by [`start`](CaptureBackend::start).
    ///
    /// `exclude_self == true` (process loopback EXCLUDE path): `native` uses
    /// the fixed process loopback format [`PROCESS_LOOPBACK_FORMAT`] (`(48000, 2)`).
    /// Do not query MixFormat or use `device_id`, since this mechanism is not
    /// tied to an endpoint.
    pub fn new(exclude_self: bool, device_id: Option<String>) -> Self {
        let native = if exclude_self {
            PROCESS_LOOPBACK_FORMAT
        } else {
            query_native_format(device_id.as_deref()).unwrap_or(FALLBACK_FORMAT)
        };
        Self {
            exclude_self,
            exclude_pids: Vec::new(),
            device_id,
            stop_flag: Arc::new(AtomicBool::new(false)),
            handle: None,
            pending_error: None,
            native: Cell::new(native),
        }
    }

    /// Request exclusion of a process tree. WASAPI process loopback takes one
    /// tree per client: `exclude_self` selects this process as the root, otherwise
    /// the first pid selects the root. Every listed pid must equal that root;
    /// duplicates are accepted. A different pid or pid 0 causes `start` to return
    /// [`Error::InvalidArg`] before spawning a thread.
    /// Excluding unrelated process trees is unsupported; a common ancestor can
    /// be passed if excluding its whole tree is acceptable.
    /// Switches the native format to the process-loopback format.
    ///
    /// Idempotent: `native` is recomputed from the resulting exclude root, so
    /// calling this again with an empty list restores the classic-loopback
    /// format instead of leaving the process-loopback one latched.
    pub fn with_exclude_pids(mut self, pids: Vec<u32>) -> Self {
        self.exclude_pids = pids;
        self.native.set(match self.exclude_root() {
            Ok(None) => query_native_format(self.device_id.as_deref()).unwrap_or(FALLBACK_FORMAT),
            // Invalid requests are rejected by `start`; never use classic loopback.
            Ok(Some(_)) | Err(_) => PROCESS_LOOPBACK_FORMAT,
        });
        self
    }

    /// Validate the request and return the root of the excluded process tree, if any.
    pub fn exclude_root(&self) -> Result<Option<u32>> {
        validate_exclusion(self.exclude_self, std::process::id(), &self.exclude_pids)
    }
}

/// Validate WASAPI's single-tree exclusion without COM or process-tree guesses.
fn validate_exclusion(exclude_self: bool, self_pid: u32, pids: &[u32]) -> Result<Option<u32>> {
    let root = if exclude_self {
        Some(self_pid)
    } else {
        pids.first().copied()
    };
    let Some(root) = root else {
        return Ok(None);
    };

    for &pid in pids {
        if pid == 0 {
            return Err(Error::InvalidArg(
                "exclude_pids: pid 0 is not a valid process id".into(),
            ));
        }
        if pid != root {
            return Err(Error::InvalidArg(format!(
                "exclude_pids: pid {pid} is outside the single process tree WASAPI can exclude (root {root}); Windows supports excluding one process tree per capture"
            )));
        }
    }
    Ok(Some(root))
}

impl Default for WasapiSystemBackend {
    /// Defaults to classic loopback on the default render endpoint (`exclude_self == false` / `device_id == None`).
    fn default() -> Self {
        Self::new(false, None)
    }
}

/// Get `(rate, channels)` from the MixFormat of the render endpoint selected by
/// `device_id` (`None` uses the default). Returns `None` if unavailable without
/// panicking. Temporarily initializes COM to perform the query.
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
        // Read packed fields by copying their values, as in parse_mix_format.
        let rate = core::ptr::addr_of!((*pwfx).nSamplesPerSec).read_unaligned();
        let channels = core::ptr::addr_of!((*pwfx).nChannels).read_unaligned();
        CoTaskMemFree(Some(pwfx as *const _ as *const _));
        Some((rate, channels))
    }
}

/// Resolve `device_id` to a render endpoint.
///
/// For `None`, call `GetDefaultAudioEndpoint(eRender, eConsole)`. For `Some(id)`,
/// enumerate ACTIVE eRender endpoints and return the first whose FriendlyName
/// matches `id`. Returns [`Error::DeviceNotFound`] if none match.
///
/// # Safety
/// COM must be initialized on the calling thread, and `enumerator` must be valid.
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

/// Read FriendlyName from the endpoint's property store. Returns `None` on failure.
///
/// # Safety
/// COM must be initialized on the calling thread, and `device` must be a valid `IMMDevice`.
unsafe fn endpoint_friendly_name(device: &IMMDevice) -> Option<String> {
    let store = device.OpenPropertyStore(STGM_READ).ok()?;
    let value = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
    // PROPVARIANT's Display converts through PropVariantToBSTR (including VT_LPWSTR).
    let name = value.to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// Enumerate active render (output) endpoints and return them as [`DeviceInfo`] entries.
///
/// Each `DeviceInfo` uses the endpoint's FriendlyName for `id` and `name`, gets
/// `sample_rate` and `channels` from MixFormat, and has
/// [`SourceKind::SystemLoopback`] as `source_kind` and `true` for `is_loopback`.
/// `is_default` is set on the entry whose FriendlyName matches the default render
/// endpoint. Skip endpoints whose FriendlyName cannot be read.
pub fn list_output_devices() -> Result<Vec<DeviceInfo>> {
    let _com = ComThread::new();
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| map_hr("CoCreateInstance(MMDeviceEnumerator)", e))?;

        // Default render FriendlyName, used to set is_default. Continue if unavailable.
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
            // Skip endpoints without a FriendlyName, since no id can be created.
            let Some(name) = endpoint_friendly_name(&device) else {
                continue;
            };
            // Get rate/channels from MixFormat, or use the required native format (48000/2).
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

/// Get `(rate, channels)` from the endpoint's MixFormat. Returns `None` if unavailable.
///
/// # Safety
/// COM must be initialized on the calling thread, and `device` must be a valid `IMMDevice`.
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
        // The facade asks before building a sink on every reopen. An endpoint
        // switch must refresh the format here; start still checks for a race
        // between this query and Initialize. Process loopback stays fixed.
        native_format_from_source(
            &self.native,
            self.handle.is_none() && matches!(self.exclude_root(), Ok(None)),
            || query_native_format(self.device_id.as_deref()),
        )
    }

    fn start(&mut self, sink: RawSink) -> Result<()> {
        let exclude_root = self.exclude_root()?;
        // Safe on duplicate start: do nothing if the thread is already running.
        if self.handle.is_some() {
            return Ok(());
        }
        if let Some(error) = self.pending_error.take() {
            return Err(error);
        }
        // Reset the flag so start can run again after a previous stop.
        self.stop_flag.store(false, Ordering::SeqCst);

        let stop_flag = self.stop_flag.clone();
        let device_id = self.device_id.clone();
        // Channel for synchronously returning setup status (COM init through successful Start).
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();

        let handle = thread::Builder::new()
            .name("flexaudio-wasapi-system".into())
            .spawn(move || run_system_thread(exclude_root, device_id, sink, stop_flag, ready_tx))
            .map_err(|e| Error::Backend(format!("spawn wasapi system thread: {e}")))?;

        match ready_rx.recv() {
            Ok(Ok(())) => {
                self.handle = Some(handle);
                Ok(())
            }
            Ok(Err(e)) => {
                // Setup failed. Join because the thread exits just after sending ready.
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
        // Safe for reentrant and duplicate stop calls.
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            self.pending_error = match h.join() {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error),
                Err(_) => Some(Error::Backend("WASAPI system owner thread panicked".into())),
            };
        }
    }
}

impl Drop for WasapiSystemBackend {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Thread body. Initialize COM, configure loopback based on `exclude_root`, and
/// run the capture loop. Report setup status to [`WasapiSystemBackend::start`]
/// through `ready_tx`. The COM guard (`_com`) is scoped to this function, so
/// `CoUninitialize` runs when the thread exits (declaration order drops it after
/// the COM objects).
///
/// - `exclude_root == None`: Classic loopback on the render endpoint selected by
///   `device_id` (`None` uses the default; see [`setup_system_loopback`]).
/// - `exclude_root == Some(root)`: Process loopback that passes the PID `root`
///   (and its tree) with [`ProcessMode::Exclude`] to
///   [`crate::process::setup_process_loopback`]. `device_id` is unused.
///
/// Classic loopback also owns a silent render client. Process exclusion never
/// creates a render client. Both use the shared [`capture_loop`].
fn run_system_thread(
    exclude_root: Option<u32>,
    device_id: Option<String>,
    sink: RawSink,
    stop_flag: Arc<AtomicBool>,
    ready_tx: mpsc::Sender<Result<()>>,
) -> Result<()> {
    // Initialize COM on this thread (uninitialize on drop). Declared first, dropped last.
    let _com = ComThread::new();

    // Run setup to get an initialized client, capture service, event, and channel count.
    // Branch on exclude_root; both paths return the same type.
    let setup = if let Some(root) = exclude_root {
        // Exclude the PID root and its tree to capture all system audio without feedback.
        // Reuse the process loopback mechanism from the `process` module.
        unsafe { crate::process::setup_process_loopback(root, ProcessMode::Exclude) }
            .map(CaptureSetup::process)
    } else {
        // Classic loopback on the render endpoint selected by device_id (None uses default).
        unsafe { setup_system_loopback(device_id.as_deref(), &sink) }
    };
    let setup = match setup {
        Ok(t) => t,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return Ok(());
        }
    };

    unsafe { capture_loop(setup, sink, &stop_flag, ready_tx) }
}

/// Set up classic loopback on the render endpoint selected by `device_id` (`None`
/// uses the default), then return the initialized `IAudioClient`,
/// `IAudioCaptureClient`, event handle, and channel count. Returns
/// [`Error::DeviceNotFound`] if `device_id` does not match.
///
/// Verify the endpoint format against the configured sink before initialization.
/// Keepalive activation failure prevents capture from starting.
///
/// # Safety
/// COM must be initialized on the calling thread.
unsafe fn setup_system_loopback(device_id: Option<&str>, sink: &RawSink) -> Result<CaptureSetup> {
    let enumerator: IMMDeviceEnumerator =
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .map_err(|e| map_hr("CoCreateInstance(MMDeviceEnumerator)", e))?;

    // Render endpoint selected by device_id (None uses default). DeviceNotFound if unmatched.
    let device: IMMDevice = resolve_render_endpoint(&enumerator, device_id)?;

    let client: IAudioClient = device
        .Activate(CLSCTX_ALL, None)
        .map_err(|e| map_hr("IMMDevice::Activate(IAudioClient)", e))?;

    // Shared MixFormat (must be freed with CoTaskMemFree after use).
    let pwfx: *mut WAVEFORMATEX = client
        .GetMixFormat()
        .map_err(|e| map_hr("IAudioClient::GetMixFormat", e))?;

    // Always free the mix format, including validation/keepalive setup failures.
    let setup = (|| {
        let format = parse_mix_format(pwfx)?;
        verify_format(format, (sink.native_rate(), sink.native_channels()))?;
        let keepalive = SilentRender::new(&device, pwfx)
            .map_err(|e| keepalive_error("cannot create classic loopback silent keepalive", e))?;
        let (capture, event) = init_loopback_capture(&client, pwfx, 0)?;
        Ok(CaptureSetup {
            client,
            capture,
            event: EventHandle(event),
            channels: format.1,
            keepalive: Some(keepalive),
        })
    })();
    CoTaskMemFree(Some(pwfx as *const _));
    setup
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio_core::raw_ring;

    /// `new` and `native_format` do not panic, whether or not a render endpoint exists.
    #[test]
    fn new_and_native_format_do_not_panic() {
        let backend = WasapiSystemBackend::new(false, None);
        let (rate, channels) = backend.native_format();
        assert!(rate > 0);
        assert!(channels > 0);
    }

    /// `new` does not panic when given a device_id (matching is deferred until start).
    /// It can be constructed with an unknown id, and native uses a valid fallback.
    #[test]
    fn new_with_device_id_does_not_panic() {
        let backend = WasapiSystemBackend::new(false, Some("no-such-endpoint".into()));
        let (rate, channels) = backend.native_format();
        assert!(rate > 0);
        assert!(channels > 0);
    }

    /// For `exclude_self == true`, native is always the fixed process loopback format `(48000, 2)`.
    /// No MixFormat query is made, so it does not depend on render endpoints or device_id.
    #[test]
    fn new_exclude_self_native_is_fixed() {
        let backend = WasapiSystemBackend::new(true, Some("ignored".into()));
        assert_eq!(backend.native_format(), (48_000, 2));
    }

    /// `with_exclude_pids` stores the pids, switches native to the process-loopback
    /// format, and validates the exclude root (`exclude_self` selects this process).
    #[test]
    fn exclude_pids_switches_to_process_loopback_format() {
        let be = WasapiSystemBackend::new(false, None).with_exclude_pids(vec![4242]);
        assert_eq!(be.exclude_pids, vec![4242]);
        assert_eq!(be.native.get(), PROCESS_LOOPBACK_FORMAT);
        assert!(matches!(be.exclude_root(), Ok(Some(4242))));
        let self_pid = std::process::id();
        let selfy = WasapiSystemBackend::new(true, None).with_exclude_pids(vec![self_pid]);
        assert!(matches!(selfy.exclude_root(), Ok(Some(root)) if root == self_pid));
        assert!(matches!(
            WasapiSystemBackend::new(false, None).exclude_root(),
            Ok(None)
        ));

        // Idempotence: clearing the list drops back out of the exclude path.
        // `native` is recomputed by `query_native_format`, which needs COM and a
        // render endpoint, so its exact value is environment-dependent here —
        // only the root (pure) is asserted; the format restoration is covered by
        // the same code path as `new`.
        let cleared = WasapiSystemBackend::new(false, None)
            .with_exclude_pids(vec![4242])
            .with_exclude_pids(vec![]);
        assert!(matches!(cleared.exclude_root(), Ok(None)));
        assert!(cleared.exclude_pids.is_empty());
    }

    #[test]
    fn validate_exclusion_accepts_root_only() {
        assert!(matches!(
            validate_exclusion(false, 100, &[42]),
            Ok(Some(42))
        ));
    }

    #[test]
    fn validate_exclusion_accepts_duplicate_root() {
        assert!(matches!(
            validate_exclusion(false, 100, &[42, 42]),
            Ok(Some(42))
        ));
    }

    #[test]
    fn validate_exclusion_accepts_self_pid() {
        assert!(matches!(
            validate_exclusion(true, 100, &[100]),
            Ok(Some(100))
        ));
        assert!(matches!(validate_exclusion(true, 100, &[]), Ok(Some(100))));
    }

    #[test]
    fn validate_exclusion_rejects_foreign_pid_with_exclude_self() {
        assert!(matches!(
            validate_exclusion(true, 100, &[100, 42]),
            Err(Error::InvalidArg(message))
                if message.starts_with("exclude_pids: pid 42 ")
                    && message.contains("root 100")
        ));
    }

    #[test]
    fn validate_exclusion_rejects_second_tree() {
        assert!(matches!(
            validate_exclusion(false, 100, &[42, 99]),
            Err(Error::InvalidArg(message))
                if message.starts_with("exclude_pids: pid 99 ")
                    && message.contains("root 42")
        ));
    }

    #[test]
    fn validate_exclusion_rejects_pid_zero() {
        for (exclude_self, pids) in [(false, vec![0]), (false, vec![42, 0]), (true, vec![0])] {
            assert!(matches!(
                validate_exclusion(exclude_self, 100, &pids),
                Err(Error::InvalidArg(message)) if message.starts_with("exclude_pids: pid 0 ")
            ));
        }
    }

    #[test]
    fn validate_exclusion_empty_list_has_no_root() {
        assert!(matches!(validate_exclusion(false, 100, &[]), Ok(None)));
    }

    #[test]
    fn start_rejects_invalid_exclusion_before_spawning_thread() {
        // The self-exclusion constructor and invalid builder avoid COM queries.
        let mut backend = WasapiSystemBackend::new(true, None).with_exclude_pids(vec![0]);
        backend.stop_flag.store(true, Ordering::SeqCst);
        let (prod, _cons) = raw_ring(1);
        let sink = RawSink::new(prod, PROCESS_LOOPBACK_FORMAT.0, PROCESS_LOOPBACK_FORMAT.1);

        assert!(matches!(
            backend.start(sink),
            Err(Error::InvalidArg(message)) if message.starts_with("exclude_pids: pid 0 ")
        ));
        assert!(backend.handle.is_none());
        assert!(backend.stop_flag.load(Ordering::SeqCst));
    }

    /// `list_output_devices` does not panic, and returned entries are marked as loopback.
    /// An empty list or `Err` is acceptable without render endpoints; panicking is not.
    #[test]
    fn list_output_devices_does_not_panic() {
        if let Ok(devices) = list_output_devices() {
            for d in &devices {
                assert!(d.is_loopback);
                assert_eq!(d.source_kind, SourceKind::SystemLoopback);
            }
        }
    }

    /// `start` → `stop` does not panic on the classic loopback path, with or without a device.
    /// `Err` is acceptable if no render endpoint exists or it cannot be opened.
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
                backend.stop(); // A duplicate stop is also safe.
            }
            Err(_e) => { /* Missing render endpoint, non-float format, etc. are acceptable. */ }
        }
    }

    /// A present endpoint must support the complete silent-keepalive startup
    /// and teardown. Only the absence of a default endpoint permits a skip.
    #[test]
    fn classic_loopback_with_default_endpoint_starts_and_stops_cleanly() {
        {
            let _com = ComThread::new();
            let enumerator: IMMDeviceEnumerator = unsafe {
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .expect("create endpoint enumerator for classic-loopback test")
            };
            match unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) } {
                Ok(_endpoint) => {}
                Err(error) if error.code().0 == 0x80070490u32 as i32 => {
                    eprintln!(
                        "Skipping classic loopback lifecycle test: no default render endpoint (headless host): {error}"
                    );
                    return;
                }
                Err(error) => panic!("query default render endpoint: {error}"),
            }
        }

        let mut backend = WasapiSystemBackend::new(false, None);
        let (rate, channels) = backend.native_format();
        let (producer, _consumer) = raw_ring(1);
        let sink = RawSink::new(producer, rate, channels);
        backend
            .start(sink)
            .expect("classic loopback with silent keepalive must start on the default endpoint");
        backend.stop();
        assert!(backend.handle.is_none(), "capture owner must be joined");
        assert!(
            backend.pending_error.is_none(),
            "capture and keepalive must stop cleanly: {:?}",
            backend.pending_error
        );
        backend.stop();
    }

    #[test]
    fn keepalive_setup_context_preserves_classified_hresult_variants() {
        use windows::core::HRESULT;

        // E_ACCESSDENIED is a permission failure and keeps its typed variant.
        let error = keepalive_error(
            "cannot create classic loopback silent keepalive",
            map_hr(
                "Initialize",
                windows::core::Error::from(HRESULT(0x80070005u32 as i32)),
            ),
        );
        assert!(matches!(
            error,
            Error::PermissionDenied {
                permission: flexaudio_core::types::Permission::SystemAudio,
                detail,
            } if !detail.is_empty()
        ));
        // Exclusive-mode conflicts are not permission problems: Backend, with context added.
        for (code, cause) in [
            (0x8889000Au32, "exclusive use"),
            (0x8889000E, "exclusive audio mode is disallowed"),
        ] {
            let error = keepalive_error(
                "cannot create classic loopback silent keepalive",
                map_hr(
                    "Initialize",
                    windows::core::Error::from(HRESULT(code as i32)),
                ),
            );
            assert!(
                matches!(
                    &error,
                    Error::Backend(message)
                        if message.starts_with("cannot create classic loopback silent keepalive: ")
                            && message.contains(cause)
                ),
                "{code:#x}: {error:?}"
            );
        }
        for code in [0x88890004u32, 0x80070490] {
            let error = keepalive_error(
                "cannot create classic loopback silent keepalive",
                map_hr(
                    "Initialize",
                    windows::core::Error::from(HRESULT(code as i32)),
                ),
            );
            assert!(matches!(error, Error::DeviceNotFound));
        }
    }

    /// `start` with an unknown device_id returns `DeviceNotFound` without panicking.
    #[test]
    fn start_with_unknown_device_id_is_device_not_found() {
        let mut backend = WasapiSystemBackend::new(false, Some("no-such-endpoint".into()));
        let (rate, channels) = backend.native_format();
        let cap = (rate as usize * channels as usize).max(1);
        let (prod, _cons) = raw_ring(cap);
        let sink = RawSink::new(prod, rate, channels);

        match backend.start(sink) {
            Ok(()) => {
                // Accept success if it happens to match in this environment; just check stop.
                backend.stop();
            }
            Err(e) => assert!(matches!(e, Error::DeviceNotFound)),
        }
    }

    /// `start` → `stop` does not panic on the `exclude_self == true` process
    /// loopback EXCLUDE path. `Err` is acceptable on unsupported OS versions or
    /// if activation fails.
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
                backend.stop(); // A duplicate stop is also safe.
            }
            Err(_e) => { /* Unsupported OS versions or process loopback activation failure are acceptable. */
            }
        }
    }
}
