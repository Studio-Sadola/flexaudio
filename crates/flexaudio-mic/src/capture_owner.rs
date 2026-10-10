//! Capture owner lifecycle and control-thread consent monitoring.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;

use cpal::traits::StreamTrait;
use flexaudio_core::backend::RawSink;
#[cfg(any(target_os = "macos", test))]
use flexaudio_core::types::Event;
use flexaudio_core::types::{Error, Result};

use crate::callback_mailbox::CallbackMailbox;
use crate::generation::Generation;
use crate::{build_stream, permission};

pub(crate) fn run(
    sink: RawSink,
    device_id: Option<String>,
    stop_flag: Arc<AtomicBool>,
    ready_tx: mpsc::Sender<Result<()>>,
    generation: Arc<Generation>,
    callback_errors: Arc<CallbackMailbox>,
) {
    // Query again on the owner to cover revocation between construction and start.
    // No authorization work runs in a cpal audio callback.
    let watch_pending = match permission::preflight() {
        Ok(watch) => watch,
        Err(error) => {
            let _ = ready_tx.send(Err(error));
            return;
        }
    };
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

    wait_until_stopped(watch_pending, &stop_flag, &generation);
    // The owner drops its stream directly. It never calls a joining backend stop.
    drop(stream);
}

fn wait_until_stopped(_watch_pending: bool, stop_flag: &AtomicBool, _generation: &Generation) {
    #[cfg(target_os = "macos")]
    if _watch_pending {
        let started = std::time::Instant::now();
        monitor_until_stopped(
            &crate::mac_permission::Native,
            stop_flag,
            _generation,
            || started.elapsed(),
            |delay| match delay {
                Some(delay) => thread::park_timeout(delay),
                None => thread::park(),
            },
        );
        return;
    }
    while !stop_flag.load(Ordering::SeqCst) {
        thread::park();
    }
}

#[cfg(any(target_os = "macos", test))]
fn monitor_until_stopped(
    provider: &dyn crate::mac_policy::Provider,
    stop_flag: &AtomicBool,
    generation: &Generation,
    elapsed: impl Fn() -> std::time::Duration,
    mut park: impl FnMut(Option<std::time::Duration>),
) {
    let mut poll = crate::mac_policy::ConsentPoll::new();
    while !stop_flag.load(Ordering::SeqCst) {
        if check_pending(&mut poll, elapsed(), provider, stop_flag, generation) {
            break;
        }
        // The existing owner's park token also wakes a two-second late poll.
        // After authorization, park without querying until stop/drop unparks us.
        park(poll.active().then(|| poll.wait_duration(elapsed())));
    }
}

#[cfg(any(target_os = "macos", test))]
fn check_pending(
    poll: &mut crate::mac_policy::ConsentPoll,
    elapsed: std::time::Duration,
    provider: &dyn crate::mac_policy::Provider,
    stop_flag: &AtomicBool,
    generation: &Generation,
) -> bool {
    if stop_flag.load(Ordering::SeqCst) {
        return false;
    }
    let event = match poll.poll(elapsed, provider) {
        Ok(Some(event @ (Event::PermissionPending { .. } | Event::PermissionGranted))) => {
            generation.emit(event);
            return false;
        }
        Ok(Some(event)) => event,
        Err(error) => Event::TerminalError { error },
        Ok(None) => return false,
    };
    terminate(generation, event);
    true
}

#[cfg(any(target_os = "macos", test))]
fn terminate(generation: &Generation, event: Event) {
    // Gate delivery before making terminality visible, on the owner thread.
    generation.emit(event);
}

#[cfg(test)]
mod tests;
