//! Capture owner lifecycle and consent checks before native startup.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;

use cpal::traits::StreamTrait;
use flexaudio_core::backend::RawSink;
use flexaudio_core::types::{Error, Result};

use crate::callback_mailbox::CallbackMailbox;
use crate::{build_stream, permission};

pub(crate) fn run(
    sink: RawSink,
    device_id: Option<String>,
    stop_flag: Arc<AtomicBool>,
    ready_tx: mpsc::Sender<Result<()>>,
    callback_errors: Arc<CallbackMailbox>,
) {
    // Query again on the owner to cover revocation between construction and start.
    // No authorization work runs in a cpal audio callback.
    let stream = match build_stream(
        sink,
        device_id.as_deref(),
        stop_flag.clone(),
        callback_errors,
    ) {
        Ok(stream) => stream,
        Err(error) => {
            let _ = ready_tx.send(Err(permission::after_failure(error)));
            return;
        }
    };
    if let Err(error) = stream.play() {
        let error = permission::after_failure(Error::Backend(format!("cpal play: {error}")));
        let _ = ready_tx.send(Err(error));
        return;
    }
    if ready_tx.send(Ok(())).is_err() {
        return;
    }

    wait_until_stopped(&stop_flag);
    // The owner drops its stream directly. It never calls a joining backend stop.
    drop(stream);
}

fn wait_until_stopped(stop_flag: &AtomicBool) {
    while !stop_flag.load(Ordering::SeqCst) {
        thread::park();
    }
}

#[cfg(test)]
mod tests;
