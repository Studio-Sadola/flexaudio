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
use std::io::{Seek, Write};
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

trait Output: Write + Seek + Send + Sync + std::panic::UnwindSafe + std::panic::RefUnwindSafe {}
impl<T: Write + Seek + Send + Sync + std::panic::UnwindSafe + std::panic::RefUnwindSafe> Output
    for T
{
}

#[derive(Debug)]
enum WriterState {
    Open,
    Finalized,
    Failed(FailureCause),
}

// File errors contain an OS code or a kind/message. Snapshot these diagnostics because
// io::Error cannot be cloned, preserving both the first cause and the writer's auto traits.
#[derive(Debug)]
enum FailureCause {
    Io {
        code: Option<i32>,
        kind: std::io::ErrorKind,
        message: String,
    },
    Unsupported(String),
    Encoder(String),
}

impl FailureCause {
    fn capture(error: EncodeError) -> Self {
        match error {
            EncodeError::Io(error) => Self::Io {
                code: error.raw_os_error(),
                kind: error.kind(),
                message: error.to_string(),
            },
            EncodeError::Unsupported(message) => Self::Unsupported(message),
            EncodeError::Encoder(message) => Self::Encoder(message),
        }
    }

    fn error(&self) -> EncodeError {
        match self {
            Self::Io {
                code,
                kind,
                message,
            } => EncodeError::Io(match code {
                Some(code) => std::io::Error::from_raw_os_error(*code),
                None => std::io::Error::new(*kind, message.clone()),
            }),
            Self::Unsupported(message) => EncodeError::Unsupported(message.clone()),
            Self::Encoder(message) => EncodeError::Encoder(message.clone()),
        }
    }
}

/// Writer that streams recording chunks to a FLAC file.
///
/// Pass interleaved `f32` samples (the same format as flexaudio's `AudioChunk.data`) to
/// [`write_chunk`](FlacWriter::write_chunk), then finalize the header with
/// [`finalize`](FlacWriter::finalize). Encoding runs synchronously on the caller thread.
///
/// Dropping an open writer without calling finalize still attempts to close the file
/// (writing remaining samples and finalizing the header). Drop never retries a failed
/// writer and never panics; call finalize to detect errors.
pub struct FlacWriter {
    // Unbuffered output avoids BufWriter retrying buffered bytes when a failed writer drops.
    file: Box<dyn Output>,
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
    state: WriterState,
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

