//! flexaudio-encode — an add-on that compresses recording chunks into FLAC files as they arrive.
//!
//! It does not depend on `flexaudio-core` and accepts only interleaved `&[f32]` samples.
//! Encoding uses the pure Rust `flacenc` crate, with no system libraries or runtime
//! network access. It can losslessly compress long recordings to roughly half the size
//! of WAV (for example, a 3-hour meeting recording of about 2 GB becomes a few hundred MB).
//!
//! Each incoming chunk is encoded in blocks and streamed to a file, so memory use stays
//! constant regardless of recording length. Stream metadata (total sample count, MD5,
//! etc.) is finalized in the header by [`FlacWriter::finalize`].
//!
//! # Example
//! ```no_run
//! use flexaudio_encode::FlacWriter;
//!
//! // Pass flexaudio's canonical format (48 kHz / stereo) directly.
//! let mut writer = FlacWriter::create("meeting.flac", 48_000, 2).unwrap();
//! for chunk in some_audio_chunks() {
//!     writer.write_chunk(chunk).unwrap();
//! }
//! writer.finalize().unwrap();
//! # fn some_audio_chunks() -> Vec<&'static [f32]> { vec![] }
//! ```

#![warn(missing_docs)]

mod error;
mod writer;

pub use error::{EncodeError, Result};
pub use writer::FlacWriter;
