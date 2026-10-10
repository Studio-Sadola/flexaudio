//! Library-owned PCM allocations and frame metadata for the frozen FlexChunk v1 ABI.
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

struct Allocation {
    samples: Box<[f32]>,
    len: usize,
    frame_index: u64,
}

fn allocations() -> &'static Mutex<HashMap<usize, Allocation>> {
    static ALLOCATIONS: OnceLock<Mutex<HashMap<usize, Allocation>>> = OnceLock::new();
    ALLOCATIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn store(samples: Vec<f32>, frame_index: u64) -> (*mut f32, usize) {
    let len = samples.len();
    // Empty carriers still need a distinct live allocation as their registry key.
    let mut samples = if len == 0 {
        vec![0.0].into_boxed_slice()
    } else {
        samples.into_boxed_slice()
    };
    let data = samples.as_mut_ptr();
    allocations()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            data as usize,
            Allocation {
                samples,
                len,
                frame_index,
            },
        );
    (data, len)
}

pub(crate) fn frame_index(data: *mut f32, len: usize) -> Option<u64> {
    allocations()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(data as usize))
        .filter(|allocation| allocation.len == len)
        .map(|allocation| allocation.frame_index)
}

pub(crate) fn release(data: *mut f32, len: usize) {
    let mut allocations = allocations().lock().unwrap_or_else(|e| e.into_inner());
    if allocations
        .get(&(data as usize))
        .is_some_and(|allocation| allocation.len == len)
    {
        // Dropping the owner frees exactly its own storage, independent of caller fields.
        if let Some(allocation) = allocations.remove(&(data as usize)) {
            drop(allocation.samples);
        }
    }
}
