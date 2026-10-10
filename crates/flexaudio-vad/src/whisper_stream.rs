//! Canonical mono16k session over the existing embedded Silero v6 backend.
//! Model SHA-256: 7776b81ad1b0350c15d7f1555943b9232eb53e9ca5d989c6d0cea9ebc8664d87.
//! One owner carries 64-sample context, recurrent state and 512-sample framing.

use crate::infer::SileroEngine;
use crate::whisper_params::{FRAME, MAX_FRAMES};
use crate::whisper_preview::{Preview, PreviewEvents};
use crate::{
    EpochEndReason, FrameProbabilities, InferenceBackend, PreviewCloseReason, VadError,
    WhisperSpeechSegment, WhisperVadError, WhisperVadEvent, WhisperVadEventKind as Kind,
    WhisperVadFailure, WhisperVadOperation as Operation, WhisperVadOptions, WhisperVadParams,
    WhisperVadPostProcessor,
};

struct CheckedSilero(SileroEngine);
impl InferenceBackend for CheckedSilero {
    fn infer(&mut self, frame: &[f32]) -> Result<f32, VadError> {
        self.0.infer_checked_16k_frame(frame)
    }
    fn reset(&mut self) -> Result<(), VadError> {
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

/// Additive whisper-compatible session. Input must be finite mono f32 in [-1,1] at 16 kHz.
///
/// Final segments reproduce pin 85a69493 on identical probabilities. Model probabilities
/// can differ from upstream v5. Finalization can wait indefinitely for a surviving neighbor.
/// All input is borrowed only for the duration of a call. One instance has one mutable owner.
/// Reset abandons pending finals; finish infers one zero-padded partial frame at true EOF.
pub struct WhisperVad {
    engine: Box<dyn InferenceBackend>,
    postprocessor: WhisperVadPostProcessor,
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
}

impl WhisperVad {
    /// Validate immutable parameters before loading the embedded model off the audio thread.
    pub fn new(
        params: WhisperVadParams,
        options: WhisperVadOptions,
    ) -> Result<Self, WhisperVadError> {
        params.validate()?;
        let engine = SileroEngine::load().map_err(|_| WhisperVadError::ModelLoad)?;
        Self::with_backend(params, options, Box::new(CheckedSilero(engine)))
    }

    pub(crate) fn with_backend(
        params: WhisperVadParams,
        options: WhisperVadOptions,
        engine: Box<dyn InferenceBackend>,
    ) -> Result<Self, WhisperVadError> {
        let derived = params.derive()?;
        Ok(Self {
            engine,
            postprocessor: WhisperVadPostProcessor::new(params)?,
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
        })
    }

    /// Carry arbitrary chunk remainders without padding. Stage events until the entire call succeeds.
    /// Validation failures preserve state and carry no terminal events.
    pub fn process(&mut self, mono_16k: &[f32]) -> Result<Vec<WhisperVadEvent>, WhisperVadFailure> {
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
                return Err(WhisperVadError::InvalidPcm {
                    sample: self.actual_samples + u64::try_from(i).expect("checked input length"),
                }
                .into());
            }
        }
        let complete = total / FRAME - self.frames;
        let mut batch = self.reserve_batch(complete)?;
        let mut terminal = self.reserve_batch(0)?;
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
                Err(error) => return Err(self.fail(error, &mut terminal)),
            };
            if let Some(preview) = &mut preview {
                let mut hints = PreviewEvents::default();
                preview.feed(prob, frame_index * 32, (frame_index + 1) * 32, &mut hints);
                Self::stage(&mut batch, self.epoch, self.seq, hints);
            }
            let segments = match processor.process(&[prob]) {
                Ok(segments) => segments,
                Err(error) => return Err(self.fail(error, &mut terminal)),
            };
            Self::stage(
                &mut batch,
                self.epoch,
                self.seq,
                segments.into_iter().map(Kind::Segment),
            );
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
    pub fn finish(&mut self) -> Result<Vec<WhisperVadEvent>, WhisperVadFailure> {
        if self.state == Lifecycle::Finished {
            self.last_probs.clear();
            self.first_frame = self.frames;
            return Ok(Vec::new());
        }
        self.require_running()?;
        let mut batch = self.reserve_batch(1)?;
        let mut terminal = self.reserve_batch(0)?;
        let mut processor = self.postprocessor.clone();
        let mut preview = self.preview.clone();
        let mut probs = Vec::with_capacity(1);
        let mut tail_segments = Vec::new();
        if self.pending_len != 0 {
            let mut frame = self.pending;
            frame[self.pending_len..].fill(0.0);
            let p = match self.infer(&frame) {
                Ok(p) => p,
                Err(error) => return Err(self.fail(error, &mut terminal)),
            };
            if let Some(preview) = &mut preview {
                let mut hints = PreviewEvents::default();
                preview.feed(p, self.frames * 32, self.actual_samples / 16, &mut hints);
                Self::stage(&mut batch, self.epoch, self.seq, hints);
            }
            tail_segments = match processor.process(&[p]) {
                Ok(segments) => segments,
                Err(error) => return Err(self.fail(error, &mut terminal)),
            };
            probs.push(p);
        }
        if let Some(preview) = &mut preview {
            let mut hints = PreviewEvents::default();
            preview.close(PreviewCloseReason::Finish, &mut hints);
            Self::stage(&mut batch, self.epoch, self.seq, hints);
        }
        let remaining = match processor.finish() {
            Ok(segments) => segments,
            Err(error) => return Err(self.fail(error, &mut terminal)),
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
    pub fn reset(&mut self) -> Result<Vec<WhisperVadEvent>, WhisperVadFailure> {
        let next_epoch = self
            .epoch
            .checked_add(1)
            .ok_or_else(|| overflow(Operation::Epoch))?;
        let mut batch = self.reserve_batch(0)?;
        if self.engine.reset().is_err() {
            return Err(self.fail(WhisperVadError::Inference, &mut batch));
        }
        if self.state == Lifecycle::Running {
            self.close_epoch(PreviewCloseReason::Reset, EpochEndReason::Reset, &mut batch);
        }
        self.postprocessor.reset();
        if let Some(preview) = &self.preview {
            // Preserve the immutable preview configuration, discard its timeline.
            let mut fresh = preview.clone();
            fresh.reset();
            self.preview = Some(fresh);
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
        Ok(batch)
    }

    /// Latest successful call only, with its absolute first frame index in this epoch.
    pub fn last_frame_probabilities(&self) -> FrameProbabilities<'_> {
        FrameProbabilities {
            first_frame_index: self.first_frame,
            values: &self.last_probs,
        }
    }

    /// Close only published hints after an attached converter fails, without successful EOF.
    pub(crate) fn abort(&mut self) -> Result<WhisperVadFailure, WhisperVadError> {
        let mut terminal = self.reserve_batch(0)?;
        Ok(self.fail(WhisperVadError::Inference, &mut terminal))
    }

    pub(crate) fn epoch(&self) -> u32 {
        self.epoch
    }

    fn infer(&mut self, frame: &[f32]) -> Result<f32, WhisperVadError> {
        let p = self
            .engine
            .infer(frame)
            .map_err(|_| WhisperVadError::Inference)?;
        if !p.is_finite() || !(0.0..=1.0).contains(&p) {
            return Err(WhisperVadError::Inference);
        }
        Ok(p)
    }

    fn require_running(&self) -> Result<(), WhisperVadError> {
        match self.state {
            Lifecycle::Running => Ok(()),
            Lifecycle::Finished => Err(WhisperVadError::SessionFinished),
            Lifecycle::Failed => Err(WhisperVadError::FailedSession),
        }
    }

    fn reserve_batch(&self, frames: u64) -> Result<Vec<WhisperVadEvent>, WhisperVadError> {
        // Three events per frame and eight closing/final events safely bound this algorithm.
        let budget = frames
            .checked_mul(3)
            .and_then(|v| v.checked_add(8))
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
        batch: &mut Vec<WhisperVadEvent>,
        epoch: u32,
        first_seq: u64,
        kinds: impl IntoIterator<Item = Kind>,
    ) {
        for kind in kinds {
            batch.push(WhisperVadEvent {
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
        batch: &mut Vec<WhisperVadEvent>,
    ) {
        if let Some(preview) = &mut self.preview {
            let mut kinds = PreviewEvents::default();
            preview.close(close, &mut kinds);
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

    fn fail(
        &mut self,
        error: WhisperVadError,
        terminal: &mut Vec<WhisperVadEvent>,
    ) -> WhisperVadFailure {
        if self.state == Lifecycle::Running {
            self.close_epoch(PreviewCloseReason::Error, EpochEndReason::Error, terminal);
        }
        self.state = Lifecycle::Failed;
        self.pending_len = 0;
        self.postprocessor.reset();
        self.last_probs.clear();
        self.first_frame = self.frames;
        WhisperVadFailure {
            error,
            terminal_events: std::mem::take(terminal),
        }
    }
}

fn overflow(operation: Operation) -> WhisperVadError {
    WhisperVadError::Overflow { operation }
}

/// Whole-buffer convenience through the sole session feed/finish path, with preview disabled.
pub fn whisper_speech_segments(
    mono_16k: &[f32],
    params: &WhisperVadParams,
) -> Result<Vec<WhisperSpeechSegment>, WhisperVadError> {
    let mut vad = WhisperVad::new(params.clone(), WhisperVadOptions::default())?;
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
#[path = "whisper_stream_tests.rs"]
mod tests;
