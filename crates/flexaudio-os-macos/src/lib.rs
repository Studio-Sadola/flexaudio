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
//! Successful tap creation does not prove consent. Five continuous seconds of exact-zero native
//! samples while an eligible external process has output I/O active produces an advisory
//! `Event::SilenceWhileSourceActive`; digital silence can cause the same observation. Unknown
//! process activity, device routing, missing delivery, or dropped samples disable inference.
//!
//! # Non-macOS
//! Native adapters are macOS-only. The private capture-health state machine also compiles on
//! other platforms so its sample, activity, and timing policy can be tested without audio devices.

#![warn(missing_docs)]

#[cfg(target_os = "macos")]
mod activity;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod capture_health;
#[cfg(target_os = "macos")]
mod common;
#[cfg(target_os = "macos")]
mod devices;
#[cfg(target_os = "macos")]
mod process;
#[cfg(target_os = "macos")]
mod processes;
#[cfg(target_os = "macos")]
mod system;
#[cfg(target_os = "macos")]
mod tap;
#[cfg(target_os = "macos")]
mod version;

#[cfg(target_os = "macos")]
pub use devices::list_output_devices;
#[cfg(target_os = "macos")]
pub use process::MacProcessBackend;
#[cfg(target_os = "macos")]
pub use processes::list_processes;
#[cfg(target_os = "macos")]
pub use system::MacSystemBackend;
