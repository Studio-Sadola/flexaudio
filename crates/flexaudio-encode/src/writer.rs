//! FLAC streaming writer ([`FlacWriter`]).
//!
//! flacenc's main entry point, `encode_with_fixed_block_size`, reads all samples from
//! [`Source`] at once, which would keep an entire long recording in memory. This uses the
//! frame-level entry point [`flacenc::encode_fixed_size_frame`] and encodes each block
//! (4096 samples/channel) as soon as it is full, writing it to the file.
//!
//! The STREAMINFO header is initially written with placeholder values (zero total samples
//! and a zero MD5), then [`FlacWriter::finalize`] seeks to the start and replaces them with
//! the final values. STREAMINFO has a fixed length of 34 bytes, so the full header is always
//! 42 bytes and can be safely overwritten.
//!
//! [`Source`]: flacenc::source::Source

use std::fs::File;
use std::io::{BufWriter, Seek, Write};
use std::path::Path;

use flacenc::bitsink::ByteSink;
use flacenc::component::{BitRepr, Stream, StreamInfo};
use flacenc::config::Encoder as EncoderConfig;
use flacenc::error::{Verified, Verify};
use flacenc::source::{Context, Fill, FrameBuf};

use crate::error::{EncodeError, Result};

/// Block size per frame (samples per channel), matching flacenc's default.
const BLOCK_SIZE: usize = 4096;

/// Quantization bit depth. Currently fixed at 16 bits.
/// flacenc supports up to 24 bits, so 24-bit FLAC could be written by expanding the scale
/// in [`quantize_i16`] and this value if needed (not currently implemented).
const BITS_PER_SAMPLE: usize = 16;

/// Maximum supported sample rate (Hz). FLAC itself can represent up to 655,350 Hz, but
/// flacenc's validation is limited to 96 kHz, so this matches that limit.
const MAX_SAMPLE_RATE: u32 = 96_000;

/// Total FLAC header length: "fLaC" magic (4 bytes) + metadata block header (4 bytes) + STREAMINFO (34 bytes).
const HEADER_LEN: usize = 42;

/// Quantize one f32 sample to a 16-bit integer (simple quantization without dithering).
///
/// The canonical quantizer is [`flexaudio_core::quantize_i16`], shared across all layers.
/// flacenc's API requires `i32`, so this delegates to the core `i16` version and widens the
/// result. The scale is 32768 (negative full scale is based on -1.0); `+1.0` is clamped to
/// 32767, out-of-range values saturate, and NaN becomes 0.
#[inline]
fn quantize_i16(x: f32) -> i32 {
    flexaudio_core::quantize_i16(x) as i32
}

/// Helper to map flacenc errors to [`EncodeError::Encoder`].
fn enc_err(e: impl std::fmt::Display) -> EncodeError {
    EncodeError::Encoder(e.to_string())
}

/// Writer that streams recording chunks to a FLAC file.
///
/// Pass interleaved `f32` samples (the same format as flexaudio's `AudioChunk.data`) to
/// [`write_chunk`](FlacWriter::write_chunk), then finalize the header with
/// [`finalize`](FlacWriter::finalize). Encoding runs synchronously on the caller thread.
///
/// Dropping without calling finalize still attempts to close the file (writing any remaining
/// samples and finalizing the header); errors are swallowed. Call finalize to ensure it is
/// complete.
pub struct FlacWriter {
    file: BufWriter<File>,
    config: Verified<EncoderConfig>,
    /// Stream information accumulated for the final header values.
    stream_info: StreamInfo,
    /// Input block buffer; flacenc stores channels separately.
    frame_buf: FrameBuf,
    /// Accumulates the MD5 and total sample count using flacenc's encoding-independent mechanism.
    context: Context,
    /// Reusable sink for serializing frame bits.
    sink: ByteSink,
    /// Carries quantized samples that do not yet fill a block.
    pending: Vec<i32>,
    channels: usize,
    /// Number of the next frame to write; frames are sequential because block size is fixed.
    frame_number: usize,
    /// If finalized, Drop does nothing.
    finalized: bool,
}

