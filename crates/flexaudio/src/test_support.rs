//! Internal backend injection for contract tests; no stability guarantee.
use crate::{CaptureBackend, OpenOptions, Result, Stream, StreamConfig};

/// Open through production validation and settings resolution with an injected backend.
pub fn open(
    config: StreamConfig,
    options: OpenOptions,
    backend: Box<dyn CaptureBackend>,
) -> Result<Stream> {
    crate::open_using(config, options, |_, _| Ok(backend))
}

/// Switch through production validation, settings resolution and atomic commit.
pub fn switch(
    stream: &mut Stream,
    config: StreamConfig,
    options: OpenOptions,
    backend: Box<dyn CaptureBackend>,
) -> Result<()> {
    stream.switch_source_using(config, None, Some(options), |_, _| Ok(backend))
}

/// The failing operation, selected before constructing a fresh fixture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopPhase {
    /// The backend's checked stop returns a typed failure.
    Stop,
    /// The backend violates its contract by panicking during checked stop.
    Panic,
    /// Capture fails before the checked stop also fails.
    TerminalAndStop,
}

/// A fresh facade stream whose backend fails at its actual stop operation.
pub fn stop_failure(phase: StopPhase) -> Result<Stream> {
    let mut stream = open(
        StreamConfig::default(),
        OpenOptions::default(),
        failing_backend(phase),
    )?;
    stream.start()?;
    Ok(stream)
}

/// Inject a backend stop failure while letting a binding fixture supply its own PCM.
pub fn failing_backend(phase: StopPhase) -> Box<dyn CaptureBackend> {
    Box::new(StopBackend {
        phase,
        stopped: false,
        events: Default::default(),
    })
}

struct StopBackend {
    phase: StopPhase,
    stopped: bool,
    events: std::collections::VecDeque<crate::Event>,
}

impl CaptureBackend for StopBackend {
    fn native_format(&self) -> (u32, u16) {
        (48_000, 2)
    }
    fn start(&mut self, _sink: crate::core::RawSink) -> Result<()> {
        Ok(())
    }
    fn stop(&mut self) {
        if !self.stopped {
            self.stopped = true;
            if self.phase == StopPhase::TerminalAndStop {
                self.events.push_back(crate::Event::TerminalError {
                    error: crate::Error::Unsupported,
                });
            }
        }
    }
    fn stop_checked(&mut self) -> Result<()> {
        self.stop();
        match self.phase {
            StopPhase::Stop | StopPhase::TerminalAndStop => Err(crate::Error::DeviceLost
                .with_context(crate::ErrorContext::new(crate::Operation::Stop))),
            StopPhase::Panic => panic!("injected backend stop panic"),
        }
    }
    fn poll_event(&mut self) -> Option<crate::Event> {
        self.events.pop_front()
    }
}
