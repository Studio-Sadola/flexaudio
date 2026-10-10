//! Raw intake, normalization and off-callback diagnostics.
use super::*;
/// Intake/processing thread body.
///
/// Pop from RawConsumer → feed [`Normalizer`] (shared Stage1 → split to primary/secondary Stage2)
/// → add `seq`, recording-zero-based PTS, peak/rms, and discontinuity flags to completed chunks →
/// push to the primary/secondary rings. On a generation change (reopen/source switch), rebuild the
/// Normalizer/Clock and set RECOVERED|DISCONTINUITY on the next chunk. Detect RawRing overflow
/// (capture-side data loss) too, and set DISCONTINUITY on the next primary and secondary chunks.
/// Flush the Normalizer on stop to emit its final tail.
pub(super) fn run_intake(
    shared: Arc<SharedState>,
    chunk_producer: ChunkProducer,
    secondary_producer: Option<SecondaryChunkProducer>,
    initial_native: (u32, u16),
    output: OutputFormat,
    secondary_output: Option<OutputFormat>,
) {
    run_intake_inner(
        shared.clone(),
        chunk_producer,
        secondary_producer,
        initial_native,
        output,
        secondary_output,
    );
    if shared.terminal.is_failed() {
        let mut backend = shared.backend.lock().unwrap_or_else(|e| e.into_inner());
        stop_backend_reconciling(&shared, &mut backend, true);
    }
}

fn run_intake_inner(
    shared: Arc<SharedState>,
    mut chunk_producer: ChunkProducer,
    mut secondary_producer: Option<SecondaryChunkProducer>,
    _initial_native: (u32, u16),
    output: OutputFormat,
    secondary_output: Option<OutputFormat>,
) {
    // Startup arguments can become stale before this thread is scheduled.
    let initial = shared.snapshot_raw(&mut []);
    let (rate, channels) = initial.native_format;
    // If Normalizer construction fails (for example, rubato setup), latch TerminalError and exit rather
    // than dying silently.
    let mut normalizer = match build_normalizer(
        &shared,
        rate,
        channels,
        output,
        secondary_output,
        initial.denoise_enabled,
    ) {
        Ok(n) => n,
        Err(e) => {
            shared.fail_terminal(e.with_context(ErrorContext::new(Operation::Normalize)));
            return;
        }
    };
    let mut clock = ClockNormalizer::new();
    let mut primary_state = tap_drain::TapState::default();
    let mut secondary_state = tap_drain::TapState::default();
    let mut current_generation = initial.generation;
    let mut capture_state = tap_drain::TapState {
        discontinuity: shared.primary_frame_index.load(Ordering::SeqCst) != 0,
        ..Default::default()
    };
    let mut capture_producer = shared
        .capture_producer
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    // Each tap retains flags and resume state until its own first delivery.
    let mut overflow_baseline: u64 = 0;

    // Pop scratch buffer (sized to the RawRing capacity so the whole ring can be drained in one call).
    let mut scratch = vec![0.0f32; RAW_RING_SAMPLES];

    'intake: loop {
        if shared.terminal.is_failed() {
            break;
        }
        let stopping = shared.stopping.load(Ordering::SeqCst);

        // Detect a generation change (reopen/source switch) and reset for the new source. Since
        // native_format may change, reread shared state and rebuild the native-dependent Stage 1
        // Normalizer. Do not reset the recording epoch here; timestamps stay continuous from zero
        // across generations.
        let snapshot = shared.snapshot_raw(&mut scratch);
        let gen = snapshot.generation;
        if gen != current_generation {
            if let Err(error) = advance_frame_index(
                &shared.capture_frame_index,
                normalizer.buffered_capture_frames() as u64,
                48_000,
            ) {
                shared.fail_terminal(error.with_context(ErrorContext::new(Operation::Normalize)));
                break 'intake;
            }
            current_generation = gen;
            capture_state.discontinuity = true;
            // Pending delivery flags belong to the discarded normalizer, never its replacement.
            primary_state.recovered = false;
            secondary_state.recovered = false;
            let (rate, channels) = snapshot.native_format;
            normalizer = match build_normalizer(
                &shared,
                rate,
                channels,
                output,
                secondary_output,
                snapshot.denoise_enabled,
            ) {
                Ok(n) => n,
                Err(e) => {
                    shared.fail_terminal(e.with_context(ErrorContext::new(Operation::Normalize)));
                    return;
                }
            };
            clock = ClockNormalizer::new();
            // The new RawConsumer counts overflows from 0 (avoid a falsely huge delta).
            overflow_baseline = 0;
        }

        // Fan out shared pending flags to each tap's local state. The watchdog is stopped by
        // switching during a source switch, so both flags should not be set together; if they are,
        // they are combined with OR.
        if snapshot.recovered || snapshot.discontinuity {
            capture_state.discontinuity = true;
        }
        if snapshot.recovered {
            primary_state.recovered = true;
            secondary_state.recovered = true;
        }
        if snapshot.discontinuity {
            primary_state.discontinuity = true;
            secondary_state.discontinuity = true;
        }

        // Drain RawRing into the Normalizer and observe overflows (capture-side data loss).
        let mut produced_any = false;
        let mut push_err: Option<Error> = None;
        let overflow_now = snapshot.overflows;
        let losses = match snapshot.losses {
            Ok(losses) => losses,
            Err(error) => {
                shared.fail_terminal(error);
                break 'intake;
            }
        };
        let observed_loss = !losses.is_empty();
        if observed_loss {
            capture_state.discontinuity = true;
            primary_state.discontinuity = true;
            secondary_state.discontinuity = true;
            for loss in losses {
                shared.push_event(Event::AudioLoss { loss });
            }
        }
        if shared.capture_enabled.load(Ordering::SeqCst)
            && (overflow_now > overflow_baseline || observed_loss)
        {
            if let Err(error) = advance_frame_index(
                &shared.capture_frame_index,
                normalizer.buffered_capture_frames() as u64,
                48_000,
            ) {
                shared.fail_terminal(error.with_context(ErrorContext::new(Operation::Normalize)));
                break 'intake;
            }
            // No DSP history or partial chunk may bridge capture-side loss. Rebuild
            // the normalized representation and reanchor its PTS, keeping stream counters.
            let (rate, channels) = snapshot.native_format;
            normalizer = match build_normalizer(
                &shared,
                rate,
                channels,
                output,
                secondary_output,
                snapshot.denoise_enabled,
            ) {
                Ok(normalizer) => normalizer,
                Err(error) => {
                    shared
                        .fail_terminal(error.with_context(ErrorContext::new(Operation::Normalize)));
                    break 'intake;
                }
            };
            clock = ClockNormalizer::new();
        }
        if snapshot.samples > 0 {
            let samples = &scratch[..snapshot.samples];
            // Device PTS: monotonic approximation based on the native sample rate (arrival time).
            let device_pts = monotonic_now_ns();
            let norm_pts = clock.normalize(device_pts);
            if let Err(e) = normalizer.push(samples, norm_pts) {
                push_err = Some(e);
            } else {
                shared
                    .last_sample_ns
                    .store(monotonic_now_ns(), Ordering::SeqCst);
                produced_any = true;
            }
        }

        // If push failed, latch TerminalError and exit intake (do not die silently).
        if let Some(e) = push_err {
            shared.fail_terminal(e.with_context(ErrorContext::new(Operation::Normalize)));
            return;
        }

        // On RawRing overflow (RT outran intake and discarded samples), set DISCONTINUITY on the
        // next primary and secondary chunks (loss before normalization affects both taps equally).
        // The PTS already reflects the gap through wall-clock re-anchoring.
        if overflow_now > overflow_baseline {
            capture_state.discontinuity = true;
            primary_state.discontinuity = true;
            secondary_state.discontinuity = true;
        }
        overflow_baseline = overflow_now;

        // On stop, flush and emit the tail (denoise delay line + resampler remainder).
        if stopping {
            if let Err(error) = normalizer.flush() {
                shared.record_cleanup(error);
            }
        }

        // Output gain stays after conversion. The capture copy gets the same snapshot
        // independently, before WhisperVadTap's mono/16 kHz conversion.
        let gain = f32::from_bits(shared.gain_bits.load(Ordering::Relaxed));
        let drained = drain_outputs(
            &shared,
            &mut normalizer,
            OutputRings {
                capture: capture_producer.as_mut(),
                primary: &mut chunk_producer,
                secondary: secondary_producer.as_mut(),
            },
            OutputStates {
                capture: &mut capture_state,
                primary: &mut primary_state,
                secondary: &mut secondary_state,
            },
            (output, secondary_output),
            gain,
            stopping,
        );
        let emitted_any = match drained {
            Ok(emitted) => emitted,
            Err(error) => {
                shared.fail_terminal(error.with_context(ErrorContext::new(Operation::Normalize)));
                break 'intake;
            }
        };

        // If stop was requested, the tail has been flushed; exit.
        if stopping {
            break;
        }

        // Sleep briefly when there is no data to avoid busy-spinning the CPU.
        if !produced_any && !emitted_any {
            thread::sleep(Duration::from_millis(2));
        }
    }
    *shared
        .capture_producer
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = capture_producer;
}

