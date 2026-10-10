//! flexaudio-vad — An offline VAD addon that runs silero-VAD in pure Rust (tract-onnx).
//!
//! It does not depend on `flexaudio-core` and accepts only a sample slice `&[f32]`. The silero-VAD
//! model (MIT) is embedded in the binary, so no runtime model file or network access is needed.
//!
//! # Streaming example
//! ```no_run
//! use flexaudio_vad::{Vad, VadConfig, VadEvent};
//! let mut vad = Vad::new(VadConfig::default()).unwrap();
//! for chunk in some_audio_chunks() {
//!     for ev in vad.process(chunk).unwrap() {
//!         match ev {
//!             VadEvent::SpeechStart { at_sample } => println!("start @ {at_sample}"),
//!             VadEvent::SpeechEnd { at_sample } => println!("end @ {at_sample}"),
//!         }
//!     }
//! }
//! # fn some_audio_chunks() -> Vec<&'static [f32]> { vec![] }
//! ```
//!
//! # Batch example
//! ```no_run
//! use flexaudio_vad::{get_speech_timestamps, VadConfig};
//! let samples: Vec<f32> = vec![0.0; 16000];
//! let segments = get_speech_timestamps(&samples, &VadConfig::default()).unwrap();
//! for s in segments {
//!     println!("{}..{}", s.start_sample, s.end_sample);
//! }
//! ```

#![warn(missing_docs)]

mod config;
mod infer;
mod resample;
mod segmenter;
mod whisper_params;
mod whisper_postprocess;
mod whisper_preview;
mod whisper_stream;
mod whisper_types;

pub use whisper_params::WhisperVadParams;
pub use whisper_postprocess::WhisperVadPostProcessor;
pub use whisper_stream::{whisper_speech_segments, WhisperVad};
pub use whisper_types::*;

pub use config::VadConfig;
pub use resample::PcmFormat;
pub use segmenter::Segment;

use infer::{SileroEngine, MODEL_FRAME_SIZE, MODEL_SAMPLE_RATE};
use resample::PcmConverter;
use segmenter::Segmenter;

/// Event finalized by VAD. Sample positions are after padding is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VadEvent {
    /// Speech start (absolute sample position after padding).
    SpeechStart {
        /// Absolute sample position where speech starts (after padding, inclusive).
        at_sample: u64,
    },
    /// Speech end (absolute sample position after padding, exclusive).
    SpeechEnd {
        /// Absolute sample position where speech ends (after padding, exclusive).
        at_sample: u64,
    },
}

/// VAD errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VadError {
    /// Failed to load the model.
    ModelLoad(String),
    /// Inference failed.
    Inference(String),
    /// Invalid configuration.
    InvalidConfig(String),
    /// Invalid input PCM format; no state was changed.
    InvalidFormat(String),
    /// PCM conversion failed.
    Resample(String),
    /// Reset failed; the instance cannot process until reset succeeds.
    Reset(String),
}

impl std::fmt::Display for VadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VadError::ModelLoad(m) => write!(f, "model load error: {m}"),
            VadError::Inference(m) => write!(f, "inference error: {m}"),
            VadError::InvalidConfig(m) => write!(f, "invalid config: {m}"),
            VadError::InvalidFormat(m) => write!(f, "invalid PCM format: {m}"),
            VadError::Resample(m) => write!(f, "resampling error: {m}"),
            VadError::Reset(m) => write!(f, "reset error: {m}"),
        }
    }
}

impl std::error::Error for VadError {}

trait InferenceBackend: Send {
    fn infer(&mut self, frame: &[f32]) -> Result<f32, VadError>;
    fn reset(&mut self) -> Result<(), VadError>;
}

impl InferenceBackend for SileroEngine {
    fn infer(&mut self, frame: &[f32]) -> Result<f32, VadError> {
        self.infer_16k_frame(frame)
    }
    fn reset(&mut self) -> Result<(), VadError> {
        SileroEngine::reset(self);
        Ok(())
    }
}

/// Streaming VAD. Each instance owns one optimized tract plan (not shared).
///
/// Pass any-length `&[f32]` input to [`Vad::process`]. It buffers samples into frames (16k=512 / 8k=256),
/// runs Silero inference, advances the segment state machine, and returns finalized events.
pub struct Vad {
    engine: Box<dyn InferenceBackend>,
    converted_sample_position: u64,
    failure: Option<VadError>,
    config: VadConfig,
    segmenter: Segmenter,

    /// Remaining samples below frame_size (at the input sample rate).
    pending: Vec<f32>,

    /// Raw speech probability for each frame computed by the most recent [`Vad::process`].
    last_probs: Vec<f32>,

    /// Front-end converter for [`Vad::process_pcm`] (any format → mono at the VAD rate).
    /// Not created for formats that need no conversion (mono at the VAD rate). Recreate it when the
    /// input format changes.
    converter: Option<PcmConverter>,
    converter_factory: fn(PcmFormat, u32) -> Result<PcmConverter, String>,
    /// Continuous 8→16 kHz rubato converter for 8 kHz configuration. Only model input is upsampled to 16 kHz;
    /// public frame counts and positions in `pending` and `segmenter` remain based on 8 kHz.
    upsampler_8k: Option<PcmConverter>,
    upsampler_factory: fn() -> Result<PcmConverter, String>,
}

impl Vad {
    /// Build VAD with the embedded, optimized model.
    ///
    /// Returns [`VadError::ModelLoad`] if plan construction (`into_optimized`) fails.
    pub fn new(config: VadConfig) -> Result<Vad, VadError> {
        config.validate().map_err(VadError::InvalidConfig)?;

        let engine = SileroEngine::load()?;
        let segmenter = Segmenter::new(&config);
        let upsampler_8k = if config.sample_rate == 8_000 {
            Some(PcmConverter::new_8k_to_16k_frame_resampler().map_err(VadError::ModelLoad)?)
        } else {
            None
        };

        Ok(Vad {
            engine: Box::new(engine),
            converted_sample_position: 0,
            failure: None,
            config,
            segmenter,
            pending: Vec::new(),
            last_probs: Vec::new(),
            converter: None,
            converter_factory: PcmConverter::new,
            upsampler_8k,
            upsampler_factory: PcmConverter::new_8k_to_16k_frame_resampler,
        })
    }

