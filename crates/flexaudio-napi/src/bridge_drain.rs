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
            forward_stream_events(stream, callback, &self.on_chunk, terminal);
            self.suppress_failed_pcm(stream);
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
            if !forward_stream_events(stream, callback, &self.on_chunk, terminal) {
                break;
            }
        }
        self.suppress_failed_pcm(stream);
    }

    fn suppress_failed_pcm(&mut self, stream: &flexaudio::Stream) {
        if stream.terminal_error().is_some() {
            self.primary_fifo.clear();
            self.secondary_fifo.clear();
            self.pending_flush_events.clear();
        }
    }
}
