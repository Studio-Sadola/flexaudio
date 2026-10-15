//! Canonical mono16k session over the existing embedded Silero v6 backend.
//! Model SHA-256: 7776b81ad1b0350c15d7f1555943b9232eb53e9ca5d989c6d0cea9ebc8664d87.
//! One owner carries 64-sample context, recurrent state and 512-sample framing.

use crate::infer::SileroEngine;
use crate::input_converter::InputConverter;
use crate::params::{FRAME, MAX_FRAMES};
use crate::preview::{Preview, MAX_PREVIEW_EVENTS};
use crate::whisper_cpp_segments::{MAX_FINISH_SEGMENTS, MAX_SEGMENTS_PER_FRAME};
use crate::{
    EpochEndReason, FrameProbabilities, InferenceBackend, InferenceError, PreviewCloseReason,
    SpeechSegment, VadError, VadEvent, VadEventKind as Kind, VadFailure, VadOperation as Operation,
    VadOptions, VadPostProcessor, VadRuntimeOptions,
};
use crate::{PcmFormat, VadParameter, VadParameterReason};

struct CheckedSilero(SileroEngine);
impl InferenceBackend for CheckedSilero {
    fn infer(&mut self, frame: &[f32]) -> Result<f32, InferenceError> {
        self.0.infer_checked_16k_frame(frame)
    }
    fn reset(&mut self) -> Result<(), InferenceError> {
        self.0.reset();
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lifecycle {
    Running,
    Finished,
    Failed,
}

/// Streaming VAD with finite normalized f32 PCM input.
/// [`Self::process`] accepts 16 kHz mono; [`Self::process_pcm`] converts other formats.
///
/// Final segments reproduce pin 85a69493 on identical probabilities. Model probabilities
/// can differ from upstream v5. Equivalence excludes the pin's int32 overflow/UB
/// near INT_MAX logical samples (about 37 hours); sample arithmetic uses u64 and
/// padded EOF ends are clamped. Finalization can wait indefinitely for a surviving neighbor.
/// All input is borrowed only for the duration of a call. One instance has one mutable owner.
/// Reset abandons pending finals; finish infers one zero-padded partial frame at true EOF.
pub struct Vad {
    engine: Box<dyn InferenceBackend>,
    postprocessor: VadPostProcessor,
    preview: Option<Preview>,
    state: Lifecycle,
    epoch: u32,
    seq: u64,
    actual_samples: u64,
    frames: u64,
    pending: [f32; 512],
    pending_len: usize,
    last_probs: Vec<f32>,
    first_frame: u64,
    input_format: Option<PcmFormat>,
    input_scalars: u64,
    converter: Option<InputConverter>,
}

impl Vad {
    /// Validate immutable parameters before loading the embedded model off the audio thread.
    pub fn new(params: VadOptions, options: VadRuntimeOptions) -> Result<Self, VadError> {
        params.validate()?;
        let engine =
            SileroEngine::load().map_err(|source| VadError::ModelLoad.with_cause(source))?;
        Self::with_backend(params, options, Box::new(CheckedSilero(engine)))
    }

    pub(crate) fn with_backend(
        params: VadOptions,
        options: VadRuntimeOptions,
        engine: Box<dyn InferenceBackend>,
    ) -> Result<Self, VadError> {
        let derived = params.derive()?;
        Ok(Self {
            engine,
            postprocessor: VadPostProcessor::new(params)?,
            preview: options.provisional.then(|| Preview::new(derived)),
            state: Lifecycle::Running,
            epoch: 0,
            seq: 0,
            actual_samples: 0,
            frames: 0,
            pending: [0.0; 512],
            pending_len: 0,
            last_probs: Vec::new(),
            first_frame: 0,
            input_format: None,
            input_scalars: 0,
            converter: None,
        })
    }

    /// Feed 16 kHz mono PCM. Chunk remainders carry across calls without padding.
    /// Event times are milliseconds on the source timeline, starting at zero.
    pub fn process(&mut self, mono_16k: &[f32]) -> Result<Vec<VadEvent>, VadFailure> {
        self.process_pcm(mono_16k, 16_000, 1)
    }

    /// Feed interleaved PCM at 8,000..=192,000 Hz with any positive channel count.
    /// Channels are averaged and resampled using the shared capture converter.
    /// Finite resampler overshoot is clipped to [-1,1] before inference;
    /// nonfinite converter output fails the session with its typed cause.
    /// Converter group delay is discarded, so event times remain milliseconds
    /// on the input timeline. Source format is fixed until a successful reset.
    /// Partial interleaved frames carry across calls and must be complete at EOF.
    /// Validation rejects the complete feed before changing session state.
    pub fn process_pcm(
        &mut self,
        samples: &[f32],
        input_sample_rate: u32,
        input_channels: u16,
    ) -> Result<Vec<VadEvent>, VadFailure> {
        self.require_running()?;
        let invalid = |field| VadError::InvalidParameter {
            field,
            reason: VadParameterReason::OutOfRange,
        };
        if !(8_000..=192_000).contains(&input_sample_rate) {
            return Err(invalid(VadParameter::InputSampleRate).into());
        }
        if input_channels == 0 {
            return Err(invalid(VadParameter::InputChannels).into());
        }
        let format = PcmFormat {
            sample_rate: input_sample_rate,
            channels: input_channels,
        };
        if self.input_format.is_some_and(|current| current != format) {
            return Err(invalid(VadParameter::InputFormat).into());
        }
        let total = self
            .input_scalars
            .checked_add(
                u64::try_from(samples.len()).map_err(|_| overflow(Operation::InputSamples))?,
            )
            .ok_or_else(|| overflow(Operation::InputSamples))?;
        let frames = total.div_ceil(u64::from(input_channels));
        let normalized = (u128::from(frames) * 16_000).div_ceil(u128::from(input_sample_rate));
        if normalized.div_ceil(u128::from(FRAME)) > u128::from(MAX_FRAMES) {
            return Err(overflow(Operation::LogicalSamples).into());
        }
        for (offset, sample) in samples.iter().enumerate() {
            if !sample.is_finite() || !(-1.0..=1.0).contains(sample) {
                return Err(VadError::InvalidPcm {
                    sample: self.input_scalars
                        + u64::try_from(offset).expect("checked input length"),
                }
                .into());
            }
        }
        if self.input_format.is_none() && (input_sample_rate != 16_000 || input_channels != 1) {
            self.converter = Some(
                InputConverter::new(format)
                    .map_err(|source| VadError::Inference.with_cause(source))?,
            );
        }
        self.input_format = Some(format);
        self.input_scalars = total;
        let converted = match &mut self.converter {
            Some(converter) => match converter.push(samples) {
                Ok(converted) => converted,
                Err(source) => {
                    return Err(self.fail(VadError::Inference.with_cause(source), &mut Vec::new()));
                }
            },
            None => return self.process_mono(samples),
        };
        self.process_converted(&converted)
    }

    fn process_converted(&mut self, converted: &[f32]) -> Result<Vec<VadEvent>, VadFailure> {
        // Conversion has consumed source input. Internal rejection cannot leave
        // a reusable session whose sample clock silently omits that input.
        match self.process_mono(converted) {
            Err(failure) if self.state == Lifecycle::Running => {
                Err(self.fail(failure.error, &mut Vec::new()))
            }
            result => result,
        }
    }

    fn process_mono(&mut self, mono_16k: &[f32]) -> Result<Vec<VadEvent>, VadFailure> {
        self.require_running()?;
        let count = u64::try_from(mono_16k.len()).map_err(|_| overflow(Operation::InputSamples))?;
        let total = self
            .actual_samples
            .checked_add(count)
            .ok_or_else(|| overflow(Operation::InputSamples))?;
        let logical_frames = total
            .checked_add(FRAME - 1)
            .ok_or_else(|| overflow(Operation::LogicalSamples))?
            / FRAME;
        if logical_frames > MAX_FRAMES {
            return Err(overflow(Operation::LogicalSamples).into());
        }
        for (i, &sample) in mono_16k.iter().enumerate() {
            if !sample.is_finite() || !(-1.0..=1.0).contains(&sample) {
                return Err(VadError::InvalidPcm {
                    sample: self.actual_samples + u64::try_from(i).expect("checked input length"),
                }
                .into());
            }
        }
        let complete = total / FRAME - self.frames;
        let mut batch = self.reserve_batch(complete)?;
        let mut probs = Vec::new();
        probs
            .try_reserve(usize::try_from(complete).map_err(|_| overflow(Operation::Capacity))?)
            .map_err(|_| overflow(Operation::Capacity))?;
        let mut processor = self.postprocessor.clone();
        let mut preview = self.preview.clone();
        let mut pending = self.pending;
        let mut pending_len = self.pending_len;
        let mut frame_index = self.frames;
        let mut rest = mono_16k;
        while !rest.is_empty() {
            let take = (512 - pending_len).min(rest.len());
            pending[pending_len..pending_len + take].copy_from_slice(&rest[..take]);
            pending_len += take;
            rest = &rest[take..];
            if pending_len != 512 {
                continue;
            }
            let prob = match self.infer(&pending) {
                Ok(prob) => prob,
                Err(error) => return Err(self.fail(error, &mut batch)),
            };
            if let Some(preview) = &mut preview {
                let hints = preview.feed(prob, frame_index * 32, (frame_index + 1) * 32);
                Self::stage(&mut batch, self.epoch, self.seq, hints);
            }
            processor.feed_frame(prob, &mut |segment| {
                Self::stage(&mut batch, self.epoch, self.seq, [Kind::Segment(segment)]);
            });
            probs.push(prob);
            pending_len = 0;
            frame_index += 1;
        }
        self.first_frame = self.frames;
        self.frames = frame_index;
        self.actual_samples = total;
        self.pending = pending;
        self.pending_len = pending_len;
        self.postprocessor = processor;
        self.preview = preview;
        self.last_probs = probs;
        self.seq += u64::try_from(batch.len()).expect("reserved event count");
        Ok(batch)
    }

    /// Infer exactly one partial tail, close preview at physical floor(N/16) ms,
    /// then deliver all remaining canonical segments and epochEnd last.
    pub fn finish(&mut self) -> Result<Vec<VadEvent>, VadFailure> {
        if self.state == Lifecycle::Finished {
            return self.finish_mono();
        }
        self.require_running()?;
        if self
            .converter
            .as_ref()
            .is_some_and(InputConverter::has_partial_frame)
        {
            return Err(VadError::InvalidParameter {
                field: VadParameter::InputFormat,
                reason: VadParameterReason::OutOfRange,
            }
            .into());
        }
        let converted = match &mut self.converter {
            Some(converter) if self.state == Lifecycle::Running => match converter.drain() {
                Ok(converted) => converted,
                Err(source) => {
                    return Err(self.fail(VadError::Inference.with_cause(source), &mut Vec::new()))
                }
            },
            _ => Vec::new(),
        };
        if converted.is_empty() {
            return self.finish_mono();
        }
        let mut events = self.process_converted(&converted)?;
        let first_frame = self.first_frame;
        let mut probabilities = std::mem::take(&mut self.last_probs);
        match self.finish_mono() {
            Ok(tail) => events.extend(tail),
            Err(mut failure) => {
                events.extend(failure.terminal_events);
                failure.terminal_events = events;
                return Err(failure);
            }
        }
        probabilities.extend_from_slice(&self.last_probs);
        self.last_probs = probabilities;
        self.first_frame = first_frame;
        Ok(events)
    }

    fn finish_mono(&mut self) -> Result<Vec<VadEvent>, VadFailure> {
        if self.state == Lifecycle::Finished {
            self.last_probs.clear();
            self.first_frame = self.frames;
            return Ok(Vec::new());
        }
        self.require_running()?;
        let mut batch = self.reserve_batch(1)?;
        let mut processor = self.postprocessor.clone();
        let mut preview = self.preview.clone();
        let mut probs = Vec::with_capacity(1);
        let mut tail_segments = Vec::new();
        tail_segments
            .try_reserve(usize::try_from(MAX_SEGMENTS_PER_FRAME).expect("constant bound"))
            .map_err(|_| overflow(Operation::Capacity))?;
        if self.pending_len != 0 {
            let mut frame = self.pending;
            frame[self.pending_len..].fill(0.0);
            let p = match self.infer(&frame) {
                Ok(p) => p,
                Err(error) => return Err(self.fail(error, &mut batch)),
            };
            if let Some(preview) = &mut preview {
                let hints = preview.feed(p, self.frames * 32, self.actual_samples / 16);
                Self::stage(&mut batch, self.epoch, self.seq, hints);
            }
            processor.feed_frame(p, &mut |segment| tail_segments.push(segment));
            probs.push(p);
        }
        if let Some(preview) = &mut preview {
            let hints = preview.close(PreviewCloseReason::Finish);
            Self::stage(&mut batch, self.epoch, self.seq, hints);
        }
        let remaining = match processor.finish() {
            Ok(segments) => segments,
            Err(error) => return Err(self.fail(error, &mut batch)),
        };
        Self::stage(
            &mut batch,
            self.epoch,
            self.seq,
            tail_segments
                .into_iter()
                .chain(remaining)
                .map(Kind::Segment),
        );
        Self::stage(
            &mut batch,
            self.epoch,
            self.seq,
            [Kind::EpochEnd {
                reason: EpochEndReason::Finish,
            }],
        );
        self.first_frame = self.frames;
        self.frames += u64::try_from(probs.len()).expect("one tail frame");
        self.last_probs = probs;
        self.postprocessor = processor;
        self.preview = preview;
        self.pending_len = 0;
        self.seq += u64::try_from(batch.len()).expect("reserved events");
        self.state = Lifecycle::Finished;
        Ok(batch)
    }

    /// Close published hints and abandon pending canonical groups, then reset model state.
    /// A failed reset closes with error reasons and does not start a new epoch.
    pub fn reset(&mut self) -> Result<Vec<VadEvent>, VadFailure> {
        let next_epoch = self
            .epoch
            .checked_add(1)
            .ok_or_else(|| overflow(Operation::Epoch))?;
        let mut batch = self.reserve_batch(0)?;
        if let Err(source) = self.engine.reset() {
            return Err(self.fail(VadError::Inference.with_cause(source), &mut batch));
        }
        if self.state == Lifecycle::Running {
            self.close_epoch(PreviewCloseReason::Reset, EpochEndReason::Reset, &mut batch);
        }
        self.postprocessor.reset();
        if let Some(preview) = &mut self.preview {
            preview.reset();
        }
        self.state = Lifecycle::Running;
        self.epoch = next_epoch;
        self.seq = 0;
        self.actual_samples = 0;
        self.frames = 0;
        self.pending_len = 0;
        self.pending.fill(0.0);
        self.last_probs.clear();
        self.first_frame = 0;
        self.input_format = None;
        self.input_scalars = 0;
        self.converter = None;
        Ok(batch)
    }

    /// Latest successful call only, with its absolute first frame index in this epoch.
    pub fn last_frame_probabilities(&self) -> FrameProbabilities<'_> {
        FrameProbabilities {
            first_frame_index: self.first_frame,
            values: &self.last_probs,
        }
    }

    /// Close only published hints after attached conversion or capture transport fails,
    /// without successful EOF.
    pub(crate) fn abort(&mut self) -> Result<VadFailure, VadError> {
        let mut terminal = self.reserve_batch(0)?;
        Ok(self.fail(VadError::Inference, &mut terminal))
    }

    pub(crate) fn epoch(&self) -> u32 {
        self.epoch
    }

    fn infer(&mut self, frame: &[f32]) -> Result<f32, VadError> {
        let p = self
            .engine
            .infer(frame)
            .map_err(|source| VadError::Inference.with_cause(source))?;
        if !p.is_finite() || !(0.0..=1.0).contains(&p) {
            return Err(VadError::Inference);
        }
        Ok(p)
    }

    fn require_running(&self) -> Result<(), VadError> {
        match self.state {
            Lifecycle::Running => Ok(()),
            Lifecycle::Finished => Err(VadError::SessionFinished),
            Lifecycle::Failed => Err(VadError::FailedSession),
        }
    }

    fn reserve_batch(&self, frames: u64) -> Result<Vec<VadEvent>, VadError> {
        // A frame emits at most five canonical segments (see feed_frame's
        // raw/seal proof) plus three preview events. EOF adds at most four
        // canonical segments, three preview closure events and one epochEnd.
        // This also reserves fatal closure in the same batch; failed staged
        // events are discarded before closing the published preview state.
        let closing =
            u64::try_from(MAX_FINISH_SEGMENTS).expect("constant bound") + MAX_PREVIEW_EVENTS + 1;
        let budget = frames
            .checked_mul(MAX_SEGMENTS_PER_FRAME + MAX_PREVIEW_EVENTS)
            .and_then(|v| v.checked_add(closing))
            .ok_or_else(|| overflow(Operation::EventSequence))?;
        self.seq
            .checked_add(budget)
            .ok_or_else(|| overflow(Operation::EventSequence))?;
        let mut batch = Vec::new();
        batch
            .try_reserve(usize::try_from(budget).map_err(|_| overflow(Operation::Capacity))?)
            .map_err(|_| overflow(Operation::Capacity))?;
        Ok(batch)
    }

    fn stage(
        batch: &mut Vec<VadEvent>,
        epoch: u32,
        first_seq: u64,
        kinds: impl IntoIterator<Item = Kind>,
    ) {
        for kind in kinds {
            batch.push(VadEvent {
                epoch,
                seq: first_seq + u64::try_from(batch.len()).expect("reserved count"),
                kind,
            });
        }
    }

    fn close_epoch(
        &mut self,
        close: PreviewCloseReason,
        end: EpochEndReason,
        batch: &mut Vec<VadEvent>,
    ) {
        if let Some(preview) = &mut self.preview {
            let kinds = preview.close(close);
            Self::stage(batch, self.epoch, self.seq, kinds);
        }
        Self::stage(
            batch,
            self.epoch,
            self.seq,
            [Kind::EpochEnd { reason: end }],
        );
        self.seq += u64::try_from(batch.len()).expect("reserved count");
    }

    fn fail(&mut self, error: VadError, terminal: &mut Vec<VadEvent>) -> VadFailure {
        // Reuse the preflighted batch, discarding unpublished staged events.
        terminal.clear();
        if self.state == Lifecycle::Running {
            self.close_epoch(PreviewCloseReason::Error, EpochEndReason::Error, terminal);
        }
        self.state = Lifecycle::Failed;
        self.pending_len = 0;
        self.postprocessor.reset();
        self.last_probs.clear();
        self.first_frame = self.frames;
        VadFailure {
            error,
            terminal_events: std::mem::take(terminal),
        }
    }
}

fn overflow(operation: Operation) -> VadError {
    VadError::Overflow { operation }
}

/// Whole-buffer convenience through the sole session feed/finish path, with preview disabled.
pub fn speech_segments(
    mono_16k: &[f32],
    params: &VadOptions,
) -> Result<Vec<SpeechSegment>, VadError> {
    let mut vad = Vad::new(params.clone(), VadRuntimeOptions::default())?;
    let mut events = vad.process(mono_16k).map_err(|failure| failure.error)?;
    events.extend(vad.finish().map_err(|failure| failure.error)?);
    Ok(events
        .into_iter()
        .filter_map(|event| match event.kind {
            Kind::Segment(segment) => Some(segment),
            _ => None,
        })
        .collect())
}

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;