    /// Process any number of f32 samples and return finalized [`VadEvent`]s.
    ///
    /// Internally buffers input into frame_size units and holds any remainder for the next call. Sample positions
    /// are continuous across calls (cumulative).
    ///
    /// Returns inference/conversion errors with their cause. After a processing failure,
    /// processing and flush return that error until reset succeeds.
    pub fn process(&mut self, samples: &[f32]) -> Result<Vec<VadEvent>, VadError> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        self.converted_sample_position += samples.len() as u64;
        let frame_size = self.config.frame_size();
        self.last_probs.clear();

        let mut segments_out = Vec::new();
        let mut events = Vec::new();

        // Append new samples to pending and consume complete frame_size units.
        self.pending.extend_from_slice(samples);

        let mut offset = 0;
        // Cannot call infer_frame (&mut self) while self.pending is borrowed, so
        // copy one frame into a local buffer before inference.
        let mut frame_buf = vec![0.0f32; frame_size];
        while offset + frame_size <= self.pending.len() {
            frame_buf.copy_from_slice(&self.pending[offset..offset + frame_size]);
            let prob = match self.infer_frame(&frame_buf) {
                Ok(prob) => prob,
                Err(error) => {
                    self.failure = Some(error.clone());
                    return Err(error);
                }
            };
            self.last_probs.push(prob);
            self.segmenter.feed(prob, &mut segments_out);
            offset += frame_size;
        }
        // Discard consumed samples.
        self.pending.drain(0..offset);

        for seg in segments_out {
            events.push(VadEvent::SpeechStart {
                at_sample: seg.start_sample,
            });
            events.push(VadEvent::SpeechEnd {
                at_sample: seg.end_sample,
            });
        }
        Ok(events)
    }

    /// Self-contained entry point that accepts a recording chunk as-is. Converts any format (`input_sample_rate` /
    /// interleaved f32 with `input_channels`) to mono and resamples to the VAD rate internally before
    /// using the same path as [`Vad::process`].
    ///
    /// This lets language bindings pass recording chunks (such as 48 kHz/stereo) without conversion.
    /// Input rates from 8,000 through 192,000 Hz and nonzero channel counts are supported.
    /// `samples` must be interleaved and its length should be a multiple of `input_channels`
    /// (partial frames are carried over, so input may be split at any point).
    ///
    /// If the input is already mono at the VAD rate ([`VadConfig::sample_rate`]), pass it directly to
    /// [`Vad::process`] without downmixing or resampling (no added cost). Otherwise, average channels
    /// to mono and resample with rubato. The resampler retains state across calls,
    /// avoiding seams in a continuous stream.
    ///
    /// The returned [`VadEvent`] `at_sample` is measured at the **internal VAD rate (`config().sample_rate` = 16000
    /// or 8000)**, not the input rate (cumulative internal position after resampling).
    /// This matches [`Vad::process`], so positions remain continuous if the APIs are mixed. Convert to seconds with
    /// `at_sample as f64 / config().sample_rate as f64`; estimate the input sample position with
    /// `at_sample * input_sample_rate / config().sample_rate`.
    ///
    /// Invalid formats are rejected before changing any state. Processing failures require a
    /// successful reset before processing resumes. Converter setup errors preserve existing state.
    pub fn process_pcm(
        &mut self,
        samples: &[f32],
        input_sample_rate: u32,
        input_channels: u16,
    ) -> Result<Vec<VadEvent>, VadError> {
        let target = self.config.sample_rate;
        let format = PcmFormat {
            sample_rate: input_sample_rate,
            channels: input_channels,
        };

        format.validate()?;
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        // Prepare replacement before modifying the existing converter.
        if self.converter.as_ref().is_none_or(|c| !c.matches(format)) {
            let replacement = if input_sample_rate == target && input_channels == 1 {
                None
            } else {
                Some((self.converter_factory)(format, target).map_err(VadError::Resample)?)
            };
            self.converter = replacement;
        }
        let Some(conv) = self.converter.as_mut() else {
            return self.process(samples);
        };
        let mut converted = Vec::new();
        if let Err(message) = conv.convert(samples, &mut converted) {
            let error = VadError::Resample(message);
            self.failure = Some(error.clone());
            return Err(error);
        }
        self.process(&converted)
    }

    /// Run one frame (public `frame_size` samples) through Silero and return its speech probability.
    ///
    /// The model requires 16 kHz / 512 samples. For 8 kHz configuration, use a stateful rubato sinc
    /// resampler to convert 256 samples to 512, then use the same 64+512 prefix and state-carrying path.
    /// Public timestamps,
    /// sample positions, and frame counts remain at the input rate (the segmenter advances by `frame_size`).
    fn infer_frame(&mut self, frame: &[f32]) -> Result<f32, VadError> {
        debug_assert_eq!(frame.len(), self.config.frame_size());
        // The public-rate context is always 64 when converted to 16 kHz (32×2 at 8 kHz).
        debug_assert_eq!(
            if self.config.sample_rate == 8000 {
                self.config.context_size() * 2
            } else {
                self.config.context_size()
            },
            64
        );
        if self.config.sample_rate == MODEL_SAMPLE_RATE {
            debug_assert_eq!(frame.len(), MODEL_FRAME_SIZE);
            self.engine.infer(frame)
        } else {
            let upsampler = self
                .upsampler_8k
                .as_mut()
                .ok_or_else(|| VadError::Inference("8 kHz resampler is unavailable".to_string()))?;
            let mut up = Vec::with_capacity(MODEL_FRAME_SIZE);
            upsampler
                .convert(frame, &mut up)
                .map_err(VadError::Inference)?;
            if up.len() != MODEL_FRAME_SIZE {
                return Err(VadError::Inference(format!(
                    "8 kHz resampler produced {} samples (expected {MODEL_FRAME_SIZE})",
                    up.len()
                )));
            }
            self.engine.infer(&up)
        }
    }

    /// Return raw speech probabilities for each frame computed by the latest [`Vad::process`].
    ///
    /// A second output, independent of segment events.
    pub fn last_frame_probabilities(&self) -> &[f32] {
        &self.last_probs
    }

    /// Force-close any open speech segment at the current position and return the
    /// resulting events (as if the input had ended here), then reset internal
    /// state so the next input starts a fresh context.
    ///
    /// This is the streaming counterpart of what [`get_speech_timestamps`] does at
    /// the end of a batch: an open segment is only ever closed by trailing silence,
    /// `max_speech_ms`, or the end of input, so a real-time caller that stops (or
    /// pauses) recognition needs a way to materialize the segment it is currently
    /// inside. Cheap and deterministic: no model inference is run (unlike feeding
    /// trailing silence), and the terminal position is `next_pos` regardless of any
    /// buffered partial frame. Returned event positions (`at_sample`) are on the
    /// pre-reset accumulator, so ordering and positions are preserved.
    ///
    /// Returns an empty vector when no speech segment is open (or the open segment
    /// is shorter than `min_speech_ms` and is discarded, matching the segmenter).
    /// Returns a latched processing error or a reset error instead of successful events.
    pub fn flush(&mut self) -> Result<Vec<VadEvent>, VadError> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        let mut segments_out = Vec::new();
        self.segmenter.flush(&mut segments_out);
        let mut events = Vec::new();
        for seg in segments_out {
            events.push(VadEvent::SpeechStart {
                at_sample: seg.start_sample,
            });
            events.push(VadEvent::SpeechEnd {
                at_sample: seg.end_sample,
            });
        }
        // Start the next input in a fresh context (reset cumulative position, state, and remainder).
        self.reset()?;
        Ok(events)
    }

    /// Reset state, context, state machine, sample position, remainder buffer, and resampler state.
    /// Returns [`VadError::Reset`] if rebuilding the resampler or resetting inference fails.
    /// Buffers and position are cleared only after those operations succeed.
    pub fn reset(&mut self) -> Result<(), VadError> {
        let replacement = if self.config.sample_rate == 8_000 {
            match (self.upsampler_factory)() {
                Ok(converter) => Some(converter),
                Err(message) => {
                    let error = VadError::Reset(message);
                    self.failure = Some(error.clone());
                    return Err(error);
                }
            }
        } else {
            None
        };
        if let Err(cause) = self.engine.reset() {
            let error = VadError::Reset(cause.to_string());
            self.failure = Some(error.clone());
            return Err(error);
        }
        self.upsampler_8k = replacement;
        self.pending.clear();
        self.last_probs.clear();
        self.segmenter.reset();
        self.converter = None;
        self.converted_sample_position = 0;
        self.failure = None;
        Ok(())
    }

    /// Actual cumulative mono samples delivered by conversion at the configured VAD rate.
    /// Includes samples buffered below one inference frame. Reset and flush return this to zero.
    /// Read before processing a chunk to anchor its event timestamps without per-call rounding.
    pub fn converted_sample_position(&self) -> u64 {
        self.converted_sample_position
    }

    /// Borrow the current configuration.
    pub fn config(&self) -> &VadConfig {
        &self.config
    }
}

