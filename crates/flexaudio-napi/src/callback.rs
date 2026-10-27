//! JS references whose last Rust owner may be dropped on a worker thread.
use super::*;
use std::ffi::c_void;
use std::sync::atomic::AtomicPtr;

struct JsReference {
    env: sys::napi_env,
    refer: AtomicPtr<c_void>,
}

// The pointer may cross threads, but reading/deleting the JS reference is restricted
// to JS callbacks and the TSFN finalizer below.
unsafe impl Send for JsReference {}
unsafe impl Sync for JsReference {}

impl JsReference {
    /// JS thread only. Explicit cleanup and finalization share this idempotent path.
    fn release(&self) {
        release_reference_with(&self.refer, |refer| unsafe {
            sys::napi_delete_reference(self.env, refer)
        });
    }
}

fn reference_release_error(raw: sys::napi_status) -> flexaudio::Error {
    let status = Status::from(raw);
    let error = failure_delivery::queue_error(
        "callback reference release",
        status,
        flexaudio::Operation::Stop,
    );
    if status == Status::Unknown {
        error.with_diagnostic_detail(format!("native N-API status code: {raw}"))
    } else {
        error
    }
}

fn release_reference_with(
    reference: &AtomicPtr<c_void>,
    release: impl FnOnce(sys::napi_ref) -> sys::napi_status,
) {
    let refer: sys::napi_ref = reference.swap(ptr::null_mut(), Ordering::SeqCst).cast();
    if refer.is_null() {
        return;
    }
    let status = release(refer);
    if Status::from(status) != Status::Ok {
        // All retained UserCallbacks belong to stream chunk, event or lifecycle delivery.
        failure_delivery::report_failed(
            flexaudio::DropFailureOwner::Stream,
            &flexaudio::ShutdownReport::new(None, Vec::new()),
            reference_release_error(status),
        );
    }
    // The native call may already have released ownership on failure; never delete twice.
}

/// Owned exclusively by the cleanup TSFN's callback closure. N-API finalizes that
/// closure on the JS thread, including when the last TSFN handle dies on a worker.
struct FinalizeReference(Arc<JsReference>);

impl FinalizeReference {
    fn release(&self) {
        self.0.release();
    }
}

impl Drop for FinalizeReference {
    fn drop(&mut self) {
        self.release();
    }
}

pub(super) struct UserCallback {
    reference: Arc<JsReference>,
    // Never called: releasing its last handle schedules JS-thread finalization.
    // Unlike napi_delete_reference, dropping a TSFN is safe on any thread.
    _cleanup: ThreadsafeFunction<(), ErrorStrategy::Fatal>,
}

impl UserCallback {
    pub fn retain(env: &Env, function: &impl NapiRaw) -> napi::Result<Arc<Self>> {
        let mut refer = ptr::null_mut();
        // No callback is owned yet; failed reference creation propagates to the JS caller.
        check_status!(unsafe {
            sys::napi_create_reference(env.raw(), function.raw(), 1, &mut refer)
        })?;
        let reference = Arc::new(JsReference {
            env: env.raw(),
            refer: AtomicPtr::new(refer.cast()),
        });
        let finalizer = FinalizeReference(reference.clone());
        let pump = env.create_function_from_closure("flexaudioReferenceCleanup", |ctx| {
            ctx.env.get_undefined()
        })?;
        let mut cleanup =
            pump.create_threadsafe_function(0, move |_: ThreadSafeCallContext<()>| {
                finalizer.release();
                Ok(Vec::<Unknown>::new())
            })?;
        cleanup.unref(env)?;
        Ok(Arc::new(Self {
            reference,
            _cleanup: cleanup,
        }))
    }

    /// JS thread only, while this callback is still registered.
    pub fn refer(&self) -> sys::napi_ref {
        self.reference.refer.load(Ordering::SeqCst).cast()
    }

    /// JS thread only. Release promptly at stop or handler replacement; the TSFN
    /// finalizer also covers failed construction, channel teardown and unwinding.
    pub fn release(&self) {
        self.reference.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_release_is_once_and_refusals_reach_the_typed_receiver() {
        const CHILD: &str = "FLEXAUDIO_NAPI_REFERENCE_RELEASE_TEST";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "callback::tests::reference_release_is_once_and_refusals_reach_the_typed_receiver",
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
        let refer = std::ptr::NonNull::<u8>::dangling().as_ptr().cast();
        let success = AtomicPtr::new(refer);
        release_reference_with(&success, |observed| {
            assert_eq!(observed.cast(), refer);
            sys::Status::napi_ok
        });
        release_reference_with(&success, |_| panic!("reference cannot be released twice"));
        assert!(received.lock().unwrap().is_empty());
        assert_ne!(reference_release_error(23), reference_release_error(24));
        for status in [Status::GenericFailure.into(), 23, 24] {
            let reference = AtomicPtr::new(refer);
            release_reference_with(&reference, |_| status);
            release_reference_with(&reference, |_| panic!("a refused delete cannot be retried"));
            assert!(reference.load(Ordering::SeqCst).is_null());
            let mut received = received.lock().unwrap();
            assert_eq!(received.len(), 1);
            let failure = received.pop().unwrap();
            assert_eq!(failure.owner, flexaudio::DropFailureOwner::Stream);
            assert!(failure.report.primary().is_none());
            assert_eq!(failure.report.cleanup(), &[reference_release_error(status)]);
            let error = &failure.report.cleanup()[0];
            let contextual = match error {
                flexaudio::Error::Diagnostic { source, .. } => source.as_ref(),
                error => error,
            };
            assert!(
                matches!(contextual, flexaudio::Error::Context { context, .. }
                if context.operation() == flexaudio::Operation::Stop)
            );
        }
        flexaudio::set_drop_failure_handler(None);
    }
}
