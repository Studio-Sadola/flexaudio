//! Separate FIFO delivery for helper ownership notifications.
use super::*;
use std::sync::atomic::AtomicUsize;

#[napi(object)]
pub struct JsLifecycleEvent {
    #[napi(js_name = "type")]
    pub kind: String,
    pub reason: Option<String>,
    pub message: Option<String>,
}

pub(super) fn is_lifecycle(event: &Event) -> bool {
    matches!(
        event,
        Event::SilenceProbeStarted
            | Event::SilenceProbeStopped { .. }
            | Event::ClassicLoopbackKeepaliveStarted
            | Event::ClassicLoopbackKeepaliveStopped { .. }
    )
}

fn project(event: Event) -> JsLifecycleEvent {
    let (kind, reason) = match event {
        Event::SilenceProbeStarted => ("silenceProbeStarted", None),
        Event::SilenceProbeStopped { reason } => ("silenceProbeStopped", Some(reason)),
        Event::ClassicLoopbackKeepaliveStarted => ("classicLoopbackKeepaliveStarted", None),
        Event::ClassicLoopbackKeepaliveStopped { reason } => {
            ("classicLoopbackKeepaliveStopped", Some(reason))
        }
        _ => {
            return JsLifecycleEvent {
                kind: "unknown".into(),
                reason: None,
                message: Some("unrecognized helper lifecycle event".into()),
            }
        }
    };
    JsLifecycleEvent {
        kind: kind.into(),
        reason: reason.map(|r| {
            use flexaudio::HelperStopReason::*;
            match r {
                SignalObserved | PermissionDenied | Inconclusive | TimedOut | StreamStopped
                | SourceSwitched | Recovery | StartupFailed | RuntimeFailed | Cancelled => {
                    r.as_str()
                }
                _ => "unknown",
            }
            .into()
        }),
        message: None,
    }
}

enum Delivery {
    Event(Arc<UserCallback>, JsLifecycleEvent),
    Release(Arc<UserCallback>),
}

pub(super) struct LifecycleDelivery {
    queue: ThreadsafeFunction<Delivery, ErrorStrategy::Fatal>,
    handler: Mutex<Option<Arc<UserCallback>>>,
    drain: Arc<LifecycleDrain>,
    report: FinalReport,
}

#[derive(Default)]
struct LifecycleDrain {
    pending: AtomicUsize,
    waiting: AtomicBool,
}

