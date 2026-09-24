//! Streaming FLAC writer ([`FlacWriter`]).
//!
//! flacenc's main entry point `encode_with_fixed_block_size` is designed to read all samples
//! from a [`Source`] at once, which for long recordings means holding all the data in memory.
//! Here the per-frame entry point [`flacenc::encode_fixed_size_frame`] is used instead, and
//! each time a block (4096 samples/ch) accumulates it is encoded and streamed to the file.
//!
//! The STREAMINFO header is written at creation with provisional values (total samples 0,
//! zero MD5), and [`FlacWriter::finalize`] seeks to the start and rewrites it with the final
//! values. STREAMINFO is a fixed 34 bytes, so the total header length is always 42 bytes and
//! never changes, which lets it be replaced safely by overwriting.
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

/// Block size per frame (samples per channel). Same as flacenc's default.
const BLOCK_SIZE: usize = 4096;

/// Quantization bit depth. Currently fixed at 16-bit.
/// flacenc itself supports up to 24-bit, so if needed, widening the scale of
/// [`quantize_i16`] and this value would allow writing 24-bit FLAC (for now this is only
/// headroom).
const BITS_PER_SAMPLE: usize = 16;

/// Upper limit of supported sample rates (Hz). The FLAC format itself can represent up to
/// 655,350 Hz, but flacenc's validation limits it to 96 kHz, so we follow that.
const MAX_SAMPLE_RATE: u32 = 96_000;

/// Total FLAC header length: "fLaC" magic 4B + metadata block header 4B + STREAMINFO 34B.
const HEADER_LEN: usize = 42;

/// Quantizes one f32 sample to a 16-bit integer (plain quantization without dither).
///
/// The canonical quantization is [`flexaudio_core::quantize_i16`] (every layer shares the
/// same implementation). flacenc's API requires `i32`, so this delegates to core's `i16`
/// version and widens the result. The scale is 32768 (negative full scale = -1.0 as the
/// reference), `+1.0` is clamped to 32767, out-of-range values saturate, and NaN becomes 0.
#[inline]
fn quantize_i16(x: f32) -> i32 {
    flexaudio_core::quantize_i16(x) as i32
}

/// Helper that maps flacenc errors to [`EncodeError::Encoder`].
fn enc_err(e: impl std::fmt::Display) -> EncodeError {
    EncodeError::Encoder(e.to_string())
}

/// Writer that incrementally writes recorded chunks to a FLAC file.
///
/// Feed interleaved `f32` (the same shape as flexaudio's `AudioChunk.data`) to
/// [`write_chunk`](FlacWriter::write_chunk), and when done, finalize the header with
/// [`finalize`](FlacWriter::finalize). Encoding runs synchronously on the calling thread.
///
/// Dropping without calling finalize still closes the file best-effort (it tries to write
/// out the partial block and finalize the header, and swallows errors). Call finalize when
/// the file must be completed reliably.
pub struct FlacWriter {
    file: BufWriter<File>,
    config: Verified<EncoderConfig>,
    /// Stream info that accumulates the final values. Written back into the header by
    /// finalize.
    stream_info: StreamInfo,
    /// Block buffer for the encoder input (flacenc keeps it split per channel).
    frame_buf: FrameBuf,
    /// Accumulates the MD5 and total sample count (a flacenc mechanism independent of
    /// encoding).
    context: Context,
    /// Sink reused to serialize frames into bits.
    sink: ByteSink,
    /// Carry-over of quantized samples that do not yet fill a block.
    pending: Vec<i32>,
    channels: usize,
    /// Number of the next frame to write (a per-frame sequence, since the block size is
    /// fixed).
    frame_number: usize,
    /// When already finalized, Drop does nothing.
    finalized: bool,
}

