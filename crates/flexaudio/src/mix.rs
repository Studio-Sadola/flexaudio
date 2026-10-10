//! Composite microphone/system capture, with checked two-lane ownership.
//! Child realtime callbacks only write their raw rings. The non-realtime mixer
//! normalizes each lane, corrects system clock drift and reports upstream loss
//! and coalesced clipping independently of output chunk metadata.
use std::num::NonZeroU64;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::stream::RAW_RING_SAMPLES;
use flexaudio_core::backend::{CaptureBackend, RawSink};
use flexaudio_core::clock::monotonic_now_ns;
use flexaudio_core::normalizer::Normalizer;
use flexaudio_core::raw_ring::{raw_ring, RawConsumer};
use flexaudio_core::types::{
    AudioLoss, AudioPath, Error, ErrorContext, ErrorKind, Event, LossReason, MixLane, Operation,
    OutputFormat, Result, ShutdownReport, CHANNELS, SAMPLE_RATE,
};
use flexaudio_core::CaptureDiagnostics;

mod child;
mod drift;
mod notices;
mod worker;
use child::*;
use drift::*;
use notices::*;
use worker::*;

#[cfg(test)]
mod contract_tests;
#[cfg(test)]
mod permission_tests;
#[cfg(test)]
mod repro_tests;
#[cfg(test)]
mod tests;

const STARVATION_FILL_THRESHOLD: Duration = Duration::from_millis(60);
const FIFO_MAX_SAMPLES: usize = 48_000;
const IDLE_SLEEP: Duration = Duration::from_millis(2);
const DRIFT_RATIO_LIMIT: f64 = 500e-6;
const DRIFT_UPDATE_INTERVAL_SAMPLES: usize = (SAMPLE_RATE as usize / 10) * CHANNELS as usize;
const DRIFT_EMA_ALPHA: f64 = 0.1;
const DRIFT_GAIN: f64 = DRIFT_RATIO_LIMIT / 19_200.0;
const DRIFT_SLEW_PER_UPDATE: f64 = 20e-6;
const FINAL_EVENT_BUDGET: usize = 512;

pub(crate) struct CompositeBackend {
    mic: Box<dyn CaptureBackend>,
    system: Box<dyn CaptureBackend>,
    mic_gain: f32,
    system_gain: f32,
    /// Set only after both producers and their final mailboxes are quiescent.
    stopping: Arc<AtomicBool>,
    mixer: Option<JoinHandle<()>>,
    notices: Arc<Notices>,
    mic_owned: bool,
    system_owned: bool,
    shutdown: Option<ShutdownReport>,
    poll_system_first: bool,
    poll_notices: bool,
    mic_granted: bool,
}

impl CompositeBackend {
    pub(crate) fn new(
        mic: Box<dyn CaptureBackend>,
        system: Box<dyn CaptureBackend>,
        mic_gain: f32,
        system_gain: f32,
    ) -> Self {
        Self {
            mic,
            system,
            mic_gain,
            system_gain,
            stopping: Arc::new(AtomicBool::new(false)),
            mixer: None,
            notices: Arc::new(Notices::default()),
            mic_owned: false,
            system_owned: false,
            shutdown: None,
            poll_system_first: false,
            poll_notices: false,
            mic_granted: false,
        }
    }

    fn child_event(&mut self, lane: MixLane, event: Event) -> Option<Event> {
        let error = match event {
            Event::TerminalError { error } => Some(error),
            Event::Error(_) => Some(Error::Backend("legacy mix child failure".into())),
            Event::PermissionDenied { permission, detail } => {
                Some(Error::PermissionDenied { permission, detail })
            }
            Event::RecoverableError { ref error }
                if matches!(
                    error.kind(),
                    ErrorKind::PermissionDenied | ErrorKind::NativeFormatChanged
                ) =>
            {
                Some(error.clone())
            }
            Event::PermissionGranted => {
                if lane != MixLane::Microphone || self.mic_granted || self.notices.failed() {
                    return None;
                }
                self.mic_granted = true;
                return Some(Event::PermissionGranted);
            }
            Event::AudioLoss { loss } => {
                let loss = if matches!(loss.path(), AudioPath::Capture { lane: None }) {
                    match loss.with_capture_lane(lane) {
                        Ok(loss) => loss,
                        Err(error) => {
                            return self.child_event(lane, Event::TerminalError { error })
                        }
                    }
                } else {
                    loss
                };
                return Some(Event::AudioLoss { loss });
            }
            Event::ShutdownError { error } => {
                self.notices
                    .cleanup(lane_error(error, Operation::Stop, lane));
                return None;
            }
            event => return Some(event),
        };
        if let Some(error) = error {
            // Return before joining producers: the facade must close delivery first.
            // The latch also suppresses mixer PCM while the facade begins teardown.
            self.notices
                .fail(lane_error(error, Operation::Normalize, lane));
        }
        self.notices.take_terminal()
    }

    fn poll_child(&mut self, lane: MixLane) -> Option<Event> {
        let event = match lane {
            MixLane::Microphone => self.mic.poll_event(),
            MixLane::SystemAudio => self.system.poll_event(),
            _ => unreachable!("only built-in Mix lanes are constructed"),
        }?;
        self.child_event(lane, event)
    }

    fn stop_producers(&mut self) {
        if std::mem::take(&mut self.mic_owned) {
            if let Err(error) = stop_child(&mut self.mic, MixLane::Microphone) {
                self.notices.cleanup(error);
            }
        }
        if std::mem::take(&mut self.system_owned) {
            if let Err(error) = stop_child(&mut self.system, MixLane::SystemAudio) {
                self.notices.cleanup(error);
            }
        }
    }