impl LifecycleDrain {
    fn defer_stop(&self) -> bool {
        if self.pending.load(Ordering::SeqCst) != 0 {
            self.waiting.store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    fn resume_if_drained(&self, post: impl FnOnce()) {
        if self.pending.load(Ordering::SeqCst) == 0 && self.waiting.swap(false, Ordering::SeqCst) {
            post();
        }
    }
}

pub(super) fn retain(env: &Env, function: &napi::JsFunction) -> napi::Result<Arc<UserCallback>> {
    UserCallback::retain(env, function)
}

impl LifecycleDelivery {
    pub fn new(
        env: &Env,
        callback: Option<&napi::JsFunction>,
        chunk: ChunkTsfnWeakCell,
        settle: stop_delivery::SettleWeakCell,
        phase: Arc<Mutex<StopPhase>>,
        report: FinalReport,
    ) -> napi::Result<Arc<Self>> {
        let drain = Arc::new(LifecycleDrain::default());
        let pump = env.create_function_from_closure("flexaudioLifecyclePump", |ctx| {
            ctx.env.get_undefined()
        })?;
        let mut queue = pump
            .create_threadsafe_function::<Delivery, Unknown, _, ErrorStrategy::Fatal>(0, {
                let drain = drain.clone();
                let report = report.clone();
                move |ctx: ThreadSafeCallContext<Delivery>| {
                    let result = match ctx.value {
                        Delivery::Release(callback) => {
                            callback.release();
                            Ok(())
                        }
                        Delivery::Event(callback, event) => {
                            let mut value = ptr::null_mut();
                            let result = (|| {
                                check_status!(unsafe {
                                    sys::napi_get_reference_value(
                                        ctx.env.raw(),
                                        callback.refer(),
                                        &mut value,
                                    )
                                })?;
                                let function: Function<JsLifecycleEvent, Unknown> =
                                    unsafe { Function::from_napi_value(ctx.env.raw(), value)? };
                                function.call(event).map(|_| ())
                            })();
                            drain.pending.fetch_sub(1, Ordering::SeqCst);
                            result
                        }
                    };
                    drain.resume_if_drained(|| {
                        stop_delivery::post_weak(&chunk, &settle, &phase, &report);
                    });
                    result?;
                    Ok(Vec::<Unknown>::new())
                }
            })?;
        queue.unref(env)?;
        Ok(Arc::new(Self {
            queue,
            handler: Mutex::new(callback.map(|f| retain(env, f)).transpose()?),
            drain,
            report,
        }))
    }

    pub fn emit(&self, event: Event) {
        // These notices contain no failure payload; typed failures use the retained shutdown report.
        if let Some(handler) = self.handler.lock().unwrap_or_else(lock_poisoned).clone() {
            self.drain.pending.fetch_add(1, Ordering::SeqCst);
            let delivery = tsfn_delivery::call(
                &self.queue,
                Delivery::Event(handler, project(event)),
                ThreadsafeFunctionCallMode::NonBlocking,
            );
            if delivery.is_err() {
                self.drain.pending.fetch_sub(1, Ordering::SeqCst);
            }
            failure_delivery::retain_queue_result(
                &self.report,
                delivery,
                "lifecycle notification",
                flexaudio::Operation::Normalize,
            );
        }
    }

    pub fn replace(&self, handler: Arc<UserCallback>) {
        if let Some(old) = self
            .handler
            .lock()
            .unwrap_or_else(lock_poisoned)
            .replace(handler)
        {
            self.release(old);
        }
    }

    pub fn release(&self, handler: Arc<UserCallback>) {
        match tsfn_delivery::call(
            &self.queue,
            Delivery::Release(handler),
            ThreadsafeFunctionCallMode::NonBlocking,
        ) {
            Ok(()) => {}
            Err(Status::Closing) => {
                // The JS host is closing; UserCallback's finalizer still releases its reference.
            }
            Err(status) => {
                record_cleanup_failure(
                    &self.report,
                    failure_delivery::queue_error(
                        "lifecycle callback release",
                        status,
                        flexaudio::Operation::Stop,
                    ),
                );
            }
        }
    }

    /// Called on JS after the bridge has stopped queueing lifecycle notifications.
    pub fn defer_stop(&self) -> bool {
        self.drain.defer_stop()
    }

    pub fn finish(&self) {
        if let Some(handler) = self.handler.lock().unwrap_or_else(lock_poisoned).take() {
            handler.release();
        }
    }
}

/// Device-free fixture exercising the real bridge routing and stop drain.
pub(super) fn mock_backend() -> Box<dyn flexaudio::CaptureBackend> {
    #[cfg(feature = "test-faults")]
    {
        Box::new(LifecycleMockBackend {
            backend: flexaudio::MockBackend::new(48_000, 2, 0.0),
            events: VecDeque::new(),
            running: false,
            reason: flexaudio::HelperStopReason::StreamStopped,
        })
    }
    #[cfg(not(feature = "test-faults"))]
    {
        Box::new(flexaudio::MockBackend::new(48_000, 2, 0.0))
    }
}

#[cfg(feature = "test-faults")]
struct LifecycleMockBackend {
    backend: flexaudio::MockBackend,
    events: VecDeque<Event>,
    running: bool,
    reason: flexaudio::HelperStopReason,
}

#[cfg(feature = "test-faults")]
impl flexaudio::CaptureBackend for LifecycleMockBackend {
    fn native_format(&self) -> (u32, u16) {
        self.backend.native_format()
    }
    fn start(&mut self, sink: flexaudio::core::backend::RawSink) -> flexaudio::Result<()> {
        self.backend.start(sink)?;
        if !self.running {
            self.running = true;
            self.events.push_back(Event::SilenceProbeStarted);
            self.events
                .push_back(Event::ClassicLoopbackKeepaliveStarted);
        }
        Ok(())
    }
    fn stop(&mut self) {
        self.backend.stop();
        if self.running {
            self.running = false;
            self.events.push_back(Event::SilenceProbeStopped {
                reason: self.reason,
            });
            self.events
                .push_back(Event::ClassicLoopbackKeepaliveStopped {
                    reason: self.reason,
                });
        }
    }
    fn set_helper_stop_reason(&mut self, reason: flexaudio::HelperStopReason) {
        self.reason = reason;
    }
    fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }
}

#[cfg(test)]
mod deferred_stop_tests {
    use super::*;

