//! Normalizes and re-converts arbitrary device frames (any SR / any ch / interleaved f32) in
//! two stages.
//!
//! ```text
//! Input (any SR/ch)
//!   │  Stage 1 (internal normalization, invariant)
//!   │   - channel mix (→stereo)
//!   │   - SR conversion (rubato, →48000)
//!   ▼
//! Internal canonical form: f32 / 48000 Hz / stereo / 20ms = 960 frame
//!   │  Stage 2 (exit, new)
//!   │   - channel conversion (stereo→mono average / mono→stereo duplicate / as-is)
//!   │   - SR conversion (rubato, 48000→output.sample_rate; passthrough if equal)
//!   ▼
//! Output: f32 / output.sample_rate / output.channels / fixed 20ms in time
//!        (48k=960 / 16k=320 / 8k=160 frame)
//! ```
//!
//! With the default output `{48000, 2}`, stage 2 is an entire passthrough (the internal
//! canonical form comes out as-is). Stage 1's SR conversion is a passthrough when
//! `in_sample_rate == 48000`, and stage 2's SR conversion when `output.sample_rate == 48000`.
//!
//! Both rubato resamplers use `FixedAsync::Input` (fixed input chunks); the variable-length
//! output they produce is gathered into an internal accumulator and sliced at 20ms-equivalent
//! boundaries. Remainders are carried over to the next round by the resampler internals and the
//! accumulator.
//!
//! PTS is assigned by tracking the device_pts corresponding to each output chunk's first sample
//! via the ratio of input→output sample offsets. seq is assigned by the stream layer.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Async, FixedAsync, Indexing, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};

use crate::types::{Error, OutputFormat, Result, CHANNELS, SAMPLE_RATE};

/// Frames in one chunk of the internal canonical form (20ms @ 48kHz). Stage 1's slicing
/// boundary.
pub const CHUNK_FRAMES: usize = 960;

/// Channel count of the internal canonical form (stereo).
const INNER_CH: usize = CHANNELS as usize; // 2

/// A processor applied to the internal normalized form (48kHz / stereo /
/// interleaved f32) *before* it is split into the output taps.
///
/// Kept behind a trait so `flexaudio-core` stays independent of any concrete
/// DSP implementation (e.g. noise suppression). The facade injects an
/// implementation; the core only sees this contract. Because the same
/// processed samples feed every output tap, a single processor affects both the
/// primary and secondary outputs.
///
/// [`process`](Self::process) runs on each batch of normalized samples in
/// place. [`flush`](Self::flush) is called once at stop to recover any tail the
/// processor is holding back (e.g. a fixed delay line); it returns the trailing
/// 48kHz / stereo interleaved samples (empty if none).
pub trait InnerProcessor: Send {
    /// Process one batch of 48kHz / stereo interleaved samples in place. The
    /// slice length is always a multiple of two (stereo).
    fn process(&mut self, samples: &mut [f32]);
    /// Flush and return any buffered tail (48kHz / stereo interleaved). Called
    /// once when the stream stops. Returns an empty vector when nothing is held.
    fn flush(&mut self) -> Vec<f32>;
}

/// Stateful two-stage converter that normalizes input device frames to the internal canonical
/// form (48k/stereo/960frame) and further re-converts them into one or more output taps
/// (primary + optionally secondary).
///
/// Stage 1 (internal normalization) runs only once, and the internal canonical form it produces
/// is supplied to each of the primary and secondary stage 2s. An injected [`InnerProcessor`] is
/// applied to the internal canonical form exactly once before branching into stage 2 (both taps
/// receive the same processed audio).
///
/// `push` accumulates interleaved samples, and `pop_chunk` (primary) / `pop_secondary`
/// (secondary) take out finished output chunks one at a time.
pub struct Normalizer {
    in_sample_rate: u32,
    in_channels: usize,

    // --- Stage 1 (internal normalization: → 48k/stereo, shared by all output taps) ---
    /// Passthrough (no resampler) for 48000 input.
    stage1_resampler: Option<ResamplerState>,
    /// Temporary buffer holding the internal canonical form (48k/stereo interleaved) produced
    /// by stage 1 in this push. It is processed ([`InnerProcessor`]) and then distributed to
    /// each output tap. Its capacity is reused.
    inner_scratch: Vec<f32>,
    /// Cumulative internal 48k frames produced by stage 1 so far (for PTS anchor calculation).
    total_inner_frames: u64,

    /// Optional processor (e.g. noise suppression) applied once to the internal canonical form
    /// before branching into stage 2.
    inner_processor: Option<Box<dyn InnerProcessor>>,

    // --- Output taps (each owns its own stage 2, output buffer, and PTS state) ---
    /// Primary output tap ([`OutputFormat`] is `output`).
    primary: OutputTap,
    /// Secondary output tap (only when configured). Has its own stage 2 and PTS state,
    /// independent of the primary.
    secondary: Option<OutputTap>,
}

/// One output tap. It receives the shared internal canonical form (48k/stereo), re-converts it
/// to its own [`OutputFormat`] via channel conversion + SR conversion, and slices fixed 20ms
/// chunks. Each tap has its own PTS anchor and output buffer (which is why the primary and
/// secondary PTS differ by tens of ms).
struct OutputTap {
    output: OutputFormat,
    /// Exit stage. `None` (full passthrough) when `output == {48000, 2}` (identical to the
    /// internal canonical form).
    stage2: Option<OutputStage>,
    /// Output awaiting completion (interleaved with output.channels). `pop` slices from here.
    out_buf: Vec<f32>,
    /// Frames in one output chunk (`output.chunk_frames()`).
    out_chunk_frames: usize,
    /// Output channel count.
    out_channels: usize,
    /// Output frame index corresponding to the start of `out_buf` (the oldest sample not yet
    /// popped).
    out_frame_origin: u64,
    /// PTS anchor: binds a device_pts (ns) to a given output frame index.
    pts_anchor: Option<PtsAnchor>,
}

#[derive(Clone, Copy)]
struct PtsAnchor {
    /// Output frame index (in the output rate).
    out_frame: u64,
    /// device_pts (ns) corresponding to that output frame.
    pts_ns: i64,
}

/// SR converter wrapping one stage of rubato `Async` (`FixedAsync::Input`).
///
/// It calls `process` for each fixed input chunk of `chunk_in_frames` and appends the
/// variable-length output to `out_buf` (the caller's accumulator). `channels` differs per stage
/// (always stereo=2 for stage 1, the output channel count for stage 2).
struct ResamplerState {
    inner: Async<f32>,
    channels: usize,
    /// Input frames rubato requires per call (fixed by `FixedAsync::Input`).
    chunk_in_frames: usize,
    /// Maximum output frames a single `process` can produce.
    max_out_frames: usize,
    /// Unprocessed input (interleaved, `channels` ch).
    in_accum: Vec<f32>,
    /// Output scratch for rubato (reused to avoid allocation).
    out_scratch: Vec<f32>,
}

