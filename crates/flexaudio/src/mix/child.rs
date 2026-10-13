//! Child capture acquisition, loss accounting and canonical FIFO ownership.
use super::*;
use flexaudio_core::startup::StartupSamples;

pub(super) struct ChildLane {
    pub(super) consumer: RawConsumer,
    pub(super) normalizer: Normalizer,
    pub(super) fifo: Vec<f32>,
    pub(super) last_supply: Instant,
    pub(super) lane: MixLane,
    pub(super) format: (u32, u16),
    pub(super) diagnostics: CaptureDiagnostics,
    pub(super) overflow_seen: u64,
    pub(super) notices: Arc<Notices>,
    pub(super) startup: Option<StartupSamples>,
    pub(super) startup_started: Instant,
    pub(super) startup_reported: bool,
    /// EOF discarded an invalid startup that never completed confirmation.
    pub(super) startup_abandoned: bool,
    /// Prefix explicitly consumed while aligning the system lane to the mic.
    pub(super) startup_system_skipped: u64,
    /// FIFO eviction also consumes the system's original canonical prefix.
    pub(super) fifo_evicted: u64,
    /// Microphone callbacks may split a native interleaved frame between buffers.
    pub(super) pending_frame: Vec<f32>,
}
impl ChildLane {
    pub(super) fn losses(&mut self) -> Result<()> {
        if self.diagnostics.take_discontinuity() {
            self.notices.discontinuity();
        }
        let total = self.consumer.overflow_count();
        if let Some(samples) = NonZeroU64::new(total.saturating_sub(self.overflow_seen)) {
            self.notices.loss(AudioLoss::raw_overflow(
                Some(self.lane),
                Some(samples),
                self.format.0,
                self.format.1,
            )?);
        }
        self.overflow_seen = total;
        for loss in self.diagnostics.drain()? {
            self.notices.loss(loss.with_capture_lane(self.lane)?);
        }
        Ok(())
    }
    pub(super) fn ingest(&mut self, scratch: &mut [f32]) -> Result<()> {
        self.ingest_at(scratch, self.startup_started.elapsed())
    }
    fn ingest_at(&mut self, scratch: &mut [f32], elapsed: Duration) -> Result<()> {
        self.losses()?;
        let got = self.consumer.pop_slice(scratch);
        if self.startup.is_some() {
            let channels = usize::from(self.format.1);
            if self.pending_frame.is_empty() {
                let whole = got - got % channels;
                self.pending_frame.extend_from_slice(&scratch[whole..got]);
                return self.accept_samples(&scratch[..whole], elapsed);
            }
            let mut samples = std::mem::take(&mut self.pending_frame);
            samples.extend_from_slice(&scratch[..got]);
            let whole = samples.len() - samples.len() % channels;
            self.pending_frame.extend_from_slice(&samples[whole..]);
            return self.accept_samples(&samples[..whole], elapsed);
        }
        self.accept_samples(&scratch[..got], elapsed)
    }
    fn accept_samples(&mut self, input: &[f32], elapsed: Duration) -> Result<()> {
        let samples = match self.startup.as_mut() {
            Some(guard) => match guard.push(input, elapsed) {
                Ok(output) => output.samples,
                Err(error) => {
                    self.retain_startup_drop();
                    return Err(lane_error(error, Operation::Normalize, self.lane));
                }
            },
            None => std::borrow::Cow::Borrowed(input),
        };
        if samples.is_empty() {
            return Ok(());
        }
        self.normalize_samples(&samples)
    }
    fn normalize_samples(&mut self, samples: &[f32]) -> Result<()> {
        std::panic::catch_unwind(AssertUnwindSafe(|| {
            self.normalizer.push(samples, monotonic_now_ns())
        }))
        .unwrap_or_else(|_| Err(Error::Backend("mix child normalizer panicked".into())))
        .map_err(|error| lane_error(error, Operation::Normalize, self.lane))?;
        self.append_output();
        Ok(())
    }
    fn append_output(&mut self) {
        let mut supplied = false;
        while let Some((chunk, _)) = self.normalizer.pop_chunk() {
            self.fifo.extend_from_slice(&chunk);
            supplied = true;
        }
        if supplied {
            self.last_supply = Instant::now();
            if self.fifo.len() > FIFO_MAX_SAMPLES {
                let excess = self.fifo.len() - FIFO_MAX_SAMPLES;
                self.fifo.drain(..excess);
                self.fifo_evicted = self
                    .fifo_evicted
                    .saturating_add(u64::try_from(excess).unwrap_or(u64::MAX));
                self.notices.loss(AudioLoss::mix_fifo_overflow(
                    self.lane,
                    NonZeroU64::new(u64::try_from(excess).expect("FIFO capacity fits u64")),
                ));
            }
        }
    }
    pub(super) fn finish(&mut self, scratch: &mut [f32]) -> Result<()> {
        while self.consumer.available() > 0 {
            self.ingest(scratch)?;
        }
        self.losses()?;
        if !self.pending_frame.is_empty() {
            self.pending_frame.clear();
            self.notices.cleanup(lane_error(
                Error::Backend("microphone input ended with an incomplete native frame; the trailing samples were discarded. Check the input callback's channel layout and retry capture".into()),
                Operation::Flush,
                self.lane,
            ));
        }
        if let Some(end) = self.startup.as_mut().map(StartupSamples::finish) {
            if let Some(event) = end.event {
                self.startup_abandoned = true;
                if !self.startup_reported {
                    self.notices.queue_final(event);
                    self.startup_reported = true;
                }
            }
            if !end.samples.is_empty() {
                self.normalize_samples(&end.samples)?;
            }
        }
        if self.startup_abandoned {
            return Ok(());
        }
        if let Err(error) = std::panic::catch_unwind(AssertUnwindSafe(|| self.normalizer.flush()))
            .unwrap_or_else(|_| {
                Err(Error::Backend(
                    "mix child normalizer panicked during flush".into(),
                ))
            })
        {
            self.notices
                .cleanup(lane_error(error, Operation::Flush, self.lane));
        }
        self.append_output();
        Ok(())
    }
    /// Failure/stop reports through notices only when success diagnostics did not.
    pub(super) fn retain_startup_drop(&mut self) {
        if !self.startup_reported {
            if let Some(event) = self
                .startup
                .as_ref()
                .and_then(StartupSamples::dropped_event)
            {
                self.notices.queue_final(event);
                self.startup_reported = true;
            }
        }
    }
    /// Terminal delivery is closed; finish validation only to account its held PCM.
    pub(super) fn reject_startup(&mut self) -> Result<()> {
        if let Some(guard) = self.startup.as_mut().filter(|guard| guard.is_pending()) {
            let end = guard.finish();
            self.startup_abandoned = true;
            if let Some(loss) = flexaudio_core::startup::discarded_startup_loss(
                &end.samples,
                self.format.0,
                self.format.1,
            )? {
                self.notices.loss(loss.with_capture_lane(self.lane)?);
            }
        }
        self.retain_startup_drop();
        Ok(())
    }
    pub(super) fn is_starved(&self, now: Instant) -> bool {
        now.duration_since(self.last_supply) >= STARVATION_FILL_THRESHOLD
    }
}

