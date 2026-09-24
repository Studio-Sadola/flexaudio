//! flexaudio-os-macos — macOS backend. Uses Core Audio Process Taps
//! (objc2-core-audio, macOS 14.4+).
//!
//! Captures the whole system audio output ([`MacSystemBackend`]) and specific processes
//! ([`MacProcessBackend`]) with Process Taps. The counterpart of WASAPI loopback on Windows /
//! the PipeWire monitor on Linux.
//!
//! # Architecture
//! The tap chain (`CATapDescription` → process tap → private aggregate device →
//! IOProc block → start) lives in the [`tap`] module, and both backends run the same chain,
//! switching the INCLUDE/EXCLUDE `TapKind`. `!Send` ObjC objects
//! (`Retained<CATapDescription>` / `RcBlock` / `TapChain`) are confined to the backend's
//! dedicated thread, and only the `Send` parts (stop flag, `JoinHandle`, format) cross
//! threads (same design as the cpal / Windows / Linux backends).
//!
//! # Permissions (TCC)
//! System/process audio capture requires TCC's `kTCCServiceAudioCapture`
//! (`NSAudioCaptureUsageDescription` in Info.plist). No private TCC SPI is used; whether
//! permission is granted is left to the OS prompt at the first capture. If tap creation is
//! rejected because permission has not been granted,
//! [`map_os_status`](common::map_os_status) maps the permission-denied OSStatus values to
//! [`Error::PermissionDenied`](flexaudio_core::types::Error).
//!
//! # Non-macOS
//! macOS only. On non-macOS targets it compiles to nothing via
//! `#![cfg(target_os = "macos")]`, and the objc2-family dependencies are pulled in only by
//! the `target.'cfg(...macos)'` section of `Cargo.toml` (Linux/Windows builds are
//! unaffected).

#![cfg(target_os = "macos")]
#![warn(missing_docs)]

mod common;
mod devices;
mod process;
mod processes;
mod system;
mod tap;
mod version;

pub use devices::list_output_devices;
pub use process::MacProcessBackend;
pub use processes::list_processes;
pub use system::MacSystemBackend;