/// Stage 2 (exit). Receives 960frame chunks of the internal canonical form 48k/stereo, and
/// produces interleaved output in the output format via channel conversion → SR conversion.
struct OutputStage {
    out_channels: usize,
    /// Resampler for 48000 → output.sample_rate. `None` (SR passthrough) when
    /// `output.sample_rate == 48000`. Applied to the samples after channel conversion.
    resampler: Option<ResamplerState>,
    /// Scratch after channel conversion and before SR conversion (48k / out_channels
    /// interleaved).
    ch_scratch: Vec<f32>,
}

impl Normalizer {
    /// Creates a normalizer given the input SR / input channel count / output format.
    ///
    /// Stage 1 normalizes the input to 48k/stereo (SR passthrough when `in_sample_rate ==
    /// 48000`; for `in_channels` 1 it duplicates mono→stereo, 2 is kept as-is, and 3 or more
    /// takes the front 2ch). Stage 2 re-converts the internal canonical form to `output`
    /// (passthrough when `output == {48000, 2}`).
    ///
    /// `output` is expected to have been checked with [`OutputFormat::validate`] by the caller
    /// (it is not clamped into the valid range here).
    ///
    /// Building a rubato resampler can fail, e.g. for extreme rate ratios. A panic would silently
    /// stop the non-RT ingest thread, so on failure this returns [`Error::Backend`] and lets the
    /// caller propagate it.
    pub fn new(in_sample_rate: u32, in_channels: u16, output: OutputFormat) -> Result<Self> {
        let in_channels = in_channels.max(1) as usize;

        // Stage 1 resampler (→48000). Runs only once for all output taps.
        let stage1_resampler = if in_sample_rate == SAMPLE_RATE {
            None
        } else {
            Some(ResamplerState::new(in_sample_rate, SAMPLE_RATE, INNER_CH)?)
        };

        Ok(Self {
            in_sample_rate,
            in_channels,
            stage1_resampler,
            inner_scratch: Vec::with_capacity(CHUNK_FRAMES * INNER_CH * 4),
            total_inner_frames: 0,
            inner_processor: None,
            primary: OutputTap::new(output)?,
            secondary: None,
        })
    }

    /// Adds a secondary output tap (expected to have been checked with
    /// [`OutputFormat::validate`]).
    ///
    /// The internal canonical form (48k/stereo) is produced only once and supplied to both the
    /// primary and secondary stage 2s. The secondary tap has its own stage 2 and PTS state and
    /// produces fixed 20ms chunks separately from the primary. Returns [`Error::Backend`] if
    /// building rubato fails.
    pub fn with_secondary(mut self, secondary: OutputFormat) -> Result<Self> {
        self.secondary = Some(OutputTap::new(secondary)?);
        Ok(self)
    }

    /// Injects a processor applied once to the internal canonical form (48k/stereo) before
    /// branching into stage 2.
    pub fn with_inner_processor(mut self, processor: Box<dyn InnerProcessor>) -> Self {
        self.inner_processor = Some(processor);
        self
    }

    /// Input sample rate (Hz).
    pub fn in_sample_rate(&self) -> u32 {
        self.in_sample_rate
    }

    /// Primary output format.
    pub fn output(&self) -> OutputFormat {
        self.primary.output
    }

    /// Secondary output format (only when a secondary tap is configured).
    pub fn secondary_output(&self) -> Option<OutputFormat> {
        self.secondary.as_ref().map(|t| t.output)
    }

    /// Whether the secondary tap is enabled.
    pub fn has_secondary(&self) -> bool {
        self.secondary.is_some()
    }

    /// Whether stage 1's SR conversion is a passthrough (in == 48000).
    pub fn is_passthrough(&self) -> bool {
        self.stage1_resampler.is_none()
    }

    /// Whether the primary output's stage 2 is a full passthrough (output == {48000, 2}).
    pub fn is_output_passthrough(&self) -> bool {
        self.primary.stage2.is_none()
    }

    /// Accumulates interleaved input samples.
    ///
    /// The length of `interleaved` must be a multiple of `in_channels`. `device_pts_ns` is the
    /// device-derived PTS corresponding to the first frame of this push.
    ///
    /// Returns [`Error::Backend`] if rubato's `process` fails (instead of panicking and silently
    /// stopping the ingest thread; the caller can stop the stream explicitly).
    pub fn push(&mut self, interleaved: &[f32], device_pts_ns: i64) -> Result<()> {
        if interleaved.is_empty() {
            return Ok(());
        }
        let in_frames = interleaved.len() / self.in_channels;
        if in_frames == 0 {
            return Ok(());
        }

        // Update each tap's PTS anchor by approximating, via the ratio, the output frame position
        // at which the start of this push will appear (approximate because of remainders held
        // inside the resampler). The primary and secondary each set theirs independently.
        self.primary
            .update_pts_anchor(self.total_inner_frames, device_pts_ns);
        if let Some(sec) = self.secondary.as_mut() {
            sec.update_pts_anchor(self.total_inner_frames, device_pts_ns);
        }

        // Stage 1: channel mix → stereo interleaved → 48k normalization. Gather what this push
        // produces into inner_scratch.
        self.inner_scratch.clear();
        let mut stereo = Vec::with_capacity(in_frames * INNER_CH);
        Self::mix_to_stereo(interleaved, self.in_channels, in_frames, &mut stereo);
        match &mut self.stage1_resampler {
            None => {
                // SR passthrough. Straight into the internal canonical form.
                self.total_inner_frames += in_frames as u64;
                self.inner_scratch.extend_from_slice(&stereo);
            }
            Some(rs) => {
                rs.in_accum.extend_from_slice(&stereo);
                let produced = rs.drain_into(&mut self.inner_scratch)?;
                self.total_inner_frames += produced;
            }
        }

        // Apply the processor (e.g. denoise) once to the internal canonical form before
        // branching into stage 2.
        if let Some(proc) = self.inner_processor.as_mut() {
            proc.process(&mut self.inner_scratch);
        }

        // Distribute the processed internal canonical form to each of the primary and secondary
        // stage 2s.
        self.distribute_inner()
    }

    /// Takes out one finished primary output chunk.
    ///
    /// Returns `(`out_chunk_frames` frames of output.channels interleaved, device_pts (ns) of
    /// the first sample)`. Returns `None` if one chunk's worth has not accumulated.
    pub fn pop_chunk(&mut self) -> Option<(Vec<f32>, i64)> {
        self.primary.pop()
    }

    /// Takes out one finished secondary output chunk (always `None` if no secondary tap is
    /// configured).
    pub fn pop_secondary(&mut self) -> Option<(Vec<f32>, i64)> {
        self.secondary.as_mut().and_then(OutputTap::pop)
    }

