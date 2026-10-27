//! One delivery policy for initial and lifecycle-deferred stop completion.

use super::*;

pub(super) type SettleWeakCell = Arc<OnceLock<Weak<ThreadsafeFunction<(), ErrorStrategy::Fatal>>>>;

pub(super) fn post(
    chunk: Option<&ChunkTsfn>,
    settle: Option<&ThreadsafeFunction<(), ErrorStrategy::Fatal>>,
    phase: &Mutex<StopPhase>,
    report: &FinalReport,
) {
    post_with(
        |mode| match chunk {
            Some(chunk) => tsfn_delivery::call(chunk, ChunkEmit::StopFlushed, mode),
            None => Err(Status::Closing),
        },
        |mode| match settle {
            Some(settle) => tsfn_delivery::call(settle, (), mode),
            None => Err(Status::Closing),
        },
        phase,
        report,
    );
}

pub(super) fn post_weak(
    chunk: &ChunkTsfnWeakCell,
    settle: &SettleWeakCell,
    phase: &Mutex<StopPhase>,
    report: &FinalReport,
) {
    post_weak_with(
        chunk,
        settle,
        |chunk, mode| tsfn_delivery::call(chunk, ChunkEmit::StopFlushed, mode),
        |settle, mode| tsfn_delivery::call(settle, (), mode),
        phase,
        report,
    );
}

pub(super) fn post_weak_with<C, S>(
    chunk: &OnceLock<Weak<C>>,
    settle: &OnceLock<Weak<S>>,
    mut send_chunk: impl FnMut(&C, ThreadsafeFunctionCallMode) -> Result<(), Status>,
    mut send_settle: impl FnMut(&S, ThreadsafeFunctionCallMode) -> Result<(), Status>,
    phase: &Mutex<StopPhase>,
    report: &FinalReport,
) {
    let chunk = chunk.get().and_then(Weak::upgrade);
    let settle = settle.get().and_then(Weak::upgrade);
    // An expired owner is a closed queue, so it still needs settlement or the typed sink.
    post_with(
        |mode| match chunk.as_deref() {
            Some(chunk) => send_chunk(chunk, mode),
            None => Err(Status::Closing),
        },
        |mode| match settle.as_deref() {
            Some(settle) => send_settle(settle, mode),
            None => Err(Status::Closing),
        },
        phase,
        report,
    );
}

fn retry_open_queue(
    mut send: impl FnMut(ThreadsafeFunctionCallMode) -> Result<(), Status>,
    report: &FinalReport,
    role: &str,
) -> Result<(), Status> {
    let mut attempt = |mode| {
        let result = send(mode);
        if let Err(status) = result {
            record_cleanup_failure(
                report,
                failure_delivery::queue_error(role, status, flexaudio::Operation::Stop),
            );
        }
        result
    };
    match attempt(ThreadsafeFunctionCallMode::NonBlocking) {
        Ok(()) => Ok(()),
        // A closing queue cannot accept a blocking retry.
        Err(Status::Closing) => Err(Status::Closing),
        // Both native queues are unbounded, so this retry cannot wait on queue capacity on JS.
        Err(_) => attempt(ThreadsafeFunctionCallMode::Blocking),
    }
}

fn post_with(
    chunk: impl FnMut(ThreadsafeFunctionCallMode) -> Result<(), Status>,
    settle: impl FnMut(ThreadsafeFunctionCallMode) -> Result<(), Status>,
    phase: &Mutex<StopPhase>,
    report: &FinalReport,
) {
    if retry_open_queue(chunk, report, "stop chunk").is_ok() {
        return;
    }
    if let Err(status) = retry_open_queue(settle, report, "stop settlement") {
        // Neither queue can settle the stop; release waiters and rescue retained failures.
        drop(take_stop_waiters(phase));
        shutdown::report_unreachable_failure(
            report,
            status,
            "stop settlement",
            flexaudio::Operation::Stop,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_chunk_retry_failures_remain_typed_when_settlement_succeeds() {
        flexaudio_core::test_support::assert_distinct_failures_retained(
            failure_delivery::queue_error(
                "stop chunk",
                Status::QueueFull,
                flexaudio::Operation::Stop,
            ),
            failure_delivery::queue_error(
                "stop chunk",
                Status::Closing,
                flexaudio::Operation::Stop,
            ),
            |first, second| {
                let phase = Mutex::new(StopPhase::Stopping { waiters: vec![] });
                let report = Arc::new(Mutex::new(None));
                let mut attempts = 0;
                post_with(
                    |_| {
                        attempts += 1;
                        Err(if attempts == 1 {
                            Status::QueueFull
                        } else {
                            Status::Closing
                        })
                    },
                    |_| Ok(()),
                    &phase,
                    &report,
                );
                assert_eq!(attempts, 2);
                assert!(matches!(*phase.lock().unwrap(), StopPhase::Stopping { .. }));
                let report = report.lock().unwrap();
                let cleanup = &report.as_ref().unwrap().binding_cleanup;
                assert_eq!(cleanup, &[first.clone(), second]);
                flexaudio::ShutdownReport::new(Some(first), cleanup[1..].to_vec())
                    .result()
                    .unwrap_err()
            },
        );
    }

    #[test]
    fn stop_delivery_retries_only_open_queues_and_uses_settlement_fallback() {
        let phase = Mutex::new(StopPhase::Stopping { waiters: vec![] });
        let report = Arc::new(Mutex::new(None));
        let mut chunk_calls = 0;
        let mut settle_calls = 0;
        post_with(
            |mode| {
                chunk_calls += 1;
                assert!(matches!(mode, ThreadsafeFunctionCallMode::NonBlocking));
                Err(Status::Closing)
            },
            |mode| {
                settle_calls += 1;
                match mode {
                    ThreadsafeFunctionCallMode::NonBlocking => Err(Status::QueueFull),
                    ThreadsafeFunctionCallMode::Blocking => Ok(()),
                }
            },
            &phase,
            &report,
        );
        assert_eq!(chunk_calls, 1);
        assert_eq!(settle_calls, 2);
        assert!(matches!(*phase.lock().unwrap(), StopPhase::Stopping { .. }));

        chunk_calls = 0;
        post_with(
            |_| {
                chunk_calls += 1;
                if chunk_calls == 1 {
                    Err(Status::QueueFull)
                } else {
                    Ok(())
                }
            },
            |_| panic!("accepted chunk delivery must not also settle"),
            &phase,
            &report,
        );
        assert_eq!(chunk_calls, 2);
    }
}
