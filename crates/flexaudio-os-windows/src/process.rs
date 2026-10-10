//! [`WasapiProcessBackend`] — per-process WASAPI loopback.
//!
//! Capture audio for a PID and its process tree with `ActivateAudioInterfaceAsync` and
//! `AUDIOCLIENT_ACTIVATION_PARAMS` (process loopback). `mode` [`ProcessMode::Include`] captures
//! only the target tree; [`ProcessMode::Exclude`] captures all system audio except that tree. This
//! corresponds to Linux's [`PwProcessBackend`](../flexaudio_os_linux) (link-factory fan-out).
//!
//! [`setup_process_loopback`] is `pub(crate)` and is also called by the `exclude_self == true` path
//! in the `system` module ([`WasapiSystemBackend`](crate::WasapiSystemBackend)) to exclude the
//! calling process PID and capture all system audio.
//!
//! # PROPVARIANT (VT_BLOB) details
//!
//! `ActivateAudioInterfaceAsync` takes `activationparams` as
//! `Option<*const windows_core::PROPVARIANT>`, so `AUDIOCLIENT_ACTIVATION_PARAMS` must be packed
//! into a VT_BLOB PROPVARIANT. In windows-core 0.54, `PROPVARIANT::from_raw` takes the private
//! `imp::PROPVARIANT` type, which cannot be named externally, and there is no public raw struct.
//! Define a `#[repr(C)]` mirror [`RawPropVariant`] that exactly matches the SDK layout, then pass it
//! to `from_raw` with `transmute`.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use flexaudio_core::backend::{CaptureBackend, RawSink};
use flexaudio_core::types::{Error, Event, ProcessMode, Result};

use crate::owner::{join_owner, OwnerShutdown};
use flexaudio_core::{ErrorContext, NativeStatus, Operation, ShutdownReport};

use windows::core::{implement, Interface, HRESULT, PROPVARIANT};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioClient, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDIOCLIENT_ACTIVATION_PARAMS,
    AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
    AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    WAVEFORMATEX,
};
use windows::Win32::Media::Multimedia::WAVE_FORMAT_IEEE_FLOAT;
use windows::Win32::System::Threading::{CreateEventW, SetEvent};

use crate::common::{
    capture_loop, init_loopback_capture, map_hr, wait_event_signaled, CaptureSetup, ComThread,
};
use windows::core::PCWSTR;

/// Process loopback has a fixed native format of `(48000, 2)`.
/// It cannot use `GetMixFormat`, so build the WAVEFORMATEX manually.
const NATIVE_RATE: u32 = 48_000;
const NATIVE_CHANNELS: u16 = 2;

/// A 24-byte x64 mirror struct that exactly matches the SDK's `PROPVARIANT`.
///
/// In windows-core 0.54, `PROPVARIANT::from_raw` takes a private `imp::PROPVARIANT` that cannot be
/// named externally, so use a layout-matched mirror and pass it with `transmute`.
///
/// The layout matches the measured raw `PROPVARIANT_0_0` / `PROPVARIANT_0_0_0` from windows-core
/// 0.54: `vt: u16` + `wReserved1/2/3: u16 ×3` (8 bytes, aligned to the value union boundary) + a
/// 16-byte value union. The union (`PROPVARIANT_0_0_0`) includes `CAUB` / `BLOB` / `CAFILETIME` and
/// other “u32 count + pointer” types, so it is 16 bytes on x64. The whole PROPVARIANT is 8+16=24
/// bytes. The VT_BLOB union starts with `BLOB { cbSize: u32, pBlobData: *mut u8 }`; x64 adds 4 bytes
/// of padding after cbSize (offset 8) for pointer alignment, placing pBlobData at offset 16.
#[repr(C)]
#[derive(Clone, Copy)]
struct RawPropVariant {
    /// VARENUM. VT_BLOB = 65.
    vt: u16,
    w_reserved1: u16,
    w_reserved2: u16,
    w_reserved3: u16,
    /// BLOB.cbSize (byte count). Offset 8.
    blob_cb_size: u32,
    /// x64 pointer-alignment padding (`cbSize:u32` to `pBlobData:ptr` on an 8-byte boundary). Offset 12.
    _pad: u32,
    /// BLOB.pBlobData (reference to blob data; keep it alive because it is not copied). Offset 16.
    blob_p_data: *mut u8,
}