    /// Flush at stop. Feeds in the processor's (denoise, etc.) trailing tail, drains the
    /// remainder of each tap's stage 2 resampler, and pads the final partial chunk with silence
    /// to align to the fixed 20ms boundary (so everything can be taken with `pop_chunk` /
    /// `pop_secondary`).
    ///
    /// Continues best-effort even if a resampler flush fails, so the stop path is not halted
    /// (the loss is limited to the last few ms).
    pub fn flush(&mut self) {
        // 1. Feed the processor's trailing tail (e.g. denoise's delay line) in as internal
        //    canonical form.
        if let Some(proc) = self.inner_processor.as_mut() {
            let tail = proc.flush();
            if !tail.is_empty() {
                self.inner_scratch.clear();
                self.inner_scratch.extend_from_slice(&tail);
                let _ = self.distribute_inner();
            }
        }
        // 2. Drain each tap's stage 2 resampler remainder and pad the partial chunk with silence.
        self.primary.flush();
        if let Some(sec) = self.secondary.as_mut() {
            sec.flush();
        }
    }

    /// Number of not-yet-taken output frames currently held in `out_buf` (primary tap).
    pub fn buffered_out_frames(&self) -> usize {
        self.primary.buffered_out_frames()
    }

    // --- Internal helpers ---

    /// Distributes the internal canonical form in `inner_scratch` to each of the primary and
    /// secondary stage 2s (to avoid a borrow conflict, the buffer is taken out temporarily,
    /// distributed, and its capacity returned).
    fn distribute_inner(&mut self) -> Result<()> {
        if self.inner_scratch.is_empty() {
            return Ok(());
        }
        let inner = std::mem::take(&mut self.inner_scratch);
        let r_primary = self.primary.feed_inner(&inner);
        let r_secondary = self
            .secondary
            .as_mut()
            .map(|sec| sec.feed_inner(&inner))
            .unwrap_or(Ok(()));
        // Put the buffer back to reuse its capacity.
        self.inner_scratch = inner;
        self.inner_scratch.clear();
        r_primary.and(r_secondary)
    }

    /// Mixes any-ch interleaved into stereo interleaved and pushes it into `dst`.
    fn mix_to_stereo(src: &[f32], in_ch: usize, in_frames: usize, dst: &mut Vec<f32>) {
        match in_ch {
            1 => {
                // mono → stereo (duplicate L=R)
                for &s in &src[..in_frames] {
                    dst.push(s);
                    dst.push(s);
                }
            }
            2 => {
                // 2ch as-is (only the needed part)
                dst.extend_from_slice(&src[..in_frames * 2]);
            }
            _ => {
                // >2ch takes the front 2ch for now.
                // TODO(BS.775): apply proper downmix coefficients for 5.1 etc.
                for f in 0..in_frames {
                    let base = f * in_ch;
                    dst.push(src[base]);
                    dst.push(src[base + 1]);
                }
            }
        }
    }
}

impl OutputTap {
    /// Creates an output tap from an output format (`output` is expected to be validated).
    fn new(output: OutputFormat) -> Result<Self> {
        let out_channels = (output.channels.max(1)) as usize;
        let out_chunk_frames = output.chunk_frames().max(1);

        // If the output exactly matches the internal canonical form, stage 2 is unnecessary
        // (passthrough).
        let stage2 = if output.sample_rate == SAMPLE_RATE && out_channels == INNER_CH {
            None
        } else {
            Some(OutputStage::new(output.sample_rate, out_channels)?)
        };

        Ok(Self {
            output,
            stage2,
            out_buf: Vec::with_capacity(out_chunk_frames * out_channels * 4),
            out_chunk_frames,
            out_channels,
            out_frame_origin: 0,
            pts_anchor: None,
        })
    }

    /// Passes the processed internal canonical form (48k/stereo interleaved, any length)
    /// through stage 2 and appends the produced output frames to `out_buf`.
    fn feed_inner(&mut self, inner_stereo: &[f32]) -> Result<()> {
        if inner_stereo.is_empty() {
            return Ok(());
        }
        match &mut self.stage2 {
            None => {
                // Stage 2 passthrough (output == {48000, 2}). Append as-is.
                self.out_buf.extend_from_slice(inner_stereo);
            }
            Some(stage) => stage.process_inner(inner_stereo, &mut self.out_buf)?,
        }
        Ok(())
    }

    /// Takes out one finished output chunk. `None` if one chunk's worth has not accumulated.
    fn pop(&mut self) -> Option<(Vec<f32>, i64)> {
        let need = self.out_chunk_frames * self.out_channels;
        if self.out_buf.len() < need {
            return None;
        }
        let pts = self.pts_for_out_frame(self.out_frame_origin);
        let chunk: Vec<f32> = self.out_buf.drain(..need).collect();
        self.out_frame_origin += self.out_chunk_frames as u64;
        Some((chunk, pts))
    }

    /// Flush at stop. Drains stage 2's resampler remainder and pads the final partial chunk
    /// with silence to align to the fixed 20ms boundary (so everything can be taken with `pop`).
    fn flush(&mut self) {
        if let Some(stage) = self.stage2.as_mut() {
            // A resampler flush failure is ignored best-effort (only the last few ms are lost).
            let _ = stage.flush_into(&mut self.out_buf);
        }
        let need = self.out_chunk_frames * self.out_channels;
        let rem = self.out_buf.len() % need;
        if rem != 0 {
            let pad = need - rem;
            self.out_buf.resize(self.out_buf.len() + pad, 0.0);
        }
    }

    /// Number of not-yet-taken output frames currently held in `out_buf`.
    fn buffered_out_frames(&self) -> usize {
        self.out_buf.len() / self.out_channels
    }

    /// Sets a PTS anchor at the output frame position corresponding to the start of this push.
    ///
    /// The output frame position is an approximation obtained by mapping the cumulative internal
    /// frame count to the output rate (not exact because of remainders held inside the
    /// resampler). `in_sample_rate` cancels out, so only the output rate and the internal rate
    /// are needed.
    fn update_pts_anchor(&mut self, total_inner_frames: u64, device_pts_ns: i64) {
        let projected_out_frame = (total_inner_frames as f64 * self.output.sample_rate as f64
            / SAMPLE_RATE as f64) as u64;
        self.pts_anchor = Some(PtsAnchor {
            out_frame: projected_out_frame,
            pts_ns: device_pts_ns,
        });
    }

    /// Computes the device_pts (ns) corresponding to output frame index `out_frame` by
    /// extrapolating from the anchor using the output rate ratio.
    fn pts_for_out_frame(&self, out_frame: u64) -> i64 {
        match self.pts_anchor {
            None => crate::clock::monotonic_now_ns(),
            Some(anchor) => {
                let frame_delta = out_frame as i64 - anchor.out_frame as i64;
                let ns_per_out_frame = 1_000_000_000_i64 / self.output.sample_rate as i64;
                anchor.pts_ns + frame_delta * ns_per_out_frame
            }
        }
    }
}

