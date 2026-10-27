//! flexaudio-core -- OS-independent core.
//!
//! The OS-independent part of the desktop audio capture abstraction library `flexaudio`.
//! Provides ring buffers, sample-rate conversion, channel mixing, 20 ms chunking, clock normalization,
//! events, and type definitions. OS-specific capture is implemented by separate `flexaudio-os-*`
//! crates that implement [`backend::CaptureBackend`], which the facade layer wires to the core.
//!
//! # Fixed contract
//! All internal processing uses interleaved `f32` / 48000 Hz / stereo 2ch / 20 ms = 960
//! frames/chunk. [`OutputFormat`] can change the external rate/channels (the Normalizer's second
//! stage resamples; e.g. 16k/1ch gives 320 frames/chunk). Output chunks remain 20 ms regardless of rate.
//!
//! Capture delivery has no callbacks. RT threads only push; consumers poll.
//! Implicit shutdown failures have a separate process-wide receiver.
//! The RT path never blocks (DROP_OLDEST / overflow drop when full). PTS comes from the device
//! and gaps are detected.
//!
//! # Two-stage ring-buffer layout
//! ```text
//! [RT cb] --push--> RawRing (rtrb, RT-safe) --pop--> [ingest/process thread]
//!                                                       |
//!                                          Normalizer (mix + rubato SRC + 960-frame slicing)
//!                                                       |
//!                                                       v
//!                                       ChunkRing (ringbuf, DROP_OLDEST) --try_pop--> [poll]
//! ```

#![warn(missing_docs)]

mod addon_error;
mod arrival;
pub use addon_error::AddonErrorKind;

pub mod backend;
pub mod capabilities;
pub use capabilities::{
    Availability, BackendKind, Capabilities, DeviceSelectionBehavior, DeviceWatcherCapabilities,
    OsVersion, PidScope, ProcessCapabilities, SideEffectCapabilities, SideEffectCapability,
    SideEffectCondition, SourceCapabilities, SourceCapability, SystemExclusionCapabilities,
    UnavailableReason, WatchScope, WatcherMode,
};
pub mod chunk_ring;
pub mod clock;
pub mod diagnostics;
mod drop_failure;
pub use drop_failure::{
    report_drop_failures, set_drop_failure_handler, DropFailure, DropFailureHandler,
    DropFailureOwner, ShutdownReceipt,
};
pub mod error_context;
mod error_equality;
mod helper_stop_reason;
pub use helper_stop_reason::HelperStopReason;
mod errors;
mod spawn_error;
/// Internal contract fixtures with no stability guarantee. Never enable in production.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod test_support;
pub use spawn_error::spawn_error;
mod device_namespace;
pub mod inventory;
mod loss;
pub mod normalizer;
pub mod process_list;
pub mod quant;
pub mod raw_ring;
pub mod secondary_ring;
mod settings;
pub use device_namespace::{DeviceIdNamespace, NamespacedDevice};
pub use settings::{
    AdjustmentReason, IgnoreReason, SettingKey, SettingOutcome, SettingResolution, SettingValue,
    SettingsReport,
};
pub mod startup;
pub mod terminal_causes;
pub mod types;
mod vad_error;

// Re-export the main types at the crate root.
pub use backend::{CaptureBackend, RawSink};
pub use chunk_ring::{chunk_ring, ChunkConsumer, ChunkProducer};
pub use clock::{monotonic_now_ns, ClockNormalizer};
pub use normalizer::{InnerProcessor, NormalizedChunk, Normalizer, CHUNK_FRAMES};
pub use quant::quantize_i16;
pub use raw_ring::{raw_ring, RawConsumer, RawProducer};
pub use secondary_ring::{secondary_chunk_ring, SecondaryChunkConsumer, SecondaryChunkProducer};
pub use types::{
    AudioChunk, AudioLoss, AudioPath, ChunkFlags, DefaultDeviceKind, DeviceEvent, DeviceInfo,
    Error, ErrorContext, ErrorGroup, ErrorKind, Event, LossReason, MixLane, NativeStatus,
    Operation, OutputFormat, OutputTap, Permission, ProcessInfo, Result, SecondaryChunk,
    ShutdownReport, SourceKind, StreamConfig, CHANNELS, SAMPLE_RATE,
};

pub use diagnostics::CaptureDiagnostics;
pub use startup::StartupDropReason;
pub use vad_error::VadErrorCode;

/// Public spelling of the canonical chunk-duration option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkMsOption {
    /// Rust, Python and C use `chunk_ms`.
    SnakeCase,
    /// JavaScript uses `chunkMs`.
    JavaScript,
}
/// One cause/remedy message with the binding's public option name.
pub fn chunk_ms_error(option: ChunkMsOption) -> Error {
    let name = match option {
        ChunkMsOption::SnakeCase => "chunk_ms",
        ChunkMsOption::JavaScript => "chunkMs",
    };
    Error::InvalidArg(format!(
        "capture uses the canonical 20 ms frame contract; set {name} to 20"
    ))
}
/// Validate the canonical frame duration before acquiring capture resources.
pub fn validate_chunk_ms(chunk_ms: u32) -> Result<()> {
    if chunk_ms == 20 {
        Ok(())
    } else {
        Err(chunk_ms_error(ChunkMsOption::SnakeCase))
    }
}
#[cfg(test)]
mod chunk_contract_tests {
    #[test]
    fn chunk_duration_rejection_explains_contract_and_remedy() {
        let error = super::validate_chunk_ms(10).expect_err("noncanonical duration must fail");
        let message = error.to_string();
        assert!(
            message.contains("canonical 20 ms frame contract"),
            "{message}"
        );
        assert!(message.contains("set chunk_ms to 20"), "{message}");
        assert!(super::validate_chunk_ms(20).is_ok());
    }
}

#[cfg(test)]
mod chunk_option_tests {
    #[test]
    fn javascript_chunk_duration_uses_its_public_option_spelling() {
        let error = super::chunk_ms_error(super::ChunkMsOption::JavaScript);
        let message = error.to_string();
        assert!(message.contains("set chunkMs to 20"), "{message}");
        assert!(message.contains("canonical 20 ms frame contract"));
    }
}
