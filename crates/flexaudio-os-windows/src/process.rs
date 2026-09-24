//! [`WasapiProcessBackend`] — per-process WASAPI loopback.
//!
//! Captures the audio of a specific PID (its process tree) via
//! `ActivateAudioInterfaceAsync` + `AUDIOCLIENT_ACTIVATION_PARAMS` (process loopback).
//! With `mode` [`ProcessMode::Include`] it captures "only the target tree's audio"; with
//! [`ProcessMode::Exclude`] it captures "all system audio except the target tree". The
//! counterpart of Linux's [`PwProcessBackend`](../flexaudio_os_linux) (link-factory fan-out).
//!
//! This module's [`setup_process_loopback`] is `pub(crate)` and is also called from the
//! `exclude_self == true` path of the `system` module
//! ([`WasapiSystemBackend`](crate::WasapiSystemBackend)) (it EXCLUDEs the host's own PID to
//! capture all system audio).
//!
//! # The PROPVARIANT (VT_BLOB) difficulty
//!
//! The `activationparams` of `ActivateAudioInterfaceAsync` is
//! `Option<*const windows_core::PROPVARIANT>`, and `AUDIOCLIENT_ACTIVATION_PARAMS` must be
//! packed into a VT_BLOB PROPVARIANT. In windows-core 0.54 the argument of
//! `PROPVARIANT::from_raw` is the private `imp::PROPVARIANT`, whose type name cannot be
//! written from outside (and there is no public raw struct either). So we define our own
//! `#[repr(C)]` mirror struct [`RawPropVariant`] that exactly matches the SDK PROPVARIANT
//! layout and pass it to `from_raw` via `transmute`.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use flexaudio_core::backend::{CaptureBackend, RawSink};
use flexaudio_core::types::{Error, ProcessMode, Result};

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

use crate::common::{capture_loop, init_loopback_capture, map_hr, wait_event_signaled, ComThread};
use windows::core::PCWSTR;

/// The native format of process loopback is fixed at `(48000, 2)`.
/// Process loopback cannot use `GetMixFormat`, so we build the WAVEFORMATEX ourselves.
const NATIVE_RATE: u32 = 48_000;
const NATIVE_CHANNELS: u16 = 2;

/// A 24-byte x64 mirror struct that exactly matches the SDK `PROPVARIANT`.
///
/// In windows-core 0.54 the `PROPVARIANT::from_raw` argument `imp::PROPVARIANT` is private
/// and its type name cannot be written from outside, so we build a layout-identical mirror
/// and pass it via `transmute`.
///
/// The layout matches the measured raw `PROPVARIANT_0_0` / `PROPVARIANT_0_0_0` of
/// windows-core 0.54: `vt: u16` + `wReserved1/2/3: u16 ×3` (8 bytes so far, aligned to the
/// value union boundary) + a 16-byte value union. The value union (`PROPVARIANT_0_0_0`)
/// contains "`u32` count + pointer" types such as `CAUB`/`BLOB`/`CAFILETIME`, so it is 16
/// bytes on x64, and therefore the whole PROPVARIANT is 8+16 = 24 bytes. The head of the
/// union for VT_BLOB is `BLOB { cbSize: u32, pBlobData: *mut u8 }`. On x64, pointer
/// alignment inserts 4 bytes of padding after cbSize (offset 8), so pBlobData is at
/// offset 16.
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
    /// x64 pointer-alignment padding (cbSize:u32 → pBlobData:ptr is on an 8-byte boundary).
    /// Offset 12.
    _pad: u32,
    /// BLOB.pBlobData (a reference to the blob itself; it is not copied, so keep it alive).
    /// Offset 16.
    blob_p_data: *mut u8,
}

const VT_BLOB_U16: u16 = 65;

// Verify at compile time that the layout (24 bytes / 8-byte alignment) matches the SDK
// PROPVARIANT. Also cross-check against the size of `PROPVARIANT` itself (it is a thin
// wrapper over the raw imp::PROPVARIANT, so it should be the same size).
//
// This mirror is a 24-byte layout that assumes 64-bit pointers (the value union is a
// "u32 count + pointer" type of 16 bytes). On 32-bit targets pointers are 4 bytes and the
// PROPVARIANT size/alignment/padding change, in which case the const asserts below fail to
// compile and reject the build. flexaudio's Windows support is 64-bit only
// (x86_64 / aarch64).
const _: () = {
    assert!(core::mem::size_of::<RawPropVariant>() == 24);
    assert!(core::mem::align_of::<RawPropVariant>() == 8);
    assert!(core::mem::size_of::<PROPVARIANT>() == core::mem::size_of::<RawPropVariant>());
    assert!(core::mem::align_of::<PROPVARIANT>() == 8);
};

