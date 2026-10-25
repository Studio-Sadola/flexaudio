//! Operation failures pass through the real facade teardown and report assembly.
use super::*;
use crate::test_support::{stop_failure, StopPhase};
use std::sync::atomic::AtomicUsize;
use std::time::Instant;

#[test]
fn distinct_failure_contract() {
    flexaudio_core::test_support::assert_distinct_failures_retained(
        Error::DeviceLost,
        Error::Backend("later capture failure".into()),
        |first, second| {
            let stream = Stream::open(
                StreamConfig::default(),
                Box::new(crate::MockBackend::new(48_000, 2, 0.0)),
            )
            .unwrap();
            stream.shared.fail_terminal(first);
            stream.shared.fail_terminal(second);
            stream.terminal_error().unwrap()
        },
    );
}

crate::stop_contract::phases! {
    BothJoins,
    Stop,
    Panic,
    TerminalAndStop,
    IntakeJoin,
    WatchdogJoin,
    StopAndReplacementRollback,
    ReplacementFinalFailures,
    FinalPollPanic,
}

pub(crate) fn check_all(observe: fn(&Stream)) {
    for phase in Phase::ALL {
        check(*phase, observe);
    }
}

fn check(phase: Phase, observe: fn(&Stream)) {
    for checked_first in [false, true] {
        let stop_cause = Error::DeviceLost
            .with_context(ErrorContext::new(Operation::Stop))
            .with_context(ErrorContext::new(Operation::Stop));
        let (mut stream, primary, cleanup) = match phase {
            Phase::FinalPollPanic => {
                let stream =
                    Stream::open(StreamConfig::default(), Box::new(FinalPollPanic::default()))
                        .unwrap();
                (
                    stream,
                    None,
                    vec![Error::Backend("backend panicked during poll_event".into())
                        .with_context(ErrorContext::new(Operation::Stop))],
                )
            }
            Phase::ReplacementFinalFailures => {
                let mut stream = stop_failure(StopPhase::Stop).unwrap();
                assert!(stream
                    .switch_backend(Box::new(FailedReplacementWithNotices::default()))
                    .is_err());
                assert!(stream.terminal_error().is_none());
                (
                    stream,
                    None,
                    vec![
                        stop_cause.clone(),
                        replacement_join_error()
                            .with_context(ErrorContext::new(Operation::Rollback)),
                        Error::Unsupported.with_context(ErrorContext::new(Operation::Rollback)),
                        stop_cause.with_context(ErrorContext::new(Operation::Rollback)),
                    ],
                )
            }
            Phase::StopAndReplacementRollback => {
                let mut stream = stop_failure(StopPhase::Stop).unwrap();
                assert!(stream.switch_backend(Box::new(FailedReplacement)).is_err());
                (
                    stream,
                    None,
                    vec![
                        stop_cause.clone(),
                        stop_cause.with_context(ErrorContext::new(Operation::Rollback)),
                    ],
                )
            }
            Phase::Stop => (
                stop_failure(StopPhase::Stop).unwrap(),
                None,
                vec![stop_cause.clone()],
            ),
            Phase::Panic => (
                stop_failure(StopPhase::Panic).unwrap(),
                None,
                vec![Error::Backend("backend panicked during stop".into())
                    .with_context(ErrorContext::new(Operation::Stop))],
            ),
            Phase::TerminalAndStop => (
                stop_failure(StopPhase::TerminalAndStop).unwrap(),
                Some(Error::Unsupported),
                vec![stop_cause],
            ),
            Phase::IntakeJoin | Phase::WatchdogJoin | Phase::BothJoins => {
                let mut stream = Stream::open(
                    StreamConfig::default(),
                    Box::new(crate::MockBackend::new(48_000, 2, 0.0)),
                )
                .unwrap();
                let mut cleanup = Vec::new();
                if matches!(phase, Phase::IntakeJoin | Phase::BothJoins) {
                    stream.worker = Some(thread::spawn(|| panic!("injected intake failure")));
                    cleanup.push(
                        Error::Backend("capture intake worker panicked".into())
                            .with_context(ErrorContext::new(Operation::Join)),
                    );
                }
                if matches!(phase, Phase::WatchdogJoin | Phase::BothJoins) {
                    stream.watchdog = Some(thread::spawn(|| panic!("injected watchdog failure")));
                    cleanup.push(
                        Error::Backend("capture watchdog panicked".into())
                            .with_context(ErrorContext::new(Operation::Join)),
                    );
                }
                (stream, None, cleanup)
            }
        };
        if checked_first {
            assert!(stream.stop_checked().is_err());
        } else {
            stream.stop();
        }
        let report = stream
            .shutdown_report()
            .expect("void stop retains a report");
        assert_eq!(report.cleanup().len(), cleanup.len());
        assert_eq!(report, ShutdownReport::new(primary, cleanup));
        assert_eq!(stream.stop_checked(), report.result());
        assert_eq!(stream.shutdown_report(), Some(report.clone()));
        stream.stop();
        assert_eq!(stream.shutdown_report(), Some(report.clone()));
        assert_eq!(stream.stop_checked(), report.result());
        observe(&stream);
    }
}