    #[test]
    fn drained_lifecycle_rescues_failures_after_closed_or_expired_queues() {
        const CHILD: &str = "FLEXAUDIO_NAPI_DEFERRED_STOP_TEST";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "lifecycle::deferred_stop_tests::drained_lifecycle_rescues_failures_after_closed_or_expired_queues",
                    "--exact",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "stdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let received = Arc::new(Mutex::new(Vec::new()));
        let output = received.clone();
        flexaudio::set_drop_failure_handler(Some(Box::new(move |failure| {
            output.lock().unwrap().push(failure.clone());
        })));
        for expired in [false, true] {
            flexaudio_core::test_support::assert_distinct_failures_retained(
                flexaudio::Error::DeviceLost,
                failure_delivery::queue_error(
                    "stop chunk",
                    Status::Closing,
                    flexaudio::Operation::Stop,
                ),
                |first, _second| {
                    let report = Arc::new(Mutex::new(Some(shutdown::ShutdownReport::from_core(
                        Some(flexaudio::ShutdownReport::new(Some(first), vec![])),
                    ))));
                    let phase = Mutex::new(StopPhase::Stopping { waiters: vec![] });
                    let drain = LifecycleDrain::default();
                    drain.pending.fetch_add(2, Ordering::SeqCst);
                    assert!(drain.defer_stop());
                    drain.pending.fetch_sub(1, Ordering::SeqCst);
                    drain.resume_if_drained(|| panic!("one lifecycle notice is still pending"));
                    assert!(received.lock().unwrap().is_empty());
                    drain.pending.fetch_sub(1, Ordering::SeqCst);
                    drain.resume_if_drained(|| {
                        let chunk_owner = Arc::new(());
                        let settle_owner = Arc::new(());
                        let chunk = OnceLock::from(Arc::downgrade(&chunk_owner));
                        let settle = OnceLock::from(Arc::downgrade(&settle_owner));
                        let live_owners = if expired {
                            drop((chunk_owner, settle_owner));
                            None
                        } else {
                            Some((chunk_owner, settle_owner))
                        };
                        stop_delivery::post_weak_with(
                            &chunk,
                            &settle,
                            |_, _| {
                                assert!(!expired, "an expired queue must not be called");
                                Err(Status::Closing)
                            },
                            |_, _| {
                                assert!(!expired, "an expired queue must not be called");
                                Err(Status::Closing)
                            },
                            &phase,
                            &report,
                        );
                        drop(live_owners);
                    });
                    assert!(matches!(*phase.lock().unwrap(), StopPhase::Stopped));
                    drain.resume_if_drained(|| panic!("deferred stop must resume only once"));
                    let mut received = received.lock().unwrap();
                    assert_eq!(
                        received.len(),
                        1,
                        "deferred stop failure must reach its sink"
                    );
                    let failure = received.pop().unwrap();
                    assert_eq!(failure.owner, flexaudio::DropFailureOwner::Stream);
                    assert!(failure
                        .report
                        .cleanup()
                        .contains(&failure_delivery::queue_error(
                            "stop settlement",
                            Status::Closing,
                            flexaudio::Operation::Stop
                        )));
                    failure.report.result().unwrap_err()
                },
            );
        }
        flexaudio::set_drop_failure_handler(None);
    }
}