/// Builds a VT_BLOB `PROPVARIANT` that points to an `AUDIOCLIENT_ACTIVATION_PARAMS`.
///
/// The caller must keep `params` alive through `ActivateAudioInterfaceAsync` and the wait
/// for completion (the BLOB is referenced, not copied).
///
/// The return value is wrapped in [`ManuallyDrop`] for memory safety. The windows-core 0.54
/// `PROPVARIANT` calls `PropVariantClear` in `Drop`, which for VT_BLOB tries to free
/// `pBlobData` with `CoTaskMemFree`. But this function's `pBlobData` points to `params` on
/// the caller's stack (not COM-allocated memory), so letting a plain `PROPVARIANT` drop
/// would free a stack pointer and corrupt the heap (STATUS_HEAP_CORRUPTION). The mirror only
/// borrows the BLOB pointer and owns no heap resources of its own, so suppressing `Drop` and
/// leaking the contents causes no real leak (the params themselves are owned and freed by
/// the caller).
///
/// # Safety
/// `params` must point to a valid `AUDIOCLIENT_ACTIVATION_PARAMS` and stay alive for the
/// whole lifetime of the returned `PROPVARIANT`.
// The `transmute` target type `windows_core::imp::bindings::PROPVARIANT` is private, so its
// type name cannot be written and `_` inference is the only option, which cannot satisfy
// clippy::missing_transmute_annotations. Layout equality is already guaranteed by the const
// asserts above, so allow it locally.
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
    // The from_raw argument type imp::PROPVARIANT is private and cannot be named, so
    // transmute with `_` inference (layout equality with RawPropVariant is guaranteed by the
    // const asserts above).
    core::mem::ManuallyDrop::new(PROPVARIANT::from_raw(core::mem::transmute::<
        RawPropVariant,
        _,
    >(raw)))
}

/// Completion handler that only signals completion to the initiating thread in
/// `ActivateCompleted`.
///
/// The initiating thread retrieves the result (`IAudioClient`) with `op.GetActivateResult`
/// (COM objects never cross threads). This handler only calls `SetEvent`, so it needs no
/// interior mutability (`&self` is enough).
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivationHandler {
    /// Event for the completion notification (manual reset). `SetEvent` in
    /// `ActivateCompleted`.
    done: HANDLE,
}

// The `#[implement]` of windows-implement 0.53 (the version pulled in by windows 0.54) has
// the `_Impl`-suffixed trait implemented on the original struct (here `ActivationHandler`).
// The generated `ActivationHandler_Impl` is a wrapper that contains `this: ActivationHandler`
// and reaches the original struct's fields through Deref, so `self.done` works directly.
impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationHandler {
    fn ActivateCompleted(
        &self,
        _operation: Option<&IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        // This is an FFI-boundary callback invoked by the OS (the WASAPI activation
        // infrastructure). A panic crossing the boundary is UB, so the body is wrapped in
        // catch_unwind. Currently it only calls SetEvent and does not panic; this is
        // insurance against future changes.
        let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
            let _ = SetEvent(self.done);
        }));
        Ok(())
    }
}

/// [`CaptureBackend`] that captures the audio of a specific PID (its tree) via per-process
/// loopback.
///
/// Initializes COM on a dedicated thread, obtains an `IAudioClient` via
/// `ActivateAudioInterfaceAsync` (`VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK` + a VT_BLOB
/// `AUDIOCLIENT_ACTIVATION_PARAMS`), Initializes it with a fixed WAVEFORMATEX
/// (48k/2ch/f32), and feeds packets to [`RawSink::push`] event-driven.
///
/// This type is `Send` (it holds `target_pid` / `mode` / the stop flag / [`JoinHandle`] /
/// the fixed format; the `!Send` COM objects are confined to the dedicated thread).
pub struct WasapiProcessBackend {
    /// PID of the process to capture.
    target_pid: u32,
    /// Capture mode. [`ProcessMode::Include`] means INCLUDE (only the target tree's audio);
    /// [`ProcessMode::Exclude`] means EXCLUDE (all system audio except the target tree).
    mode: ProcessMode,
    /// Running flag (double-start guard / stop request / drop check). `Send`.
    stop_flag: Arc<AtomicBool>,
    /// Handle of the thread that owns COM/capture (`Some` after start).
    handle: Option<JoinHandle<()>>,
    /// Fixed native format `(48000, 2)`.
    native: (u32, u16),
}

