//! Consumer polling and delivery controls.
use super::*;
impl Stream {
    /// Temporarily pause capture delivery.
    ///
    /// Keep OS-side capture running but stop delivering completed chunks. While paused,
    /// [`poll_chunk`](Self::poll_chunk) returns no new chunks. Intake continues internally to keep the
    /// device active, enabling a quick resume and avoiding false watchdog stall reports. Does nothing
    /// if already paused (safe to call repeatedly).
    ///
    /// When this call returns, any chunk being assembled by the intake thread has either been queued
    /// or discarded. After draining chunks already queued, [`poll_chunk`](Self::poll_chunk) returns no
    /// new chunks.
    ///
    /// Calling this before [`start`](Self::start) sets the flag, but it takes effect only once intake starts.
    pub fn pause(&self) {
        // Set the flag while holding delivery. The intake thread uses the same lock for pushes, so no
        // assembled chunk can enter the queue after this returns.
        let _g = self
            .shared
            .delivery
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.shared.paused.store(true, Ordering::SeqCst);
    }

    /// Resume delivery after [`pause`](Self::pause).
    ///
    /// After resume, mark the first chunk delivered on each stream (primary and secondary) with
    /// [`ChunkFlags::DISCONTINUITY`] to signal the time gap to consumers. Each stream's `seq` remains
    /// continuous across the pause; no silence is inserted for the paused interval. Does nothing if
    /// not paused (safe to call repeatedly).
    /// Returns the stored permission error if capture has terminally failed.
    pub fn resume(&self) -> Result<()> {
        // Hold delivery while advancing the generation and setting paused=false, making this one
        // enqueue boundary. Intake reads the generation under the same lock, so the first chunk for
        // each tap after resume is reliably marked DISCONTINUITY.
        let _g = self
            .shared
            .delivery
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Delivery is already held: inspect the recorded cause without acquiring
        // backend in reverse order. Only terminal_error() waits for OS shutdown.
        if let Some(error) = self.shared.terminal.error() {
            return Err(error);
        }
        // Advance the generation only if actually paused (resume on an unpaused stream must not add
        // an unnecessary DISCONTINUITY).
        if self.shared.paused.load(Ordering::SeqCst) {
            self.shared.resume_generation.fetch_add(1, Ordering::SeqCst);
            self.shared.paused.store(false, Ordering::SeqCst);
        }
        Ok(())
    }

    /// Whether currently paused.
    pub fn is_paused(&self) -> bool {
        self.shared.paused.load(Ordering::SeqCst)
    }

    /// Change input gain (linear multiplier). 1.0 = unchanged, 2.0 ≈ +6 dB, 0.0 = silence.
    ///
    /// May be called at any time during recording; takes effect from the next completed chunk (20 ms
    /// granularity). Samples are clamped to `-1.0..=1.0` after multiplication. At 1.0, samples are
    /// untouched (byte-for-byte passthrough). Values must be finite and >= 0, or
    /// [`Error::InvalidArg`] is returned and the current value remains unchanged.
    pub fn set_gain(&self, gain: f32) -> Result<()> {
        if !gain.is_finite() || gain < 0.0 {
            return Err(Error::InvalidArg(format!(
                "gain must be finite and >= 0.0, got {gain}"
            )));
        }
        self.shared
            .gain_bits
            .store(gain.to_bits(), Ordering::Relaxed);
        Ok(())
    }

    /// Current input gain (linear multiplier).
    pub fn gain(&self) -> f32 {
        f32::from_bits(self.shared.gain_bits.load(Ordering::Relaxed))
    }

    /// Retrieve one completed chunk (non-blocking). Returns `None` if none is available.
    ///
    /// Returned chunks contain interleaved `f32` in the output format (`config.output`). Chunks are
    /// fixed at 20 ms, with `data.len() == frames * output.channels`. With the default `{48000, 2}`,
    /// `frames == 960` (`data.len() == 1920`). `peak`/`rms` are computed from final data. `seq`
    /// increases monotonically.
    pub fn poll_chunk(&mut self) -> Option<AudioChunk> {
        let _delivery = self
            .shared
            .delivery
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if self.shared.terminal.is_failed() {
            return None;
        }
        self.chunk_consumer.try_pop()
    }

    /// Retrieve one completed secondary tap chunk (non-blocking). Returns `None` if none is available.
    ///
    /// Secondary chunks are generated only when `config.secondary_output` is `Some`; otherwise this
    /// always returns `None`. Secondary PTS values use the same zero-based recording clock as the
    /// primary [`AudioChunk`], but are independent and lag the primary by 20–60 ms due to group delay
    /// in the secondary Stage2 resampler. Match primary and secondary by `pts_ns` (time), since each
    /// tap has its own `seq` counter.
    pub fn poll_secondary(&mut self) -> Option<SecondaryChunk> {
        let _delivery = self
            .shared
            .delivery
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if self.shared.terminal.is_failed() {
            return None;
        }
        self.secondary_consumer.as_mut().and_then(|c| c.try_pop())
    }

    /// Terminal capture failure, retained after stop. Confirmed permission denial
    /// is terminal; a backend mailbox that cannot be reconciled also fails closed.
    /// Once this returns `Some`, backend shutdown has finished, including both
    /// children of a Mix source. This may wait for the control thread to finish
    /// stopping capture; audio delivery is gated immediately when failure is recorded.
    /// Create a new stream after changing OS settings and restarting the app.
    pub fn terminal_error(&self) -> Option<Error> {
        if !self.shared.terminal.is_failed() {
            return None;
        }
        // Every terminal shutdown holds backend until stop returns. Take the same
        // lock before exposing the error, without holding delivery or joining any
        // threads here. Internal callers already holding backend read the stored
        // cause directly rather than recursively entering this accessor.
        let mut backend = self
            .shared
            .backend
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.shared.stop_backend_owned(&mut backend);
        self.shared.terminal.error()
    }

    /// Enable or disable noise suppression (RNNoise) in the internal canonical format.
    ///
    /// Call before [`start`](Self::start) (applied when the intake thread builds Normalizer; changes
    /// during recording take effect on the next generation change, a source switch or automatic
    /// recovery). When enabled, denoise runs once on the 48kHz/stereo internal canonical format, so
    /// both taps receive denoised audio (+10ms fixed latency). Core does not depend on denoise; this
    /// facade injects the implementation.
    pub fn set_denoise(&self, enabled: bool) {
        let _consumer = self
            .shared
            .raw_consumer
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.shared.denoise_enabled.store(enabled, Ordering::SeqCst);
    }

    /// Retrieve one undelivered event (non-blocking). Returns `None` if none is available.
    pub fn poll_event(&mut self) -> Option<Event> {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
    }

    /// Total chunks discarded by the chunk ring through DROP_OLDEST.
    pub fn dropped_chunks(&self) -> u64 {
        self.chunk_consumer.dropped_count()
    }

    /// Reference to the current configuration.
    pub fn config(&self) -> &StreamConfig {
        &self.config
    }

    /// Native format `(sample_rate, channels)` of the current backend.
    ///
    /// Value obtained from the backend at open. Unchanged on watchdog recovery, but updated to the
    /// new backend's value when [`switch_source`](Self::switch_source) changes the source. For display
    /// and diagnostics (output format is `config().output`).
    pub fn native_format(&self) -> (u32, u16) {
        // Recover and read the value even if poisoned (avoid a panic cascade).
        *self
            .shared
            .native_format
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }
}
