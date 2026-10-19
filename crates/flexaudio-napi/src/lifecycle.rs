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
    pending: Arc<AtomicUsize>,
    waiting: Arc<AtomicBool>,
}

pub(super) fn retain(env: &Env, function: &napi::JsFunction) -> napi::Result<Arc<UserCallback>> {
    let mut refer = ptr::null_mut();
    check_status!(unsafe { sys::napi_create_reference(env.raw(), function.raw(), 1, &mut refer) })?;
    Ok(UserCallback::new(env.raw(), refer))
}

impl LifecycleDelivery {
    pub fn new(
        env: &Env,
        callback: Option<&napi::JsFunction>,
        chunk: ChunkTsfnWeakCell,
    ) -> napi::Result<Arc<Self>> {
        let pending = Arc::new(AtomicUsize::new(0));
        let waiting = Arc::new(AtomicBool::new(false));
        let pump = env.create_function_from_closure("flexaudioLifecyclePump", |ctx| {
            ctx.env.get_undefined()
        })?;
        let mut queue = pump
            .create_threadsafe_function::<Delivery, Unknown, _, ErrorStrategy::Fatal>(0, {
                let pending = pending.clone();
                let waiting = waiting.clone();
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
                            pending.fetch_sub(1, Ordering::SeqCst);
                            result
                        }
                    };
                    if pending.load(Ordering::SeqCst) == 0 && waiting.swap(false, Ordering::SeqCst)
                    {
                        if let Some(chunk) = chunk.get().and_then(Weak::upgrade) {
                            chunk.call(
                                ChunkEmit::StopFlushed,
                                ThreadsafeFunctionCallMode::NonBlocking,
                            );
                        }
                    }
                    result?;
                    Ok(Vec::<Unknown>::new())
                }
            })?;
        queue.unref(env)?;
        Ok(Arc::new(Self {
            queue,
            handler: Mutex::new(callback.map(|f| retain(env, f)).transpose()?),
            pending,
            waiting,
        }))
    }

    pub fn emit(&self, event: Event) {
        if let Some(handler) = self.handler.lock().unwrap_or_else(lock_poisoned).clone() {
            self.pending.fetch_add(1, Ordering::SeqCst);
            if self.queue.call(
                Delivery::Event(handler, project(event)),
                ThreadsafeFunctionCallMode::NonBlocking,
            ) != Status::Ok
            {
                self.pending.fetch_sub(1, Ordering::SeqCst);
            }
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
        self.queue.call(
            Delivery::Release(handler),
            ThreadsafeFunctionCallMode::NonBlocking,
        );
    }

    /// Called on JS after the bridge has stopped queueing lifecycle notifications.
    pub fn defer_stop(&self) -> bool {
        if self.pending.load(Ordering::SeqCst) != 0 {
            self.waiting.store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
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