const VT_BLOB_U16: u16 = 65;

// Compile-time checks verify that the layout (24 bytes / 8-byte aligned) matches the SDK PROPVARIANT.
// Also compare against the size of `PROPVARIANT` itself (a thin wrapper over raw imp::PROPVARIANT,
// so it should have the same size).
//
// This mirror's 24-byte layout assumes 64-bit pointers (the value union's “u32 count + pointer”
// type is 16 bytes). On 32-bit targets, 4-byte pointers change PROPVARIANT size/alignment/padding,
// causing the const assertions below to fail at build time. flexaudio supports only 64-bit Windows
// (x86_64 / aarch64).
const _: () = {
    assert!(core::mem::size_of::<RawPropVariant>() == 24);
    assert!(core::mem::align_of::<RawPropVariant>() == 8);
    assert!(core::mem::size_of::<PROPVARIANT>() == core::mem::size_of::<RawPropVariant>());
    assert!(core::mem::align_of::<PROPVARIANT>() == 8);
};

/// Build a VT_BLOB `PROPVARIANT` pointing to `AUDIOCLIENT_ACTIVATION_PARAMS`.
///
/// The caller must keep `params` alive through `ActivateAudioInterfaceAsync` and completion waiting
/// (the BLOB is referenced, not copied).
///
/// Wrap the return value in [`ManuallyDrop`] for memory safety. windows-core 0.54's `PROPVARIANT`
/// calls `PropVariantClear` on drop, which tries to free `pBlobData` with `CoTaskMemFree` for
/// VT_BLOB. Here `pBlobData` points to the caller's stack `params` (not COM-allocated memory), so
/// dropping a plain `PROPVARIANT` would free a stack pointer and corrupt the heap
/// (STATUS_HEAP_CORRUPTION). The mirror only borrows the BLOB pointer and owns no heap resource, so
/// suppressing `Drop` and leaking the wrapper is harmless; the caller owns and frees `params`.
///
/// # Safety
/// `params` must point to a valid `AUDIOCLIENT_ACTIVATION_PARAMS` and remain alive for the lifetime
/// of the returned `PROPVARIANT`.
// The `transmute` target type `windows_core::imp::bindings::PROPVARIANT` is private and cannot be
// named, so it must be inferred as `_` and cannot satisfy clippy::missing_transmute_annotations.
// The const assertions above guarantee the matching layout, so allow this locally.
#[allow(clippy::missing_transmute_annotations)]
unsafe fn make_blob_propvariant(
    params: *mut AUDIOCLIENT_ACTIVATION_PARAMS,
) -> core::mem::ManuallyDrop<PROPVARIANT> {
    let raw = RawPropVariant {
        vt: VT_BLOB_U16,
        w_reserved1: 0,
        w_reserved2: 0,
        w_reserved3: 0,
        blob_cb_size: core::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
        _pad: 0,
        blob_p_data: params as *mut u8,
    };
    // `from_raw`'s argument type imp::PROPVARIANT is private, so infer the `transmute` target as `_`;
    // the const assertions above guarantee it matches RawPropVariant's layout.
    core::mem::ManuallyDrop::new(PROPVARIANT::from_raw(core::mem::transmute::<
        RawPropVariant,
        _,
    >(raw)))
}

/// Completion handler that only notifies the initiating thread through `ActivateCompleted`.
///
/// The initiating thread retrieves the result (`IAudioClient`) with `op.GetActivateResult`, keeping
/// COM objects on one thread. This handler only calls `SetEvent`, so it needs no interior mutability
/// and `&self` is sufficient.
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivationHandler {
    /// Manual-reset event for completion notification. `ActivateCompleted` calls `SetEvent`.
    done: HANDLE,
}