impl ResamplerState {
    /// Creates a fixed-ratio `in_sr` → `out_sr` resampler with `channels` ch.
    ///
    /// Returns [`Error::Backend`] if building rubato fails (instead of panicking and silently
    /// stopping the thread).
    fn new(in_sr: u32, out_sr: u32, channels: usize) -> Result<Self> {
        let ratio = out_sr as f64 / in_sr as f64;
        // The fixed input chunk is 20ms worth of input frames (rubato keeps remainders
        // internally).
        let chunk_in_frames = (in_sr as usize / 50).max(64);

        let params = SincInterpolationParameters {
            sinc_len: 128,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 128,
            window: WindowFunction::BlackmanHarris2,
        };

        let inner = Async::<f32>::new_sinc(
            ratio,
            1.0, // fixed ratio (no variable resampling needed)
            &params,
            chunk_in_frames,
            channels,
            FixedAsync::Input,
        )
        .map_err(|e| Error::Backend(format!("rubato sinc resampler construction failed: {e}")))?;

        let max_out_frames = inner.output_frames_max();

        Ok(Self {
            inner,
            channels,
            chunk_in_frames,
            max_out_frames,
            in_accum: Vec::with_capacity(chunk_in_frames * channels * 4),
            out_scratch: vec![0.0; max_out_frames * channels],
        })
    }

    /// Resamples as much of what has accumulated in `in_accum` as possible in units of
    /// chunk_in_frames and appends the produced interleaved samples to `out_buf`. Returns the
    /// number of output frames produced.
    ///
    /// Returns [`Error::Backend`] if building the rubato adapter or `process_into_buffer` fails
    /// (instead of panicking and silently stopping the ingest thread).
    fn drain_into(&mut self, out_buf: &mut Vec<f32>) -> Result<u64> {
        let step = self.chunk_in_frames * self.channels;
        let mut produced = 0u64;

        while self.in_accum.len() >= step {
            let in_adapter =
                InterleavedSlice::new(&self.in_accum[..step], self.channels, self.chunk_in_frames)
                    .map_err(|e| {
                        Error::Backend(format!("rubato interleaved input adapter failed: {e}"))
                    })?;

            let mut out_adapter = InterleavedSlice::new_mut(
                &mut self.out_scratch[..],
                self.channels,
                self.max_out_frames,
            )
            .map_err(|e| {
                Error::Backend(format!("rubato interleaved output adapter failed: {e}"))
            })?;

            let indexing = Indexing {
                input_offset: 0,
                output_offset: 0,
                partial_len: None,
                active_channels_mask: None,
            };

            let (_in_used, out_written) = self
                .inner
                .process_into_buffer(&in_adapter, &mut out_adapter, Some(&indexing))
                .map_err(|e| Error::Backend(format!("rubato process_into_buffer failed: {e}")))?;

            let n_samples = out_written * self.channels;
            out_buf.extend_from_slice(&self.out_scratch[..n_samples]);
            produced += out_written as u64;

            // Remove the consumed input (with FixedAsync::Input the consumption is fixed at
            // chunk_in_frames).
            self.in_accum.drain(..step);
        }
        Ok(produced)
    }

    /// At stop, finally feeds the remainder left in `in_accum` (less than one input chunk) with
    /// `partial_len` and appends the produced interleaved samples to `out_buf`. Returns the
    /// number of output frames produced.
    ///
    /// This drains the rounding leftover input (up to just under 20ms). `in_accum` is empty
    /// after the call. It does not flush the resampler's internal filter-bank delay (a few ms).
    fn flush_into(&mut self, out_buf: &mut Vec<f32>) -> Result<u64> {
        let remaining = self.in_accum.len() / self.channels;
        if remaining == 0 {
            return Ok(0);
        }
        // Pad the input with silence up to one chunk and report only the valid length via
        // `partial_len`.
        self.in_accum
            .resize(self.chunk_in_frames * self.channels, 0.0);

        let in_adapter = InterleavedSlice::new(
            &self.in_accum[..self.chunk_in_frames * self.channels],
            self.channels,
            self.chunk_in_frames,
        )
        .map_err(|e| Error::Backend(format!("rubato interleaved input adapter failed: {e}")))?;

        let mut out_adapter = InterleavedSlice::new_mut(
            &mut self.out_scratch[..],
            self.channels,
            self.max_out_frames,
        )
        .map_err(|e| Error::Backend(format!("rubato interleaved output adapter failed: {e}")))?;

        let indexing = Indexing {
            input_offset: 0,
            output_offset: 0,
            partial_len: Some(remaining),
            active_channels_mask: None,
        };

        let (_in_used, out_written) = self
            .inner
            .process_into_buffer(&in_adapter, &mut out_adapter, Some(&indexing))
            .map_err(|e| Error::Backend(format!("rubato flush process_into_buffer failed: {e}")))?;

        let n_samples = out_written * self.channels;
        out_buf.extend_from_slice(&self.out_scratch[..n_samples]);
        self.in_accum.clear();
        Ok(out_written as u64)
    }
}

impl OutputStage {
    /// Creates the exit stage given the output rate / output channel count.
    ///
    /// When `out_sample_rate == 48000`, SR conversion is a passthrough (channel conversion
    /// only). A rubato build failure is propagated as [`Error::Backend`].
    fn new(out_sample_rate: u32, out_channels: usize) -> Result<Self> {
        let resampler = if out_sample_rate == SAMPLE_RATE {
            None
        } else {
            // Convert from the internal canonical form 48000 to out_sample_rate with
            // out_channels ch.
            Some(ResamplerState::new(
                SAMPLE_RATE,
                out_sample_rate,
                out_channels,
            )?)
        };
        Ok(Self {
            out_channels,
            resampler,
            ch_scratch: Vec::with_capacity(CHUNK_FRAMES * out_channels),
        })
    }

    /// Processes the internal canonical form (48k/stereo interleaved, any length) and appends
    /// interleaved samples in the output format to `out_buf`. The length must be a multiple of
    /// `INNER_CH` (stereo).
    fn process_inner(&mut self, inner_stereo: &[f32], out_buf: &mut Vec<f32>) -> Result<()> {
        let frames = inner_stereo.len() / INNER_CH;
        if frames == 0 {
            return Ok(());
        }

        // Channel conversion: stereo → out_channels.
        self.ch_scratch.clear();
        match self.out_channels {
            1 => {
                // stereo → mono (L/R average).
                for f in 0..frames {
                    let l = inner_stereo[f * 2];
                    let r = inner_stereo[f * 2 + 1];
                    self.ch_scratch.push((l + r) * 0.5);
                }
            }
            2 => {
                self.ch_scratch
                    .extend_from_slice(&inner_stereo[..frames * 2]);
            }
            _ => {
                // validate should have restricted this to 1/2. If it arrives anyway, get by
                // duplicating L.
                for f in 0..frames {
                    let l = inner_stereo[f * 2];
                    for _ in 0..self.out_channels {
                        self.ch_scratch.push(l);
                    }
                }
            }
        }

        // SR conversion: 48000 → out_sample_rate. On passthrough, ch_scratch goes straight to
        // the output.
        match &mut self.resampler {
            None => {
                out_buf.extend_from_slice(&self.ch_scratch);
            }
            Some(rs) => {
                rs.in_accum.extend_from_slice(&self.ch_scratch);
                rs.drain_into(out_buf)?;
            }
        }
        Ok(())
    }

