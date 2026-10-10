//! Per-tap producer clock, delivery gate, flags, and ring publication.
use super::*;

#[derive(Default)]
pub(super) struct TapState {
    pub seq: u64,
    pub recovered: bool,
    pub discontinuity: bool,
    pub resume_generation: u64,
    pub last_pts: i64,
}

pub(super) enum Ring<'a> {
    Capture(&'a mut ChunkProducer),
    Primary(&'a mut ChunkProducer),
    Secondary(&'a mut SecondaryChunkProducer),
}

pub(super) fn drain(
    shared: &SharedState,
    mut pop: impl FnMut() -> Option<NormalizedChunk>,
    format: OutputFormat,
    gain: f32,
    counter: &AtomicU64,
    state: &mut TapState,
    mut ring: Ring<'_>,
) -> Result<bool> {
    let channels = usize::from(format.channels);
    let capture = matches!(ring, Ring::Capture(_));
    let primary = matches!(ring, Ring::Primary(_));
    let mut emitted = false;
    while let Some(chunk) = pop() {
        let mut data = chunk.samples;
        let raw_pts = chunk.pts_ns;
        let mut integrity_flags = chunk.flags;
        let frames = data.len() / channels;
        let frame_index = advance_frame_index(counter, frames as u64, format.sample_rate)?;
        // Popped frames advance even while discarded, retaining flags until delivery.
        if shared.paused.load(Ordering::SeqCst) {
            continue;
        }
        // Preserve the legacy output PTS/metrics preparation before the delivery lock.
        let prepared = if capture {
            None
        } else {
            let pts = apply_epoch(shared, raw_pts).max(state.last_pts);
            state.last_pts = pts;
            if apply_gain(&mut data, gain) {
                integrity_flags |= ChunkFlags::CLIPPED;
            }
            Some((pts, peak_rms(&data)))
        };
        let _delivery = shared.delivery.lock().unwrap_or_else(|e| e.into_inner());
        if shared.paused.load(Ordering::SeqCst) || shared.terminal.is_failed() {
            continue;
        }
        let (pts_ns, (peak, rms)) = prepared.unwrap_or_else(|| {
            let pts = apply_epoch(shared, raw_pts);
            if apply_gain(&mut data, gain) {
                integrity_flags |= ChunkFlags::CLIPPED;
            }
            (pts, peak_rms(&data))
        });
        let resumed = shared.resume_generation.load(Ordering::SeqCst);
        let mut flags = integrity_flags;
        if state.recovered && !capture && !shared.stopping.load(Ordering::SeqCst) {
            flags |= ChunkFlags::RECOVERED | ChunkFlags::DISCONTINUITY;
        } else if state.recovered && !capture {
            flags |= ChunkFlags::DISCONTINUITY;
        }
        if state.discontinuity || resumed != state.resume_generation {
            flags |= ChunkFlags::DISCONTINUITY;
        }
        let chunk = AudioChunk {
            data,
            frames,
            frame_index,
            pts_ns,
            seq: state.seq,
            flags,
            dropped_before: 0,
            peak,
            rms,
        };
        match &mut ring {
            Ring::Capture(producer) => {
                producer.push(chunk);
            }
            Ring::Primary(producer) => {
                if let Some(total) = producer.push(chunk) {
                    shared.push_event(Event::ChunkDropped { count: total });
                }
            }
            Ring::Secondary(producer) => {
                let dropped_before = producer.dropped_count();
                let dropped = producer.push(SecondaryChunk {
                    samples: chunk.data,
                    frames: chunk.frames,
                    frame_index: chunk.frame_index,
                    pts_ns: chunk.pts_ns,
                    seq: chunk.seq,
                    flags: chunk.flags,
                    dropped_before: 0,
                    peak: chunk.peak,
                    rms: chunk.rms,
                });
                if let Some(total) = dropped {
                    let samples = (total - dropped_before)
                        .checked_mul(frames as u64)
                        .and_then(|frames| frames.checked_mul(u64::from(format.channels)))
                        .and_then(NonZeroU64::new);
                    let loss = AudioLoss::output_overflow(
                        OutputTap::Secondary,
                        samples,
                        format.sample_rate,
                        format.channels,
                    )?;
                    shared.push_event(Event::AudioLoss { loss });
                }
            }
        }
        if primary && flags.contains(ChunkFlags::RECOVERED) {
            shared.push_event(Event::StreamRecovered);
        }
        state.recovered = false;
        state.discontinuity = false;
        state.resume_generation = resumed;
        state.seq += 1;
        emitted = true;
    }
    Ok(emitted)
}