impl FlacWriter {
    /// Create a new 16-bit FLAC file at `path`, overwriting any existing file.
    ///
    /// Supported values are `channels` 1..=2 and `sample_rate` 1..=96,000 Hz. Out-of-range
    /// values return [`EncodeError::Unsupported`] without creating a file.
    pub fn create<P: AsRef<Path>>(path: P, sample_rate: u32, channels: u16) -> Result<FlacWriter> {
        if !(1..=2).contains(&channels) {
            return Err(EncodeError::Unsupported(format!(
                "channels must be 1 or 2, got {channels}"
            )));
        }
        if !(1..=MAX_SAMPLE_RATE).contains(&sample_rate) {
            return Err(EncodeError::Unsupported(format!(
                "sample rate must be 1..={MAX_SAMPLE_RATE} Hz, got {sample_rate}"
            )));
        }

        let config = EncoderConfig::default()
            .into_verified()
            .map_err(|(_, e)| enc_err(e))?;
        let mut stream_info =
            StreamInfo::new(sample_rate as usize, channels as usize, BITS_PER_SAMPLE)
                .map_err(enc_err)?;
        // Declare a fixed-block-size stream, matching flacenc's batch entry point.
        stream_info
            .set_block_sizes(BLOCK_SIZE, BLOCK_SIZE)
            .map_err(enc_err)?;
        let frame_buf = FrameBuf::with_size(channels as usize, BLOCK_SIZE).map_err(enc_err)?;
        let context = Context::new(BITS_PER_SAMPLE, channels as usize);

        let mut writer = FlacWriter {
            file: BufWriter::new(File::create(path)?),
            config,
            stream_info,
            frame_buf,
            context,
            sink: ByteSink::new(),
            pending: Vec::new(),
            channels: channels as usize,
            frame_number: 0,
            finalized: false,
        };
        // Write a placeholder header; finalize overwrites it with final values of the same length.
        let header = writer.header_bytes()?;
        writer.file.write_all(&header)?;
        Ok(writer)
    }

    /// Append interleaved `f32` samples.
    ///
    /// The length must be a multiple of `channels` (`AudioChunk.data` from flexaudio can be
    /// passed directly). Otherwise, this returns [`EncodeError::Unsupported`] and writes
    /// nothing. A partial block is buffered and written by the next call or during finalize.
    pub fn write_chunk(&mut self, interleaved: &[f32]) -> Result<()> {
        if interleaved.len() % self.channels != 0 {
            return Err(EncodeError::Unsupported(format!(
                "chunk length {} is not a multiple of channels {}",
                interleaved.len(),
                self.channels
            )));
        }
        self.pending.reserve(interleaved.len());
        self.pending
            .extend(interleaved.iter().map(|&x| quantize_i16(x)));
        self.drain_full_blocks()
    }

    /// Write any remaining samples, finalize the header, and close the file.
    ///
    /// Consumes `self`, so the type prevents further calls to
    /// [`write_chunk`](FlacWriter::write_chunk). Drop makes a best-effort attempt at the same
    /// operation, but only finalize reports write errors, so finalize is recommended.
    pub fn finalize(mut self) -> Result<()> {
        let result = self.finish_inner();
        // Prevent Drop from running this twice; do not retry even if it failed.
        self.finalized = true;
        result
    }

    /// Encode and write all full blocks from pending samples.
    fn drain_full_blocks(&mut self) -> Result<()> {
        let block_len = BLOCK_SIZE * self.channels;
        // encode_block needs &mut self, so pending cannot stay borrowed. Take it out, process
        // it, then remove the consumed samples when restoring it.
        let pending = std::mem::take(&mut self.pending);
        let mut consumed = 0;
        let mut result = Ok(());
        while consumed + block_len <= pending.len() {
            if let Err(e) = self.encode_block(&pending[consumed..consumed + block_len]) {
                result = Err(e);
                break;
            }
            consumed += block_len;
        }
        self.pending = pending;
        self.pending.drain(..consumed);
        result
    }

    /// Encode one block of quantized samples (the final block may be partial) into one frame
    /// and write it to the file.
    fn encode_block(&mut self, block: &[i32]) -> Result<()> {
        self.frame_buf.fill_interleaved(block).map_err(enc_err)?;
        // Accumulate the MD5 and total sample count here.
        self.context.fill_interleaved(block).map_err(enc_err)?;

        let frame = flacenc::encode_fixed_size_frame(
            &self.config,
            &self.frame_buf,
            self.frame_number,
            &self.stream_info,
        )
        .map_err(enc_err)?;
        // Accumulate min/max frame size statistics for the header written during finalize.
        self.stream_info.update_frame_info(&frame);

        self.sink.clear();
        frame.write(&mut self.sink).map_err(enc_err)?;
        // FLAC frames end on byte boundaries, so as_slice returns all of the data.
        self.file.write_all(self.sink.as_slice())?;
        self.frame_number += 1;
        Ok(())
    }

