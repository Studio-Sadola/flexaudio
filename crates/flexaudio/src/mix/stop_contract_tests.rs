//! Child stops and mixer joins use the production composite owner.
use super::*;
use crate::test_support::{failing_backend, StopPhase};

crate::stop_contract::phases! {
    MicrophoneStop,
    SystemStop,
    MixerJoin,
    TerminalAndStop,
    RestorePrevious,
    FinalPollPanic,
    FailedStart,
}

fn clean() -> Box<dyn CaptureBackend> {
    Box::new(crate::MockBackend::new(48_000, 2, 0.0))
}

pub(crate) fn check_all(observe: fn(&CompositeBackend)) {
    for phase in Phase::ALL {
        check(*phase, observe);
    }
}

fn check(phase: Phase, observe: fn(&CompositeBackend)) {
    if matches!(phase, Phase::FailedStart) {
        check_failed_start(observe);
        return;
    }
    if matches!(phase, Phase::RestorePrevious) {
        check_restore(observe);
        return;
    }
    for checked_first in [false, true] {
        let (mic, system) = match phase {
            Phase::MicrophoneStop => (failing_backend(StopPhase::Stop), clean()),
            Phase::FinalPollPanic => (
                Box::new(crate::stream::stop_contract_tests::FinalPollPanic::default())
                    as Box<dyn CaptureBackend>,
                clean(),
            ),
            Phase::SystemStop => (clean(), failing_backend(StopPhase::Stop)),
            Phase::TerminalAndStop => (failing_backend(StopPhase::TerminalAndStop), clean()),
            Phase::MixerJoin | Phase::RestorePrevious | Phase::FailedStart => (clean(), clean()),
        };
        let mut mix = CompositeBackend::new(mic, system, 1.0, 1.0);
        if matches!(phase, Phase::MixerJoin) {
            mix.mixer = Some(thread::spawn(|| panic!("injected mixer failure")));
        } else {
            let (producer, _consumer) = flexaudio_core::raw_ring(4096);
            mix.start(RawSink::new(producer, 48_000, 2)).unwrap();
        }
        if checked_first {
            assert!(mix.stop_checked().is_err());
        } else {
            mix.stop();
        }
        let report = mix
            .shutdown
            .clone()
            .expect("void mix stop retains its report");
        let lane = if matches!(phase, Phase::SystemStop) {
            MixLane::SystemAudio
        } else {
            MixLane::Microphone
        };
        let primary = if matches!(phase, Phase::FinalPollPanic) {
            Some(lane_error(
                Error::Backend("mix child panicked during final event drain".into()),
                Operation::Stop,
                lane,
            ))
        } else {
            matches!(phase, Phase::TerminalAndStop)
                .then(|| lane_error(Error::Unsupported, Operation::Normalize, lane))
        };
        if matches!(phase, Phase::FinalPollPanic) {
            assert_eq!(report.primary(), primary.as_ref());
            assert!(report.cleanup().is_empty());
            assert_eq!(mix.stop_checked(), report.result());
            observe(&mix);
            continue;
        }
        let cleanup = if matches!(phase, Phase::MixerJoin) {
            Error::Backend("mix worker panicked".into())
                .with_context(ErrorContext::new(Operation::Join))
        } else {
            lane_error(
                Error::DeviceLost.with_context(ErrorContext::new(Operation::Stop)),
                Operation::Stop,
                lane,
            )
        };
        assert_eq!(report, ShutdownReport::new(primary, vec![cleanup]));
        assert_eq!(mix.stop_checked(), report.result());
        assert_eq!(mix.shutdown, Some(report.clone()));
        mix.stop();
        assert_eq!(mix.shutdown, Some(report.clone()));
        assert_eq!(mix.stop_checked(), report.result());
        observe(&mix);
    }
}

struct FailsStart;

impl CaptureBackend for FailsStart {
    fn native_format(&self) -> (u32, u16) {
        (48_000, 2)
    }
    fn start(&mut self, _: RawSink) -> Result<()> {
        Err(Error::DeviceNotFound)
    }
    fn stop(&mut self) {}
}

fn check_failed_start(observe: fn(&CompositeBackend)) {
    for fail_mic in [false, true] {
        let (mic, system): (Box<dyn CaptureBackend>, Box<dyn CaptureBackend>) = if fail_mic {
            (Box::new(FailsStart), clean())
        } else {
            (clean(), Box::new(FailsStart))
        };
        let mut mix = CompositeBackend::new(mic, system, 1.0, 1.0);
        let (producer, _consumer) = flexaudio_core::raw_ring(4096);
        let error = mix.start(RawSink::new(producer, 48_000, 2)).unwrap_err();
        assert_eq!(error.root(), &Error::DeviceNotFound);
        assert!(mix.stop_checked().is_ok());
        assert_eq!(mix.shutdown, Some(ShutdownReport::new(None, Vec::new())));
        while let Some(event) = mix.poll_event() {
            assert!(!matches!(event, Event::TerminalError { .. }));
        }
        observe(&mix);
    }
}

#[test]
fn failed_start_has_clean_shutdown_and_no_terminal_notice() {
    check(Phase::FailedStart, |_| {});
}

#[test]
fn distinct_failure_contract() {
    flexaudio_core::test_support::assert_distinct_failures_retained(
        Error::DeviceLost,
        Error::Backend("later capture failure".into()),
        |first, second| {
            let mix = CompositeBackend::new(clean(), clean(), 1.0, 1.0);
            mix.notices.fail(first);
            mix.notices.fail(second);
            mix.notices.report().primary().unwrap().clone()
        },
    );
}

#[test]
fn mix_final_poll_panic_is_retained_for_both_stop_orders() {
    check(Phase::FinalPollPanic, |_| {});
}

fn check_restore(observe: fn(&CompositeBackend)) {
    struct RestartFailure {
        starts: usize,
    }
    impl CaptureBackend for RestartFailure {
        fn native_format(&self) -> (u32, u16) {
            (48_000, 2)
        }
        fn start(&mut self, _: RawSink) -> Result<()> {
            self.starts += 1;
            if self.starts > 1 {
                Err(Error::DeviceNotFound)
            } else {
                Ok(())
            }
        }
        fn stop(&mut self) {}
        fn stop_checked(&mut self) -> Result<()> {
            if self.starts > 1 {
                Err(Error::DeviceLost)
            } else {
                Ok(())
            }
        }
    }
    let mut mix = CompositeBackend::new(Box::new(RestartFailure { starts: 0 }), clean(), 1.0, 1.0);
    let (producer, _consumer) = flexaudio_core::raw_ring(4096);
    mix.start(RawSink::new(producer, 48_000, 2)).unwrap();
    mix.stop();
    assert!(mix.shutdown.as_ref().unwrap().result().is_ok());
    let (producer, _consumer) = flexaudio_core::raw_ring(4096);
    assert!(mix.start(RawSink::new(producer, 48_000, 2)).is_err());
    let report = mix
        .shutdown
        .clone()
        .expect("failed restart retains cleanup");
    assert!(report.primary().is_none());
    assert_eq!(
        report.cleanup(),
        &[lane_error(
            Error::DeviceLost,
            Operation::Stop,
            MixLane::Microphone
        )]
    );
    assert_eq!(mix.stop_checked(), report.result());
    mix.stop();
    assert_eq!(mix.shutdown, Some(report));
    observe(&mix);
}
