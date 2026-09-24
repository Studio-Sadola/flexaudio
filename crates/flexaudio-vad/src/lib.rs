//! flexaudio-vad — a VAD add-on that runs silero-VAD offline in pure Rust (tract-onnx).
//!
//! It does not depend on `flexaudio-core` and takes only `&[f32]` samples. The silero-VAD
//! model (MIT) is embedded in the binary, so neither a model file nor a network is needed at
//! runtime.
//!
//! # Example (streaming)
//! ```no_run
//! use flexaudio_vad::{Vad, VadConfig, VadEvent};
//! let mut vad = Vad::new(VadConfig::default()).unwrap();
//! for chunk in some_audio_chunks() {
//!     for ev in vad.process(chunk) {
//!         match ev {
//!             VadEvent::SpeechStart { at_sample } => println!("start @ {at_sample}"),
//!             VadEvent::SpeechEnd { at_sample } => println!("end @ {at_sample}"),
//!         }
//!     }
//! }
//! # fn some_audio_chunks() -> Vec<&'static [f32]> { vec![] }
//! ```
//!
//! # Example (batch)
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

pub use config::VadConfig;
pub use resample::PcmFormat;
pub use segmenter::Segment;

use infer::{SileroEngine, MODEL_FRAME_SIZE, MODEL_SAMPLE_RATE};
use resample::PcmConverter;
use segmenter::Segmenter;

/// An event finalized by the VAD. Sample positions are after padding is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VadEvent {
    /// Speech start (absolute sample position after padding).
    SpeechStart {
        /// Absolute sample position where speech started (after padding, inclusive).
        at_sample: u64,
    },
    /// Speech end (absolute sample position after padding, exclusive).
    SpeechEnd {
        /// Absolute sample position where speech ended (after padding, exclusive).
        at_sample: u64,
    },
}

/// VAD error type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VadError {
    /// Failed to load the model.
    ModelLoad(String),
    /// Error during inference.
    Inference(String),
    /// Invalid configuration value.
    InvalidConfig(String),
}

impl std::fmt::Display for VadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VadError::ModelLoad(m) => write!(f, "model load error: {m}"),
            VadError::Inference(m) => write!(f, "inference error: {m}"),
            VadError::InvalidConfig(m) => write!(f, "invalid config: {m}"),
        }
    }
}

impl std::error::Error for VadError {}

/// Streaming VAD. Each instance owns one optimized tract plan (not shared).
///
/// Feeding `&[f32]` of any length to [`Vad::process`] groups it internally into frames
/// (16k=512 / 8k=256), runs silero inference, drives the segment state machine, and returns
/// the finalized events.
pub struct Vad {
    engine: SileroEngine,
    config: VadConfig,
    segmenter: Segmenter,

    /// Leftover samples that do not fill a frame_size (in terms of the input sample rate).
    pending: Vec<f32>,

    /// Raw speech probability of each frame computed by the latest [`Vad::process`].
    last_probs: Vec<f32>,

    /// Front-end converter for [`Vad::process_pcm`] (any format → mono at the VAD rate).
    /// Not created for formats that need no conversion (mono at the VAD rate). Rebuilt when
    /// the input format changes.
    converter: Option<PcmConverter>,
    /// Continuous 8→16 kHz rubato converter, used only with the 8 kHz setting. Only the model
    /// input is raised to 16 kHz; the public frame counts and positions stay on the 8 kHz
    /// basis of `pending` and `segmenter`.
    upsampler_8k: Option<PcmConverter>,
}