// In windows-implement 0.53 (used by windows 0.54), `#[implement]` implements the `_Impl`-suffixed
// trait for the original struct (`ActivationHandler`). The generated `ActivationHandler_Impl` wrapper
// contains `this: ActivationHandler` and dereferences to its fields, so `self.done` works directly.
impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationHandler {
    fn ActivateCompleted(
        &self,
        _operation: Option<&IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        // The OS (WASAPI activation infrastructure) calls this FFI-boundary callback. A panic crossing
        // the boundary is UB, so wrap the body in catch_unwind. It currently only calls SetEvent and
        // cannot panic, but keep the guard for future changes.
        catch_unwind(AssertUnwindSafe(|| unsafe { SetEvent(self.done) }))
            .unwrap_or_else(|_| Err(windows::core::Error::from(HRESULT(0x80004005u32 as i32))))
    }
}

/// [`CaptureBackend`] that captures audio for a PID and its process tree through process loopback.
///
/// Initialize COM on a dedicated thread, get `IAudioClient` with `ActivateAudioInterfaceAsync`
/// (`VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK` + VT_BLOB `AUDIOCLIENT_ACTIVATION_PARAMS`), initialize
/// with a fixed WAVEFORMATEX (48k/2ch/f32), and send event-driven packets to [`RawSink::push`].
///
/// This type is `Send` (it stores only `target_pid` / `mode` / stop flag / [`JoinHandle`] / fixed
/// format). `!Send` COM objects remain on the dedicated thread.
pub struct WasapiProcessBackend {
    /// PID of the process to capture.
    target_pid: u32,
    /// Capture mode. [`ProcessMode::Include`] captures only the target tree;
    /// [`ProcessMode::Exclude`] captures all system audio except the target tree.
    mode: ProcessMode,
    /// Running flag (guards duplicate start, signals stop, and is checked on drop). `Send`.
    stop_flag: Arc<AtomicBool>,
    /// Handle for the thread that owns COM/capture (`Some` after start).
    handle: Option<JoinHandle<ShutdownReport>>,
    /// Runtime failure reported once by the next start (watchdog reopen).
    pending_error: Option<Error>,
    shutdown: OwnerShutdown,
    /// Fixed native format `(48000, 2)`.
    native: (u32, u16),
}

impl WasapiProcessBackend {
    /// Build a backend from the target PID and `mode` (does not connect yet).
    pub fn new(target_pid: u32, mode: ProcessMode) -> Self {
        Self {
            target_pid,
            mode,
            stop_flag: Arc::new(AtomicBool::new(false)),
            handle: None,
            pending_error: None,
            shutdown: OwnerShutdown::default(),
            native: (NATIVE_RATE, NATIVE_CHANNELS),
        }
    }

    /// PID of the process to capture.
    pub fn target_pid(&self) -> u32 {
        self.target_pid
    }

    /// Capture mode ([`ProcessMode::Include`] / [`ProcessMode::Exclude`]).
    pub fn mode(&self) -> ProcessMode {
        self.mode
    }
}

impl CaptureBackend for WasapiProcessBackend {
    fn native_format(&self) -> (u32, u16) {
        self.native
    }