#[test]
fn final_poll_panic_is_retained_for_both_stop_orders() {
    check(Phase::FinalPollPanic, |_| {});
}

#[derive(Default)]
pub(crate) struct FinalPollPanic {
    stopped: bool,
    panicked: bool,
}

impl CaptureBackend for FinalPollPanic {
    fn native_format(&self) -> (u32, u16) {
        (48_000, 2)
    }
    fn start(&mut self, _: RawSink) -> Result<()> {
        Ok(())
    }
    fn stop(&mut self) {
        self.stopped = true;
    }
    fn poll_event(&mut self) -> Option<Event> {
        if self.stopped && !std::mem::replace(&mut self.panicked, true) {
            panic!("injected final poll failure");
        }
        None
    }
}

#[test]
fn rejected_replacement_join_notice_and_distinct_stop_failure_are_retained() {
    check(Phase::ReplacementFinalFailures, |_| {});
}

struct StartFailureSequence {
    starts: Arc<AtomicUsize>,
    failures: std::ops::RangeInclusive<usize>,
}

impl CaptureBackend for StartFailureSequence {
    fn native_format(&self) -> (u32, u16) {
        (48_000, 2)
    }
    fn start(&mut self, _: RawSink) -> Result<()> {
        let attempt = self.starts.fetch_add(1, Ordering::SeqCst) + 1;
        if self.failures.contains(&attempt) {
            Err(Error::DeviceNotFound)
        } else {
            Ok(())
        }
    }
    fn stop(&mut self) {}
}

fn mix_start_failure_sequence(
    starts: Arc<AtomicUsize>,
    failures: std::ops::RangeInclusive<usize>,
) -> Box<dyn CaptureBackend> {
    Box::new(crate::mix::CompositeBackend::new(
        Box::new(StartFailureSequence { starts, failures }),
        Box::new(StartFailureSequence {
            starts: Arc::new(AtomicUsize::new(0)),
            failures: 0..=0,
        }),
        1.0,
        1.0,
    ))
}

#[test]
fn failed_initial_start_is_returned_once_without_shutdown_failure() {
    let mut stream = Stream::open(
        StreamConfig::default(),
        mix_start_failure_sequence(Arc::new(AtomicUsize::new(0)), 1..=1),
    )
    .unwrap();
    assert_eq!(
        stream.start(),
        Err(Error::DeviceNotFound.with_context(
            ErrorContext::new(Operation::Start).with_lane(flexaudio_core::MixLane::Microphone),
        ))
    );
    assert!(stream.stop_checked().is_ok());
    assert!(stream.shutdown_report().unwrap().cleanup().is_empty());
    assert!(stream.terminal_error().is_none());
}

