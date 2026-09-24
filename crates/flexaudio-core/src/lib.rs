//! flexaudio-core: the OS-independent core.
//!
//! The OS-independent part of `flexaudio`, a desktop audio capture abstraction library.
//! It provides ring buffers, sample-rate conversion, channel mixing, 20ms chunking, clock
//! normalization, and event/type definitions. OS-specific capture is handled by separate crates
//! (`flexaudio-os-*`) that implement [`backend::CaptureBackend`], and the facade layer wires the
//! two together.
//!
//! # Fixed contract
//! All internal processing uses interleaved `f32` / 48000 Hz / stereo 2ch / 20ms = 960
//! frames/chunk. The externally delivered rate/channels can be changed with [`OutputFormat`]
//! (the Normalizer's second stage re-converts; e.g. 16k/1ch is 320 frames/chunk), and output
//! chunks are 20ms in time regardless of the rate.
//!
//! The public API has no callbacks. The RT thread only pushes, and the consumer side polls.
//! The RT path is non-blocking (DROP_OLDEST / overflow drop when full). PTS comes from the
//! device, and gaps are detected.
//!
//! # Two-stage ring buffer layout
//! ```text
//! [RT cb] --push--> RawRing (rtrb, RT-safe) --pop--> [ingest/processing thread]
//!                                                       |
//!                                          Normalizer (mix + rubato SRC + 960 slicing)
//!                                                       |
//!                                                       v
//!                                       ChunkRing (ringbuf, DROP_OLDEST) --try_pop--> [poll]
//! ```

#![warn(missing_docs)]

pub mod backend;
pub mod chunk_ring;
pub mod clock;
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
pub use normalizer::{InnerProcessor, Normalizer, CHUNK_FRAMES};
pub use quant::quantize_i16;
pub use raw_ring::{raw_ring, RawConsumer, RawProducer};
pub use secondary_ring::{secondary_chunk_ring, SecondaryChunkConsumer, SecondaryChunkProducer};
pub use types::{
    AudioChunk, ChunkFlags, DeviceEvent, DeviceInfo, Error, Event, OutputFormat, ProcessInfo,
    Result, SecondaryChunk, SourceKind, StreamConfig, CHANNELS, SAMPLE_RATE,
};