    /// At stop, drains the SR resampler's remainder and appends it to `out_buf` (a no-op for a
    /// passthrough stage, which holds no remainder).
    fn flush_into(&mut self, out_buf: &mut Vec<f32>) -> Result<()> {
        if let Some(rs) = self.resampler.as_mut() {
            rs.flush_into(out_buf)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    /// Helper for the default output ({48000, 2}).
    fn default_out() -> OutputFormat {
        OutputFormat::default()
    }

    #[test]
    fn mono_48k_to_stereo_duplicates_channels() {
        let mut n = Normalizer::new(48_000, 1, default_out()).expect("normalizer");
        assert!(n.is_passthrough());
        assert!(n.is_output_passthrough());
        // 960 frames of mono input (passthrough, so exactly one chunk).
        let mono: Vec<f32> = (0..CHUNK_FRAMES).map(|i| (i as f32) * 0.001).collect();
        n.push(&mono, 0).expect("push");
        let (chunk, _pts) = n.pop_chunk().expect("one chunk");
        assert_eq!(chunk.len(), CHUNK_FRAMES * 2);
        // L == R holds for every frame.
        for f in 0..CHUNK_FRAMES {
            assert_eq!(chunk[f * 2], chunk[f * 2 + 1], "L==R at frame {f}");
            assert_eq!(chunk[f * 2], mono[f]);
        }
    }

    #[test]
    fn passthrough_preserves_frame_count() {
        let mut n = Normalizer::new(48_000, 2, default_out()).expect("normalizer");
        assert!(n.is_passthrough());
        assert!(n.is_output_passthrough());
        // 2 chunks + a remainder.
        let frames = CHUNK_FRAMES * 2 + 100;
        let stereo: Vec<f32> = (0..frames * 2).map(|i| (i as f32) * 1e-4).collect();
        n.push(&stereo, 0).expect("push");

        let mut got_frames = 0usize;
        while let Some((c, _)) = n.pop_chunk() {
            assert_eq!(c.len(), CHUNK_FRAMES * 2);
            got_frames += CHUNK_FRAMES;
        }
        // Exactly 2 chunks can be taken, and the 100 frame remainder stays.
        assert_eq!(got_frames, CHUNK_FRAMES * 2);
        assert_eq!(n.buffered_out_frames(), 100);
    }

    #[test]
    fn stereo_44100_to_48000_yields_about_50_chunks_per_second() {
        let mut n = Normalizer::new(44_100, 2, default_out()).expect("normalizer");
        assert!(!n.is_passthrough());

        // 1 second of a 44100Hz stereo sine wave.
        let in_frames = 44_100;
        let freq = 440.0_f32;
        let mut interleaved = Vec::with_capacity(in_frames * 2);
        for i in 0..in_frames {
            let s = (2.0 * PI * freq * (i as f32) / 44_100.0).sin() * 0.5;
            interleaved.push(s); // L
            interleaved.push(s); // R
        }

        // Must not panic even with fragmented pushes (simulating small buffer arrivals on real
        // hardware).
        let mut pts = 0i64;
        for block in interleaved.chunks(441 * 2) {
            n.push(block, pts).expect("push");
            pts += (block.len() as i64 / 2) * 1_000_000_000 / 44_100;
        }

        let mut chunks = 0usize;
        while let Some((c, _pts)) = n.pop_chunk() {
            assert_eq!(c.len(), CHUNK_FRAMES * 2);
            chunks += 1;
        }
        assert!(
            (47..=50).contains(&chunks),
            "expected ~50 chunks, got {chunks}"
        );
    }

    #[test]
    fn pts_increases_monotonically_across_chunks() {
        let mut n = Normalizer::new(48_000, 2, default_out()).expect("normalizer");
        let frames = CHUNK_FRAMES * 3;
        let stereo = vec![0.0f32; frames * 2];
        n.push(&stereo, 100_000_000).expect("push");

        let mut last = i64::MIN;
        let mut count = 0;
        while let Some((_, pts)) = n.pop_chunk() {
            assert!(pts >= last, "pts must be non-decreasing");
            last = pts;
            count += 1;
        }
        assert_eq!(count, 3);
    }

    // --- Stage 2 (exit) verification ---

    /// 48k/stereo input + output {16000, 1} → 320 frame mono chunks.
    #[test]
    fn output_16k_mono_yields_320_frame_mono_chunks() {
        let out = OutputFormat {
            sample_rate: 16_000,
            channels: 1,
        };
        let mut n = Normalizer::new(48_000, 2, out).expect("normalizer");
        assert!(n.is_passthrough()); // stage 1 is an SR passthrough (48k input).
        assert!(!n.is_output_passthrough()); // stage 2 is active.

        // 1 second of a 48k stereo sine wave (fragmented pushes).
        let in_frames = 48_000;
        let freq = 440.0_f32;
        let mut pts = 0i64;
        for blk in 0..(in_frames / 480) {
            let mut block = Vec::with_capacity(480 * 2);
            for j in 0..480 {
                let i = blk * 480 + j;
                let s = (2.0 * PI * freq * (i as f32) / 48_000.0).sin() * 0.5;
                block.push(s);
                block.push(s);
            }
            n.push(&block, pts).expect("push");
            pts += 480 * 1_000_000_000 / 48_000;
        }

        let mut chunks = 0usize;
        while let Some((c, _)) = n.pop_chunk() {
            assert_eq!(c.len(), 320, "16k mono 20ms = 320 sample (mono)");
            chunks += 1;
        }
        // 16000/320 = 50 chunks/second. About 50 because of resampler latency.
        assert!(
            (47..=50).contains(&chunks),
            "expected ~50 chunks, got {chunks}"
        );
    }

    /// Output {16000, 2} → 320 frame, 640 sample (stereo).
    #[test]
    fn output_16k_stereo_yields_320_frame_640_sample_chunks() {
        let out = OutputFormat {
            sample_rate: 16_000,
            channels: 2,
        };
        let mut n = Normalizer::new(48_000, 2, out).expect("normalizer");
        let in_frames = 48_000;
        let stereo: Vec<f32> = (0..in_frames * 2)
            .map(|i| ((i / 2) as f32 * 0.0001).sin() * 0.3)
            .collect();
        for block in stereo.chunks(480 * 2) {
            n.push(block, 0).expect("push");
        }
        let mut chunks = 0usize;
        while let Some((c, _)) = n.pop_chunk() {
            assert_eq!(c.len(), 640, "16k stereo 20ms = 320 frame * 2 = 640 sample");
            chunks += 1;
        }
        assert!(
            (47..=50).contains(&chunks),
            "expected ~50 chunks, got {chunks}"
        );
    }

    /// Output {8000, 2} → 160 frame, 320 sample.
    #[test]
    fn output_8k_stereo_yields_160_frame_chunks() {
        let out = OutputFormat {
            sample_rate: 8_000,
            channels: 2,
        };
        let mut n = Normalizer::new(48_000, 2, out).expect("normalizer");
        let stereo: Vec<f32> = (0..48_000 * 2)
            .map(|i| (i as f32 * 1e-5).sin() * 0.2)
            .collect();
        for block in stereo.chunks(480 * 2) {
            n.push(block, 0).expect("push");
        }
        let mut chunks = 0usize;
        while let Some((c, _)) = n.pop_chunk() {
            assert_eq!(c.len(), 320, "8k stereo 20ms = 160 frame * 2 = 320 sample");
            chunks += 1;
        }
        assert!(
            (47..=50).contains(&chunks),
            "expected ~50 chunks, got {chunks}"
        );
    }

    /// stereo→mono is the L/R average (antiphase L=+a, R=-a approaches 0).
    #[test]
    fn stereo_to_mono_is_lr_average() {
        // Use output 48000/mono to verify only the channel conversion with SR passthrough.
        let out = OutputFormat {
            sample_rate: 48_000,
            channels: 1,
        };
        let mut n = Normalizer::new(48_000, 2, out).expect("normalizer");
        // Fully antiphase (L=+0.5, R=-0.5) → average 0.
        let mut stereo = Vec::with_capacity(CHUNK_FRAMES * 2);
        for _ in 0..CHUNK_FRAMES {
            stereo.push(0.5);
            stereo.push(-0.5);
        }
        n.push(&stereo, 0).expect("push");
        let (chunk, _) = n.pop_chunk().expect("one mono chunk");
        assert_eq!(chunk.len(), CHUNK_FRAMES); // mono 960 sample.
        for &s in &chunk {
            assert!(
                s.abs() < 1e-6,
                "the antiphase average should be near 0: {s}"
            );
        }
    }

    // --- Value verification helpers (confirm amplitude and frequency are preserved) ---

    /// RMS (linear) of a sample sequence. For a sine wave of amplitude A it is A/√2.
    fn rms(samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        let sum_sq: f64 = samples.iter().map(|&x| (x as f64) * (x as f64)).sum();
        (sum_sq / samples.len() as f64).sqrt() as f32
    }

    /// Counts positive→negative / negative→positive zero crossings. A period crosses twice, so
    /// the estimated frequency = (crossings / 2) / seconds. Pass the middle to avoid the
    /// start/end transients.
    fn zero_crossings(samples: &[f32]) -> usize {
        let mut crossings = 0;
        for w in samples.windows(2) {
            // Only strict sign changes (exactly 0 is ignored).
            if (w[0] < 0.0 && w[1] >= 0.0) || (w[0] >= 0.0 && w[1] < 0.0) {
                crossings += 1;
            }
        }
        crossings
    }

    /// Resampling a 44.1kHz/mono 440Hz sine to 48kHz/stereo preserves the amplitude (RMS) and
    /// frequency (zero-crossing estimate). To avoid resampler ringing, it streams 1 second and
    /// measures only the middle chunks.
    #[test]
    fn resample_44100_to_48000_preserves_amplitude_and_frequency() {
        let mut n = Normalizer::new(44_100, 1, default_out()).expect("normalizer");
        let freq = 440.0_f32;
        let amp = 0.5_f32;
        let in_rate = 44_100usize;
        // Stream 2 seconds to get enough chunks (with room to discard transients).
        let total_frames = in_rate * 2;
        let mut pts = 0i64;
        for blk in 0..(total_frames / 441) {
            let mut block = Vec::with_capacity(441);
            for j in 0..441 {
                let i = blk * 441 + j;
                block.push((2.0 * PI * freq * (i as f32) / in_rate as f32).sin() * amp);
            }
            n.push(&block, pts).expect("push");
            pts += 441 * 1_000_000_000 / in_rate as i64;
        }

        // Concatenate all chunks (the output is 48k/stereo/960frame).
        let mut left: Vec<f32> = Vec::new();
        while let Some((c, _)) = n.pop_chunk() {
            assert_eq!(c.len(), CHUNK_FRAMES * 2);
            // Take only the L channel (mono→stereo duplication, so L==R).
            for f in 0..CHUNK_FRAMES {
                assert_eq!(c[f * 2], c[f * 2 + 1], "mono input, so L==R");
                left.push(c[f * 2]);
            }
        }
        assert!(
            left.len() >= 48_000,
            "at least 1 second of output is required: {}",
            left.len()
        );

        // Discard the transients (0.25 seconds each at start and end = 12000 sample) and
        // measure the middle 1 second.
        let start = 12_000;
        let mid = &left[start..start + 48_000];

        // Amplitude: the sine's RMS is amp/√2 ≈ 0.3536. Within ±5% after the resampler.
        let got_rms = rms(mid);
        let expect_rms = amp / std::f32::consts::SQRT_2;
        let rms_err = ((got_rms - expect_rms) / expect_rms).abs();
        assert!(
            rms_err < 0.05,
            "RMS preservation error too large: got={got_rms} expect={expect_rms} err={rms_err}"
        );

        // Frequency: zero crossings in the middle 1 second (48000 sample) ≈ 2*440 = 880.
        // Within ±2%.
        let crossings = zero_crossings(mid);
        let est_freq = crossings as f32 / 2.0; // 1 second, so crossings/2 = Hz.
        let freq_err = ((est_freq - freq) / freq).abs();
        assert!(
            freq_err < 0.02,
            "frequency not preserved: crossings={crossings} estimate={est_freq}Hz err={freq_err}"
        );
    }

    /// Actual channel count and sample values of 16k/mono output: downsampling a 48k/stereo
    /// 440Hz input to 16k/mono preserves amplitude/frequency with 1ch, 320sample.
    #[test]
    fn output_16k_mono_preserves_values() {
        let out = OutputFormat {
            sample_rate: 16_000,
            channels: 1,
        };
        let mut n = Normalizer::new(48_000, 2, out).expect("normalizer");
        let freq = 440.0_f32;
        let amp = 0.5_f32;
        let in_rate = 48_000usize;
        let total_frames = in_rate * 2;
        let mut pts = 0i64;
        for blk in 0..(total_frames / 480) {
            let mut block = Vec::with_capacity(480 * 2);
            for j in 0..480 {
                let i = blk * 480 + j;
                let s = (2.0 * PI * freq * (i as f32) / in_rate as f32).sin() * amp;
                block.push(s); // L
                block.push(s); // R
            }
            n.push(&block, pts).expect("push");
            pts += 480 * 1_000_000_000 / in_rate as i64;
        }

        let mut mono: Vec<f32> = Vec::new();
        while let Some((c, _)) = n.pop_chunk() {
            assert_eq!(c.len(), 320, "16k/mono 20ms = 320 sample (1ch)");
            mono.extend_from_slice(&c);
        }
        assert!(
            mono.len() >= 16_000,
            "at least 1 second is required: {}",
            mono.len()
        );

        // Discard the transients and measure the middle 1 second (16000 sample).
        let start = 4_000;
        let mid = &mono[start..start + 16_000];

        // Averaging an in-phase L==R signal leaves the level unchanged → RMS ≈ amp/√2.
        let got_rms = rms(mid);
        let expect_rms = amp / std::f32::consts::SQRT_2;
        let rms_err = ((got_rms - expect_rms) / expect_rms).abs();
        assert!(
            rms_err < 0.05,
            "16k/mono RMS preservation error: got={got_rms} expect={expect_rms} err={rms_err}"
        );

        // Frequency: crossings over the middle 1 second of 16000 sample ≈ 880. Within ±2%.
        let est_freq = zero_crossings(mid) as f32 / 2.0;
        let freq_err = ((est_freq - freq) / freq).abs();
        assert!(
            freq_err < 0.02,
            "16k/mono frequency preservation error: estimate={est_freq}Hz err={freq_err}"
        );
    }

    /// PTS increases monotonically, and the delta between adjacent chunks is ~20ms (1e7 ns ±
    /// tolerance). Confirms by value that the PTS anchor is extrapolated correctly on the 48k
    /// passthrough path.
    #[test]
    fn pts_delta_is_about_20ms_between_chunks() {
        let mut n = Normalizer::new(48_000, 2, default_out()).expect("normalizer");
        // Push 480 frame (10ms) at a time with pts (simulating small buffer arrivals on real
        // hardware).
        let mut device_pts = 1_000_000_000i64; // arbitrary origin.
        let block_frames = 480usize;
        for _ in 0..20 {
            let stereo = vec![0.1f32; block_frames * 2];
            n.push(&stereo, device_pts).expect("push");
            device_pts += block_frames as i64 * 1_000_000_000 / 48_000;
        }

        let mut pts_list = Vec::new();
        while let Some((_, pts)) = n.pop_chunk() {
            pts_list.push(pts);
        }
        assert!(
            pts_list.len() >= 5,
            "enough chunks are required: {}",
            pts_list.len()
        );

        // 20ms = 20_000_000 ns. Tolerance ±5% (1e6 ns).
        for w in pts_list.windows(2) {
            let delta = w[1] - w[0];
            assert!(delta > 0, "PTS strictly increases: {} -> {}", w[0], w[1]);
            assert!(
                (delta - 20_000_000).abs() <= 1_000_000,
                "adjacent PTS delta is not ~20ms: {delta} ns"
            );
        }
    }

    /// Even when the input samples are empty / a fraction (less than a multiple of
    /// in_channels), it returns Ok without panicking and no chunk is produced (boundary /
    /// defensive).
    #[test]
    fn push_empty_and_subframe_are_noops() {
        let mut n = Normalizer::new(48_000, 2, default_out()).expect("normalizer");
        // Empty.
        n.push(&[], 0).expect("empty push ok");
        // stereo (2ch) but only 1 sample → in_frames=0, early return.
        n.push(&[0.5], 0).expect("subframe push ok");
        assert!(
            n.pop_chunk().is_none(),
            "a fraction alone produces no chunk"
        );
        assert_eq!(n.buffered_out_frames(), 0);
    }

    /// Frequency-0 (silent DC) input yields all-zero output too (confirms the peak/rms 0 path).
    #[test]
    fn silence_input_yields_zero_output() {
        let mut n = Normalizer::new(48_000, 2, default_out()).expect("normalizer");
        let stereo = vec![0.0f32; CHUNK_FRAMES * 2];
        n.push(&stereo, 0).expect("push");
        let (chunk, _) = n.pop_chunk().expect("one chunk");
        assert!(
            chunk.iter().all(|&s| s == 0.0),
            "silent input gives silent output"
        );
    }

    // --- Secondary tap (dual output) verification ---

    /// Without a secondary tap, `pop_secondary` always returns `None` and `has_secondary` is
    /// false.
    #[test]
    fn no_secondary_tap_by_default() {
        let mut n = Normalizer::new(48_000, 2, default_out()).expect("normalizer");
        assert!(!n.has_secondary());
        assert_eq!(n.secondary_output(), None);
        let stereo = vec![0.1f32; CHUNK_FRAMES * 2];
        n.push(&stereo, 0).expect("push");
        assert!(n.pop_secondary().is_none(), "None without a secondary tap");
    }

    /// Produces both primary 48k/stereo + secondary 16k/mono from a single stage 1. The primary
    /// emits 960frame/stereo and the secondary 320frame/mono. Both are about 50 chunks/second.
    #[test]
    fn dual_output_primary_and_secondary_shapes() {
        let secondary = OutputFormat {
            sample_rate: 16_000,
            channels: 1,
        };
        let mut n = Normalizer::new(48_000, 2, default_out())
            .expect("normalizer")
            .with_secondary(secondary)
            .expect("secondary");
        assert!(n.has_secondary());
        assert_eq!(n.secondary_output(), Some(secondary));

        // Push 1 second of 48k/stereo, 480 frame at a time.
        let mut pts = 0i64;
        for _ in 0..100 {
            let block = vec![0.2f32; 480 * 2];
            n.push(&block, pts).expect("push");
            pts += 480 * 1_000_000_000 / 48_000;
        }

        let mut primary_chunks = 0usize;
        while let Some((c, _)) = n.pop_chunk() {
            assert_eq!(
                c.len(),
                CHUNK_FRAMES * 2,
                "primary is 48k/stereo = 1920 sample"
            );
            primary_chunks += 1;
        }
        let mut secondary_chunks = 0usize;
        while let Some((c, _)) = n.pop_secondary() {
            assert_eq!(c.len(), 320, "secondary is 16k/mono = 320 sample");
            secondary_chunks += 1;
        }
        assert!(
            (47..=50).contains(&primary_chunks),
            "primary ~50 chunks: {primary_chunks}"
        );
        assert!(
            (47..=50).contains(&secondary_chunks),
            "secondary ~50 chunks: {secondary_chunks}"
        );
    }

    /// Both the primary and secondary taps are on the PTS axis derived from the same pushes, and
    /// each increases monotonically by 20ms between adjacent chunks (each tap has its own
    /// independent PTS anchor).
    #[test]
    fn dual_output_taps_share_pts_axis() {
        let secondary = OutputFormat {
            sample_rate: 16_000,
            channels: 1,
        };
        let mut n = Normalizer::new(48_000, 2, default_out())
            .expect("normalizer")
            .with_secondary(secondary)
            .expect("secondary");

        let mut device_pts = 1_000_000_000i64;
        for _ in 0..40 {
            let block = vec![0.1f32; 480 * 2];
            n.push(&block, device_pts).expect("push");
            device_pts += 480 * 1_000_000_000 / 48_000;
        }

        let mut primary_pts = Vec::new();
        while let Some((_, p)) = n.pop_chunk() {
            primary_pts.push(p);
        }
        let mut secondary_pts = Vec::new();
        while let Some((_, p)) = n.pop_secondary() {
            secondary_pts.push(p);
        }
        assert!(primary_pts.len() >= 5 && secondary_pts.len() >= 5);
        for w in primary_pts.windows(2) {
            assert!((w[1] - w[0] - 20_000_000).abs() <= 1_000_000);
        }
        for w in secondary_pts.windows(2) {
            assert!((w[1] - w[0] - 20_000_000).abs() <= 1_000_000);
        }
        // The first PTS of both taps starts near the same push origin (within tens of ms).
        assert!(
            (primary_pts[0] - secondary_pts[0]).abs() < 100_000_000,
            "primary and secondary start PTS should be close: {} vs {}",
            primary_pts[0],
            secondary_pts[0]
        );
    }

    // --- InnerProcessor (equivalent to the denoise hook) and stop flush ---

    /// Test processor: doubles every sample (holds no trailing tail).
    struct DoubleProcessor;
    impl InnerProcessor for DoubleProcessor {
        fn process(&mut self, s: &mut [f32]) {
            for x in s.iter_mut() {
                *x *= 2.0;
            }
        }
        fn flush(&mut self) -> Vec<f32> {
            Vec::new()
        }
    }

    /// Test processor: a fixed delay line of `hold` samples (simulating denoise's delay line).
    /// The output is the input delayed by `hold` samples (the first `hold` are silence).
    /// `flush` returns the last `hold` samples.
    struct DelayProcessor {
        held: Vec<f32>,
    }
    impl DelayProcessor {
        fn new(hold: usize) -> Self {
            Self {
                held: vec![0.0; hold],
            }
        }
    }
    impl InnerProcessor for DelayProcessor {
        fn process(&mut self, s: &mut [f32]) {
            self.held.extend_from_slice(s);
            let n = s.len();
            s.copy_from_slice(&self.held[..n]);
            self.held.drain(..n);
        }
        fn flush(&mut self) -> Vec<f32> {
            std::mem::take(&mut self.held)
        }
    }

    /// The InnerProcessor is applied to the internal canonical form before branching into stage
    /// 2 (with the primary 48k/stereo passthrough, the output is simply doubled).
    #[test]
    fn inner_processor_applies_before_stage2() {
        let mut n = Normalizer::new(48_000, 2, default_out())
            .expect("normalizer")
            .with_inner_processor(Box::new(DoubleProcessor));
        let stereo: Vec<f32> = (0..CHUNK_FRAMES * 2).map(|i| (i as f32) * 1e-4).collect();
        n.push(&stereo, 0).expect("push");
        let (chunk, _) = n.pop_chunk().expect("one chunk");
        for (i, &s) in chunk.iter().enumerate() {
            assert!(
                (s - stereo[i] * 2.0).abs() < 1e-6,
                "sample {i} should be doubled"
            );
        }
    }

    /// The InnerProcessor affects both the primary and secondary taps (the secondary 16k/mono
    /// is doubled too).
    #[test]
    fn inner_processor_affects_both_taps() {
        let secondary = OutputFormat {
            sample_rate: 48_000,
            channels: 2,
        };
        // Make the secondary 48k/stereo (passthrough) too, so the doubling is observable
        // unaltered.
        let mut n = Normalizer::new(48_000, 2, default_out())
            .expect("normalizer")
            .with_secondary(secondary)
            .expect("secondary")
            .with_inner_processor(Box::new(DoubleProcessor));
        let stereo = vec![0.25f32; CHUNK_FRAMES * 2];
        n.push(&stereo, 0).expect("push");
        let (p, _) = n.pop_chunk().expect("primary chunk");
        let (s, _) = n.pop_secondary().expect("secondary chunk");
        assert!(
            p.iter().all(|&x| (x - 0.5).abs() < 1e-6),
            "primary is doubled"
        );
        assert!(
            s.iter().all(|&x| (x - 0.5).abs() < 1e-6),
            "secondary is doubled too"
        );
    }

    /// The stop flush feeds in the processor's trailing tail, which can be taken out as a final
    /// chunk. The held part of the delay-line processor appears in an extra chunk after flush.
    #[test]
    fn stop_flush_emits_processor_tail() {
        // Delay line with hold = 4 sample (2 stereo frame).
        let mut n = Normalizer::new(48_000, 2, default_out())
            .expect("normalizer")
            .with_inner_processor(Box::new(DelayProcessor::new(4)));
        // Push 1 chunk of non-zero, identifiable input.
        let stereo: Vec<f32> = (0..CHUNK_FRAMES * 2)
            .map(|i| (i as f32 + 1.0) * 1e-4)
            .collect();
        n.push(&stereo, 0).expect("push");

        // Before flush: 1 chunk (the first 4 sample are the delay's silence).
        let (c0, _) = n.pop_chunk().expect("first chunk");
        assert_eq!(c0.len(), CHUNK_FRAMES * 2);
        assert!(
            c0[..4].iter().all(|&x| x == 0.0),
            "the first 4 sample are the delay's silence"
        );
        assert!(n.pop_chunk().is_none(), "only 1 chunk before flush");

        // flush: the delay line's last 4 sample come out as an extra chunk (with silence
        // padding).
        n.flush();
        let (c1, _) = n.pop_chunk().expect("flushed tail chunk");
        assert_eq!(
            c1.len(),
            CHUNK_FRAMES * 2,
            "the final chunk is padded to 20ms"
        );
        // The last 4 sample = the last 4 sample of the input.
        let last4 = &stereo[stereo.len() - 4..];
        for (i, &x) in c1[..4].iter().enumerate() {
            assert!(
                (x - last4[i]).abs() < 1e-6,
                "the flush tail should match the end of the input"
            );
        }
    }

    /// The stop flush also drains the secondary tap's stage 2 resampler remainder (the tail
    /// arrives even for 16k/mono).
    #[test]
    fn stop_flush_drains_secondary_resampler() {
        let secondary = OutputFormat {
            sample_rate: 16_000,
            channels: 1,
        };
        let mut n = Normalizer::new(48_000, 2, default_out())
            .expect("normalizer")
            .with_secondary(secondary)
            .expect("secondary");
        // Push an amount close to just under 1 chunk (a remainder stays in the resampler).
        let stereo = vec![0.3f32; CHUNK_FRAMES * 2];
        n.push(&stereo, 0).expect("push");

        // Count the secondary chunks available before flush.
        let mut before = 0usize;
        while n.pop_secondary().is_some() {
            before += 1;
        }
        n.flush();
        // After flush, one or more final chunks are added (the remainder is drained).
        let mut after = 0usize;
        while let Some((c, _)) = n.pop_secondary() {
            assert_eq!(
                c.len(),
                320,
                "secondary is aligned to the fixed 20ms boundary"
            );
            after += 1;
        }
        assert!(after >= 1, "flush should drain the secondary tap's tail");
        let _ = before;
    }
}
