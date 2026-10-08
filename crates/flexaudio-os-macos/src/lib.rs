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
//! Successful tap creation does not prove consent. There is no public system-audio permission
//! status API. Five continuous seconds of exact-zero native samples with eligible external output
//! activity trigger one active self-probe per generation. It renders a roughly 300 ms diagnostic
//! signal on the default output and captures our own process in a separate private tap. Recognizing
//! that signal suppresses the warning; proven rendering with continuous exact-zero diagnostic
//! capture produces terminal `Event::PermissionDenied` for `SystemAudio`. Inconclusive setup,
//! rendering, capture, or timeout produces `Event::SilenceWhileSourceActive` and continues capture.
//! The signal may enter user capture when our own process is included. Stop cancels the probe
//! without late notifications. Unknown activity/routing, missing delivery, or dropped samples
//! disable the trigger; genuine digital silence alone never establishes permission denial.
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
mod native_probe;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod probe;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod probe_signal;
#[cfg(target_os = "macos")]
mod process;
#[cfg(target_os = "macos")]
mod processes;
#[cfg(target_os = "macos")]
mod system;
#[cfg(target_os = "macos")]
mod tap;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod terminal;
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
