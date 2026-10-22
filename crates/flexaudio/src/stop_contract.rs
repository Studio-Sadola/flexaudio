//! Built-in construction requires registration in the executable stop contract.
use crate::CaptureBackend;

macro_rules! stop_paths {
    ($($(#[$meta:meta])* $path:ident),* $(,)?) => {
        #[derive(Clone, Copy, Debug)]
        pub(crate) enum StopPath {
            $($(#[$meta])* $path),*
        }
        #[cfg(test)]
        const ALL: &[StopPath] = &[$($(#[$meta])* StopPath::$path),*];
    };
}

stop_paths! {
    Stream,
    Mix,
    Mock,
    StallableMock,
    PanickingMock,
    ReopeningMock,
    Microphone,
    #[cfg(target_os = "linux")] LinuxSystem,
    #[cfg(target_os = "linux")] LinuxProcess,
    #[cfg(target_os = "linux")] LinuxWatcher,
    #[cfg(target_os = "windows")] WindowsSystem,
    #[cfg(target_os = "windows")] WindowsProcess,
    #[cfg(target_os = "macos")] MacSystem,
    #[cfg(target_os = "macos")] MacProcess,
    #[cfg(target_os = "macos")] MacTap,
    #[cfg(target_os = "macos")] MacProbe,
}

/// Only the built-in factory uses this bound; external backends remain unchanged.
pub(crate) trait RegisteredBuiltin {
    const STOP_PATHS: &'static [StopPath];
}

pub(crate) fn builtin<B: CaptureBackend + RegisteredBuiltin + 'static>(
    backend: B,
) -> Box<dyn CaptureBackend> {
    Box::new(registered(backend))
}

pub(crate) fn registered<B: RegisteredBuiltin>(backend: B) -> B {
    debug_assert!(!B::STOP_PATHS.is_empty());
    backend
}

macro_rules! register {
    ($backend:ty => $($path:ident),+ $(,)?) => {
        impl RegisteredBuiltin for $backend {
            const STOP_PATHS: &'static [StopPath] = &[$(StopPath::$path),+];
        }
    };
}

register!(crate::Stream => Stream);
register!(crate::mix::CompositeBackend => Mix);
register!(crate::MockBackend => Mock);
register!(crate::mock::StallableMockBackend => StallableMock);
register!(crate::mock::PanickingMockBackend => PanickingMock);
register!(crate::mock::StallThenPanicOnReopenBackend => ReopeningMock);
register!(flexaudio_mic::CpalMicBackend => Microphone);
#[cfg(target_os = "linux")]
register!(flexaudio_os_linux::PwSystemBackend => LinuxSystem);
#[cfg(target_os = "linux")]
register!(flexaudio_os_linux::PwDeviceWatcher => LinuxWatcher);
#[cfg(target_os = "linux")]
register!(flexaudio_os_linux::PwProcessBackend => LinuxProcess);
#[cfg(target_os = "windows")]
register!(flexaudio_os_windows::WasapiSystemBackend => WindowsSystem);
#[cfg(target_os = "windows")]
register!(flexaudio_os_windows::WasapiProcessBackend => WindowsProcess);
#[cfg(target_os = "macos")]
register!(flexaudio_os_macos::MacSystemBackend => MacSystem, MacTap, MacProbe);
#[cfg(target_os = "macos")]
register!(flexaudio_os_macos::MacProcessBackend => MacProcess);

#[cfg(test)]
mod tests;