struct OutputRings<'a> {
    capture: Option<&'a mut ChunkProducer>,
    primary: &'a mut ChunkProducer,
    secondary: Option<&'a mut SecondaryChunkProducer>,
}
struct OutputStates<'a> {
    capture: &'a mut tap_drain::TapState,
    primary: &'a mut tap_drain::TapState,
    secondary: &'a mut tap_drain::TapState,
}
/// Drain all normalized outputs through the same publication path.
fn drain_outputs(
    shared: &SharedState,
    normalizer: &mut Normalizer,
    mut rings: OutputRings<'_>,
    states: OutputStates<'_>,
    formats: (OutputFormat, Option<OutputFormat>),
    gain: f32,
    stopping: bool,
) -> Result<bool> {
    let (output, secondary_output) = formats;
    if let Some(producer) = rings.capture.as_deref_mut() {
        tap_drain::drain(
            shared,
            || normalizer.pop_capture_with_metadata(stopping),
            OutputFormat::default(),
            gain,
            &shared.capture_frame_index,
            states.capture,
            tap_drain::Ring::Capture(producer),
        )?;
    }
    let mut emitted = tap_drain::drain(
        shared,
        || normalizer.pop_chunk_with_metadata(),
        output,
        gain,
        &shared.primary_frame_index,
        states.primary,
        tap_drain::Ring::Primary(rings.primary),
    )?;
    if let (Some(producer), Some(format)) = (rings.secondary.as_deref_mut(), secondary_output) {
        emitted |= tap_drain::drain(
            shared,
            || normalizer.pop_secondary_with_metadata(),
            format,
            gain,
            &shared.secondary_frame_index,
            states.secondary,
            tap_drain::Ring::Secondary(producer),
        )?;
    }
    Ok(emitted)
}