impl Vad {
    /// Optimizes the embedded model and constructs the VAD.
    ///
    /// Returns [`VadError::ModelLoad`] if building the plan (`into_optimized`) fails.
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
            engine,
            config,
            segmenter,
            pending: Vec::new(),
            last_probs: Vec::new(),
            converter: None,
            upsampler_8k,
        })
    }

    /// Processes f32 samples of any length and returns the finalized [`VadEvent`]s.
    ///
    /// Samples are grouped internally into frame_size units, and those that do not fill a
    /// frame are kept until the next call. Sample positions are continuous across calls
    /// (cumulative).
    pub fn process(&mut self, samples: &[f32]) -> Vec<VadEvent> {
        let frame_size = self.config.frame_size();
        self.last_probs.clear();

        let mut segments_out = Vec::new();
        let mut events = Vec::new();

        // Concatenate pending + the new samples and consume them in frame_size units.
        self.pending.extend_from_slice(samples);

        let mut offset = 0;
        // infer_frame takes &mut self and cannot be given data while self.pending is borrowed,
        // so copy one frame into a local buffer before inference.
        let mut frame_buf = vec![0.0f32; frame_size];
        while offset + frame_size <= self.pending.len() {
            frame_buf.copy_from_slice(&self.pending[offset..offset + frame_size]);
            // On inference failure, fall back to silence (0.0) and continue.
            let prob = self.infer_frame(&frame_buf).unwrap_or(0.0);
            self.last_probs.push(prob);
            self.segmenter.feed(prob, &mut segments_out);
            offset += frame_size;
        }
        // Discard the consumed part.
        self.pending.drain(0..offset);

        for seg in segments_out {
            events.push(VadEvent::SpeechStart {
                at_sample: seg.start_sample,
            });
            events.push(VadEvent::SpeechEnd {
                at_sample: seg.end_sample,
            });
        }
        events
    }

    /// Self-contained entry point that accepts recorded chunks as-is. Any format
    /// (interleaved f32 at `input_sample_rate` / `input_channels`) is downmixed to mono and
    /// resampled to the VAD rate internally, then run through the same path as
    /// [`Vad::process`].
    ///
    /// The aim is to let each language binding feed recorded chunks (e.g. 48k/stereo) as-is,
    /// without converting them. `samples` is interleaved, and its length is expected to be a
    /// multiple of `input_channels` (partial frames are carried over internally to the next
    /// call, so the input may be split at any position).
    ///
    /// If the input is already mono at the VAD rate ([`VadConfig::sample_rate`]), it is passed
    /// to [`Vad::process`] as-is with neither downmixing nor resampling (no extra cost).
    /// Otherwise it is downmixed to mono by averaging the channels and resampled with rubato.
    /// The resampler keeps state across calls, so no seams appear even on a continuous stream.
    ///
    /// The `at_sample` of the returned [`VadEvent`]s is **in samples at the VAD's internal rate
    /// (`config().sample_rate` = 16000 or 8000)**, not in input samples (the cumulative
    /// internal position after resampling). It is aligned with [`Vad::process`], so positions
    /// stay continuous even when the two are mixed. To convert to seconds use
    /// `at_sample as f64 / config().sample_rate as f64`; if an approximate input sample
    /// position is needed, `at_sample * input_sample_rate / config().sample_rate` approximates
    /// it.
    ///
    /// If building or running the resampler fails (e.g. an extreme rate ratio), that call's
    /// input is discarded and an empty event list is returned (ingestion is not stopped by a
    /// panic; the same policy as [`Vad::process`] falling back to silence on inference
    /// failure).
    pub fn process_pcm(
        &mut self,
        samples: &[f32],
        input_sample_rate: u32,
        input_channels: u16,
    ) -> Vec<VadEvent> {
        let target = self.config.sample_rate;
        let format = PcmFormat {
            sample_rate: input_sample_rate,
            channels: input_channels,
        };

        // If the input format changed since last time, drop the converter (it is rebuilt below
        // if needed).
        if let Some(c) = &self.converter {
            if !c.matches(format) {
                self.converter = None;
            }
        }

        // If no conversion is needed (already mono at the VAD rate), go to the existing path
        // without extra copying or resampling.
        if input_sample_rate == target && input_channels <= 1 {
            return self.process(samples);
        }

        // Prepare the converter (first time, or on a format change). On construction failure,
        // discard this call and continue.
        if self.converter.is_none() {
            match PcmConverter::new(format, target) {
                Ok(c) => self.converter = Some(c),
                Err(_) => return Vec::new(),
            }
        }

        // Downmix to mono + resample to get mono at the VAD rate, then feed the existing path.
        let mut converted = Vec::new();
        {
            let conv = self
                .converter
                .as_mut()
                .expect("converter was prepared just above");
            if conv.convert(samples, &mut converted).is_err() {
                return Vec::new();
            }
        }
        self.process(&converted)
    }

    /// Runs one frame (public `frame_size` samples) through silero and returns the speech
    /// probability.
    ///
    /// The model is 16 kHz / 512 samples only. With the 8 kHz setting, a stateful rubato sinc
    /// resampler turns 256 samples into 512, which are then passed to the same 64+512
    /// prefix and state carry-over path. The public-side times, sample positions, and frame
    /// counts stay in terms of the input rate (the segmenter advances by `frame_size`).
    fn infer_frame(&mut self, frame: &[f32]) -> Result<f32, VadError> {
        debug_assert_eq!(frame.len(), self.config.frame_size());
        // The public-rate context converted to 16 kHz is always 64 (8 kHz is 32×2).
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
            self.engine.infer_16k_frame(frame)
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
            self.engine.infer_16k_frame(&up)
        }
    }

    /// Returns the raw speech probability of each frame computed by the latest
    /// [`Vad::process`].
    ///
    /// A second output, independent of the segment events.
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
    pub fn flush(&mut self) -> Vec<VadEvent> {
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
        // The next input starts from a fresh context (reset the cumulative position, state,
        // and leftovers).
        self.reset();
        events
    }

    /// Resets the state / context / state machine / sample position / leftover buffer /
    /// resampler state.
    pub fn reset(&mut self) {
        self.engine.reset();
        self.pending.clear();
        self.last_probs.clear();
        self.segmenter.reset();
        // Drop the converter. The next process_pcm rebuilds it for the format, so the
        // resampler's internal delay and leftovers are reset along with it.
        self.converter = None;
        if self.config.sample_rate == 8_000 {
            // The dedicated constructor includes the pre-roll. Rebuilding it as a general
            // converter would yield 508 samples on the first call only and break the
            // correspondence with public frames, so always use this one.
            self.upsampler_8k = PcmConverter::new_8k_to_16k_frame_resampler().ok();
        }
    }

    /// Reference to the current configuration.
    pub fn config(&self) -> &VadConfig {
        &self.config
    }
}