/// Batch processing (equivalent to Silero `get_speech_timestamps`).
///
/// Process all samples at once and return finalized segments. If speech continues at the end, extend it to the input end.
pub fn get_speech_timestamps(
    samples: &[f32],
    config: &VadConfig,
) -> Result<Vec<Segment>, VadError> {
    let mut vad = Vad::new(config.clone())?;
    let frame_size = config.frame_size();

    let mut out = Vec::new();
    let mut chunks = samples.chunks_exact(frame_size);
    for frame in chunks.by_ref() {
        let prob = vad.infer_frame(frame)?;
        vad.last_probs.push(prob);
        vad.segmenter.feed(prob, &mut out);
    }
    // Like Silero, discard incomplete frames without inference. Finalize speech still open at end of input.
    vad.segmenter.flush(&mut out);

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::VadConfig;
    use crate::segmenter::{Segment, Segmenter};

    struct TestBackend {
        fail_infer: bool,
        fail_reset: bool,
    }
    impl InferenceBackend for TestBackend {
        fn infer(&mut self, _: &[f32]) -> Result<f32, VadError> {
            if self.fail_infer {
                Err(VadError::Inference("injected inference failure".into()))
            } else {
                Ok(0.9)
            }
        }
        fn reset(&mut self) -> Result<(), VadError> {
            if self.fail_reset {
                Err(VadError::Reset("injected reset failure".into()))
            } else {
                self.fail_infer = false;
                Ok(())
            }
        }
    }
    fn test_vad(fail_infer: bool, fail_reset: bool) -> Vad {
        let config = VadConfig::default();
        Vad {
            engine: Box::new(TestBackend {
                fail_infer,
                fail_reset,
            }),
            segmenter: Segmenter::new(&config),
            config,
            converted_sample_position: 0,
            failure: None,
            pending: Vec::new(),
            last_probs: Vec::new(),
            converter: None,
            converter_factory: PcmConverter::new,
            upsampler_8k: None,
            upsampler_factory: PcmConverter::new_8k_to_16k_frame_resampler,
        }
    }

    #[test]
    fn inference_failure_is_not_silence_and_requires_reset() {
        let mut vad = test_vad(true, false);
        let error = vad.process(&[0.0; 512]).unwrap_err();
        assert!(matches!(error, VadError::Inference(_)));
        assert!(vad.last_frame_probabilities().is_empty());
        assert_eq!(vad.process(&[]).unwrap_err(), error);
        assert_eq!(vad.flush().unwrap_err(), error);
        vad.reset().unwrap();
        assert_eq!(vad.converted_sample_position(), 0);
        vad.process(&[0.0; 512]).unwrap();
        assert_eq!(vad.last_frame_probabilities(), &[0.9]);
    }

    #[test]
    fn converter_construction_failure_preserves_existing_state() {
        let mut vad = test_vad(false, false);
        vad.process_pcm(&[0.0; 220], 11_025, 1).unwrap();
        let position = vad.converted_sample_position();
        vad.converter_factory = |_, _| Err("injected converter construction failure".into());
        let error = vad.process_pcm(&[0.0; 512], 48_000, 2).unwrap_err();
        assert!(matches!(error, VadError::Resample(_)));
        assert!(error
            .to_string()
            .contains("injected converter construction"));
        assert_eq!(vad.converted_sample_position(), position);
        assert!(vad.converter.as_ref().unwrap().matches(PcmFormat {
            sample_rate: 11_025,
            channels: 1
        }));
        // Setup failure did not mutate state, so the existing format can continue without reset.
        vad.process_pcm(&[0.0; 220], 11_025, 1).unwrap();
    }

    #[test]
    fn conversion_failure_is_typed_and_requires_reset() {
        let mut vad = test_vad(false, false);
        vad.process_pcm(&[0.0; 220], 11_025, 1).unwrap();
        vad.converter.as_mut().unwrap().fail_conversion();
        let error = vad.process_pcm(&[0.0; 220], 11_025, 1).unwrap_err();
        assert!(matches!(error, VadError::Resample(_)));
        assert!(error.to_string().contains("injected conversion failure"));
        assert_eq!(vad.process_pcm(&[], 11_025, 1).unwrap_err(), error);
        assert_eq!(vad.flush().unwrap_err(), error);
        vad.reset().unwrap();
        vad.process_pcm(&[0.0; 220], 11_025, 1).unwrap();
    }

    #[test]
    fn reset_failure_is_typed_and_blocks_processing() {
        let mut vad = test_vad(false, true);
        vad.process(&[0.0; 100]).unwrap();
        let error = vad.reset().unwrap_err();
        assert!(matches!(error, VadError::Reset(_)));
        assert!(error.to_string().contains("injected reset failure"));
        assert_eq!(vad.converted_sample_position(), 100);
        assert_eq!(vad.process(&[]).unwrap_err(), error);
    }

    #[test]
    fn reset_propagates_8k_resampler_construction_failure() {
        let mut vad = test_vad(false, false);
        vad.config.sample_rate = 8_000;
        vad.upsampler_8k = Some(PcmConverter::new_8k_to_16k_frame_resampler().unwrap());
        vad.upsampler_factory = || Err("injected 8 kHz reset construction failure".into());
        let error = vad.reset().unwrap_err();
        assert!(matches!(error, VadError::Reset(_)));
        assert!(error.to_string().contains("injected 8 kHz"));
        assert!(vad.upsampler_8k.is_some());
        assert_eq!(vad.flush().unwrap_err(), error);
        vad.upsampler_factory = PcmConverter::new_8k_to_16k_frame_resampler;
        vad.reset().unwrap();
        vad.process(&[0.0; 256]).unwrap();
    }

    #[test]
    fn flush_propagates_reset_failure() {
        let mut vad = test_vad(false, true);
        vad.process(&[0.0; 512]).unwrap();
        assert!(matches!(vad.flush(), Err(VadError::Reset(_))));
    }

    #[test]
    fn invalid_pcm_does_not_mutate_state() {
        let mut vad = test_vad(false, false);
        vad.process_pcm(&[0.0; 220], 11_025, 2).unwrap();
        let position = vad.converted_sample_position();
        let pending = vad.pending.clone();
        for (rate, channels) in [(16_000, 0), (0, 1), (7_999, 1), (192_001, 1), (u32::MAX, 1)] {
            assert!(matches!(
                vad.process_pcm(&[1.0; 512], rate, channels),
                Err(VadError::InvalidFormat(_))
            ));
            assert_eq!(vad.converted_sample_position(), position);
            assert_eq!(vad.pending, pending);
            assert!(vad.converter.as_ref().unwrap().matches(PcmFormat {
                sample_rate: 11_025,
                channels: 2
            }));
        }
    }

    #[test]
    fn cumulative_converted_position_has_no_chunk_rounding_drift() {
        let mut vad = test_vad(false, false);
        let mut converter = PcmConverter::new(
            PcmFormat {
                sample_rate: 11_025,
                channels: 1,
            },
            16_000,
        )
        .unwrap();
        let mut actual = Vec::new();
        for _ in 0..1_000 {
            let anchor = vad.converted_sample_position();
            assert_eq!(anchor, actual.len() as u64);
            converter.convert(&[0.0; 220], &mut actual).unwrap();
            vad.process_pcm(&[0.0; 220], 11_025, 1).unwrap();
            assert_eq!(vad.converted_sample_position(), actual.len() as u64);
        }
        assert!(vad.converted_sample_position() > 319 * 1_000 + 100);
        let mut bulk = test_vad(false, false);
        bulk.process_pcm(&vec![0.0; 220_000], 11_025, 1).unwrap();
        assert_eq!(
            bulk.converted_sample_position(),
            vad.converted_sample_position()
        );
        vad.flush().unwrap();
        assert_eq!(vad.converted_sample_position(), 0);
    }

    /// Helper that feeds probabilities to the segmenter and returns segments (assumes frame_size=512 at 16 kHz).
    fn run_probs(config: &VadConfig, probs: &[f32]) -> Vec<Segment> {
        let mut seg = Segmenter::new(config);
        let mut out = Vec::new();
        for &p in probs {
            seg.feed(p, &mut out);
        }
        seg.flush(&mut out);
        out
    }

    fn base_config() -> VadConfig {
        // Set pad=0 to simplify testing boundary logic. Assumes 512 samples per frame.
        VadConfig {
            threshold: 0.5,
            neg_threshold: Some(0.35),
            min_speech_ms: 0, // Disable discarding (overridden by individual tests).
            min_silence_ms: 0,
            speech_pad_ms: 0,
            max_speech_ms: 0,
            sample_rate: 16000,
        }
    }

    /// Downsample real 16 kHz audio to 8 kHz with rubato, then compare the 8 kHz VAD path with
    /// passing the same 8 kHz signal directly to the model using proper sinc 8→16 kHz conversion.
    ///
    /// 0.05 allows resampler implementation differences on real audio while remaining far below the
    /// 0.3705613 maximum measured with the old sample-repetition method. The 0.5 decisions must match for every frame.
    #[test]
    fn eight_khz_inference_matches_sinc_upsampled_reference() {
        let wav = include_bytes!("../tests/fixtures/jp_2spk_FF_4s_16k.wav");
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        let samples16: Vec<f32> = wav[44..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]) as f32 / 32768.0)
            .collect();

        let mut downsampler = PcmConverter::new(
            PcmFormat {
                sample_rate: 16_000,
                channels: 1,
            },
            8_000,
        )
        .expect("16 kHz to 8 kHz resampler");
        let mut samples8 = Vec::new();
        downsampler
            .convert(&samples16, &mut samples8)
            .expect("downsample fixture");
        samples8.truncate(samples8.len() / 256 * 256);
        assert!(!samples8.is_empty());

        let mut reference_upsampler = PcmConverter::new_8k_to_16k_frame_resampler()
            .expect("8 kHz to 16 kHz reference resampler");
        let mut reference16 = Vec::with_capacity(samples8.len() * 2);
        for frame in samples8.as_chunks::<256>().0 {
            let before = reference16.len();
            reference_upsampler
                .convert(frame, &mut reference16)
                .expect("upsample reference frame");
            assert_eq!(reference16.len() - before, 512);
        }

        let mut vad16 = Vad::new(VadConfig::default()).expect("16 kHz model load");
        let _ = vad16.process(&reference16).unwrap();
        let probs16 = vad16.last_frame_probabilities();

        let cfg8 = VadConfig {
            sample_rate: 8_000,
            ..VadConfig::default()
        };
        let mut vad8 = Vad::new(cfg8).expect("8 kHz model load");
        let _ = vad8.process(&samples8).unwrap();
        let probs8 = vad8.last_frame_probabilities().to_vec();

        assert_eq!(probs8.len(), samples8.len() / 256);
        assert_eq!(probs8.len(), probs16.len());
        for (frame, (&prob8, &prob16)) in probs8.iter().zip(probs16.iter()).enumerate() {
            assert!(
                (prob8 - prob16).abs() <= 0.05,
                "frame {frame}: 8 kHz={prob8}, sinc 16 kHz reference={prob16}"
            );
            assert_eq!(
                prob8 >= 0.5,
                prob16 >= 0.5,
                "frame {frame}: speech threshold decision differs"
            );
        }

        // After reset, the dedicated constructor still starts with pre-roll, so each 512-sample frame
        // and probability sequence match the initial run.
        vad8.reset().unwrap();
        let _ = vad8.process(&samples8).unwrap();
        assert_eq!(vad8.last_frame_probabilities(), probs8);
    }

    #[test]
    fn neg_threshold_default_formula() {
        let mut c = VadConfig::default();
        assert_eq!(c.resolved_neg_threshold(), (0.5 - 0.15_f32).max(0.01));
        c.threshold = 0.1;
        assert_eq!(c.resolved_neg_threshold(), 0.01); // Clamp minimum.
        c.neg_threshold = Some(0.2);
        assert_eq!(c.resolved_neg_threshold(), 0.2); // Explicit value takes precedence.
    }

    #[test]
    fn simple_speech_then_silence() {
        // 512 samples per frame. min_silence=512 (1 frame), min_speech=0.
        let mut c = base_config();
        c.min_silence_ms = 32; // 32ms @16k = 512 samples = 1 frame
                               // 20 speech frames → 30 silence frames
        let mut probs = vec![0.9f32; 20];
        probs.extend(vec![0.1f32; 30]);
        let segs = run_probs(&c, &probs);
        assert_eq!(segs.len(), 1, "exactly one segment");
        let s = segs[0];
        // Start = beginning of frame 0 = 0.
        assert_eq!(s.start_sample, 0);
        // End = start of silence (beginning of frame 20 = 20*512).
        assert_eq!(s.end_sample, 20 * 512);
    }

    #[test]
    fn min_speech_discards_short_segment() {
        let mut c = base_config();
        c.min_silence_ms = 32; // 1 frame
        c.min_speech_ms = 250; // 250ms @16k = 4000 samples ≈ 7.8 frames
                               // 5 speech frames (5*512=2560 < 4000) → should be discarded
        let mut probs = vec![0.9f32; 5];
        probs.extend(vec![0.1f32; 10]);
        let segs = run_probs(&c, &probs);
        assert!(
            segs.is_empty(),
            "short segment must be discarded, got {segs:?}"
        );

        // 10 speech frames (5120 >= 4000) → accepted
        let mut probs2 = vec![0.9f32; 10];
        probs2.extend(vec![0.1f32; 10]);
        let segs2 = run_probs(&c, &probs2);
        assert_eq!(segs2.len(), 1);
        assert_eq!(segs2[0].len_samples(), 10 * 512);
    }

    #[test]
    fn min_silence_boundary_keeps_segment_together() {
        // Short silence (< min_silence) does not end the segment.
        let mut c = base_config();
        c.min_silence_ms = 192; // 192ms @16k = 3072 samples = 6 frames
                                // 10 speech → 3 silence (3*512=1536 < 3072, so continue) → 10 speech → long silence
        let mut probs = vec![0.9f32; 10];
        probs.extend(vec![0.1f32; 3]);
        probs.extend(vec![0.9f32; 10]);
        probs.extend(vec![0.1f32; 10]); // 10*512=5120 >= 3072, so finalize.
        let segs = run_probs(&c, &probs);
        assert_eq!(segs.len(), 1, "short gap must NOT split, got {segs:?}");
        assert_eq!(segs[0].start_sample, 0);
        // End = start of silence after the second speech burst = beginning of frame 23.
        assert_eq!(segs[0].end_sample, 23 * 512);
    }

    #[test]
    fn min_silence_just_over_splits() {
        // Silence just exceeding min_silence ends the segment.
        let mut c = base_config();
        c.min_silence_ms = 64; // 64ms = 1024 samples = 2 frames
                               // 5 speech → 3 silence (3*512=1536) → 5 speech → silence
                               // On the third silence frame, evaluate (frame_start - temp_end):
                               //   temp_end is the start of the first silence frame. Start of silence frame N - temp_end = (N-1)*512.
                               //   >= 1024 when N-1 >= 2 → N>=3 → finalize when feeding frame 3.
        let mut probs = vec![0.9f32; 5];
        probs.extend(vec![0.1f32; 3]);
        probs.extend(vec![0.9f32; 5]);
        probs.extend(vec![0.1f32; 5]);
        let segs = run_probs(&c, &probs);
        assert_eq!(segs.len(), 2, "long gap must split into two, got {segs:?}");
        assert_eq!(segs[0].start_sample, 0);
        assert_eq!(segs[0].end_sample, 5 * 512); // Start of silence.
    }

    #[test]
    fn speech_pad_extends_and_clamps() {
        let mut c = base_config();
        c.min_silence_ms = 32; // 1 frame
        c.speech_pad_ms = 32; // 32ms = 512 samples
                              // Frames 2..5 are speech (prepend two silence frames so start padding is not clamped to 0).
        let mut probs = vec![0.1f32; 2]; // Frames 0 and 1 are silence.
        probs.extend(vec![0.9f32; 4]); // Frames 2..5 are speech (start=2*512=1024).
        probs.extend(vec![0.1f32; 5]); // Silence.
        let segs = run_probs(&c, &probs);
        assert_eq!(segs.len(), 1);
        // Start = 1024 - 512 = 512.
        assert_eq!(segs[0].start_sample, 1024 - 512);
        // End = silence start (6*512=3072) + 512 = 3584.
        assert_eq!(segs[0].end_sample, 6 * 512 + 512);
    }

    #[test]
    fn speech_pad_start_clamps_at_zero() {
        // Speech starts in frame 0 → start padding clamps to 0 without underflow.
        let mut c = base_config();
        c.min_silence_ms = 32;
        c.speech_pad_ms = 64; // 1024 samples
        let mut probs = vec![0.9f32; 5];
        probs.extend(vec![0.1f32; 5]);
        let segs = run_probs(&c, &probs);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].start_sample, 0, "start pad must clamp to 0");
    }

    #[test]
    fn pad_does_not_overlap_previous_segment() {
        // With two segments, clamp padding so the later start padding does not overlap the earlier end padding.
        let mut c = base_config();
        c.min_silence_ms = 64; // 2 frames
        c.speech_pad_ms = 192; // 3072 samples = 6 frames (intentionally large).
                               // 5 speech → 3 silence (finalize) → 5 speech → silence
        let mut probs = vec![0.9f32; 5];
        probs.extend(vec![0.1f32; 3]);
        probs.extend(vec![0.9f32; 5]);
        probs.extend(vec![0.1f32; 5]);
        let segs = run_probs(&c, &probs);
        assert_eq!(segs.len(), 2);
        // seg0 end padding and seg1 start padding do not overlap (seg1.start >= seg0.end).
        assert!(
            segs[1].start_sample >= segs[0].end_sample,
            "seg1.start ({}) must be >= seg0.end ({})",
            segs[1].start_sample,
            segs[0].end_sample
        );
    }

    #[test]
    fn max_speech_forces_split_when_no_silence() {
        // Force-split when max_speech is exceeded without any silence.
        let mut c = base_config();
        c.max_speech_ms = 192; // 3072 samples = 6 frames
        c.min_silence_ms = 32;
        // 20 continuous speech frames (no silence). Split every max_speech=6 frames.
        let probs = vec![0.9f32; 20];
        let segs = run_probs(&c, &probs);
        assert!(
            segs.len() >= 2,
            "max_speech must force at least one split, got {segs:?}"
        );
        // Each segment is cut at about max_speech.
        for s in &segs {
            assert!(
                s.len_samples() <= c.ms_to_samples(c.max_speech_ms) + 512,
                "segment {s:?} exceeds max_speech by more than one frame"
            );
        }
    }

    #[test]
    fn gray_zone_keeps_triggered() {
        // Speech continues in the gray zone threshold > prob >= neg_threshold (no split).
        let mut c = base_config(); // threshold 0.5, neg 0.35
        c.min_silence_ms = 32;
        let mut probs = vec![0.9f32; 5];
        probs.extend(vec![0.4f32; 5]); // Gray zone (0.35 <= 0.4 < 0.5).
        probs.extend(vec![0.9f32; 5]);
        probs.extend(vec![0.1f32; 5]); // Actual silence.
        let segs = run_probs(&c, &probs);
        assert_eq!(segs.len(), 1, "gray zone must not split, got {segs:?}");
        assert_eq!(segs[0].end_sample, 15 * 512);
    }

    // ---- Inference-path smoke tests ----

    #[test]
    fn vad_loads_model() {
        let vad = Vad::new(VadConfig::default());
        assert!(vad.is_ok(), "model load failed: {:?}", vad.err());
    }

    #[test]
    fn zeros_produce_low_prob_no_speech() {
        let mut vad = Vad::new(VadConfig::default()).unwrap();
        let zeros = vec![0.0f32; 16000];
        let events = vad.process(&zeros).unwrap();
        // Silent input does not produce SpeechStart.
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, VadEvent::SpeechStart { .. })),
            "silence must not trigger SpeechStart, got {events:?}"
        );
        let probs = vad.last_frame_probabilities();
        assert_eq!(probs.len(), 16000 / 512, "expected one prob per frame");
        for &p in probs {
            assert!((0.0..=1.0).contains(&p), "prob {p} out of [0,1]");
            assert!(p < 0.5, "silence prob {p} unexpectedly high");
        }
    }

    #[test]
    fn process_streams_across_calls() {
        // All frames are processed even when partial samples cross call boundaries.
        let mut vad = Vad::new(VadConfig::default()).unwrap();
        // 300 + 300 + ... crosses 512-sample boundaries. Split 512*4 = 2048 samples into pieces.
        let total = 2048usize;
        let chunk = vec![0.0f32; 300];
        let mut fed = 0usize;
        let mut total_probs = 0usize;
        while fed < total {
            let take = chunk.len().min(total - fed);
            vad.process(&chunk[..take]).unwrap();
            total_probs += vad.last_frame_probabilities().len();
            fed += take;
        }
        assert_eq!(
            total_probs,
            total / 512,
            "all complete frames must be inferred"
        );
    }

    #[test]
    fn reset_clears_state() {
        let mut vad = Vad::new(VadConfig::default()).unwrap();
        vad.process(&vec![0.0f32; 1000]).unwrap(); // Leave a remainder in pending.
        vad.reset().unwrap();
        assert_eq!(vad.last_frame_probabilities().len(), 0);
        assert_eq!(vad.config().sample_rate, 16000);
    }

    #[test]
    fn invalid_sample_rate_rejected() {
        let c = VadConfig {
            sample_rate: 44100,
            ..VadConfig::default()
        };
        let r = Vad::new(c);
        assert!(matches!(r, Err(VadError::InvalidConfig(_))));
    }

    #[test]
    fn batch_get_speech_timestamps_on_silence() {
        let zeros = vec![0.0f32; 16000];
        let segs = get_speech_timestamps(&zeros, &VadConfig::default()).unwrap();
        assert!(segs.is_empty(), "silence yields no segments, got {segs:?}");
    }

    // ---- flush (force-finalize an open segment) ----

    /// With threshold=0, every frame counts as speech and no silence arrives, so `process` does not
    /// finalize the segment. `flush` can force-finalize it; afterward, reset
    /// is complete (cumulative position returns to 0) and the next input starts in a fresh context.
    #[test]
    fn flush_closes_open_speech_segment() {
        let cfg = VadConfig {
            threshold: 0.0,
            neg_threshold: Some(0.0),
            min_speech_ms: 0,
            min_silence_ms: 0,
            speech_pad_ms: 0,
            max_speech_ms: 0,
            sample_rate: 16_000,
        };
        let mut vad = Vad::new(cfg).unwrap();
        // Feed one second. No silence arrives, so process does not finalize the segment.
        let sig = vec![0.1f32; 16_000];
        let during = vad.process(&sig).unwrap();
        assert!(
            during.is_empty(),
            "Without silence, process does not finalize the segment: {during:?}"
        );
        // flush finalizes the open segment → speechStart + speechEnd pair.
        let flushed = vad.flush().unwrap();
        assert_eq!(
            flushed.len(),
            2,
            "flush finalizes the open segment: {flushed:?}"
        );
        assert!(matches!(flushed[0], VadEvent::SpeechStart { at_sample: 0 }));
        assert!(matches!(flushed[1], VadEvent::SpeechEnd { .. }));

        // flush resets state, starting a fresh context. The same input starts again at 0.
        let after = vad.process(&sig).unwrap();
        assert!(
            after.is_empty(),
            "After reset, the new segment is not finalized: {after:?}"
        );
        let flushed2 = vad.flush().unwrap();
        assert_eq!(flushed2.len(), 2);
        assert_eq!(
            flushed[0], flushed2[0],
            "After reset, the start position returns to 0 and the same input has the same start"
        );
    }

    /// flush returns empty when no segment is open (silence only).
    #[test]
    fn flush_on_idle_returns_empty() {
        let mut vad = Vad::new(VadConfig::default()).unwrap();
        let zeros = vec![0.0f32; 16_000];
        vad.process(&zeros).unwrap();
        let flushed = vad.flush().unwrap();
        assert!(
            flushed.is_empty(),
            "flush is empty when no speech is open: {flushed:?}"
        );
    }

    /// An open segment shorter than min_speech is discarded by flush as well (per the segmenter filter).
    #[test]
    fn flush_discards_segment_shorter_than_min_speech() {
        let cfg = VadConfig {
            threshold: 0.0,
            neg_threshold: Some(0.0),
            min_speech_ms: 1_000, // Discard segments shorter than 1 second.
            min_silence_ms: 0,
            speech_pad_ms: 0,
            max_speech_ms: 0,
            sample_rate: 16_000,
        };
        let mut vad = Vad::new(cfg).unwrap();
        // Feed only 300 ms (< min_speech 1000 ms), then flush.
        let sig = vec![0.1f32; 16_000 * 300 / 1000];
        vad.process(&sig).unwrap();
        let flushed = vad.flush().unwrap();
        assert!(
            flushed.is_empty(),
            "Open segments shorter than min_speech are discarded by flush too: {flushed:?}"
        );
    }

    // ---- process_pcm (self-contained entry point) ----

    /// Deterministic in-band (<8 kHz) test signal. A composite wave with harmonics makes resampling effects visible.
    /// Silero does not treat synthetic waves as speech (low probability with defaults), so events are
    /// generated mainly with a low threshold. Verify resampling by comparing probability sequences.
    fn harmonics(rate: usize, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = i as f32 / rate as f32;
                let mut v = 0.0f32;
                for (k, f) in [180.0f32, 540.0, 1200.0, 2600.0].iter().enumerate() {
                    v += (0.4 / (k as f32 + 1.0)) * (2.0 * std::f32::consts::PI * f * t).sin();
                }
                v * 0.5
            })
            .collect()
    }

    /// Duplicate mono into interleaved stereo (L=R).
    fn to_stereo(mono: &[f32]) -> Vec<f32> {
        let mut s = Vec::with_capacity(mono.len() * 2);
        for &v in mono {
            s.push(v);
            s.push(v);
        }
        s
    }

    /// Passing 16 kHz/mono to process_pcm behaves exactly like process (event and probability sequences
    /// are bit-identical): the pass-through path.
    #[test]
    fn process_pcm_passthrough_matches_process() {
        let sig = harmonics(16_000, 16_000);
        let mut vad = Vad::new(VadConfig::default()).unwrap();

        let e_pcm = vad.process_pcm(&sig, 16_000, 1).unwrap();
        let p_pcm = vad.last_frame_probabilities().to_vec();

        vad.reset().unwrap();
        let e_proc = vad.process(&sig).unwrap();
        let p_proc = vad.last_frame_probabilities().to_vec();

        assert_eq!(
            e_pcm, e_proc,
            "passthrough event sequence differs from process"
        );
        assert_eq!(
            p_pcm, p_proc,
            "passthrough probability sequence is not bit-identical to process"
        );
        assert_eq!(p_pcm.len(), 16_000 / 512, "unexpected frame count");
    }

    /// Splitting 48 kHz/stereo input across process_pcm calls produces the same event and probability sequences
    /// as a single batch (resampler state continues across calls without seams). Split at odd lengths
    /// so input also crosses frame boundaries.
    #[test]
    fn process_pcm_split_matches_bulk() {
        // Silero gives synthetic signals low speech probability, so configure events to appear
        // regardless of probability. threshold=0 treats all frames as speech; max_speech
        // force-splits at fixed intervals. Event positions depend on frames after resampling, so
        // differences between split and batch input shift the event sequence and reveal seams.
        let cfg = VadConfig {
            threshold: 0.0,
            neg_threshold: Some(0.0),
            min_speech_ms: 0,
            min_silence_ms: 0,
            speech_pad_ms: 0,
            max_speech_ms: 200, // Force-split every 3200 samples (200 ms at 16 kHz).
            sample_rate: 16_000,
        };
        let stereo = to_stereo(&harmonics(48_000, 48_000));

        let mut vad = Vad::new(cfg).unwrap();

        // Submit as one batch.
        let bulk_events = vad.process_pcm(&stereo, 48_000, 2).unwrap();
        let bulk_probs = vad.last_frame_probabilities().to_vec();

        vad.reset().unwrap();

        // Submit in chunks of 777 samples (odd length, crossing frame boundaries).
        let mut split_events = Vec::new();
        let mut split_probs = Vec::new();
        for chunk in stereo.chunks(777) {
            let evs = vad.process_pcm(chunk, 48_000, 2).unwrap();
            split_events.extend(evs);
            split_probs.extend_from_slice(vad.last_frame_probabilities());
        }

        assert_eq!(
            bulk_probs, split_probs,
            "split and batch probability sequences differ (a seam was introduced)"
        );
        assert_eq!(
            bulk_events, split_events,
            "split and batch event sequences differ"
        );
        assert!(
            !bulk_events.is_empty(),
            "this configuration should produce speech events (verify the test is effective)"
        );
    }

    /// After reset, processing the same input with process_pcm produces identical event and probability sequences
    /// (reset also clears resampler state).
    #[test]
    fn process_pcm_reset_is_deterministic() {
        let stereo = to_stereo(&harmonics(48_000, 24_000));

        let mut vad = Vad::new(VadConfig::default()).unwrap();
        let e1 = vad.process_pcm(&stereo, 48_000, 2).unwrap();
        let p1 = vad.last_frame_probabilities().to_vec();

        vad.reset().unwrap();
        let e2 = vad.process_pcm(&stereo, 48_000, 2).unwrap();
        let p2 = vad.last_frame_probabilities().to_vec();

        assert_eq!(e1, e2, "same input after reset produced different events");
        assert_eq!(
            p1, p2,
            "same input after reset produced different probabilities"
        );
    }

    /// Processing 48 kHz/stereo through process_pcm should nearly match creating the same waveform directly at 16 kHz/mono and passing it to process.
    /// Compare probabilities excluding transients (a few frames at each end).
    #[test]
    fn process_pcm_48k_stereo_matches_direct_16k() {
        let n16 = 16_000usize;
        let ref16 = harmonics(16_000, n16);
        let stereo48 = to_stereo(&harmonics(48_000, n16 * 3));

        let mut ref_vad = Vad::new(VadConfig::default()).unwrap();
        let ref_events = ref_vad.process(&ref16).unwrap();
        let ref_probs = ref_vad.last_frame_probabilities().to_vec();

        let mut pcm_vad = Vad::new(VadConfig::default()).unwrap();
        let pcm_events = pcm_vad.process_pcm(&stereo48, 48_000, 2).unwrap();
        let pcm_probs = pcm_vad.last_frame_probabilities().to_vec();

        // Resampler latency can change frame count by at most one. Compare probabilities in the shared middle region.
        let n = ref_probs.len().min(pcm_probs.len());
        assert!(n >= 8, "at least 8 frames are required: {n}");
        for k in 2..(n - 2) {
            let d = (ref_probs[k] - pcm_probs[k]).abs();
            assert!(
                d < 0.05,
                "frame {k}: probability difference between direct 16 kHz and converted paths is too large: {d} (ref={} pcm={})",
                ref_probs[k],
                pcm_probs[k]
            );
        }
        // With defaults, synthetic signals count as silence, so both paths produce no speech events (equivalent).
        assert_eq!(
            ref_events, pcm_events,
            "converted-path events differ from direct 16 kHz events"
        );
    }
}
