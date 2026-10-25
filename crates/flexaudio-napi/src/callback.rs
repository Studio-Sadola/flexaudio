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
        let refer: sys::napi_ref = self.refer.swap(ptr::null_mut(), Ordering::SeqCst).cast();
        if !refer.is_null() {
            // Reference release carries no parked failure payload; JS owns the finalizer lifetime.
            let _ = unsafe { sys::napi_delete_reference(self.env, refer) };
        }
    }
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
