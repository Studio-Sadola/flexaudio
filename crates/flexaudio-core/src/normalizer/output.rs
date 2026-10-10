//! Output conversion, chunking and provenance.
use super::*;
impl OutputTap {
    /// Create an output tap from an output format (expects `output` to be validated).
    pub(super) fn new(output: OutputFormat) -> Result<Self> {
        output.validate()?;
        let out_channels = usize::from(output.channels);
        let out_chunk_frames = output.chunk_frames().max(1);

        // Skip stage 2 (passthrough) when output exactly matches the internal canonical form.
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
            canonical_clock: false,
            padding: std::collections::VecDeque::new(),
        })
    }

    /// Pass the processed internal canonical form (48k/stereo interleaved, any length) through
    /// stage 2 and append the resulting output frames to `out_buf`.
    pub(super) fn feed_inner(&mut self, inner_stereo: &[f32]) -> Result<()> {
        if inner_stereo.is_empty() {
            return Ok(());
        }
        match &mut self.stage2 {
            None => {
                // Stage 2 passthrough (`output == {48000, 2}`). Append unchanged.
                self.out_buf.extend_from_slice(inner_stereo);
            }
            Some(stage) => stage.process_inner(inner_stereo, &mut self.out_buf)?,
        }
        Ok(())
    }

    /// Retrieve one completed output chunk. Returns `None` if a full chunk is not buffered.
    pub(super) fn pop(&mut self) -> Option<NormalizedChunk> {
        let need = self.out_chunk_frames * self.out_channels;
        if self.out_buf.len() < need {
            return None;
        }
        let pts = self.pts_for_out_frame(self.out_frame_origin);
        let chunk: Vec<f32> = self.out_buf.drain(..need).collect();
        let start = self.out_frame_origin * self.out_channels as u64;
        let end = start + need as u64;
        let padding = self
            .padding
            .iter()
            .map(|range| range.end.min(end).saturating_sub(range.start.max(start)))
            .sum::<u64>();
        let mut flags = ChunkFlags::empty();
        if padding > 0 {
            flags |= ChunkFlags::PADDED;
        }
        if padding == need as u64 {
            flags |= ChunkFlags::SILENCE;
        }
        while self.padding.front().is_some_and(|range| range.end <= end) {
            self.padding.pop_front();
        }
        self.out_frame_origin += self.out_chunk_frames as u64;
        Some(NormalizedChunk {
            samples: chunk,
            pts_ns: pts,
            flags,
        })
    }

    /// Flush on stop. Drain the stage 2 resampler remainder and pad a final partial chunk with
    /// silence to align it to the fixed 20ms boundary for retrieval with `pop`.
    pub(super) fn flush(&mut self) -> Result<()> {
        let result = if let Some(stage) = self.stage2.as_mut() {
            stage.flush_into(&mut self.out_buf).map(|_| ())
        } else {
            Ok(())
        };
        let need = self.out_chunk_frames * self.out_channels;
        let rem = self.out_buf.len() % need;
        if rem != 0 {
            let pad = need - rem;
            let start =
                self.out_frame_origin * self.out_channels as u64 + self.out_buf.len() as u64;
            self.padding.push_back(start..start + pad as u64);
            self.out_buf.resize(self.out_buf.len() + pad, 0.0);
        }
        result
    }

    /// Number of output frames buffered but not yet retrieved from `out_buf`.
    pub(super) fn buffered_out_frames(&self) -> usize {
        self.out_buf.len() / self.out_channels
    }

    /// Set the PTS anchor for the output frame position corresponding to the start of this push.
    ///
    /// The output frame position is an approximation mapped from the total internal frame count
    /// to the output rate (not exact because the resampler retains a remainder). `in_sample_rate`
    /// cancels out, so only the output and internal rates are needed.
    pub(super) fn update_pts_anchor(
        &mut self,
        total_inner_frames: u64,
        device_pts_ns: i64,
    ) -> Result<()> {
        let projected_out_frame = if self.canonical_clock {
            u64::try_from(
                u128::from(total_inner_frames) * u128::from(self.output.sample_rate)
                    / u128::from(SAMPLE_RATE),
            )
            .map_err(|_| Error::InvalidState("canonical output timeline exhausted".into()))?
        } else {
            // Preserve aecdc40's floating-point projection for caller-visible taps.
            (total_inner_frames as f64 * self.output.sample_rate as f64 / SAMPLE_RATE as f64) as u64
        };
        self.pts_anchor = Some(PtsAnchor {
            out_frame: projected_out_frame,
            pts_ns: device_pts_ns,
        });
        Ok(())
    }

    /// Extrapolate `device_pts` (ns) for output frame index `out_frame` from the anchor using the
    /// output sample rate.
    pub(super) fn pts_for_out_frame(&self, out_frame: u64) -> i64 {
        match self.pts_anchor {
            None => crate::clock::monotonic_now_ns(),
            Some(anchor) if !self.canonical_clock => {
                let frame_delta = out_frame as i64 - anchor.out_frame as i64;
                let ns_per_out_frame = 1_000_000_000_i64 / self.output.sample_rate as i64;
                anchor.pts_ns + frame_delta * ns_per_out_frame
            }
            Some(anchor) => {
                let frame_delta = i128::from(out_frame) - i128::from(anchor.out_frame);
                let pts = i128::from(anchor.pts_ns)
                    + (frame_delta * 1_000_000_000).div_euclid(i128::from(self.output.sample_rate));
                i64::try_from(pts).unwrap_or(if pts < 0 { i64::MIN } else { i64::MAX })
            }
        }
    }
}
