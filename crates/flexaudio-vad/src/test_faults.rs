//! Thread-local invalid model bytes for binding initialization-order tests.
use std::{cell::Cell, marker::PhantomData, rc::Rc};

thread_local! {
    static LOADS: Cell<Option<usize>> = const { Cell::new(None) };
}

/// Substitute invalid ONNX bytes until this thread-bound guard is dropped.
/// Nested guards on the same thread are not supported. Other threads are unaffected.
pub struct ModelLoadFault(PhantomData<Rc<()>>);

impl ModelLoadFault {
    /// Enable the fault and start counting model parser attempts on this thread.
    pub fn new() -> Self {
        LOADS.with(|loads| {
            assert!(loads.get().is_none(), "model load fault already active");
            loads.set(Some(0));
        });
        Self(PhantomData)
    }

    /// Count attempts to load the invalid model while this guard is active.
    pub fn attempts(&self) -> usize {
        LOADS.with(|loads| loads.get().expect("model load fault is active"))
    }
}

impl Default for ModelLoadFault {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ModelLoadFault {
    fn drop(&mut self) {
        LOADS.with(|loads| loads.set(None));
    }
}

pub(crate) fn model_bytes(original: &'static [u8]) -> &'static [u8] {
    LOADS.with(|loads| match loads.get() {
        Some(count) => {
            loads.set(Some(count + 1));
            // A truncated protobuf varint is invalid ONNX input. Use the real
            // parser so failure follows the production ModelLoad error path.
            &[0xff]
        }
        None => original,
    })
}
