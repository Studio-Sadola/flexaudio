//! Shared bridge startup and capture intake for normal, flush and stop polling.
use super::*;

/// Node owns callback ordering; ordinary facade pull APIs remain independent.
pub(super) fn start_bridge_stream(env: &Env, stream: &mut flexaudio::Stream) -> napi::Result<()> {
    stream
        .enable_ordered_delivery()
        .map_err(|error| boundary::throw_error(env, error))?;
    stream
        .start()
        .map_err(|error| boundary::throw_error(env, error))
}

impl PairingBridge {
    pub(super) fn drain_stream(
        &mut self,
        stream: &mut flexaudio::Stream,
        callback: Option<&EventTsfn>,
        terminal: &Mutex<Option<StreamTerminal>>,
    ) {
        loop {
            forward_stream_events(
                stream,
                callback,
                &self.on_chunk,
                terminal,
                self.lifecycle.as_deref(),
                &self.report,
            );
            self.suppress_failed_pcm(stream, terminal);
            self.drain_capture(stream);
            while let Some(chunk) = stream.poll_chunk_ordered() {
                self.on_primary(chunk);
            }
            while let Some(chunk) = stream.poll_secondary_ordered() {
                self.on_secondary(chunk);
            }
            self.drain_pairs();
            // A startup notice can be published after the first event drain and
            // gate this pass. Queue that notice, then retry before flush/stop.
            if !forward_stream_events(
                stream,
                callback,
                &self.on_chunk,
                terminal,
                self.lifecycle.as_deref(),
                &self.report,
            ) {
                break;
            }
        }
        self.suppress_failed_pcm(stream, terminal);
    }

    fn suppress_failed_pcm(
        &mut self,
        stream: &flexaudio::Stream,
        terminal: &Mutex<Option<StreamTerminal>>,
    ) {
        if let Some(error) = stream.terminal_error() {
            retain_stream_terminal(terminal, error);
            self.primary_fifo.clear();
            self.secondary_fifo.clear();
        }
    }
}
