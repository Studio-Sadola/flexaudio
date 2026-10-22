//! Operation failures pass through the real facade teardown and report assembly.
use super::*;
use crate::test_support::{stop_failure, StopPhase};

crate::stop_contract::phases! {
    BothJoins,
    Stop,
    Panic,
    TerminalAndStop,
    IntakeJoin,
    WatchdogJoin,
    StopAndReplacementRollback,
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
