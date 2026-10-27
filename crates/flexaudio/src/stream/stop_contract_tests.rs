//! Operation failures pass through the real facade teardown and report assembly.
use super::*;
use crate::test_support::{stop_failure, StopPhase};
use std::sync::atomic::AtomicUsize;
use std::time::Instant;

#[test]
fn drop_routes_new_failures_and_suppresses_explicit_stop_duplicates() {
    const CHILD: &str = "FLEXAUDIO_DROP_RECEIVER_TEST";
    if let Some(mode) = std::env::var_os(CHILD) {
        let received = Arc::new(Mutex::new(Vec::new()));
        let output = received.clone();
        match mode.to_str().unwrap() {
            "handler" => crate::set_drop_failure_handler(Some(Box::new(move |failure| {
                output.lock().unwrap().push(failure.clone());
            }))),
            "panic" => {
                crate::set_drop_failure_handler(Some(Box::new(|_| panic!("receiver failed"))))
            }
            "default" => crate::set_drop_failure_handler(None),
            _ => panic!("unknown child mode"),
        }
        drop(stop_failure(StopPhase::Stop).unwrap());
        if mode == "handler" {
            let failures = received.lock().unwrap();
            assert_eq!(failures.len(), 1);
            assert_eq!(failures[0].owner, crate::DropFailureOwner::Stream);
            assert_eq!(failures[0].report.cleanup()[0].root(), &Error::DeviceLost);
            drop(failures);
            let mut stream = stop_failure(StopPhase::Stop).unwrap();
            assert!(stream.stop_checked().is_err());
            drop(stream);
            assert_eq!(received.lock().unwrap().len(), 1);
            #[cfg(target_os = "linux")]
            {
                drop(crate::DeviceWatcher::failing_join_fixture());
                let failures = received.lock().unwrap();
                assert_eq!(failures.len(), 2);
                assert_eq!(failures[1].owner, crate::DropFailureOwner::DeviceWatcher);
            }
            let before = received.lock().unwrap().len();
            crate::mock::drop_failure_fixtures();
            let failures = received.lock().unwrap();
            assert_eq!(failures.len(), before + 4);
            for failure in &failures[before..] {
                assert_eq!(failure.owner, crate::DropFailureOwner::MockCapture);
                assert!(failure.report.result().is_err());
            }
        }
        crate::set_drop_failure_handler(None);
        return;
    }
    for mode in ["handler", "default", "panic"] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["stream::stop_contract_tests::drop_routes_new_failures_and_suppresses_explicit_stop_duplicates", "--exact", "--nocapture"])
            .env(CHILD, mode)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "{mode}: {stderr}\n{}",
            String::from_utf8_lossy(&output.stdout)
        );
        if mode != "handler" {
            assert!(
                stderr.contains("flexaudio Stream implicit shutdown: device lost"),
                "{stderr}"
            );
        }
    }
}

#[test]
fn plain_stop_drop_reports_unread_failures_but_checked_and_report_handoffs_do_not() {
    const CHILD: &str = "FLEXAUDIO_STREAM_RECEIPT_TEST";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["stream::stop_contract_tests::plain_stop_drop_reports_unread_failures_but_checked_and_report_handoffs_do_not", "--exact"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let observed = Arc::new(Mutex::new(Vec::new()));
    let output = observed.clone();
    crate::set_drop_failure_handler(Some(Box::new(move |failure| {
        output.lock().unwrap().push(failure.clone())
    })));
    let mut stream = stop_failure(StopPhase::Stop).unwrap();
    stream.stop();
    stream.stop();
    assert!(observed.lock().unwrap().is_empty());
    drop(stream);
    assert_eq!(observed.lock().unwrap().len(), 1);
    let mut stream = stop_failure(StopPhase::Stop).unwrap();
    let first = stream.stop_checked();
    assert!(first.is_err());
    assert_eq!(stream.stop_checked(), first);
    drop(stream);
    assert_eq!(observed.lock().unwrap().len(), 1);
    let mut stream = stop_failure(StopPhase::Stop).unwrap();
    assert!(stream.shutdown_report().is_none());
    stream.stop();
    let first = stream.shutdown_report().unwrap();
    assert!(first.result().is_err());
    assert_eq!(stream.shutdown_report(), Some(first));
    drop(stream);
    assert_eq!(observed.lock().unwrap().len(), 1);
    let mut stream = stop_failure(StopPhase::Stop).unwrap();
    stream.stop();
    while stream.poll_event().is_some() {}
    drop(stream);
    assert_eq!(observed.lock().unwrap().len(), 1);
    let mut stream = stop_failure(StopPhase::TerminalAndStop).unwrap();
    stream.stop();
    assert_eq!(stream.terminal_error(), Some(Error::Unsupported));
    drop(stream);
    let failures = observed.lock().unwrap();
    assert_eq!(failures.len(), 2);
    assert!(failures[1].report.primary().is_none());
    assert_eq!(failures[1].report.cleanup()[0].root(), &Error::DeviceLost);
    drop(failures);
    let mut stream = Stream::open(
        StreamConfig::default(),
        Box::new(crate::MockBackend::new(48_000, 2, 0.0)),
    )
    .unwrap();
    let vad = Error::Vad {
        code: flexaudio_core::VadErrorCode::Inference,
    };
    stream.shared.record_cleanup(vad.clone());
    assert_eq!(stream.stop_checked(), Ok(()));
    drop(stream);
    assert_eq!(observed.lock().unwrap().len(), 3);
    assert_eq!(observed.lock().unwrap()[2].report.cleanup(), &[vad]);
    struct FailedStart;
    impl CaptureBackend for FailedStart {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 2)
        }
        fn start(&mut self, _: RawSink) -> Result<()> {
            Err(Error::Unsupported)
        }
        fn stop(&mut self) {}
        fn stop_checked(&mut self) -> Result<()> {
            Err(Error::DeviceLost)
        }
    }
    let mut stream = Stream::open(StreamConfig::default(), Box::new(FailedStart)).unwrap();
    let error = stream.start().unwrap_err();
    assert_eq!(error.root(), &Error::Unsupported);
    drop(stream);
    assert_eq!(
        observed.lock().unwrap().len(),
        3,
        "startup rollback already reached start()'s caller"
    );
    crate::set_drop_failure_handler(None);
}

