//! flexaudio-encode — an add-on that incrementally compresses recorded chunks into a FLAC
//! file.
//!
//! It does not depend on `flexaudio-core` and takes only interleaved `&[f32]` samples.
//! Encoding is done by the pure-Rust flacenc, so neither system libraries nor a network at
//! runtime are needed. A long recording kept as WAV reaches gigabytes; this compresses it
//! losslessly to roughly half (e.g. a 3-hour meeting recording: WAV about 2GB → FLAC a few
//! hundred MB).
//!
//! Each received chunk is encoded block by block and streamed to the file, so memory usage is
//! constant regardless of recording length. The stream info (total samples, MD5, etc.) is
//! written back into the header and finalized by [`FlacWriter::finalize`].
//!
//! # Example
//! ```no_run
//! use flexaudio_encode::FlacWriter;
//!
//! // Assumes flexaudio's canonical format (48kHz / stereo) is passed as-is.
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