    fn start(&mut self, sink: RawSink) -> Result<()> {
        if self.handle.is_some() {
            return Ok(());
        }
        if let Some(error) = self.pending_error.take() {
            return Err(error);
        }
        if self.target_pid == 0 {
            return Err(Error::InvalidArg("target_pid must be positive".into()));
        }
        crate::format::verify_format(self.native, (sink.native_rate(), sink.native_channels()))?;
        self.stop_flag.store(false, Ordering::SeqCst);

        let stop_flag = self.stop_flag.clone();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let (event_tx, event_rx) = mpsc::channel();
        let target_pid = self.target_pid;
        let mode = self.mode;

        let handle = thread::Builder::new()
            .name("flexaudio-wasapi-process".into())
            .spawn(move || {
                run_process_thread(target_pid, mode, sink, stop_flag, ready_tx, event_tx)
            })
            .map_err(|_| {
                self.stop_flag.store(true, Ordering::SeqCst);
                Error::Backend("capture owner thread could not be started".into())
                    .with_context(ErrorContext::new(Operation::Start))
            })?;

        match ready_rx.recv() {
            Ok(Ok(())) => {
                self.handle = Some(handle);
                self.shutdown.reset(event_rx);
                Ok(())
            }
            Ok(Err(e)) => {
                self.stop_flag.store(true, Ordering::SeqCst);
                ShutdownReport::new(Some(e), join_owner(handle).cleanup().to_vec()).result()
            }
            Err(_) => {
                self.stop_flag.store(true, Ordering::SeqCst);
                ShutdownReport::new(
                    Some(
                        Error::Backend("capture owner exited before reporting readiness".into())
                            .with_context(ErrorContext::new(Operation::Start)),
                    ),
                    join_owner(handle).cleanup().to_vec(),
                )
                .result()
            }
        }
    }

    fn stop(&mut self) {
        let _ = self.stop_checked();
    }

    fn stop_checked(&mut self) -> Result<()> {
        self.stop_flag.store(true, Ordering::SeqCst);
        let handle = self.handle.take();
        let joined_owner = handle.is_some();
        let result = self.shutdown.finish(handle);
        if joined_owner {
            self.pending_error = result.as_ref().err().cloned();
        }
        result
    }

    fn poll_event(&mut self) -> Option<Event> {
        self.shutdown.poll_event()
    }
}

impl Drop for WasapiProcessBackend {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Fixed WAVEFORMATEX for process loopback (48000 / 2ch / f32).
/// Process loopback cannot use `GetMixFormat`, so build this manually.
fn fixed_process_format() -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16, // = 3
        nChannels: NATIVE_CHANNELS,                // 2
        nSamplesPerSec: NATIVE_RATE,               // 48000
        wBitsPerSample: 32,
        nBlockAlign: 8,                   // channels * bits/8 = 2 * 4
        nAvgBytesPerSec: NATIVE_RATE * 8, // rate * blockAlign
        cbSize: 0,
    }
}

/// Owner thread body. Initialize COM, get `IAudioClient` through process-loopback activation, and
/// run the capture loop with the fixed format. Report setup success or failure to
/// [`WasapiProcessBackend::start`] over `ready_tx`.
fn run_process_thread(
    target_pid: u32,
    mode: ProcessMode,
    sink: RawSink,
    stop_flag: Arc<AtomicBool>,
    ready_tx: mpsc::Sender<Result<()>>,
    event_tx: mpsc::Sender<Event>,
) -> ShutdownReport {
    let _com = ComThread::new();

    let setup = unsafe { setup_process_loopback(target_pid, mode) };
    let setup = match setup {
        Ok(t) => CaptureSetup::process(t),
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return ShutdownReport::new(None, Vec::new());
        }
    };

    unsafe { capture_loop(setup, sink, &stop_flag, ready_tx, event_tx) }
}