    fn drain_child_final(&mut self, lane: MixLane) {
        for _ in 0..FINAL_EVENT_BUDGET {
            let event = std::panic::catch_unwind(AssertUnwindSafe(|| match lane {
                MixLane::Microphone => self.mic.poll_event(),
                MixLane::SystemAudio => self.system.poll_event(),
                _ => unreachable!("only built-in Mix lanes are constructed"),
            }));
            match event {
                Ok(Some(event)) => {
                    if let Some(event) = self.child_event(lane, event) {
                        self.notices.queue_final(event);
                    }
                }
                Ok(None) => return,
                Err(_) => {
                    self.notices.fail(lane_error(
                        Error::Backend("mix child panicked during final event drain".into()),
                        Operation::Stop,
                        lane,
                    ));
                    return;
                }
            }
        }
        self.notices.fail(lane_error(
            Error::Backend("mix child final event mailbox could not be reconciled".into()),
            Operation::Stop,
            lane,
        ));
    }
}

impl CaptureBackend for CompositeBackend {
    fn native_format(&self) -> (u32, u16) {
        (SAMPLE_RATE, CHANNELS)
    }

    fn start(&mut self, sink: RawSink) -> Result<()> {
        if self.mixer.is_some() {
            return Ok(());
        }
        let previous_report = self.shutdown.take();
        let previous_notices = std::mem::replace(&mut self.notices, Arc::new(Notices::default()));
        let previous_granted = std::mem::replace(&mut self.mic_granted, false);
        self.stopping = Arc::new(AtomicBool::new(false));
        let started = (|| {
            let mut mic = start_child(&mut self.mic, MixLane::Microphone, self.notices.clone())?;
            self.mic_owned = true;
            let system =
                match start_child(&mut self.system, MixLane::SystemAudio, self.notices.clone()) {
                    Ok(system) => system,
                    Err(error) => {
                        self.notices.retain_primary(error.clone());
                        self.stop_producers();
                        if let Err(error) = mic.losses() {
                            self.notices.cleanup(error);
                        }
                        return Err(error);
                    }
                };
            self.system_owned = true;
            let stopping = self.stopping.clone();
            let notices = self.notices.clone();
            let mic_gain = self.mic_gain;
            let system_gain = self.system_gain;
            // Retain consumers outside the spawn closure until the worker owns
            // them, so failed spawn rollback can still drain final diagnostics.
            let lanes = Arc::new(Mutex::new(Some((mic, system))));
            let worker_lanes = lanes.clone();
            let mixer = match thread::Builder::new()
                .name("flexaudio-mix".into())
                .spawn(move || {
                    let (mic, system) = worker_lanes
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take()
                        .expect("mixer lanes have one owner");
                    run_mixer(mic, system, mic_gain, system_gain, sink, stopping, notices)
                }) {
                Ok(mixer) => mixer,
                Err(error) => {
                    let error = Error::Backend(format!("spawn mix thread: {error}"))
                        .with_context(ErrorContext::new(Operation::Start));
                    self.notices.retain_primary(error.clone());
                    self.stop_producers();
                    if let Some((mut mic, mut system)) =
                        lanes.lock().unwrap_or_else(|e| e.into_inner()).take()
                    {
                        for lane in [&mut mic, &mut system] {
                            if let Err(error) = lane.losses() {
                                self.notices.cleanup(error);
                            }
                        }
                    }
                    return Err(error);
                }
            };
            self.mixer = Some(mixer);
            Ok(())
        })();
        if let Err(error) = started {
            self.notices.retain_primary(error);
            let result = self.stop_checked();
            if previous_report.is_some() {
                self.shutdown = previous_report;
                self.notices = previous_notices;
                self.mic_granted = previous_granted;
            }
            return result;
        }
        Ok(())
    }

    fn stop(&mut self) {
        let _ = self.stop_checked();
    }

    fn stop_checked(&mut self) -> Result<()> {
        if let Some(report) = &self.shutdown {
            return report.result();
        }
        self.stop_producers();
        // Keep the mixer alive through producer stops; final denial closes its
        // publication gate before the graceful raw/normalizer/FIFO drain begins.
        self.drain_child_final(MixLane::Microphone);
        self.drain_child_final(MixLane::SystemAudio);
        self.stopping.store(true, Ordering::SeqCst);
        if let Some(handle) = self.mixer.take() {
            if handle.join().is_err() {
                self.notices.fail(
                    Error::Backend("mix worker panicked".into())
                        .with_context(ErrorContext::new(Operation::Join)),
                );
            }
        }
        let report = self.notices.report();
        let result = report.result();
        self.shutdown = Some(report);
        result
    }

    fn poll_event(&mut self) -> Option<Event> {
        if let Some(event) = self.notices.take_terminal() {
            return Some(event);
        }
        self.poll_notices = !self.poll_notices;
        if self.poll_notices {
            if let Some(event) = self.notices.poll() {
                return Some(event);
            }
        }
        self.poll_system_first = !self.poll_system_first;
        let (first, second) = if self.poll_system_first {
            (MixLane::SystemAudio, MixLane::Microphone)
        } else {
            (MixLane::Microphone, MixLane::SystemAudio)
        };
        self.poll_child(first)
            .or_else(|| self.poll_child(second))
            .or_else(|| self.notices.poll())
    }
}

impl Drop for CompositeBackend {
    fn drop(&mut self) {
        self.stop();
    }
}

fn lane_error(error: Error, operation: Operation, lane: MixLane) -> Error {
    error.with_context(ErrorContext::new(operation).with_lane(lane))
}
