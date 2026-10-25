//! Internal Windows shutdown injection fixtures; no stability guarantee.
//!
//! These fixtures run on Windows CI without an audio endpoint. They substitute
//! native client operations, use real worker joins and event handles, and pass
//! every outcome through the production session and backend stop owners.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};

use flexaudio_core::{Error, ErrorContext, HelperStopReason, Operation, Result, ShutdownReport};
use windows::core::PCWSTR;
use windows::Win32::System::Threading::CreateEventW;

use crate::common::{map_hr_at, EventHandle};
use crate::lifecycle::{Session, StreamClient};
use crate::owner::{finish_capture, OwnerShutdown};

/// Registered Windows backend path. Internal, with no stability guarantee.
#[derive(Clone, Copy, Debug)]
pub enum Path {
    /// Classic system capture, including its optional render client.
    System,
    /// Per-process capture.
    Process,
}

macro_rules! phases {
    ($(#[$meta:meta])* pub enum $name:ident {
        $($(#[$variant_meta:meta])* $variant:ident,)*
    }) => {
        $(#[$meta])*
        pub enum $name {
            $($(#[$variant_meta])* $variant,)*
        }

        impl $name {
            /// Every operation tested for each registered Windows path.
            pub const ALL: &'static [Self] = &[$(Self::$variant,)*];
        }
    };
}

phases! {
    /// Failing operation. Internal, with no stability guarantee.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Phase {
        /// Native capture stop (and render stop on the system path).
        Stop,
        /// Owner thread panic observed by its real join operation.
        Join,
        /// Native event close after the owner has formed its report.
        Close,
        /// Both capture and render clients panic during stop on the system path.
        StopPanics,
        /// Both event handles fail to close on the system path.
        BothCloses,
        /// Terminal capture cause followed by native stop failure.
        TerminalAndStop,
        /// Session Drop observes a native stop failure.
        Drop,
        /// Native startup failure with rollback and event-close cleanup.
        StartupRollback,
        /// Native startup failure whose rollback succeeds.
        StartupFailure,
    }
}

/// Drive a fresh backend fixture and check void, checked, and repeated stops.
/// Internal test API, with no stability guarantee.
pub fn check(path: Path, phase: Phase) {
    match path {
        Path::System => crate::system::WasapiSystemBackend::check_stop_phase(phase, |_| {}),
        Path::Process => crate::process::WasapiProcessBackend::check_stop_phase(phase, |_| {}),
    }
}

/// Check every system phase and observe each backend that passed the assertions.
/// Internal test API, with no stability guarantee.
pub fn check_system(observe: fn(&crate::WasapiSystemBackend)) {
    for &phase in Phase::ALL {
        crate::WasapiSystemBackend::check_stop_phase(phase, observe);
    }
}

/// Check every process phase and observe each backend that passed the assertions.
/// Internal test API, with no stability guarantee.
pub fn check_process(observe: fn(&crate::WasapiProcessBackend)) {
    for &phase in Phase::ALL {
        crate::WasapiProcessBackend::check_stop_phase(phase, observe);
    }
}

fn stop_error(render: bool) -> Error {
    Error::Backend(if render {
        "injected render stop failure".into()
    } else {
        "injected capture stop failure".into()
    })
    .with_context(ErrorContext::new(Operation::Stop))
}

fn start_error() -> Error {
    Error::Backend("injected capture start failure".into())
        .with_context(ErrorContext::new(Operation::Start))
}

fn close_error(call: &'static str) -> Error {
    map_hr_at(
        Operation::Stop,
        call,
        windows::core::Error::from(windows::core::HRESULT(0x80070006u32 as i32)),
    )
}

struct InjectedClient {
    phase: Phase,
    render: bool,
}

impl StreamClient for InjectedClient {
    fn start(&mut self) -> Result<()> {
        if matches!(self.phase, Phase::StartupRollback | Phase::StartupFailure) && !self.render {
            Err(start_error())
        } else {
            Ok(())
        }
    }

    fn stop(&mut self) -> Result<()> {
        match self.phase {
            Phase::Stop | Phase::TerminalAndStop | Phase::Drop | Phase::StartupRollback => {
                Err(stop_error(self.render))
            }
            Phase::StopPanics => panic!("injected client stop panic"),
            Phase::Join | Phase::Close | Phase::BothCloses | Phase::StartupFailure => Ok(()),
        }
    }

    fn fill_silence(&mut self) -> Result<()> {
        Ok(())
    }
}

pub(crate) fn prepare(
    shutdown: &mut OwnerShutdown,
    handle: &mut Option<JoinHandle<ShutdownReport>>,
    phase: Phase,
    system: bool,
    stop_flag: Arc<AtomicBool>,
) -> ShutdownReport {
    let (event_tx, event_rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    shutdown.reset(event_rx);
    let cleanup = shutdown.cleanup.clone();
    let worker = thread::spawn(move || {
        if phase == Phase::Join {
            panic!("injected capture owner panic");
        }
        let native_event = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
            .expect("fixture event creation");
        let mut event = EventHandle::new(native_event, cleanup.clone(), "CloseHandle(capture)");
        if matches!(
            phase,
            Phase::Close | Phase::BothCloses | Phase::StartupRollback
        ) {
            event.inject_close_failure();
        }
        let _render_event = (system && phase == Phase::BothCloses).then(|| {
            let native_event = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
                .expect("fixture render event creation");
            let mut event =
                EventHandle::new(native_event, cleanup.clone(), "CloseHandle(keepalive)");
            event.inject_close_failure();
            event
        });
        let capture = InjectedClient {
            phase,
            render: false,
        };
        let render = system.then_some(InjectedClient {
            phase,
            render: true,
        });
        let session = Session::start_and_report_with_cleanup(
            capture,
            render,
            |status| {
                ready_tx.send(status).expect("fixture readiness receiver");
                true
            },
            Some(event_tx.clone()),
            Arc::new(Mutex::new(HelperStopReason::StreamStopped)),
            cleanup,
        );
        let Some(mut session) = session else {
            return ShutdownReport::new(None, Vec::new());
        };
        while !stop_flag.load(Ordering::SeqCst) {
            thread::yield_now();
        }
        let capture = if phase == Phase::TerminalAndStop {
            Err(Error::DeviceLost)
        } else {
            Ok(())
        };
        if phase == Phase::Drop {
            drop(session);
            finish_capture(capture, || Ok(()), &event_tx)
        } else {
            finish_capture(
                capture,
                || {
                    session.retain_shutdown_outcome();
                    Ok(())
                },
                &event_tx,
            )
        }
        // The real event owner drops after the returned report is constructed.
    });

    let mut cleanup = Vec::new();
    let mut primary = match phase {
        Phase::TerminalAndStop => Some(Error::DeviceLost),
        Phase::StartupRollback | Phase::StartupFailure => Some(start_error()),
        Phase::Stop
        | Phase::Join
        | Phase::Close
        | Phase::Drop
        | Phase::StopPanics
        | Phase::BothCloses => None,
    };
    match phase {
        Phase::Stop | Phase::TerminalAndStop | Phase::Drop => {
            cleanup.push(stop_error(false));
            if system {
                cleanup.push(stop_error(true));
            }
        }
        Phase::Join => cleanup.push(
            Error::Backend("capture owner thread panicked".into())
                .with_context(ErrorContext::new(Operation::Join)),
        ),
        Phase::StopPanics => {
            cleanup.push(
                Error::Backend("capture audio client panicked during teardown".into())
                    .with_context(ErrorContext::new(Operation::Stop)),
            );
            if system {
                cleanup.push(
                    Error::Backend("render audio client panicked during teardown".into())
                        .with_context(ErrorContext::new(Operation::Stop)),
                );
            }
        }
        Phase::Close => cleanup.push(close_error("CloseHandle(capture)")),
        Phase::BothCloses => {
            if system {
                cleanup.push(close_error("CloseHandle(keepalive)"));
            }
            cleanup.push(close_error("CloseHandle(capture)"));
        }
        Phase::StartupRollback => {
            if system {
                cleanup.push(stop_error(true));
            }
            cleanup.push(close_error("CloseHandle(capture)"));
        }
        Phase::StartupFailure => {}
    }
    if matches!(phase, Phase::StartupRollback | Phase::StartupFailure) {
        let error = ready_rx
            .recv()
            .expect("startup failure readiness")
            .unwrap_err();
        assert_eq!(error, start_error());
        assert_eq!(
            shutdown.retain_start_failure(error, worker),
            ShutdownReport::new(primary.clone(), cleanup.clone()).result()
        );
        primary = None;
        if phase == Phase::StartupFailure {
            assert_eq!(shutdown.result(), Ok(()));
            while let Some(event) = shutdown.poll_event() {
                assert!(!matches!(
                    event,
                    flexaudio_core::Event::TerminalError { .. }
                ));
            }
        }
    } else {
        if phase != Phase::Join {
            ready_rx
                .recv()
                .expect("successful fixture readiness")
                .expect("fixture capture start");
        }
        *handle = Some(worker);
    }
    ShutdownReport::new(primary, cleanup)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_retains_each_injected_stop_phase() {
        for phase in Phase::ALL {
            check(Path::System, *phase);
        }
    }

    #[test]
    fn process_retains_each_injected_stop_phase() {
        for phase in Phase::ALL {
            check(Path::Process, *phase);
        }
    }
}