/// For batch processing (equivalent to silero's `get_speech_timestamps`).
///
/// Processes all samples at once and returns the finalized segments. If speech is ongoing at
/// the end, the segment extends to the end of the input.
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
    // As in silero, the partial frame is discarded without inference. If speech is ongoing at
    // the end of the input, finalize it.
    vad.segmenter.flush(&mut out);

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::VadConfig;
    use crate::segmenter::{Segment, Segmenter};

    /// Helper that feeds a probability sequence to the segmenter and gets the segments,
    /// assuming frame_size=512 @16k.
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
        // pad=0 makes the pure boundary logic easier to verify. Assumes 512 samples/frame.
        VadConfig {
            threshold: 0.5,
            neg_threshold: Some(0.35),
            min_speech_ms: 0, // disable discarding (overridden in individual tests)
            min_silence_ms: 0,
            speech_pad_ms: 0,
            max_speech_ms: 0,
            sample_rate: 16000,
        }
    }

    /// Downsamples real 16 kHz speech to 8 kHz with rubato, and compares passing the same
    /// 8 kHz signal directly to the model via a correct sinc 8→16 kHz conversion against
    /// passing it through `Vad`'s 8 kHz path.
    ///
    /// 0.05 tolerates resampler implementation differences on real speech while staying well
    /// below the maximum difference of 0.3705613 measured with the old sample repetition. The
    /// 0.5 decision must match on every frame.
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
        let _ = vad16.process(&reference16);
        let probs16 = vad16.last_frame_probabilities();

        let cfg8 = VadConfig {
            sample_rate: 8_000,
            ..VadConfig::default()
        };
        let mut vad8 = Vad::new(cfg8).expect("8 kHz model load");
        let _ = vad8.process(&samples8);
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

        // After reset it again starts from the dedicated constructor's pre-roll, so the
        // 1 frame = 512 samples correspondence and the probability sequence are the same as
        // the first time.
        vad8.reset();
        let _ = vad8.process(&samples8);
        assert_eq!(vad8.last_frame_probabilities(), probs8);
    }

    #[test]
    fn neg_threshold_default_formula() {
        let mut c = VadConfig::default();
        assert_eq!(c.resolved_neg_threshold(), (0.5 - 0.15_f32).max(0.01));
        c.threshold = 0.1;
        assert_eq!(c.resolved_neg_threshold(), 0.01); // clamp lower bound
        c.neg_threshold = Some(0.2);
        assert_eq!(c.resolved_neg_threshold(), 0.2); // explicit value wins
    }

    #[test]
    fn simple_speech_then_silence() {
        // 512 samples/frame. min_silence=512 (=1 frame), min_speech=0.
        let mut c = base_config();
        c.min_silence_ms = 32; // 32ms @16k = 512 samples = 1 frame
                               // 20 frames of speech → 30 frames of silence
        let mut probs = vec![0.9f32; 20];
        probs.extend(vec![0.1f32; 30]);
        let segs = run_probs(&c, &probs);
        assert_eq!(segs.len(), 1, "exactly one segment");
        let s = segs[0];
        // Start = start of frame 0 = 0.
        assert_eq!(s.start_sample, 0);
        // End = silence start position (start of frame 20 = 20*512).
        assert_eq!(s.end_sample, 20 * 512);
    }

    #[test]
    fn min_speech_discards_short_segment() {
        let mut c = base_config();
        c.min_silence_ms = 32; // 1 frame
        c.min_speech_ms = 250; // 250ms @16k = 4000 samples ≈ 7.8 frames
                               // 5 frames of speech (5*512=2560 < 4000) → must be discarded
        let mut probs = vec![0.9f32; 5];
        probs.extend(vec![0.1f32; 10]);
        let segs = run_probs(&c, &probs);
        assert!(
            segs.is_empty(),
            "short segment must be discarded, got {segs:?}"
        );

        // 10 frames of speech (5120 >= 4000) → accepted
        let mut probs2 = vec![0.9f32; 10];
        probs2.extend(vec![0.1f32; 10]);
        let segs2 = run_probs(&c, &probs2);
        assert_eq!(segs2.len(), 1);
        assert_eq!(segs2[0].len_samples(), 10 * 512);
    }

    #[test]
    fn min_silence_boundary_keeps_segment_together() {
        // A short silence (< min_silence) does not split the segment.
        let mut c = base_config();
        c.min_silence_ms = 192; // 192ms @16k = 3072 samples = 6 frames
                                // 10 speech → 3 silence (3*512=1536 < 3072, so it continues)
                                // → 10 speech → long silence
        let mut probs = vec![0.9f32; 10];
        probs.extend(vec![0.1f32; 3]);
        probs.extend(vec![0.9f32; 10]);
        probs.extend(vec![0.1f32; 10]); // finalized since 10*512=5120 >= 3072
        let segs = run_probs(&c, &probs);
        assert_eq!(segs.len(), 1, "short gap must NOT split, got {segs:?}");
        assert_eq!(segs[0].start_sample, 0);
        // End = silence start after the second speech block = start of frame 23.
        assert_eq!(segs[0].end_sample, 23 * 512);
    }

    #[test]
    fn min_silence_just_over_splits() {
        // A silence just exceeding min_silence splits.
        let mut c = base_config();
        c.min_silence_ms = 64; // 64ms = 1024 samples = 2 frames
                               // 5 speech → 3 silence (3*512=1536) → 5 speech → silence
                               // At the 3rd silent frame, (frame_start - temp_end) is evaluated:
                               //   temp_end is the start of the 1st silent frame.
                               //   Start of Nth silent frame - temp_end = (N-1)*512.
                               //   It is >= 1024 when N-1 >= 2 → N>=3 → finalized at the
                               //   feed of the 3rd frame.
        let mut probs = vec![0.9f32; 5];
        probs.extend(vec![0.1f32; 3]);
        probs.extend(vec![0.9f32; 5]);
        probs.extend(vec![0.1f32; 5]);
        let segs = run_probs(&c, &probs);
        assert_eq!(segs.len(), 2, "long gap must split into two, got {segs:?}");
        assert_eq!(segs[0].start_sample, 0);
        assert_eq!(segs[0].end_sample, 5 * 512); // silence start position
    }

    #[test]
    fn speech_pad_extends_and_clamps() {
        let mut c = base_config();
        c.min_silence_ms = 32; // 1 frame
        c.speech_pad_ms = 32; // 32ms = 512 samples
                              // Frames 2..5 are speech (2 silent frames are placed first so
                              // the start pad is not clamped at 0)
        let mut probs = vec![0.1f32; 2]; // frames 0,1 silent
        probs.extend(vec![0.9f32; 4]); // frames 2..5 speech (start=2*512=1024)
        probs.extend(vec![0.1f32; 5]); // silence
        let segs = run_probs(&c, &probs);
        assert_eq!(segs.len(), 1);
        // Start = 1024 - 512 = 512.
        assert_eq!(segs[0].start_sample, 1024 - 512);
        // End = silence start (6*512=3072) + 512 = 3584.
        assert_eq!(segs[0].end_sample, 6 * 512 + 512);
    }

    #[test]
    fn speech_pad_start_clamps_at_zero() {
        // Speech from frame 0 → the start pad does not underflow and becomes 0.
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
        // With 2 segments, the later one's start pad is clamped so it does not intrude on the
        // earlier one's end pad.
        let mut c = base_config();
        c.min_silence_ms = 64; // 2 frames
        c.speech_pad_ms = 192; // 3072 samples = 6 frames (on the large side)
                               // 5 speech → 3 silence (finalized) → 5 speech → silence
        let mut probs = vec![0.9f32; 5];
        probs.extend(vec![0.1f32; 3]);
        probs.extend(vec![0.9f32; 5]);
        probs.extend(vec![0.1f32; 5]);
        let segs = run_probs(&c, &probs);
        assert_eq!(segs.len(), 2);
        // seg0's end pad and seg1's start pad do not overlap (seg1.start >= seg0.end).
        assert!(
            segs[1].start_sample >= segs[0].end_sample,
            "seg1.start ({}) must be >= seg0.end ({})",
            segs[1].start_sample,
            segs[0].end_sample
        );
    }

    #[test]
    fn max_speech_forces_split_when_no_silence() {
        // Exceeding max_speech with no silence at all forces a split.
        let mut c = base_config();
        c.max_speech_ms = 192; // 3072 samples = 6 frames
        c.min_silence_ms = 32;
        // 20 frames of continuous speech (no silence). Split every max_speech=6 frames.
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
        // In the gray zone threshold > prob >= neg_threshold, speech continues (no split).
        let mut c = base_config(); // threshold 0.5, neg 0.35
        c.min_silence_ms = 32;
        let mut probs = vec![0.9f32; 5];
        probs.extend(vec![0.4f32; 5]); // gray (0.35 <= 0.4 < 0.5)
        probs.extend(vec![0.9f32; 5]);
        probs.extend(vec![0.1f32; 5]); // real silence
        let segs = run_probs(&c, &probs);
        assert_eq!(segs.len(), 1, "gray zone must not split, got {segs:?}");
        assert_eq!(segs[0].end_sample, 15 * 512);
    }

    // ---- Inference path smoke tests ----

    #[test]
    fn vad_loads_model() {
        let vad = Vad::new(VadConfig::default());
        assert!(vad.is_ok(), "model load failed: {:?}", vad.err());
    }

    #[test]
    fn zeros_produce_low_prob_no_speech() {
        let mut vad = Vad::new(VadConfig::default()).unwrap();
        let zeros = vec![0.0f32; 16000];
        let events = vad.process(&zeros);
        // Silent input produces no SpeechStart.
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
        // All frames are processed even when leftover samples straddle call boundaries.
        let mut vad = Vad::new(VadConfig::default()).unwrap();
        // 300 + 300 + ... straddles 512 boundaries. A total of 512*4 = 2048 samples in small
        // pieces.
        let total = 2048usize;
        let chunk = vec![0.0f32; 300];
        let mut fed = 0usize;
        let mut total_probs = 0usize;
        while fed < total {
            let take = chunk.len().min(total - fed);
            vad.process(&chunk[..take]);
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
        vad.process(&vec![0.0f32; 1000]); // leave leftovers in pending
        vad.reset();
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

    // ---- flush (force-finalizing open speech) ----

    /// With threshold=0 every frame is treated as speech, so no silence ever arrives and
    /// `process` never finalizes a segment. `flush` can force-finalize the open speech; after
    /// that the VAD has been reset (the cumulative position returns to a 0 origin) and the
    /// next input starts from a fresh context.
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
        // Feed 1 second. No silence arrives, so process leaves the segment unfinalized.
        let sig = vec![0.1f32; 16_000];
        let during = vad.process(&sig);
        assert!(
            during.is_empty(),
            "no silence arrives, so process does not finalize a segment: {during:?}"
        );
        // flush finalizes the open speech → a speechStart + speechEnd pair.
        let flushed = vad.flush();
        assert_eq!(
            flushed.len(),
            2,
            "flush finalizes the open speech: {flushed:?}"
        );
        assert!(matches!(flushed[0], VadEvent::SpeechStart { at_sample: 0 }));
        assert!(matches!(flushed[1], VadEvent::SpeechEnd { .. }));

        // After flush it has been reset = a fresh context. With the same input the start
        // position returns to a 0 origin.
        let after = vad.process(&sig);
        assert!(
            after.is_empty(),
            "after reset the new segment is not finalized: {after:?}"
        );
        let flushed2 = vad.flush();
        assert_eq!(flushed2.len(), 2);
        assert_eq!(
            flushed[0], flushed2[0],
            "after reset the start returns to a 0 origin: same input, same start"
        );
    }

    /// flush returns empty when no speech is open (silence only).
    #[test]
    fn flush_on_idle_returns_empty() {
        let mut vad = Vad::new(VadConfig::default()).unwrap();
        let zeros = vec![0.0f32; 16_000];
        vad.process(&zeros);
        let flushed = vad.flush();
        assert!(
            flushed.is_empty(),
            "flush with no speech is empty: {flushed:?}"
        );
    }

    /// Open speech shorter than min_speech is discarded even by flush (follows the
    /// segmenter's filter).
    #[test]
    fn flush_discards_segment_shorter_than_min_speech() {
        let cfg = VadConfig {
            threshold: 0.0,
            neg_threshold: Some(0.0),
            min_speech_ms: 1_000, // discard anything under 1 second.
            min_silence_ms: 0,
            speech_pad_ms: 0,
            max_speech_ms: 0,
            sample_rate: 16_000,
        };
        let mut vad = Vad::new(cfg).unwrap();
        // Feed only 300ms (< min_speech 1000ms) and flush.
        let sig = vec![0.1f32; 16_000 * 300 / 1000];
        vad.process(&sig);
        let flushed = vad.flush();
        assert!(
            flushed.is_empty(),
            "an open segment shorter than min_speech is discarded even by flush: {flushed:?}"
        );
    }

    // ---- process_pcm (self-contained entry point) ----

    /// Deterministic in-band (<8kHz) test signal. A synthetic wave with stacked harmonics
    /// shows whether resampling works. Note that silero does not treat synthetic waves as
    /// speech (the probability is low with the default settings), so events themselves are
    /// mainly produced with low-threshold settings. Here the correctness of resampling is
    /// checked by matching probability sequences.
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

    /// Duplicates mono into interleaved stereo (L=R).
    fn to_stereo(mono: &[f32]) -> Vec<f32> {
        let mut s = Vec::with_capacity(mono.len() * 2);
        for &v in mono {
            s.push(v);
            s.push(v);
        }
        s
    }

    /// Passing 16k/mono to process_pcm behaves exactly like process (event and probability
    /// sequences match bit for bit) = the passthrough path.
    #[test]
    fn process_pcm_passthrough_matches_process() {
        let sig = harmonics(16_000, 16_000);
        let mut vad = Vad::new(VadConfig::default()).unwrap();

        let e_pcm = vad.process_pcm(&sig, 16_000, 1);
        let p_pcm = vad.last_frame_probabilities().to_vec();

        vad.reset();
        let e_proc = vad.process(&sig);
        let p_proc = vad.last_frame_probabilities().to_vec();

        assert_eq!(
            e_pcm, e_proc,
            "passthrough event sequence does not match process"
        );
        assert_eq!(
            p_pcm, p_proc,
            "passthrough probability sequence is not bit-identical to process"
        );
        assert_eq!(p_pcm.len(), 16_000 / 512, "unexpected frame count");
    }

    /// Passing 48k/stereo to process_pcm in pieces yields the same event and probability
    /// sequences as passing it all at once (resampler state continues across calls = no
    /// seams). Splitting at an odd length also straddles frame boundaries.
    #[test]
    fn process_pcm_split_matches_bulk() {
        // silero does not treat the synthetic signal as speech (low probability), so use
        // settings that produce events regardless of the probability values. threshold=0
        // treats every frame as speech, and max_speech forces a split at a fixed length. Event
        // positions are determined by the frame count after resampling, so if split and bulk
        // disagree the event sequences shift = this also detects seams.
        let cfg = VadConfig {
            threshold: 0.0,
            neg_threshold: Some(0.0),
            min_speech_ms: 0,
            min_silence_ms: 0,
            speech_pad_ms: 0,
            max_speech_ms: 200, // forced split every 200ms @16k = 3200 samples.
            sample_rate: 16_000,
        };
        let stereo = to_stereo(&harmonics(48_000, 48_000));

        let mut vad = Vad::new(cfg).unwrap();

        // Feed all at once.
        let bulk_events = vad.process_pcm(&stereo, 48_000, 2);
        let bulk_probs = vad.last_frame_probabilities().to_vec();

        vad.reset();

        // Feed in pieces (777 samples = an odd length that straddles frame boundaries).
        let mut split_events = Vec::new();
        let mut split_probs = Vec::new();
        for chunk in stereo.chunks(777) {
            let evs = vad.process_pcm(chunk, 48_000, 2);
            split_events.extend(evs);
            split_probs.extend_from_slice(vad.last_frame_probabilities());
        }

        assert_eq!(
            bulk_probs, split_probs,
            "split and bulk probability sequences differ (a seam is present)"
        );
        assert_eq!(
            bulk_events, split_events,
            "split and bulk event sequences differ"
        );
        assert!(
            !bulk_events.is_empty(),
            "this configuration should produce speech events (checks the test is effective)"
        );
    }

    /// Feeding the same input to process_pcm after reset yields exactly the same event and
    /// probability sequences (reset also initializes the resampler state).
    #[test]
    fn process_pcm_reset_is_deterministic() {
        let stereo = to_stereo(&harmonics(48_000, 24_000));

        let mut vad = Vad::new(VadConfig::default()).unwrap();
        let e1 = vad.process_pcm(&stereo, 48_000, 2);
        let p1 = vad.last_frame_probabilities().to_vec();

        vad.reset();
        let e2 = vad.process_pcm(&stereo, 48_000, 2);
        let p2 = vad.last_frame_probabilities().to_vec();

        assert_eq!(
            e1, e2,
            "same input after reset does not give the same events"
        );
        assert_eq!(
            p1, p2,
            "same input after reset does not give the same probabilities"
        );
    }

    /// The result of feeding 48k/stereo to process_pcm nearly matches the result of building
    /// the same waveform directly as 16k/mono and feeding it to process. The probabilities are
    /// compared excluding the transients (the first and last few frames).
    #[test]
    fn process_pcm_48k_stereo_matches_direct_16k() {
        let n16 = 16_000usize;
        let ref16 = harmonics(16_000, n16);
        let stereo48 = to_stereo(&harmonics(48_000, n16 * 3));

        let mut ref_vad = Vad::new(VadConfig::default()).unwrap();
        let ref_events = ref_vad.process(&ref16);
        let ref_probs = ref_vad.last_frame_probabilities().to_vec();

        let mut pcm_vad = Vad::new(VadConfig::default()).unwrap();
        let pcm_events = pcm_vad.process_pcm(&stereo48, 48_000, 2);
        let pcm_probs = pcm_vad.last_frame_probabilities().to_vec();

        // The frame counts can differ by up to 1 frame due to resampler delay. Compare
        // probabilities in the middle of the common part.
        let n = ref_probs.len().min(pcm_probs.len());
        assert!(n >= 8, "enough frames are required: {n}");
        for k in 2..(n - 2) {
            let d = (ref_probs[k] - pcm_probs[k]).abs();
            assert!(
                d < 0.05,
                "frame {k}: direct-16k vs converted-path prob gap too large: {d} (ref={} pcm={})",
                ref_probs[k],
                pcm_probs[k]
            );
        }
        // With the default settings the synthetic signal is treated as silence = no speech
        // events on either path (equivalent).
        assert_eq!(
            ref_events, pcm_events,
            "conversion path event sequence does not match direct 16k"
        );
    }
}
