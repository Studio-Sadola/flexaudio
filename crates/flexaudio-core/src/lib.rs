//! flexaudio-core — OS-independent core.
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
//! The public API has no callbacks. RT threads only push; consumers poll.
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

pub mod backend;
pub mod chunk_ring;
pub mod clock;
pub mod diagnostics;
pub mod error_context;
mod errors;
mod loss;
pub mod normalizer;
pub mod process_list;
pub mod quant;
pub mod raw_ring;
pub mod secondary_ring;
pub mod types;

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
