//! Non-realtime normalization, mixing and graceful final drain.
use super::*;
pub(super) fn mix_and_push(
    mic: &mut ChildLane,
    system: &mut ChildLane,
    mic_gain: f32,
    system_gain: f32,
    drift: &mut DriftCorrection,
    sink: &mut RawSink,
    mixed: &mut Vec<f32>,
) -> bool {
    let ch = CHANNELS as usize;
    let ratio = drift.controller.ratio;
    let steady_frames =
        (mic.fifo.len() / ch).min(drift.stitcher.producible(system.fifo.len() / ch, ratio));
    if steady_frames > 0 {
        // Steady-state path: read the system side at ratio r with linear interpolation,
        // then mix the sides.
        mixed.clear();
        drift
            .stitcher
            .pull(&mut system.fifo, ratio, steady_frames, mixed);
        let count = steady_frames * ch;
        let mut clipped = false;
        for (i, out) in mixed.iter_mut().enumerate() {
            let value = mic.fifo[i] * mic_gain + *out * system_gain;
            *out = clamp_sample(value, &mut clipped);
        }
        if clipped {
            mic.notices.clipped();
        }
        mic.fifo.drain(..count);
        drift
            .controller
            .on_output(count, mic.fifo.len(), system.fifo.len());
        // As in stream.rs capture, monotonic now is sufficient for pts (the sink handles
        // it separately by contract).
        mic.notices.publish(sink, mixed);
        return true;
    }

    // Starvation path (no correction; retain existing semantics).
    let now = Instant::now();
    let (mic_take, system_take) = if !mic.fifo.is_empty() && system.is_starved(now) {
        // System stopped supplying: output all mic data. Flush any remaining fraction
        // (at most one frame) that lacked a right interpolation frame, then reset phase.
        drift.stitcher.reset();
        (mic.fifo.len(), system.fifo.len())
    } else if mic.fifo.is_empty() && !system.fifo.is_empty() && mic.is_starved(now) {
        drift.stitcher.reset();
        (0, system.fifo.len())
    } else {
        return false;
    };

    let count = mic_take.max(system_take);
    mixed.clear();
    let mut clipped = false;
    for i in 0..count {
        let m = if i < mic_take { mic.fifo[i] } else { 0.0 };
        let s = if i < system_take { system.fifo[i] } else { 0.0 };
        mixed.push(clamp_sample(m * mic_gain + s * system_gain, &mut clipped));
    }
    if clipped {
        mic.notices.clipped();
    }
    mic.fifo.drain(..mic_take);
    system.fifo.drain(..system_take);

    // As in stream.rs capture, monotonic now is sufficient for pts (the sink handles it
    // separately by contract).
    mic.notices.publish(sink, mixed);
    true
}

fn clamp_sample(value: f32, clipped: &mut bool) -> f32 {
    let clamped = value.clamp(-1.0, 1.0);
    if clamped != value {
        *clipped = true;
    }
    clamped
}

pub(super) fn run_mixer(
    mut mic: ChildLane,
    mut system: ChildLane,
    mic_gain: f32,
    system_gain: f32,
    mut sink: RawSink,
    stopping: Arc<AtomicBool>,
    notices: Arc<Notices>,
) {
    let mut scratch = vec![0.0; RAW_RING_SAMPLES];
    let mut mixed = Vec::with_capacity(FIFO_MAX_SAMPLES);
    let mut drift = DriftCorrection::new();
    let start = Instant::now();
    while !stopping.load(Ordering::SeqCst) && !notices.failed() {
        for lane in [&mut mic, &mut system] {
            if let Err(error) = lane.ingest(&mut scratch) {
                notices.fail(error);
            }
        }
        if notices.failed() {
            break;
        }
        let primed = (!mic.fifo.is_empty() && !system.fifo.is_empty())
            || start.elapsed() >= STARVATION_FILL_THRESHOLD;
        if !primed
            || !mix_and_push(
                &mut mic,
                &mut system,
                mic_gain,
                system_gain,
                &mut drift,
                &mut sink,
                &mut mixed,
            )
        {
            thread::sleep(IDLE_SLEEP);
        }
    }
    if !notices.failed() {
        for lane in [&mut mic, &mut system] {
            if let Err(error) = lane.finish(&mut scratch) {
                notices.fail(error);
            }
        }
        if !notices.failed() {
            // Producer clocks are finished: pair all remaining samples at unity
            // ratio and pad only the shorter lane. No starvation delay on stop.
            mixed.clear();
            let mut clipped = false;
            for i in 0..mic.fifo.len().max(system.fifo.len()) {
                let m = mic.fifo.get(i).copied().unwrap_or(0.0);
                let s = system.fifo.get(i).copied().unwrap_or(0.0);
                mixed.push(clamp_sample(m * mic_gain + s * system_gain, &mut clipped));
            }
            if clipped {
                notices.clipped();
            }
            if !mixed.is_empty() {
                notices.publish(&mut sink, &mixed);
            }
        }
    }
    // A failed mixer still owns its consumers until producer shutdown confirms
    // that no final loss can arrive.
    while !stopping.load(Ordering::SeqCst) {
        thread::sleep(IDLE_SLEEP);
    }
    // Terminal shutdown discards PCM but retains final callback observations.
    for lane in [&mut mic, &mut system] {
        if let Err(error) = lane.losses() {
            notices.cleanup(error);
        }
    }
}
