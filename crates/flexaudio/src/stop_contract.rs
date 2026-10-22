//! Built-in construction requires registration in the executable stop contract.
use crate::CaptureBackend;

#[cfg(test)]
macro_rules! stop_paths {
    ($($(#[$meta:meta])* $path:ident($backend:ty)),* $(,)?) => {
        #[derive(Clone, Copy, Debug)]
        pub(crate) enum StopPath {
            $($(#[$meta])* $path(fn(fn(&$backend)))),*
        }
        impl StopPath {
            fn name(&self) -> &'static str {
                match self {
                    $($(#[$meta])* Self::$path(_) => stringify!($path)),*
                }
            }
        }
        const ALL: &[&str] = &[$($(#[$meta])* stringify!($path)),*];
    };
}

#[cfg(test)]
stop_paths! {
    Stream(crate::Stream),
    Mix(crate::mix::CompositeBackend),
    Mock(crate::MockBackend),
    StallableMock(crate::mock::StallableMockBackend),
    PanickingMock(crate::mock::PanickingMockBackend),
    ReopeningMock(crate::mock::StallThenPanicOnReopenBackend),
    Microphone(flexaudio_mic::CpalMicBackend),
    #[cfg(target_os = "linux")] LinuxSystem(flexaudio_os_linux::PwSystemBackend),
    #[cfg(target_os = "linux")] LinuxProcess(flexaudio_os_linux::PwProcessBackend),
    #[cfg(target_os = "linux")] LinuxWatcher(flexaudio_os_linux::PwDeviceWatcher),
    #[cfg(target_os = "windows")] WindowsSystem(flexaudio_os_windows::WasapiSystemBackend),
    #[cfg(target_os = "windows")] WindowsProcess(flexaudio_os_windows::WasapiProcessBackend),
    #[cfg(target_os = "macos")] MacSystem(flexaudio_os_macos::MacSystemBackend),
    #[cfg(target_os = "macos")] MacProcess(flexaudio_os_macos::MacProcessBackend),
    #[cfg(target_os = "macos")] MacTap(flexaudio_os_macos::MacSystemBackend),
    #[cfg(target_os = "macos")] MacProbe(flexaudio_os_macos::MacSystemBackend),
}

/// Only the built-in factory uses this bound; external backends remain unchanged.
pub(crate) trait RegisteredBuiltin {
    #[cfg(test)]
    const STOP_PATHS: &'static [StopPath];
}

pub(crate) fn builtin<B: CaptureBackend + RegisteredBuiltin + 'static>(
    backend: B,
) -> Box<dyn CaptureBackend> {
    Box::new(registered(backend))
}

pub(crate) fn registered<B: RegisteredBuiltin>(backend: B) -> B {
    backend
}

macro_rules! register {
    ($($(#[$meta:meta])* $backend:ty => $($path:ident($fixture:path)),+;)+) => {
        $($(#[$meta])*
            impl RegisteredBuiltin for $backend {
                #[cfg(test)]
                const STOP_PATHS: &'static [StopPath] = &[$(StopPath::$path({
                    // Check the runner against both Self and the path's backend.
                    let fixture: fn(fn(&Self)) = $fixture;
                    fixture
                })),+];
            }
        )+

        // The factory bound and executable inventory come from this same list.
        #[cfg(test)]
        const REGISTRATIONS: &[&[StopPath]] = &[
            $($(#[$meta])* <$backend as RegisteredBuiltin>::STOP_PATHS,)+
        ];
    };
}

register! {
    crate::mix::CompositeBackend => Mix(crate::mix::stop_contract_tests::check_all);
    crate::MockBackend => Mock(crate::mock::check_sine);
    crate::mock::StallableMockBackend => StallableMock(crate::mock::check_stallable);
    crate::mock::PanickingMockBackend => PanickingMock(crate::mock::check_panicking);
    crate::mock::StallThenPanicOnReopenBackend => ReopeningMock(crate::mock::check_reopening);
    flexaudio_mic::CpalMicBackend => Microphone(flexaudio_mic::test_support::check_all);
    #[cfg(target_os = "linux")]
    flexaudio_os_linux::PwSystemBackend => LinuxSystem(flexaudio_os_linux::test_support::check_system);
    #[cfg(target_os = "linux")]
    flexaudio_os_linux::PwDeviceWatcher => LinuxWatcher(flexaudio_os_linux::test_support::check_watcher);
    #[cfg(target_os = "linux")]
    flexaudio_os_linux::PwProcessBackend => LinuxProcess(flexaudio_os_linux::test_support::check_process);
    #[cfg(target_os = "windows")]
    flexaudio_os_windows::WasapiSystemBackend => WindowsSystem(flexaudio_os_windows::test_support::check_system);
    #[cfg(target_os = "windows")]
    flexaudio_os_windows::WasapiProcessBackend => WindowsProcess(flexaudio_os_windows::test_support::check_process);
    #[cfg(target_os = "macos")]
    flexaudio_os_macos::MacSystemBackend =>
        MacSystem(flexaudio_os_macos::test_support::check_system),
        MacTap(flexaudio_os_macos::test_support::check_tap),
        MacProbe(flexaudio_os_macos::test_support::check_probe);
    #[cfg(target_os = "macos")]
    flexaudio_os_macos::MacProcessBackend => MacProcess(flexaudio_os_macos::test_support::check_process);
}

#[cfg(test)]
macro_rules! phases {
    ($($phase:ident),+ $(,)?) => {
        #[derive(Clone, Copy)]
        pub(crate) enum Phase { $($phase),+ }
        impl Phase {
            const ALL: &'static [Self] = &[$(Self::$phase),+];
        }
    };
}

#[cfg(test)]
pub(crate) use phases;

#[cfg(test)]
mod tests;