impl FlacWriter {
    /// Creates a new 16-bit FLAC file at `path` (an existing file is overwritten).
    ///
    /// The supported range is `channels` 1..=2 and `sample_rate` 1..=96,000 Hz. Out of range
    /// returns [`EncodeError::Unsupported`] (no file is created).
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
        // Declare it as a fixed-block-size stream (same convention as flacenc's bulk entry
        // point).
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
        // Provisional header. finalize overwrites it with the final values at the same length.
        let header = writer.header_bytes()?;
        writer.file.write_all(&header)?;
        Ok(writer)
    }

    /// Appends interleaved `f32` samples.
    ///
    /// The length must be a multiple of `channels` (flexaudio's `AudioChunk.data` can be
    /// passed as-is). Otherwise returns [`EncodeError::Unsupported`] and writes nothing.
    /// A remainder smaller than a block is carried over internally and written by the next
    /// call or by finalize.
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

    /// Writes out the partial frame, finalizes the header, and closes the file.
    ///
    /// It consumes self, so further [`write_chunk`](FlacWriter::write_chunk) calls are
    /// impossible by type. Drop performs the same work best-effort, but only this method can
    /// detect write errors, so finalize is recommended.
    pub fn finalize(mut self) -> Result<()> {
        let result = self.finish_inner();
        // Prevent running twice in Drop (no retry even if it failed).
        self.finalized = true;
        result
    }

    /// Encodes and writes out every full block from pending.
    fn drain_full_blocks(&mut self) -> Result<()> {
        let block_len = BLOCK_SIZE * self.channels;
        // encode_block takes &mut self and cannot be called while self.pending is borrowed,
        // so take it out, process it, and drop the consumed part when putting it back.
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

    /// Encodes one block of quantized samples (only the final block may be partial) into one
    /// frame and writes it to the file.
    fn encode_block(&mut self, block: &[i32]) -> Result<()> {
        self.frame_buf.fill_interleaved(block).map_err(enc_err)?;
        // The MD5 and total sample count are accumulated here.
        self.context.fill_interleaved(block).map_err(enc_err)?;

        let frame = flacenc::encode_fixed_size_frame(
            &self.config,
            &self.frame_buf,
            self.frame_number,
            &self.stream_info,
        )
        .map_err(enc_err)?;
        // Accumulate the min/max frame size statistics (reflected in the header by finalize).
        self.stream_info.update_frame_info(&frame);

        self.sink.clear();
        frame.write(&mut self.sink).map_err(enc_err)?;
        // A FLAC frame ends on a byte boundary, so as_slice gets it without losing anything.
        self.file.write_all(self.sink.as_slice())?;
        self.frame_number += 1;
        Ok(())
    }

    /// The body of finalize. Also called from Drop.
    fn finish_inner(&mut self) -> Result<()> {
        // Emit the remainder as the final frame (FLAC allows only the final frame to be
        // short).
        if !self.pending.is_empty() {
            let tail = std::mem::take(&mut self.pending);
            self.encode_block(&tail)?;
        }

        // Finalize STREAMINFO.
        // - min/max block size: RFC 9639 does not count the final (partial) block, but
        //   update_frame_info does count it, and a remainder under 16 samples produces a
        //   header that violates the spec (claxon and others reject it). This is a
        //   fixed-block-size stream, so restore the declared values.
        // - Total samples: update_frame_info accumulates it too, but the Context value is
        //   authoritative.
        self.stream_info
            .set_block_sizes(BLOCK_SIZE, BLOCK_SIZE)
            .map_err(enc_err)?;
        self.stream_info.set_md5_digest(&self.context.md5_digest());
        self.stream_info
            .set_total_samples(self.context.total_samples());

        // Overwrite the provisional header at the start with the final header of the same
        // length.
        let header = self.header_bytes()?;
        self.file.rewind()?;
        self.file.write_all(&header)?;
        self.file.flush()?;
        Ok(())
    }

    /// Builds the FLAC header ("fLaC" + STREAMINFO block) from the current StreamInfo.
    ///
    /// Always [`HEADER_LEN`] bytes. finalize overwrites the start of the file, relying on the
    /// length being the same at creation and at finalize.
    fn header_bytes(&self) -> Result<Vec<u8>> {
        let mut info = self.stream_info.clone();
        if self.frame_number == 0 {
            // While there are no frames at all, the min/max frame sizes are still flacenc's
            // sentinel values (u32::MAX / 0), so set them to FLAC's "0 = unknown" before
            // writing.
            info.set_frame_sizes(0, 0).map_err(enc_err)?;
        }
        // Serializing a Stream with no frames = only the magic + STREAMINFO block.
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
            // Best-effort. Errors are swallowed (call finalize to detect them).
            let _ = self.finish_inner();
        }
    }
}

// flacenc's types (Verified etc.) do not implement Debug, so this cannot be derived and is
// written by hand.
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
        // Positive full scale does not fit in 16 bits, so it is clamped.
        assert_eq!(quantize_i16(1.0), 32767);
        // Out-of-range values saturate.
        assert_eq!(quantize_i16(2.0), 32767);
        assert_eq!(quantize_i16(-2.0), -32768);
        // Non-finite values.
        assert_eq!(quantize_i16(f32::NAN), 0);
        assert_eq!(quantize_i16(f32::INFINITY), 32767);
        assert_eq!(quantize_i16(f32::NEG_INFINITY), -32768);
        // Rounding is to nearest (round half away from zero). 1.5/32768 is exact in binary.
        assert_eq!(quantize_i16(1.5 / 32768.0), 2);
        assert_eq!(quantize_i16(-1.5 / 32768.0), -2);
    }
}
