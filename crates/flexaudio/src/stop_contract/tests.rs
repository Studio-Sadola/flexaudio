//! The production registrations drive typed fixtures; paths only name coverage.
use super::*;
use std::cell::Cell;

thread_local! {
    static OBSERVED: Cell<usize> = const { Cell::new(0) };
}

fn check_fixture<B>(fixture: fn(fn(&B)), path: &StopPath) {
    OBSERVED.with(|count| count.set(0));
    fixture(|_: &B| OBSERVED.with(|count| count.set(count.get() + 1)));
    assert!(
        OBSERVED.with(Cell::get) > 0,
        "{} fixture did not observe its backend",
        path.name()
    );
}

#[test]
fn registered_stop_paths_retain_injected_operation_failures() {
    // Stream owns the facade teardown but is not a factory CaptureBackend.
    let stream = StopPath::Stream(crate::stream::stop_contract_tests::check_all);
    stream.check();
    let mut covered = vec![stream.name()];
    for registration in REGISTRATIONS {
        assert!(!registration.is_empty());
        for path in *registration {
            path.check();
            covered.push(path.name());
        }
    }
    for path in ALL {
        assert_eq!(
            covered.iter().filter(|covered| **covered == *path).count(),
            1,
            "{} must have exactly one fixture registration",
            path
        );
    }
}

impl StopPath {
    // This executes the runner carried by the registration. The match cannot
    // select an unrelated backend: each variant fixes its runner's type.
    fn check(&self) {
        match self {
            Self::Stream(fixture) => check_fixture(*fixture, self),
            Self::Mix(fixture) => check_fixture(*fixture, self),
            Self::Mock(fixture) => check_fixture(*fixture, self),
            Self::StallableMock(fixture) => check_fixture(*fixture, self),
            Self::PanickingMock(fixture) => check_fixture(*fixture, self),
            Self::ReopeningMock(fixture) => check_fixture(*fixture, self),
            Self::Microphone(fixture) => check_fixture(*fixture, self),
            #[cfg(target_os = "linux")]
            Self::LinuxSystem(fixture) => check_fixture(*fixture, self),
            #[cfg(target_os = "linux")]
            Self::LinuxProcess(fixture) => check_fixture(*fixture, self),
            #[cfg(target_os = "linux")]
            Self::LinuxWatcher(fixture) => check_fixture(*fixture, self),
            #[cfg(target_os = "windows")]
            Self::WindowsSystem(fixture) => check_fixture(*fixture, self),
            #[cfg(target_os = "windows")]
            Self::WindowsProcess(fixture) => check_fixture(*fixture, self),
            #[cfg(target_os = "macos")]
            Self::MacSystem(fixture) => check_fixture(*fixture, self),
            #[cfg(target_os = "macos")]
            Self::MacProcess(fixture) => check_fixture(*fixture, self),
            #[cfg(target_os = "macos")]
            Self::MacTap(fixture) => check_fixture(*fixture, self),
            #[cfg(target_os = "macos")]
            Self::MacProbe(fixture) => check_fixture(*fixture, self),
        }
    }
}