#[test]
fn returned_resume_and_switch_failures_acknowledge_only_the_returned_cause() {
    const CHILD: &str = "FLEXAUDIO_STREAM_RESULT_RECEIPT_TEST";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["stream::stop_contract_tests::returned_resume_and_switch_failures_acknowledge_only_the_returned_cause", "--exact"])
            .env(CHILD, "1").output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let observed = Arc::new(Mutex::new(Vec::new()));
    let output = observed.clone();
    crate::set_drop_failure_handler(Some(Box::new(move |failure| {
        output.lock().unwrap().push(failure.clone())
    })));
    let stream = stop_failure(StopPhase::Stop).unwrap();
    stream.shared.fail_terminal(Error::Unsupported);
    assert_eq!(stream.resume(), Err(Error::Unsupported));
    drop(stream);
    let mut stream = stop_failure(StopPhase::TerminalAndStop).unwrap();
    assert_eq!(
        stream.switch_backend(Box::new(crate::MockBackend::new(48_000, 2, 0.0))),
        Err(Error::Unsupported)
    );
    drop(stream);
    crate::set_drop_failure_handler(None);
    let failures = observed.lock().unwrap();
    assert_eq!(failures.len(), 2);
    for failure in failures.iter() {
        assert!(failure.report.primary().is_none());
        assert_eq!(failure.report.cleanup().len(), 1);
        assert_eq!(failure.report.cleanup()[0].root(), &Error::DeviceLost);
    }
}

#[test]
fn successful_start_reports_only_unread_retired_shutdown_failures() {
    const CHILD: &str = "FLEXAUDIO_STREAM_RETIRED_RECEIPT_TEST";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["stream::stop_contract_tests::successful_start_reports_only_unread_retired_shutdown_failures", "--exact"])
            .env(CHILD, "1").output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    struct FirstStop(bool);
    impl CaptureBackend for FirstStop {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 2)
        }
        fn start(&mut self, _: RawSink) -> Result<()> {
            Ok(())
        }
        fn stop(&mut self) {}
        fn stop_checked(&mut self) -> Result<()> {
            if std::mem::replace(&mut self.0, false) {
                Err(Error::Unsupported)
            } else {
                Ok(())
            }
        }
    }
    let observed = Arc::new(Mutex::new(Vec::new()));
    let output = observed.clone();
    crate::set_drop_failure_handler(Some(Box::new(move |failure| {
        output.lock().unwrap().push(failure.clone())
    })));
    for checked in [false, true] {
        let mut stream = Stream::open(StreamConfig::default(), Box::new(FirstStop(true))).unwrap();
        if checked {
            assert!(stream.stop_checked().is_err());
        } else {
            stream.stop();
        }
        stream.start().unwrap();
        assert_eq!(observed.lock().unwrap().len(), 1);
        stream.stop_checked().unwrap();
        drop(stream);
        assert_eq!(observed.lock().unwrap().len(), 1);
    }
    crate::set_drop_failure_handler(None);
    assert_eq!(
        observed.lock().unwrap()[0].report.cleanup()[0].root(),
        &Error::Unsupported
    );
}

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