/// Set up process loopback and return the initialized `IAudioClient` / `IAudioCaptureClient` / event
/// handle / channel count (fixed at 2).
///
/// Use `mode` to select INCLUDE (target tree only) or EXCLUDE (all system audio except the target
/// tree). The `exclude_self == true` path in the `system` module reuses this with the calling
/// process PID and [`ProcessMode::Exclude`], which is why this function is `pub(crate)`.
///
/// # Safety
/// COM must be initialized on the calling thread.
#[allow(clippy::type_complexity)]
pub(crate) unsafe fn setup_process_loopback(
    target_pid: u32,
    mode: ProcessMode,
) -> Result<(
    IAudioClient,
    windows::Win32::Media::Audio::IAudioCaptureClient,
    HANDLE,
    u16,
)> {
    // Same OS-version check as enumeration. Keep reactive mapping for E_NOTIMPL / E_NOINTERFACE.
    crate::version::ensure_process_loopback_supported()?;

    // Build activation params. `mode` selects INCLUDE or EXCLUDE.
    let loopback_mode = match mode {
        ProcessMode::Include => PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
        ProcessMode::Exclude => PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
    };
    let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: target_pid,
                ProcessLoopbackMode: loopback_mode,
            },
        },
    };

    // Build the VT_BLOB PROPVARIANT. Keep params / prop alive through ActivateAudioInterfaceAsync and
    // completion (`GetActivateResult`), since the BLOB is referenced. `prop` is `ManuallyDrop` because
    // freeing the stack BLOB with `PropVariantClear` would corrupt the heap (see `make_blob_propvariant` docs).
    let prop = make_blob_propvariant(&mut params as *mut _);

    // Completion notification event (manual reset=true / initially non-signaled).
    let done_event = CreateEventW(None, true, false, PCWSTR::null())
        .map_err(|e| map_hr("CreateEventW(activation done)", e))?;

    // Completion handler (only calls SetEvent). Keep it alive until WaitForSingleObject completes
    // so its reference count remains valid.
    let handler: IActivateAudioInterfaceCompletionHandler =
        ActivationHandler { done: done_event }.into();

    let op: IActivateAudioInterfaceAsyncOperation = match ActivateAudioInterfaceAsync(
        VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
        &IAudioClient::IID,
        // Dereference ManuallyDrop with `&*prop`, converting `&PROPVARIANT` to `*const PROPVARIANT`.
        Some(&*prop as *const _),
        &handler,
    ) {
        Ok(op) => op,
        Err(e) => {
            let _ = CloseHandle(done_event);
            // Older OS versions without process loopback return E_NOINTERFACE/E_NOTIMPL, etc.
            return Err(map_process_activation_err("ActivateAudioInterfaceAsync", e));
        }
    };

    // Wait up to 5 seconds. Map a timeout to a Backend error.
    if !wait_event_signaled(done_event, 5000) {
        let _ = CloseHandle(done_event);
        return Err(Error::Backend(
            "process loopback activation timed out".into(),
        ));
    }
    // The completion event is no longer needed. Keep params/prop/handler alive until this function ends.
    let _ = CloseHandle(done_event);

    // Retrieve the activation result on the initiating thread (keep COM objects on one thread).
    let mut hr = HRESULT(0);
    let mut unknown: Option<windows::core::IUnknown> = None;
    op.GetActivateResult(&mut hr, &mut unknown)
        .map_err(|e| map_hr("GetActivateResult", e))?;
    if let Err(e) = hr.ok() {
        return Err(map_process_activation_err("activation result HRESULT", e));
    }
    let unknown =
        unknown.ok_or_else(|| Error::Backend("activation returned null interface".into()))?;
    let client: IAudioClient = unknown
        .cast()
        .map_err(|e| map_hr("cast activated IUnknown to IAudioClient", e))?;

    // Initialize with the fixed format, then set up the event and capture.
    // AUTOCONVERTPCM matches the official ApplicationLoopback sample (process loopback provides no
    // MixFormat, so let the engine convert to the requested format).
    let wfx = fixed_process_format();
    let (capture, event) = init_loopback_capture(
        &client,
        &wfx as *const WAVEFORMATEX,
        AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    )?;

    // Keep params/prop/handler/op alive until here before dropping them (BLOB reference and handler lifetime).
    drop(op);
    drop(handler);
    // `prop` is `ManuallyDrop`. It only contains a BLOB pointer (borrowing params) and owns no resource,
    // so it is safe to leak without calling `PropVariantClear`. This avoids freeing a stack pointer
    // (heap corruption), and the leak has no practical impact.
    let _ = prop; // Explicitly keep prop alive until Initialize completes.
    let _ = params; // Keep params alive until Initialize completes too.

    Ok((client, capture, event, NATIVE_CHANNELS))
}