pub(super) fn start_child(
    child: &mut Box<dyn CaptureBackend>,
    lane: MixLane,
    notices: Arc<Notices>,
) -> Result<ChildLane> {
    let (rate, channels) = child.native_format();
    // Validate the normalizer before acquiring a native producer.
    let normalizer = Normalizer::new(rate, channels, OutputFormat::default())
        .map_err(|error| lane_error(error, Operation::Normalize, lane))?;
    let (producer, consumer) = raw_ring(RAW_RING_SAMPLES);
    let sink = RawSink::new(producer, rate, channels);
    let diagnostics = sink.diagnostics();
    let mut state = ChildLane {
        consumer,
        normalizer,
        fifo: Vec::with_capacity(FIFO_MAX_SAMPLES),
        last_supply: Instant::now(),
        lane,
        format: (rate, channels),
        diagnostics,
        overflow_seen: 0,
        notices,
        startup: (lane == MixLane::Microphone).then(|| StartupSamples::new(rate, channels)),
        startup_started: Instant::now(),
        startup_reported: false,
        startup_abandoned: false,
        startup_system_skipped: 0,
        fifo_evicted: 0,
        pending_frame: Vec::with_capacity(usize::from(channels)),
    };
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| child.start(sink)))
        .unwrap_or_else(|_| Err(Error::Backend("mix child panicked during start".into())))
        .map_err(|error| lane_error(error, Operation::Start, lane));
    if result.is_err() {
        if let Err(error) = stop_child(child, lane) {
            state.notices.cleanup(error);
        }
        if let Err(error) = state.losses() {
            state.notices.cleanup(error);
        }
    }
    result?;
    Ok(state)
}

pub(super) fn stop_child(child: &mut Box<dyn CaptureBackend>, lane: MixLane) -> Result<()> {
    std::panic::catch_unwind(AssertUnwindSafe(|| child.stop_checked()))
        .unwrap_or_else(|_| Err(Error::Backend("mix child panicked during stop".into())))
        .map_err(|error| lane_error(error, Operation::Stop, lane))
}

#[cfg(test)]
#[path = "startup_tests.rs"]
mod startup_tests;
