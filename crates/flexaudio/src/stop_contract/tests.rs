//! Adding a registered path requires a fixture arm: this match has no wildcard.
use super::*;

#[test]
fn registered_stop_paths_retain_injected_operation_failures() {
    for path in ALL {
        check(*path);
    }
}

fn check(path: StopPath) {
    match path {
        StopPath::Stream => {
            use crate::stream::stop_contract_tests::{check, Phase::*};
            for phase in [Stop, Panic, TerminalAndStop, IntakeJoin, WatchdogJoin] {
                check(phase);
            }
        }
        StopPath::Mix => {
            use crate::mix::stop_contract_tests::{check, Phase::*};
            for phase in [
                MicrophoneStop,
                SystemStop,
                MixerJoin,
                TerminalAndStop,
                RestorePrevious,
            ] {
                check(phase);
            }
        }
        StopPath::Mock => crate::mock::check_stop_contract(crate::mock::StopFixture::Sine),
        StopPath::StallableMock => {
            crate::mock::check_stop_contract(crate::mock::StopFixture::Stallable)
        }
        StopPath::PanickingMock => {
            crate::mock::check_stop_contract(crate::mock::StopFixture::Panicking)
        }
        StopPath::ReopeningMock => {
            crate::mock::check_stop_contract(crate::mock::StopFixture::Reopening)
        }
        StopPath::Microphone => {
            use flexaudio_mic::test_support::{check, Phase::*};
            for phase in [Join, TerminalAndJoin, ConsumedTerminalAndJoin] {
                check(phase);
            }
        }
        #[cfg(target_os = "linux")]
        StopPath::LinuxSystem => {
            use flexaudio_os_linux::test_support::{check, Phase::*};
            for phase in [
                SystemJoin,
                SystemTerminalAndJoin,
                SystemConsumedTerminalAndJoin,
            ] {
                check(phase);
            }
        }
        #[cfg(target_os = "linux")]
        StopPath::LinuxProcess => {
            use flexaudio_os_linux::test_support::{check, Phase::*};
            for phase in [
                ProcessJoin,
                ProcessTerminalAndJoin,
                ProcessConsumedTerminalAndJoin,
            ] {
                check(phase);
            }
        }
        #[cfg(target_os = "linux")]
        StopPath::LinuxWatcher => {
            use flexaudio_os_linux::test_support::{check, Phase::*};
            for phase in [WatcherJoin, WatcherTerminalAndJoin] {
                check(phase);
            }
        }
        #[cfg(target_os = "windows")]
        StopPath::WindowsSystem => {
            use flexaudio_os_windows::test_support::{check, Path, Phase};
            for phase in Phase::ALL {
                check(Path::System, *phase);
            }
        }
        #[cfg(target_os = "windows")]
        StopPath::WindowsProcess => {
            use flexaudio_os_windows::test_support::{check, Path, Phase};
            for phase in Phase::ALL {
                check(Path::Process, *phase);
            }
        }
        #[cfg(target_os = "macos")]
        StopPath::MacSystem => {
            use flexaudio_os_macos::test_support::{check, Phase::*};
            for phase in [SystemStop, SystemJoin] {
                check(phase);
            }
        }
        #[cfg(target_os = "macos")]
        StopPath::MacProcess => {
            use flexaudio_os_macos::test_support::{check, Phase::*};
            for phase in [ProcessStop, ProcessJoin] {
                check(phase);
            }
        }
        #[cfg(target_os = "macos")]
        StopPath::MacTap => {
            use flexaudio_os_macos::test_support::{check, Phase::*};
            for phase in [TapStop, TapIoProc, TapAggregate, TapDestroy] {
                check(phase);
            }
        }
        #[cfg(target_os = "macos")]
        StopPath::MacProbe => {
            use flexaudio_os_macos::test_support::{check, Phase::*};
            for phase in [
                ProbeStop,
                ProbeIoProc,
                ProbeAggregate,
                ProbeTap,
                ProbeCleanupCancelled,
            ] {
                check(phase);
            }
        }
    }
}
