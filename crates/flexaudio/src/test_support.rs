//! Backend injection for binding contract tests using the production facade paths.
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
