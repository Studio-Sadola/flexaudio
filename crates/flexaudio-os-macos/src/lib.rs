//! flexaudio-os-macos — macOS backend using Core Audio Process Taps
//! (objc2-core-audio, macOS 14.4+).
//!
//! Captures all system audio output ([`MacSystemBackend`]) or a specific process
//! ([`MacProcessBackend`]) with Process Tap. Equivalent to Windows WASAPI loopback / Linux PipeWire
//! monitor.
//!
//! # Architecture
//! The tap chain (`CATapDescription` → process tap → private aggregate device → IOProc block →
//! start) lives in the [`tap`] module. Both backends use the same chain, switching the `TapKind`
//! between INCLUDE and EXCLUDE. `!Send` ObjC objects (`Retained<CATapDescription>` / `RcBlock` /
//! `TapChain`) stay on the backend's dedicated thread; only the `Send` state (stop flag,
//! `JoinHandle`, and format) crosses threads (the same design as the cpal / Windows / Linux backends).
//!
//! # Permissions (TCC)
//! System/process audio capture requires TCC's `kTCCServiceAudioCapture`
//! (`NSAudioCaptureUsageDescription` in Info.plist). We do not use private TCC SPI; the OS prompt
//! on first capture determines permission. If tap creation is rejected because permission has not
//! been granted, [`map_os_status`](common::map_os_status) maps the permission-related OSStatus to
//! [`Error::PermissionDenied`](flexaudio_core::types::Error).
//!
//! # Non-macOS
//! macOS only. `#![cfg(target_os = "macos")]` makes this compile as an empty crate on other
//! platforms, and objc2 dependencies are included only in the `target.'cfg(...macos)'` section of
//! `Cargo.toml` (Linux/Windows builds are unaffected).

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
