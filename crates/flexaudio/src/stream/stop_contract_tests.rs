//! Operation failures pass through the real facade teardown and report assembly.
use super::*;
use crate::test_support::{stop_failure, StopPhase};

#[derive(Clone, Copy)]
pub(crate) enum Phase {
    Stop,
    Panic,
    TerminalAndStop,
    IntakeJoin,
    WatchdogJoin,
}

pub(crate) fn check(phase: Phase) {
    for checked_first in [false, true] {
        let stop_cause = Error::DeviceLost
            .with_context(ErrorContext::new(Operation::Stop))
            .with_context(ErrorContext::new(Operation::Stop));
        let (mut stream, primary, cleanup) = match phase {
            Phase::Stop => (
                stop_failure(StopPhase::Stop).unwrap(),
                None,
                stop_cause.clone(),
            ),
            Phase::Panic => (
                stop_failure(StopPhase::Panic).unwrap(),
                None,
                Error::Backend("backend panicked during stop".into())
                    .with_context(ErrorContext::new(Operation::Stop)),
            ),
            Phase::TerminalAndStop => (
                stop_failure(StopPhase::TerminalAndStop).unwrap(),
                Some(Error::Unsupported),
                stop_cause,
            ),
            Phase::IntakeJoin | Phase::WatchdogJoin => {
                let mut stream = Stream::open(
                    StreamConfig::default(),
                    Box::new(crate::MockBackend::new(48_000, 2, 0.0)),
                )
                .unwrap();
                let handle = thread::spawn(|| panic!("injected facade worker failure"));
                match phase {
                    Phase::IntakeJoin => stream.worker = Some(handle),
                    Phase::WatchdogJoin => stream.watchdog = Some(handle),
                    Phase::Stop | Phase::Panic | Phase::TerminalAndStop => unreachable!(),
                }
                (
                    stream,
                    None,
                    Error::Backend("capture worker panicked".into())
                        .with_context(ErrorContext::new(Operation::Join)),
                )
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
        assert_eq!(report, ShutdownReport::new(primary, vec![cleanup]));
        assert_eq!(stream.stop_checked(), report.result());
        assert_eq!(stream.shutdown_report(), Some(report.clone()));
        stream.stop();
        assert_eq!(stream.shutdown_report(), Some(report.clone()));
        assert_eq!(stream.stop_checked(), report.result());
    }
}
