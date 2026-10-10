//! Child capture acquisition, loss accounting and canonical FIFO ownership.
use super::*;

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
}
impl ChildLane {
    pub(super) fn losses(&mut self) -> Result<()> {
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
        self.losses()?;
        let got = self.consumer.pop_slice(scratch);
        if got == 0 {
            return Ok(());
        }
        std::panic::catch_unwind(AssertUnwindSafe(|| {
            self.normalizer.push(&scratch[..got], monotonic_now_ns())
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