        Self::with_output(sample_rate, channels, Box::new(File::create(path)?))
    }

    fn with_output(sample_rate: u32, channels: u16, file: Box<dyn Output>) -> Result<Self> {
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
            file,
            config,
            stream_info,
            frame_buf,
            context,
            sink: ByteSink::new(),
            pending: Vec::new(),
            channels: channels as usize,
            frame_number: 0,
            state: WriterState::Open,
        };
        // Write a placeholder header; finalize overwrites it with final values of the same length.
        let header = writer.header_bytes()?;
        let result = writer.file.write_all(&header).map_err(EncodeError::Io);
        writer.latch(result)?;
        Ok(writer)
    }

    /// Append interleaved `f32` samples.
    ///
    /// The length must be a multiple of `channels` (`AudioChunk.data` from flexaudio can be
    /// passed directly). Otherwise, this returns [`EncodeError::Unsupported`] and writes
    /// nothing. Encoding or I/O failure permanently fails the writer; later writes and
    /// finalize return the first cause. A partial block is buffered and written by the next call or during finalize.
    pub fn write_chunk(&mut self, interleaved: &[f32]) -> Result<()> {
        self.check_open()?;
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
        let result = self.drain_full_blocks();
        self.latch(result)
    }

    /// Write any remaining samples, finalize the header, and close the file.
    ///
    /// Consumes `self`, so the type prevents further calls to
    /// [`write_chunk`](FlacWriter::write_chunk). Drop makes a best-effort attempt at the same
    /// operation, but only finalize reports write errors, so finalize is recommended.
    pub fn finalize(mut self) -> Result<()> {
        self.finish()
    }

    fn check_open(&self) -> Result<()> {
        match &self.state {
            WriterState::Open => Ok(()),
            WriterState::Finalized => Err(EncodeError::Unsupported("writer is finalized".into())),
            WriterState::Failed(error) => Err(error.error()),
        }
    }

    fn latch(&mut self, result: Result<()>) -> Result<()> {
        if let Err(error) = result {
            self.state = WriterState::Failed(FailureCause::capture(error));
            return self.check_open();
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        self.check_open()?;
        let result = self.finish_inner();
        self.latch(result)?;
        self.state = WriterState::Finalized;
        Ok(())
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
        // Publish the FLAC magic last. Until that commit succeeds, even permanently failed
        // output cannot look finalized. File is unbuffered; no fallible work follows commit.
        self.file.flush()?;
        self.file.rewind()?;
        self.file.write_all(b"FAIL")?;
        self.file.write_all(&header[4..])?;
        self.file.flush()?;
        self.file.rewind()?;
        self.file.write_all(&header[..4])?;
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
        if matches!(self.state, WriterState::Open) {
            // Best effort; swallow errors. Call finalize to detect them.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.finish()));
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
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, SeekFrom};
    use std::sync::{Arc, Mutex};

    struct FaultOutput {
        data: Arc<Mutex<Cursor<Vec<u8>>>>,
        calls: Arc<Mutex<usize>>,
        fail_at: usize,
        partial: bool,
        fail_flush_at: usize,
        flushes: usize,
    }

    impl Write for FaultOutput {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            if *calls == self.fail_at {
                if self.partial {
                    return self.data.lock().unwrap().write(&bytes[..bytes.len() / 2]);
                }
                return Err(std::io::Error::other("injected first failure"));
            }
            if self.partial && *calls == self.fail_at + 1 {
                return Err(std::io::Error::other("injected first failure"));
            }
            self.data.lock().unwrap().write(bytes)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.flushes += 1;
            if self.flushes == self.fail_flush_at {
                Err(std::io::Error::other("injected flush failure"))
            } else {
                Ok(())
            }
        }
    }
    impl Seek for FaultOutput {
        fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
            self.data.lock().unwrap().seek(position)
        }
    }

    #[test]
    fn failed_frame_is_never_retried_or_finalized() {
        for partial in [false, true] {
            let data = Arc::new(Mutex::new(Cursor::new(Vec::new())));
            let calls = Arc::new(Mutex::new(0));
            let output = FaultOutput {
                data: data.clone(),
                calls: calls.clone(),
                fail_at: 3,
                partial,
                fail_flush_at: usize::MAX,
                flushes: 0,
            };
            let mut writer = FlacWriter::with_output(48_000, 1, Box::new(output)).unwrap();
            writer.write_chunk(&vec![0.25; BLOCK_SIZE]).unwrap();
            let first = writer
                .write_chunk(&vec![0.5; BLOCK_SIZE])
                .unwrap_err()
                .to_string();
            let count = writer.context.total_samples();
            let digest = writer.context.md5_digest();
            let bytes = data.lock().unwrap().get_ref().clone();
            let writes = *calls.lock().unwrap();
            // Even invalid input after failure reports the original cause.
            assert_eq!(writer.write_chunk(&[]).unwrap_err().to_string(), first);
            assert_eq!(writer.finish().unwrap_err().to_string(), first);
            assert_eq!(writer.context.total_samples(), count);
            assert_eq!(writer.context.md5_digest(), digest);
            assert_eq!(writer.finalize().unwrap_err().to_string(), first);
            assert_eq!(*calls.lock().unwrap(), writes);
            assert_eq!(*data.lock().unwrap().get_ref(), bytes);
            // Placeholder STREAMINFO still advertises zero total samples and zero digest.
            assert!(bytes[22..42].iter().all(|&byte| byte == 0));
        }
    }

    #[test]
    fn failed_drop_and_header_flush_do_not_retry() {
        let data = Arc::new(Mutex::new(Cursor::new(Vec::new())));
        let calls = Arc::new(Mutex::new(0));
        let mut writer = FlacWriter::with_output(
            48_000,
            1,
            Box::new(FaultOutput {
                data: data.clone(),
                calls: calls.clone(),
                fail_at: 2,
                partial: false,
                fail_flush_at: usize::MAX,
                flushes: 0,
            }),
        )
        .unwrap();
        assert!(writer.write_chunk(&vec![0.25; BLOCK_SIZE]).is_err());
        let before = *calls.lock().unwrap();
        drop(writer);
        assert_eq!(*calls.lock().unwrap(), before);

        let mut writer = FlacWriter::with_output(
            48_000,
            1,
            Box::new(FaultOutput {
                data: data.clone(),
                calls: calls.clone(),
                fail_at: usize::MAX,
                partial: false,
                fail_flush_at: 1,
                flushes: 0,
            }),
        )
        .unwrap();
        writer.write_chunk(&[0.25]).unwrap();
        let first = writer.finish().unwrap_err().to_string();
        assert!(first.contains("flush failure"));
        let before = *calls.lock().unwrap();
        assert_eq!(writer.finalize().unwrap_err().to_string(), first);
        assert_eq!(*calls.lock().unwrap(), before);
    }

    #[test]
    fn header_publication_failure_invalidates_magic_and_latches() {
        // Fail metadata publication (including partial write), metadata flush, or magic commit.
        for (fail_at, partial, fail_flush_at) in [
            (4, false, usize::MAX),
            (4, true, usize::MAX),
            (usize::MAX, false, 2),
            (5, false, usize::MAX),
            (5, true, usize::MAX),
        ] {
            let data = Arc::new(Mutex::new(Cursor::new(Vec::new())));
            let calls = Arc::new(Mutex::new(0));
            let mut writer = FlacWriter::with_output(
                48_000,
                1,
                Box::new(FaultOutput {
                    data: data.clone(),
                    calls: calls.clone(),
                    fail_at,
                    partial,
                    fail_flush_at,
                    flushes: 0,
                }),
            )
            .unwrap();
            writer.write_chunk(&[0.25]).unwrap();
            let first = writer.finish().unwrap_err().to_string();
            assert_ne!(&data.lock().unwrap().get_ref()[..4], b"fLaC");
            let before = *calls.lock().unwrap();
            assert_eq!(writer.finalize().unwrap_err().to_string(), first);
            assert_eq!(*calls.lock().unwrap(), before);
        }
    }

    #[test]
    fn encoder_failure_and_auto_traits() {
        fn assert_send_sync<T: Send + Sync + std::panic::UnwindSafe + std::panic::RefUnwindSafe>() {
        }
        assert_send_sync::<FlacWriter>();
        let data = Arc::new(Mutex::new(Cursor::new(Vec::new())));
        let calls = Arc::new(Mutex::new(0));
        let mut writer = FlacWriter::with_output(
            48_000,
            1,
            Box::new(FaultOutput {
                data,
                calls: calls.clone(),
                fail_at: usize::MAX,
                partial: false,
                fail_flush_at: usize::MAX,
                flushes: 0,
            }),
        )
        .unwrap();
        writer.frame_number = usize::MAX;
        let first = writer.write_chunk(&vec![0.25; BLOCK_SIZE]).unwrap_err();
        assert!(matches!(first, EncodeError::Encoder(_)));
        let count = writer.context.total_samples();
        let digest = writer.context.md5_digest();
        assert_eq!(
            writer.write_chunk(&[]).unwrap_err().to_string(),
            first.to_string()
        );
        assert_eq!(writer.context.total_samples(), count);
        assert_eq!(writer.context.md5_digest(), digest);
        assert_eq!(
            writer.finalize().unwrap_err().to_string(),
            first.to_string()
        );
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[test]
    fn invalid_chunk_does_not_fail_an_open_writer() {
        let data = Arc::new(Mutex::new(Cursor::new(Vec::new())));
        let output = FaultOutput {
            data,
            calls: Arc::new(Mutex::new(0)),
            fail_at: usize::MAX,
            partial: false,
            fail_flush_at: usize::MAX,
            flushes: 0,
        };
        let mut writer = FlacWriter::with_output(48_000, 2, Box::new(output)).unwrap();
        assert!(writer.write_chunk(&[0.5]).is_err());
        writer.write_chunk(&[0.5, 0.5]).unwrap();
        writer.finalize().unwrap();
    }

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