    /// Implementation of finalize, also called by Drop.
    fn finish_inner(&mut self) -> Result<()> {
        // Write remaining samples as the final frame; only the last FLAC frame may be short.
        if !self.pending.is_empty() {
            let tail = std::mem::take(&mut self.pending);
            self.encode_block(&tail)?;
        }

        // Finalize STREAMINFO.
        // - Min/max block size: RFC 9639 excludes the final partial block, but
        //   update_frame_info includes it. If the tail has fewer than 16 samples, this creates
        //   a non-compliant header that claxon and others reject. Restore the declared value
        //   because this is a fixed-block-size stream.
        // - Total samples: update_frame_info also accumulates this, but Context is authoritative.
        self.stream_info
            .set_block_sizes(BLOCK_SIZE, BLOCK_SIZE)
            .map_err(enc_err)?;
        self.stream_info.set_md5_digest(&self.context.md5_digest());
        self.stream_info
            .set_total_samples(self.context.total_samples());

        // Overwrite the placeholder header at the start with the final header of the same length.
        let header = self.header_bytes()?;
        self.file.rewind()?;
        self.file.write_all(&header)?;
        self.file.flush()?;
        Ok(())
    }

    /// Build a FLAC header ("fLaC" + STREAMINFO block) from the current StreamInfo.
    ///
    /// Always [`HEADER_LEN`] bytes. finalize overwrites the start of the file, relying on the
    /// header length staying constant between creation and finalize.
    fn header_bytes(&self) -> Result<Vec<u8>> {
        let mut info = self.stream_info.clone();
        if self.frame_number == 0 {
            // With no frames, min/max frame sizes remain at flacenc's sentinel values
            // (u32::MAX / 0). Write them as FLAC's "0 = unknown" instead.
            info.set_frame_sizes(0, 0).map_err(enc_err)?;
        }
        // Serializing a Stream with no frames writes only the magic and STREAMINFO block.
        let stream = Stream::with_stream_info(info);
        let mut sink = ByteSink::new();
        stream.write(&mut sink).map_err(enc_err)?;
        let bytes = sink.into_inner();
        debug_assert_eq!(bytes.len(), HEADER_LEN);
        Ok(bytes)
    }
}

impl Drop for FlacWriter {
    fn drop(&mut self) {
        if !self.finalized {
            // Best effort; swallow errors. Call finalize to detect them.
            let _ = self.finish_inner();
        }
    }
}

// flacenc types such as Verified do not implement Debug, so implement it manually.
impl std::fmt::Debug for FlacWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlacWriter")
            .field("sample_rate", &self.stream_info.sample_rate())
            .field("channels", &self.channels)
            .field("frame_number", &self.frame_number)
            .field("pending_samples", &self.pending.len())
            .field("finalized", &self.finalized)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::quantize_i16;

    #[test]
    fn quantize_reference_points() {
        assert_eq!(quantize_i16(0.0), 0);
        assert_eq!(quantize_i16(0.25), 8192);
        assert_eq!(quantize_i16(-1.0), -32768);
        // Positive full scale does not fit in 16 bits, so clamp it.
        assert_eq!(quantize_i16(1.0), 32767);
        // Out-of-range values saturate.
        assert_eq!(quantize_i16(2.0), 32767);
        assert_eq!(quantize_i16(-2.0), -32768);
        // Non-finite values.
        assert_eq!(quantize_i16(f32::NAN), 0);
        assert_eq!(quantize_i16(f32::INFINITY), 32767);
        assert_eq!(quantize_i16(f32::NEG_INFINITY), -32768);
        // Round to nearest (round half away from zero). 1.5/32768 is exact in binary.
        assert_eq!(quantize_i16(1.5 / 32768.0), 2);
        assert_eq!(quantize_i16(-1.5 / 32768.0), -2);
    }
}
