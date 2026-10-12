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
    let formats = (output, secondary_output);
    let initial = shared.snapshot_raw(&mut []);
    let mut input = match intake_generation::IntakeGeneration::new(&shared, &initial, formats, 0) {
        Ok(input) => input,
        Err(error) => {
            shared.fail_terminal(error.with_context(ErrorContext::new(Operation::Normalize)));
            return;
        }
    };
    let mut primary_state = tap_drain::TapState::default();
    let mut secondary_state = tap_drain::TapState::default();
    let mut capture_state = tap_drain::TapState {
        discontinuity: shared.primary_frame_index.load(Ordering::SeqCst) != 0,
        ..Default::default()
    };
    let mut capture_producer = shared
        .capture_producer
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    let mut scratch = vec![0.0f32; RAW_RING_SAMPLES];
    'intake: loop {
        if shared.terminal.is_failed() {
            break;
        }
        let stopping = shared.stopping.load(Ordering::SeqCst);
        let snapshot = shared.snapshot_raw(&mut scratch);
        if snapshot.generation != input.number {
            // Close the OLD guard, format and DSP before adopting the replacement.
            let old_recovered = (primary_state.recovered, secondary_state.recovered);
            primary_state.recovered = false;
            secondary_state.recovered = false;
            let closed = match input.finish(&shared, intake_generation::GenerationEnd::Replaced) {
                Ok(progress) => progress,
                Err(error) => {
                    shared
                        .fail_terminal(error.with_context(ErrorContext::new(Operation::Normalize)));
                    break 'intake;
                }
            };
            if closed.discontinuity {
                capture_state.discontinuity = true;
                primary_state.discontinuity = true;
                secondary_state.discontinuity = true;
            }
            if closed.pushed {
                primary_state.discontinuity |= old_recovered.0;
                secondary_state.discontinuity |= old_recovered.1;
                if let Err(error) = drain_outputs(
                    &shared,
                    &mut input.normalizer,
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
                    formats,
                    f32::from_bits(shared.gain_bits.load(Ordering::Relaxed)),
                    true,
                ) {
                    shared
                        .fail_terminal(error.with_context(ErrorContext::new(Operation::Normalize)));
                    break 'intake;
                }
            }
            input = match intake_generation::IntakeGeneration::new(
                &shared,
                &snapshot,
                formats,
                input.minimum_pts,
            ) {
                Ok(input) => input,
                Err(error) => {
                    shared
                        .fail_terminal(error.with_context(ErrorContext::new(Operation::Normalize)));
                    break 'intake;
                }
            };
            capture_state.discontinuity = true;
        }
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
        let overflowed = snapshot.overflows > input.overflows;
        let losses = match snapshot.losses {
            Ok(losses) => losses,
            Err(error) => {
                shared.fail_terminal(error);
                break 'intake;
            }
        };
        let observed_loss = !losses.is_empty();
        if observed_loss {
            for loss in losses {
                shared.push_event(Event::AudioLoss { loss });
            }
        }
        if overflowed || observed_loss || snapshot.startup_drop.is_some() {
            capture_state.discontinuity = true;
            primary_state.discontinuity = true;
            secondary_state.discontinuity = true;
        }
        if shared.capture_enabled.load(Ordering::SeqCst) && (overflowed || observed_loss) {
            if let Err(error) = input.rebuild_after_loss(&shared, snapshot.denoise_enabled, formats)
            {
                shared.fail_terminal(error.with_context(ErrorContext::new(Operation::Normalize)));
                break 'intake;
            }
        }
        if let Some((frames, rate)) = snapshot.startup_drop {
            if let Err(error) = input.external_drop(&shared, frames, rate) {
                shared.fail_terminal(error);
                break 'intake;
            }
        }
        if let Err(error) = input.check_deadline(&shared) {
            shared.fail_terminal(error.with_context(ErrorContext::new(Operation::Normalize)));
            break 'intake;
        }
        let mut produced_any = false;
        if snapshot.samples > 0 {
            // Suppressed raw input still establishes producer health for the watchdog.
            shared
                .last_sample_ns
                .store(monotonic_now_ns(), Ordering::SeqCst);
            match input.accept(
                &shared,
                &scratch[..snapshot.samples],
                overflowed || observed_loss,
            ) {
                Ok(progress) => {
                    produced_any = progress.pushed;
                    if progress.discontinuity {
                        capture_state.discontinuity = true;
                        primary_state.discontinuity = true;
                        secondary_state.discontinuity = true;
                    }
                }
                Err(error) => {
                    shared
                        .fail_terminal(error.with_context(ErrorContext::new(Operation::Normalize)));
                    break 'intake;
                }
            }
        }
        input.overflows = snapshot.overflows;
        if stopping {
            match input.finish(&shared, intake_generation::GenerationEnd::Stopped) {
                Ok(progress) if progress.discontinuity => {
                    capture_state.discontinuity = true;
                    primary_state.discontinuity = true;
                    secondary_state.discontinuity = true;
                }
                Ok(_) => {}
                Err(error) => {
                    shared.record_cleanup(error.with_context(ErrorContext::new(Operation::Flush)))
                }
            }
        }
        let emitted = match drain_outputs(
            &shared,
            &mut input.normalizer,
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
            formats,
            f32::from_bits(shared.gain_bits.load(Ordering::Relaxed)),
            stopping,
        ) {
            Ok(emitted) => emitted,
            Err(error) => {
                shared.fail_terminal(error.with_context(ErrorContext::new(Operation::Normalize)));
                break 'intake;
            }
        };
        if stopping {
            break;
        }
        if !produced_any && !emitted {
            thread::sleep(Duration::from_millis(2));
        }
    }
    if let Err(error) = input.reject_pending(&shared) {
        shared.record_cleanup(error);
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