/// Map HRESULT errors from process-loopback activation to [`Error::UnsupportedOsVersion`] on older
/// (unsupported) OS versions, or [`Error::Backend`] otherwise.
fn map_process_activation_err(ctx: &'static str, e: windows::core::Error) -> Error {
    // E_NOTIMPL = 0x80004001 / E_NOINTERFACE = 0x80004002. OS versions without process loopback
    // (such as older Windows 10 releases) may return these.
    const E_NOTIMPL: i32 = 0x80004001u32 as i32;
    const E_NOINTERFACE: i32 = 0x80004002u32 as i32;
    let code = e.code().0;
    if code == E_NOTIMPL || code == E_NOINTERFACE {
        Error::UnsupportedOsVersion.with_context(
            ErrorContext::new(Operation::Start).with_native_status(NativeStatus::HResult {
                call: ctx,
                bits: code as u32,
            }),
        )
    } else {
        map_hr(ctx, e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio_core::raw_ring;
    // `ProcessMode` is available through `super::*` via the parent module's `use`.

    /// `new` + `native_format` return fixed `(48000, 2)` without panicking.
    #[test]
    fn new_and_native_format_are_fixed() {
        let backend = WasapiProcessBackend::new(1234, ProcessMode::Include);
        assert_eq!(backend.native_format(), (48_000, 2));
        assert_eq!(backend.target_pid(), 1234);
        assert_eq!(backend.mode(), ProcessMode::Include);
    }

    /// Check at runtime, in addition to the const assertions, that the PROPVARIANT mirror matches
    /// the SDK layout (24 bytes / 8-byte aligned).
    #[test]
    fn raw_propvariant_layout_matches_sdk() {
        assert_eq!(core::mem::size_of::<RawPropVariant>(), 24);
        assert_eq!(core::mem::align_of::<RawPropVariant>(), 8);
        assert_eq!(core::mem::size_of::<PROPVARIANT>(), 24);
    }

    /// `start` → `stop` does not panic regardless of device or target PID availability.
    /// `Err` is acceptable for an invalid target PID or unsupported OS; a panic is not.
    #[test]
    fn start_then_stop_tolerates_missing_target() {
        // A nonexistent PID. Activation may succeed but Initialize/capture can fail.
        let mut backend = WasapiProcessBackend::new(0xFFFF_FFFE, ProcessMode::Include);
        let (rate, channels) = backend.native_format();
        let cap = (rate as usize * channels as usize).max(1);
        let (prod, _cons) = raw_ring(cap);
        let sink = RawSink::new(prod, rate, channels);

        match backend.start(sink) {
            Ok(()) => {
                backend.stop();
                backend.stop();
            }
            Err(_e) => { /* Unsupported OS / activation failure is allowed. */ }
        }
    }

    /// End-to-end test that records from a real process playing audio.
    /// Set `FLEXAUDIO_TEST_PID` and run with
    /// `cargo test -p flexaudio-os-windows -- --ignored`.
    #[test]
    #[ignore = "requires a real process playing audio; run with `FLEXAUDIO_TEST_PID=<pid> cargo test -p flexaudio-os-windows -- --ignored` on a Windows machine"]
    fn end_to_end_captures_real_audio() {
        use std::time::Duration;

        let pid: u32 = std::env::var("FLEXAUDIO_TEST_PID")
            .ok()
            .and_then(|s| s.parse().ok())
            .expect("set FLEXAUDIO_TEST_PID to the PID of a process playing audio");

        let mut backend = WasapiProcessBackend::new(pid, ProcessMode::Include);
        let (rate, channels) = backend.native_format();
        let cap = rate as usize * channels as usize * 2; // About 2 seconds.
        let (prod, mut cons) = raw_ring(cap);
        let sink = RawSink::new(prod, rate, channels);

        backend.start(sink).expect("start should succeed");
        thread::sleep(Duration::from_millis(800));
        backend.stop();

        let mut buf = vec![0.0f32; cap];
        let got = cons.pop_slice(&mut buf);
        assert!(got > 0, "expected captured samples, got none");
    }
}

#[cfg(test)]
mod repro_tests {
    use super::*;

    #[test]
    fn checked_stop_after_capture_failure_publishes_terminality() {
        let mut backend = WasapiProcessBackend::new(1, ProcessMode::Include);
        let primary = crate::common::map_hr(
            "IAudioCaptureClient::GetBuffer",
            windows::core::Error::from(windows::core::HRESULT(0x80070005u32 as i32)),
        );
        let (event_tx, event_rx) = mpsc::channel();
        backend.shutdown.reset(event_rx);
        let capture_error = primary.clone();
        backend.handle = Some(thread::spawn(move || {
            crate::owner::finish_capture(Err(capture_error), || Ok(()), &event_tx)
        }));

        // Explicit stop before any watchdog reopen still publishes the capture
        // cause, so the facade latches terminality before delivering final PCM.
        let result = backend.stop_checked();
        assert_eq!(result, Err(primary.clone()));
        assert_eq!(primary.kind(), flexaudio_core::ErrorKind::PermissionDenied);
        assert!(
            matches!(backend.poll_event(), Some(Event::TerminalError { error }) if error == primary)
        );
        assert!(backend.poll_event().is_none());
        assert_eq!(backend.stop_checked(), result);
    }

    #[test]
    fn repro_p6_activation_signalling_failure_is_returned() {
        // A null event deterministically fails SetEvent; no activation or device is opened.
        let handler = ActivationHandler {
            done: HANDLE::default(),
        };
        assert!(handler.ActivateCompleted(None).is_err());
    }

    #[test]
    fn repro_p6_stop_reports_owner_error() {
        let mut backend = WasapiProcessBackend::new(1, ProcessMode::Include);
        backend.handle = Some(thread::spawn(|| {
            ShutdownReport::new(
                None,
                vec![Error::Backend("injected owner shutdown failure".into())],
            )
        }));
        backend.stop();
        assert!(
            backend.pending_error.is_some(),
            "control: join retained the error"
        );
        let result = backend.stop_checked();
        assert!(result.is_err());
        assert!(
            matches!(backend.poll_event(), Some(Event::ShutdownError { error }) if error.kind() == flexaudio_core::ErrorKind::Backend)
        );
        assert_eq!(backend.stop_checked(), result);
        assert!(backend.poll_event().is_none());
    }

    #[test]
    fn checked_stop_does_not_restore_a_consumed_reopen_failure() {
        let mut backend = WasapiProcessBackend::new(1, ProcessMode::Include);
        backend.handle = Some(thread::spawn(|| {
            ShutdownReport::new(None, vec![Error::DeviceLost])
        }));
        let result = backend.stop_checked();
        let (producer, _consumer) = flexaudio_core::raw_ring(16);
        let first_retry = backend.start(RawSink::new(producer, 48_000, 2));
        assert_eq!(first_retry, result);
        assert!(backend.pending_error.is_none());
        assert_eq!(backend.stop_checked(), result);
        assert!(
            backend.pending_error.is_none(),
            "a subsequent retry must reach native startup"
        );
    }

    #[test]
    fn owner_panic_is_checked_and_does_not_expose_its_payload() {
        let mut backend = WasapiProcessBackend::new(1, ProcessMode::Include);
        backend.handle = Some(thread::spawn(|| panic!("injected private panic payload")));
        let error = backend.stop_checked().unwrap_err();
        assert!(
            matches!(&error, Error::Context { context, .. } if context.operation() == Operation::Join)
        );
        assert!(!error.to_string().contains("private panic payload"));
        assert_eq!(backend.stop_checked(), Err(error));
        assert!(matches!(
            backend.poll_event(),
            Some(Event::ShutdownError { .. })
        ));
        assert!(backend.poll_event().is_none());
    }
}