#[test]
fn failed_replacement_start_restores_capture_without_rollback_failure() {
    let mut stream = Stream::open(
        StreamConfig::default(),
        Box::new(crate::MockBackend::new(48_000, 2, 0.0)),
    )
    .unwrap();
    stream.start().unwrap();
    assert_eq!(
        stream
            .switch_backend(mix_start_failure_sequence(
                Arc::new(AtomicUsize::new(0)),
                1..=1,
            ))
            .unwrap_err()
            .root(),
        &Error::DeviceNotFound
    );
    assert!(stream.terminal_error().is_none());
    assert!(stream.stop_checked().is_ok());
    assert!(stream.shutdown_report().unwrap().cleanup().is_empty());
    while let Some(event) = stream.poll_event() {
        assert!(!matches!(event, Event::TerminalError { .. }));
    }
}

#[test]
fn watchdog_retries_failed_starts_without_latching_or_polluting_shutdown() {
    let starts = Arc::new(AtomicUsize::new(0));
    check_watchdog_start_failures(mix_start_failure_sequence(starts.clone(), 2..=3), starts);
}

#[cfg(target_os = "linux")]
#[test]
fn watchdog_retries_linux_readiness_failures_without_terminal_notice() {
    let starts = Arc::new(AtomicUsize::new(0));
    check_watchdog_start_failures(
        Box::new(flexaudio_os_linux::test_support::capture_start_failures(
            starts.clone(),
            2..=3,
        )),
        starts,
    );
}

fn check_watchdog_start_failures(backend: Box<dyn CaptureBackend>, starts: Arc<AtomicUsize>) {
    let mut stream = Stream::open(StreamConfig::default(), backend).unwrap();
    stream.start().unwrap();
    let idle = i64::try_from(STALL_THRESHOLD.as_nanos()).unwrap();
    stream
        .shared
        .last_sample_ns
        .store(monotonic_now_ns() - idle, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(5);
    while starts.load(Ordering::SeqCst) < 4 {
        assert!(
            stream.terminal_error().is_none(),
            "failed reopen latched terminal state"
        );
        assert!(
            Instant::now() < deadline,
            "watchdog did not retry failed starts"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert!(stream.stop_checked().is_ok());
    assert!(stream.shutdown_report().unwrap().cleanup().is_empty());
    let events = std::iter::from_fn(|| stream.poll_event()).collect::<Vec<_>>();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event,
                Event::RecoverableError { error } if error.root() == &Error::DeviceNotFound
            ))
            .count(),
        2
    );
    assert!(!events
        .iter()
        .any(|event| matches!(event, Event::TerminalError { .. })));
}

fn replacement_join_error() -> Error {
    Error::Backend("replacement worker panicked during join".into())
        .with_context(ErrorContext::new(Operation::Join))
}

#[derive(Default)]
struct FailedReplacementWithNotices {
    worker: Option<thread::JoinHandle<()>>,
    events: std::collections::VecDeque<Event>,
}

impl CaptureBackend for FailedReplacementWithNotices {
    fn native_format(&self) -> (u32, u16) {
        (48_000, 2)
    }

    fn start(&mut self, _sink: RawSink) -> Result<()> {
        self.worker = Some(thread::spawn(|| {
            panic!("injected replacement join failure")
        }));
        Err(Error::DeviceNotFound)
    }

    fn stop(&mut self) {
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                self.events.push_back(Event::ShutdownError {
                    error: replacement_join_error(),
                });
                self.events.push_back(Event::TerminalError {
                    error: Error::Unsupported,
                });
            }
        }
    }

    fn stop_checked(&mut self) -> Result<()> {
        self.stop();
        Err(Error::DeviceLost.with_context(ErrorContext::new(Operation::Stop)))
    }

    fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }
}

struct FailedReplacement;

impl CaptureBackend for FailedReplacement {
    fn native_format(&self) -> (u32, u16) {
        (48_000, 2)
    }

    fn start(&mut self, _sink: RawSink) -> Result<()> {
        Err(Error::DeviceNotFound)
    }

    fn stop(&mut self) {}

    fn stop_checked(&mut self) -> Result<()> {
        Err(Error::DeviceLost.with_context(ErrorContext::new(Operation::Stop)))
    }

    fn poll_event(&mut self) -> Option<Event> {
        None
    }
}
