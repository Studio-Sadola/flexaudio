//! Internal stop-failure injection contract. No stability guarantee.

use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Instant;

use flexaudio_core::backend::CaptureBackend;
use flexaudio_core::{Error, NativeStatus, Operation, Result, ShutdownReport};
use objc2_core_audio_types::{AudioBufferList, AudioTimeStamp};

use crate::cleanup::{inject, CleanupLedger, NativeCleanup};
use crate::probe::{GenerationProbe, Probe, ProbeControl, ProbeOutcome, PROBE_TIMEOUT};
use crate::tap::TapResources;
use crate::{MacProcessBackend, MacSystemBackend};

macro_rules! phases {
    ($(#[$meta:meta])* pub enum $name:ident {
        $($(#[$variant_meta:meta])* $variant:ident,)*
    }) => {
        $(#[$meta])*
        pub enum $name {
            $($(#[$variant_meta])* $variant,)*
        }

        impl $name {
            /// Every stop and native release operation in the fixture contract.
            pub const ALL: &'static [Self] = &[$(Self::$variant,)*];
        }
    };
}

phases! {
    /// Stop or native release phase selected by the facade's exhaustive contract.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Phase {
        /// System owner returns a stop failure.
        SystemStop,
        /// System owner panics before join.
        SystemJoin,
        /// Process owner returns a stop failure.
        ProcessStop,
        /// Process owner panics before join.
        ProcessJoin,
        /// Main tap callback stop fails.
        TapStop,
        /// Main tap callback destruction fails.
        TapIoProc,
        /// Main tap aggregate destruction fails.
        TapAggregate,
        /// Main process tap destruction fails.
        TapDestroy,
        /// Probe callback stop fails.
        ProbeStop,
        /// Probe callback destruction fails.
        ProbeIoProc,
        /// Probe aggregate destruction fails.
        ProbeAggregate,
        /// Probe process tap destruction fails.
        ProbeTap,
        /// Probe cleanup fails while stop cancels result publication.
        ProbeCleanupCancelled,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Path {
    System,
    Process,
    Tap,
    Probe,
}

impl Phase {
    fn path(self) -> Path {
        match self {
            Self::SystemStop | Self::SystemJoin => Path::System,
            Self::ProcessStop | Self::ProcessJoin => Path::Process,
            Self::TapStop | Self::TapIoProc | Self::TapAggregate | Self::TapDestroy => Path::Tap,
            Self::ProbeStop
            | Self::ProbeIoProc
            | Self::ProbeAggregate
            | Self::ProbeTap
            | Self::ProbeCleanupCancelled => Path::Probe,
        }
    }
}

/// System-owner fixture identity. Internal, with no stability guarantee.
#[derive(Clone, Copy)]
pub struct SystemPath;

/// Process-owner fixture identity. Internal, with no stability guarantee.
#[derive(Clone, Copy)]
pub struct ProcessPath;

/// Tap-release fixture identity. Internal, with no stability guarantee.
#[derive(Clone, Copy)]
pub struct TapPath;

/// Probe-release fixture identity. Internal, with no stability guarantee.
#[derive(Clone, Copy)]
pub struct ProbePath;

pub(crate) trait FixtureBackend: CaptureBackend {
    fn retained_report(&self) -> &ShutdownReport;
}

// An inert native callback ID. Injection intercepts every release before the OS
// sees this fixture's IDs; no audio device or capture permission is required.
pub(crate) unsafe extern "C-unwind" fn unused_io_proc(
    _: u32,
    _: NonNull<AudioTimeStamp>,
    _: NonNull<AudioBufferList>,
    _: NonNull<AudioTimeStamp>,
    _: NonNull<AudioBufferList>,
    _: NonNull<AudioTimeStamp>,
    _: *mut std::ffi::c_void,
) -> i32 {
    0
}

struct CleanupProbe {
    cancel: bool,
}

#[derive(Clone, Copy)]
enum Exit {
    Stop,
    Drop,
    Rollback,
}

impl Probe for CleanupProbe {
    fn run(&mut self, control: &ProbeControl) -> ProbeOutcome {
        crate::native_probe::inject_cleanup(control);
        if self.cancel {
            control.cancellation_flag().store(true, Ordering::Release);
        }
        ProbeOutcome::Present
    }
}

fn owner_result(phase: Phase, cleanup: Arc<CleanupLedger>, exit: Exit) -> Result<()> {
    let native = match phase {
        Phase::SystemStop | Phase::ProcessStop => {
            return Err(Error::Backend("injected owner stop failure".into()));
        }
        Phase::SystemJoin | Phase::ProcessJoin => panic!("injected owner join failure"),
        Phase::TapStop => NativeCleanup::TapStop,
        Phase::TapIoProc => NativeCleanup::TapIoProc,
        Phase::TapAggregate => NativeCleanup::TapAggregate,
        Phase::TapDestroy => NativeCleanup::TapDestroy,
        Phase::ProbeStop => NativeCleanup::ProbeStop,
        Phase::ProbeIoProc => NativeCleanup::ProbeIoProc,
        Phase::ProbeAggregate => NativeCleanup::ProbeAggregate,
        Phase::ProbeTap => NativeCleanup::ProbeTap,
        Phase::ProbeCleanupCancelled => NativeCleanup::ProbeStop,
    };
    inject(native, || {
        match phase {
            Phase::TapStop | Phase::TapIoProc | Phase::TapAggregate | Phase::TapDestroy => {
                match exit {
                    Exit::Rollback => {
                        assert_eq!(TapResources::rollback_fixture(&cleanup), Error::DeviceLost);
                    }
                    Exit::Stop | Exit::Drop => {
                        let mut resources = TapResources::fixture(cleanup);
                        if matches!(exit, Exit::Stop) {
                            resources.retain_shutdown_outcome();
                        }
                        drop(resources);
                    }
                }
            }
            Phase::ProbeStop
            | Phase::ProbeIoProc
            | Phase::ProbeAggregate
            | Phase::ProbeTap
            | Phase::ProbeCleanupCancelled => {
                let control = ProbeControl::new(
                    Instant::now() + PROBE_TIMEOUT,
                    Arc::new(AtomicBool::new(false)),
                )
                .with_cleanup_ledger(cleanup);
                let (events, lifecycle) = std::sync::mpsc::channel();
                let control = control.with_lifecycle(
                    events,
                    Arc::new(std::sync::Mutex::new(
                        flexaudio_core::HelperStopReason::Cancelled,
                    )),
                );
                let decision = GenerationProbe::default().on_silence(
                    &mut CleanupProbe {
                        cancel: phase == Phase::ProbeCleanupCancelled,
                    },
                    &control,
                );
                if phase == Phase::ProbeCleanupCancelled {
                    assert!(
                        decision.is_none(),
                        "cancelled probe must not publish a result"
                    );
                    assert!(control.cancelled());
                    assert_eq!(
                        lifecycle.try_iter().collect::<Vec<_>>(),
                        [
                            flexaudio_core::Event::SilenceProbeStarted,
                            flexaudio_core::Event::SilenceProbeStopped {
                                reason: flexaudio_core::HelperStopReason::Cancelled,
                            },
                        ],
                        "cancelled cleanup must close the lifecycle without publishing evidence"
                    );
                } else {
                    assert!(
                        decision.is_some(),
                        "cleanup failure must invalidate probe evidence"
                    );
                }
            }
            Phase::SystemStop | Phase::SystemJoin | Phase::ProcessStop | Phase::ProcessJoin => {
                unreachable!("owner failures return before native cleanup")
            }
        }
        Ok(())
    })
}

fn assert_backend<B: FixtureBackend>(
    mut backend: B,
    phase: Phase,
    primary: Option<Error>,
    checked_first: bool,
    observe: impl Fn(&B),
) {
    let checked = if checked_first {
        Some(backend.stop_checked())
    } else {
        backend.stop();
        None
    };
    let report = backend.retained_report().clone();
    assert_eq!(
        report.primary(),
        primary.as_ref(),
        "capture primary must stay distinct"
    );
    assert!(
        !report.cleanup().is_empty(),
        "observed cleanup failure must be retained"
    );
    let expected_count = if phase == Phase::ProbeIoProc { 2 } else { 1 };
    assert_eq!(
        report.cleanup().len(),
        expected_count,
        "cleanup attempts must be retained exactly once"
    );
    for error in report.cleanup() {
        let Error::Context { context, .. } = error else {
            panic!("cleanup failure must keep operation context: {error:?}");
        };
        let join = matches!(phase, Phase::SystemJoin | Phase::ProcessJoin);
        assert_eq!(
            context.operation(),
            if join {
                Operation::Join
            } else {
                Operation::Stop
            }
        );
        match phase {
            Phase::SystemStop | Phase::ProcessStop => {
                assert_eq!(
                    error.root(),
                    &Error::Backend("injected owner stop failure".into())
                );
            }
            Phase::SystemJoin | Phase::ProcessJoin => {
                assert_eq!(
                    error.root(),
                    &Error::Backend("capture owner thread panicked".into())
                );
            }
            Phase::TapStop => assert_native(context.native_status(), "AudioDeviceStop"),
            Phase::TapIoProc => {
                assert_native(context.native_status(), "AudioDeviceDestroyIOProcID")
            }
            Phase::TapAggregate => assert_native(
                context.native_status(),
                "AudioHardwareDestroyAggregateDevice",
            ),
            Phase::TapDestroy => {
                assert_native(context.native_status(), "AudioHardwareDestroyProcessTap")
            }
            Phase::ProbeStop | Phase::ProbeCleanupCancelled => {
                assert_native(context.native_status(), "capture IOProc stop")
            }
            Phase::ProbeIoProc => {
                assert_native(context.native_status(), "capture IOProc destruction")
            }
            Phase::ProbeAggregate => {
                assert_native(context.native_status(), "aggregate destruction")
            }
            Phase::ProbeTap => assert_native(context.native_status(), "private tap destruction"),
        }
    }
    if let Some(checked) = checked {
        assert_eq!(checked, report.result());
    }
    assert_eq!(backend.stop_checked(), report.result());
    assert_eq!(backend.retained_report(), &report);
    while backend.poll_event().is_some() {}
    backend.stop();
    assert_eq!(
        backend.retained_report(),
        &report,
        "second stop must preserve the report"
    );
    assert_eq!(backend.stop_checked(), report.result());
    observe(&backend);
}

fn assert_native(status: Option<NativeStatus>, call: &'static str) {
    assert_eq!(status, Some(NativeStatus::OsStatus { call, value: -1 }));
}

fn check_phase<B: FixtureBackend>(
    phase: Phase,
    construct: impl Fn(JoinHandle<Result<()>>, Arc<CleanupLedger>, Option<Error>) -> B,
    observe: impl Fn(&B),
) {
    for primary in [None, Some(Error::DeviceLost)] {
        for checked_first in [false, true] {
            for exit in [Exit::Stop, Exit::Drop, Exit::Rollback] {
                let cleanup = Arc::new(CleanupLedger::default());
                let owner_cleanup = cleanup.clone();
                let handle = thread::spawn(move || owner_result(phase, owner_cleanup, exit));
                assert_backend(
                    construct(handle, cleanup, primary.clone()),
                    phase,
                    primary.clone(),
                    checked_first,
                    &observe,
                );
            }
        }
    }
}

fn check_path<B: FixtureBackend>(
    path: Path,
    construct: impl Fn(JoinHandle<Result<()>>, Arc<CleanupLedger>, Option<Error>) -> B,
    observe: impl Fn(&B),
) {
    for &phase in Phase::ALL {
        if phase.path() == path {
            check_phase(phase, &construct, &observe);
        }
    }
}

/// Check system owner phases and observe the same backends after retention assertions.
/// Internal test API, with no stability guarantee.
pub fn check_system(observe: fn(&MacSystemBackend, SystemPath)) {
    check_path(
        Path::System,
        MacSystemBackend::shutdown_fixture,
        |backend| {
            observe(backend, SystemPath);
        },
    );
}

/// Check process owner phases and observe the same backends after retention assertions.
/// Internal test API, with no stability guarantee.
pub fn check_process(observe: fn(&MacProcessBackend, ProcessPath)) {
    check_path(
        Path::Process,
        MacProcessBackend::shutdown_fixture,
        |backend| {
            observe(backend, ProcessPath);
        },
    );
}

/// Check tap release phases and observe the system owners retaining their cleanup.
/// Internal test API, with no stability guarantee.
pub fn check_tap(observe: fn(&MacSystemBackend, TapPath)) {
    check_path(Path::Tap, MacSystemBackend::shutdown_fixture, |backend| {
        observe(backend, TapPath);
    });
}

/// Check probe release phases and observe the system owners retaining their cleanup.
/// Internal test API, with no stability guarantee.
pub fn check_probe(observe: fn(&MacSystemBackend, ProbePath)) {
    check_path(Path::Probe, MacSystemBackend::shutdown_fixture, |backend| {
        observe(backend, ProbePath);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_retains_each_injected_owner_phase() {
        check_system(|_, _: SystemPath| {});
    }

    #[test]
    fn process_retains_each_injected_owner_phase() {
        check_process(|_, _: ProcessPath| {});
    }

    #[test]
    fn tap_retains_each_injected_release_phase() {
        check_tap(|_, _: TapPath| {});
    }

    #[test]
    fn probe_retains_each_injected_release_phase() {
        check_probe(|_, _: ProbePath| {});
    }
}
