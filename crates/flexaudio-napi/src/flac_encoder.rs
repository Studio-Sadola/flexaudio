//! Pure Rust FLAC lifecycle; Node conversion belongs to the public wrapper.

use flexaudio_encode::{EncodeError, FlacWriter};
use std::path::{Path, PathBuf};

type Result<T> = std::result::Result<T, EncoderError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ErrorKind {
    InvalidArgument,
    Failure,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct EncoderError {
    pub(super) kind: ErrorKind,
    pub(super) reason: String,
}

impl EncoderError {
    fn new(kind: ErrorKind, reason: impl Into<String>) -> Self {
        Self {
            kind,
            reason: reason.into(),
        }
    }
}

impl From<EncodeError> for EncoderError {
    fn from(error: EncodeError) -> Self {
        let kind = match error {
            EncodeError::Unsupported(_) => ErrorKind::InvalidArgument,
            _ => ErrorKind::Failure,
        };
        Self::new(kind, error.to_string())
    }
}

/// Build the path for FLAC rotation `index` (1-based; pure function).
///
/// Same convention as CLI `split_file_path`: `rec.flac` becomes `rec-001.flac, rec-002.flac, …`
/// with a three-digit zero-padded sequence before the extension. Digits grow naturally from file
/// 1000 onward. Paths without extensions append the sequence. The parent directory is preserved.
fn split_flac_path(base: &Path, index: u64) -> PathBuf {
    let stem = base
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = match base.extension() {
        Some(ext) => format!("{stem}-{index:03}.{}", ext.to_string_lossy()),
        None => format!("{stem}-{index:03}"),
    };
    base.with_file_name(name)
}

enum FlacEncoderState {
    Open,
    Finalized,
    Failed(EncoderError),
}

fn validate_flac_destination(base: &Path) -> Result<()> {
    if base.is_dir() || base.file_stem().is_none_or(|stem| stem.is_empty()) {
        return Err(EncoderError::new(
            ErrorKind::InvalidArgument,
            "destination must have a nonempty file stem and must not be a directory",
        ));
    }
    Ok(())
}

pub(super) struct RotatingFlacEncoder {
    /// Output base path (sequence naming base when split; used unchanged for a single file).
    base: PathBuf,
    state: FlacEncoderState,
    sample_rate: u32,
    channels: u16,
    /// Frame threshold per file (splitSeconds × sampleRate). 0 = single file.
    frames_per_file: u64,
    /// Current writer. None immediately after rotation (lazily created on the next chunk).
    writer: Option<FlacWriter>,
    /// Frames written to the current file (reset to 0 on rotation).
    frames_in_current: u64,
    /// Sequence number of the next file to open (1-based; meaningful only when splitting).
    file_index: u64,
}

impl RotatingFlacEncoder {
    /// Create a FLAC writer. Omitted/0 `splitSeconds` uses one file; at least 1 enables timed rotation.
    ///
    /// `channels` must be 1..=2, `sampleRate` 1..=96000 Hz (otherwise InvalidArg). When splitting,
    /// the first file (`name-001.flac`) is created immediately.
    pub(super) fn create(
        base: PathBuf,
        sample_rate: u32,
        channels: u16,
        split_seconds: Option<u32>,
    ) -> Result<RotatingFlacEncoder> {
        validate_flac_destination(&base)?;
        let frames_per_file = u64::from(split_seconds.unwrap_or(0)) * u64::from(sample_rate);
        let file_index = 1;
        // Open base for a single file, or name-001.ext as the first split file.
        let first_path = if frames_per_file > 0 {
            split_flac_path(&base, file_index)
        } else {
            base.clone()
        };
        let writer =
            FlacWriter::create(&first_path, sample_rate, channels).map_err(EncoderError::from)?;
        Ok(RotatingFlacEncoder {
            base,
            state: FlacEncoderState::Open,
            sample_rate,
            channels,
            frames_per_file,
            writer: Some(writer),
            frames_in_current: 0,
            file_index,
        })
    }

    /// Path of the next file to open when splitting.
    fn next_path(&self) -> PathBuf {
        if self.frames_per_file > 0 {
            split_flac_path(&self.base, self.file_index)
        } else {
            self.base.clone()
        }
    }

    /// Append interleaved f32 (length must be a multiple of `channels`, otherwise InvalidArg).
    ///
    /// After writing, if the current file's frame count reaches the threshold, finalize immediately and
    /// rotate to the next file (the next `writeChunk` starts the new file).
    pub(super) fn write_chunk(&mut self, samples: &[f32]) -> Result<()> {
        self.ensure_open()?;
        if !samples.len().is_multiple_of(usize::from(self.channels)) {
            return Err(EncoderError::new(
                ErrorKind::InvalidArgument,
                format!(
                    "chunk length {} is not a multiple of channels {}",
                    samples.len(),
                    self.channels
                ),
            ));
        }
        if samples.is_empty() {
            return Ok(());
        }
        let result = self.write_validated(samples);
        self.latch_result(result)
    }

    fn ensure_open(&self) -> Result<()> {
        match &self.state {
            FlacEncoderState::Open => Ok(()),
            FlacEncoderState::Finalized => Err(EncoderError::new(
                ErrorKind::Failure,
                "writer is already finalized",
            )),
            FlacEncoderState::Failed(error) => Err(error.clone()),
        }
    }

    fn latch_result(&mut self, result: Result<()>) -> Result<()> {
        if let Err(error) = &result {
            self.state = FlacEncoderState::Failed(error.clone());
        }
        result
    }

    fn write_validated(&mut self, samples: &[f32]) -> Result<()> {
        // writer=None immediately after rotation. Open the next file here (lazy creation).
        if self.writer.is_none() {
            let path = self.next_path();
            self.writer = Some(
                FlacWriter::create(&path, self.sample_rate, self.channels)
                    .map_err(EncoderError::from)?,
            );
        }
        let writer = self.writer.as_mut().expect("opened immediately above");
        writer.write_chunk(samples).map_err(EncoderError::from)?;

        // Frame count = sample count / channel count. write_chunk validated divisibility.
        let frames = samples.len() as u64 / u64::from(self.channels);
        self.frames_in_current += frames;

        if self.frames_per_file > 0 && self.frames_in_current >= self.frames_per_file {
            // Threshold reached. Finalize the current file; the next chunk starts the next file.
            let done = self.writer.take().expect("written immediately above");
            done.finalize().map_err(EncoderError::from)?;
            self.file_index += 1;
            self.frames_in_current = 0;
        }
        Ok(())
    }

    /// Write out the remainder, finalize the header, and close the open file. Safe to call repeatedly
    /// after success (subsequent calls are no-ops); failures remain errors on every subsequent call.
    /// Dropping without calling this still closes through `FlacWriter`'s
    /// best-effort Drop, but call this to detect write errors.
    pub(super) fn finalize(&mut self) -> Result<()> {
        if matches!(self.state, FlacEncoderState::Finalized) {
            return Ok(());
        }
        self.ensure_open()?;
        let result = match self.writer.take() {
            Some(writer) => writer.finalize().map_err(EncoderError::from),
            None => Ok(()),
        };
        self.latch_result(result)?;
        self.state = FlacEncoderState::Finalized;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn flac_encoder_finalize_never_reopens_destination() {
        let path =
            std::env::temp_dir().join(format!("flexnapi_{}_finalized.flac", std::process::id()));
        let mut encoder = RotatingFlacEncoder::create(path.clone(), 48_000, 2, None).unwrap();
        encoder.finalize().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(encoder.write_chunk(&[0.0; 2]).is_err());
        encoder.finalize().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn flac_encoder_validates_before_lazy_rotation_open() {
        let base = std::env::temp_dir().join(format!("flexnapi_{}_lazy.flac", std::process::id()));
        let first = split_flac_path(&base, 1);
        let second = split_flac_path(&base, 2);
        let mut encoder = RotatingFlacEncoder::create(base.clone(), 48_000, 2, Some(1)).unwrap();
        encoder.write_chunk(&vec![0.0; 96_000]).unwrap();
        std::fs::write(&second, b"existing data").unwrap();
        assert!(encoder.write_chunk(&[0.0]).is_err());
        assert_eq!(std::fs::read(&second).unwrap(), b"existing data");
        std::fs::remove_file(&second).unwrap();
        std::fs::create_dir(&second).unwrap();
        let cause = encoder.write_chunk(&[0.0; 2]).unwrap_err().reason;
        std::fs::remove_dir(&second).unwrap();
        assert_eq!(encoder.write_chunk(&[0.0; 2]).unwrap_err().reason, cause);
        assert_eq!(encoder.finalize().unwrap_err().reason, cause);
        assert!(!second.exists());
        std::fs::remove_file(first).unwrap();
    }

    #[test]
    fn flac_destination_rejects_directories_and_missing_stems() {
        for path in [
            Path::new(""),
            Path::new("."),
            Path::new(".."),
            Path::new("/"),
        ] {
            assert!(validate_flac_destination(path).is_err());
        }
        assert!(validate_flac_destination(&std::env::temp_dir()).is_err());
    }

    // --- split_flac_path (sequence naming; same convention as CLI) ---

    #[test]
    fn split_flac_path_numbering() {
        // With an extension, insert a three-digit zero-padded sequence before it.
        assert_eq!(
            split_flac_path(Path::new("rec.flac"), 1),
            PathBuf::from("rec-001.flac")
        );
        assert_eq!(
            split_flac_path(Path::new("rec.flac"), 12),
            PathBuf::from("rec-012.flac")
        );
        // Digits grow naturally from file 1000 onward.
        assert_eq!(
            split_flac_path(Path::new("rec.flac"), 1000),
            PathBuf::from("rec-1000.flac")
        );
        // Without an extension, append the sequence.
        assert_eq!(
            split_flac_path(Path::new("rec"), 3),
            PathBuf::from("rec-003")
        );
        // The parent directory is preserved.
        assert_eq!(
            split_flac_path(Path::new("/tmp/out/meeting.flac"), 2),
            PathBuf::from("/tmp/out/meeting-002.flac")
        );
    }
}