impl WasapiProcessBackend {
    /// Builds the backend from the target PID and `mode` (does not connect yet).
    pub fn new(target_pid: u32, mode: ProcessMode) -> Self {
        Self {
            target_pid,
            mode,
            stop_flag: Arc::new(AtomicBool::new(false)),
            handle: None,
            native: (NATIVE_RATE, NATIVE_CHANNELS),
        }
    }

    /// The PID to capture.
    pub fn target_pid(&self) -> u32 {
        self.target_pid
    }

    /// The stored capture mode ([`ProcessMode::Include`] / [`ProcessMode::Exclude`]).
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
        self.stop_flag.store(false, Ordering::SeqCst);

        let stop_flag = self.stop_flag.clone();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let target_pid = self.target_pid;
        let mode = self.mode;

        let handle = thread::Builder::new()
            .name("flexaudio-wasapi-process".into())
            .spawn(move || {
                run_process_thread(target_pid, mode, sink, stop_flag, ready_tx);
            })
            .map_err(|e| Error::Backend(format!("spawn wasapi process thread: {e}")))?;

        match ready_rx.recv() {
            Ok(Ok(())) => {
                self.handle = Some(handle);
                Ok(())
            }
            Ok(Err(e)) => {
                self.stop_flag.store(false, Ordering::SeqCst);
                let _ = handle.join();
                Err(e)
            }
            Err(_) => {
                self.stop_flag.store(false, Ordering::SeqCst);
                let _ = handle.join();
                Err(Error::Backend(
                    "wasapi process thread exited before reporting readiness".into(),
                ))
            }
        }
    }

    fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for WasapiProcessBackend {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Fixed WAVEFORMATEX for per-process loopback (48000 / 2ch / f32).
/// Process loopback cannot use `GetMixFormat`, so we build it ourselves.
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

/// Body of the owning thread. Initializes COM, obtains an `IAudioClient` via process
/// loopback activation, and runs the capture loop with the fixed format. Reports setup
/// success or failure to [`WasapiProcessBackend::start`] through `ready_tx`.
fn run_process_thread(
    target_pid: u32,
    mode: ProcessMode,
    sink: RawSink,
    stop_flag: Arc<AtomicBool>,
    ready_tx: mpsc::Sender<Result<()>>,
) {
    let _com = ComThread::new();

    let setup = unsafe { setup_process_loopback(target_pid, mode) };
    let (client, capture, event, channels) = match setup {
        Ok(t) => t,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    if ready_tx.send(Ok(())).is_err() {
        return;
    }

    unsafe { capture_loop(&client, &capture, event, channels, sink, &stop_flag) };
}

/// Sets up process loopback and returns the Initialized `IAudioClient` /
/// `IAudioCaptureClient` / event handle / channel count (fixed at 2).
///
/// `mode` selects INCLUDE (only the target tree) / EXCLUDE (all system audio except the
/// target tree). The `exclude_self == true` path of the `system` module reuses this by
/// passing the host's own PID with [`ProcessMode::Exclude`] (hence `pub(crate)`).
///
/// # Safety
/// COM must already be initialized on the calling thread.
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
    // Same version check as enumeration. The reactive E_NOTIMPL / E_NOINTERFACE mapping is
    // kept as well.
    crate::version::ensure_process_loopback_supported()?;

    // Build the activation params. `mode` switches between INCLUDE/EXCLUDE.
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

    // Build the VT_BLOB PROPVARIANT. params / prop must stay alive through
    // ActivateAudioInterfaceAsync + the wait for completion (GetActivateResult) (the BLOB is
    // a reference). `prop` is `ManuallyDrop` because freeing the stack BLOB with
    // `PropVariantClear` would corrupt the heap (see the doc of `make_blob_propvariant`).
    let prop = make_blob_propvariant(&mut params as *mut _);

    // Completion notification event (manual reset = true / initially non-signaled).
    let done_event = CreateEventW(None, true, false, PCWSTR::null())
        .map_err(|e| map_hr("CreateEventW(activation done)", e))?;

    // Completion handler (only calls SetEvent). Not dropped until WaitForSingleObject
    // completes (kept alive by its reference count).
    let handler: IActivateAudioInterfaceCompletionHandler =
        ActivationHandler { done: done_event }.into();

    let op: IActivateAudioInterfaceAsyncOperation = match ActivateAudioInterfaceAsync(
        VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
        &IAudioClient::IID,
        // `&*prop` unwraps the ManuallyDrop: `&PROPVARIANT` → `*const PROPVARIANT`.
        Some(&*prop as *const _),
        &handler,
    ) {
        Ok(op) => op,
        Err(e) => {
            let _ = CloseHandle(done_event);
            // Older OSes (without process loopback support) return E_NOINTERFACE/E_NOTIMPL
            // or similar.
            return Err(map_process_activation_err("ActivateAudioInterfaceAsync", e));
        }
    };

    // Wait for completion (5 seconds). A timeout maps to a Backend error.
    if !wait_event_signaled(done_event, 5000) {
        let _ = CloseHandle(done_event);
        return Err(Error::Backend(
            "process loopback activation timed out".into(),
        ));
    }
    // The completion event is no longer needed. params/prop/handler stay alive until the end
    // of this function.
    let _ = CloseHandle(done_event);

    // Retrieve the activation result (on the initiating thread; COM never crosses threads).
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

    // Initialize with the fixed format → event → capture.
    // AUTOCONVERTPCM is the same flag as in the official ApplicationLoopback sample (process
    // loopback returns no MixFormat, so conversion to the requested format is left to the
    // engine).
    let wfx = fixed_process_format();
    let (capture, event) = init_loopback_capture(
        &client,
        &wfx as *const WAVEFORMATEX,
        AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    )?;

    // Keep params/prop/handler/op alive up to here, then drop (BLOB reference, handler
    // lifetime).
    drop(op);
    drop(handler);
    // `prop` is `ManuallyDrop`. Its contents are only the BLOB pointer (a borrow of params)
    // and own no resources, so it may be leaked without calling `PropVariantClear`. This
    // avoids freeing a stack pointer (heap corruption). The leak does no real harm.
    let _ = prop; // Explicit touch to keep prop alive until Initialize completes.
    let _ = params; // Also keep params alive until Initialize completes.

    Ok((client, capture, event, NATIVE_CHANNELS))
}

/// Maps an HRESULT error from process loopback activation to
/// [`Error::UnsupportedOsVersion`] on older (unsupported) OSes, and to [`Error::Backend`]
/// otherwise.
fn map_process_activation_err(ctx: &str, e: windows::core::Error) -> Error {
    // E_NOTIMPL = 0x80004001 / E_NOINTERFACE = 0x80004002. OSes without process loopback
    // support (older Windows 10 etc.) can reject with these.
    const E_NOTIMPL: i32 = 0x80004001u32 as i32;
    const E_NOINTERFACE: i32 = 0x80004002u32 as i32;
    let code = e.code().0;
    if code == E_NOTIMPL || code == E_NOINTERFACE {
        Error::UnsupportedOsVersion
    } else {
        map_hr(ctx, e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio_core::raw_ring;
    // `ProcessMode` is visible from `super::*` through the parent module's `use`.

    /// `new` + `native_format` return the fixed `(48000, 2)` and do not panic.
    #[test]
    fn new_and_native_format_are_fixed() {
        let backend = WasapiProcessBackend::new(1234, ProcessMode::Include);
        assert_eq!(backend.native_format(), (48_000, 2));
        assert_eq!(backend.target_pid(), 1234);
        assert_eq!(backend.mode(), ProcessMode::Include);
    }

    /// The PROPVARIANT mirror matches the SDK layout (24 B / 8-byte alignment) (checked at
    /// runtime in addition to the const asserts).
    #[test]
    fn raw_propvariant_layout_matches_sdk() {
        assert_eq!(core::mem::size_of::<RawPropVariant>(), 24);
        assert_eq!(core::mem::align_of::<RawPropVariant>(), 8);
        assert_eq!(core::mem::size_of::<PROPVARIANT>(), 24);
    }

    /// `start` → `stop` does not panic regardless of whether the device/target PID exists.
    /// `Err` is allowed for an invalid target PID or an unsupported OS (only panics are
    /// not).
    #[test]
    fn start_then_stop_tolerates_missing_target() {
        // A nonexistent PID. Activation itself may succeed, but Initialize/capture may fail.
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
            Err(_e) => { /* unsupported OS / activation failure is allowed */ }
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
        let cap = rate as usize * channels as usize * 2; // about 2 seconds
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
