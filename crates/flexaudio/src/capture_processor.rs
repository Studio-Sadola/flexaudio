//! Shared gain snapshot before canonical capture fans out into output converters.
use super::*;

pub(super) struct CaptureProcessor {
    inner: Option<Box<dyn InnerProcessor>>,
    gain_bits: Arc<AtomicU32>,
}

impl CaptureProcessor {
    pub(super) fn new(inner: Option<Box<dyn InnerProcessor>>, gain_bits: Arc<AtomicU32>) -> Self {
        Self { inner, gain_bits }
    }
}

impl InnerProcessor for CaptureProcessor {
    fn process(&mut self, samples: &mut [f32]) {
        if let Some(inner) = self.inner.as_mut() {
            inner.process(samples);
        }
        apply_gain(
            samples,
            f32::from_bits(self.gain_bits.load(Ordering::Relaxed)),
        );
    }

    fn flush(&mut self) -> Vec<f32> {
        let mut tail = self
            .inner
            .as_mut()
            .map(|inner| inner.flush())
            .unwrap_or_default();
        apply_gain(
            &mut tail,
            f32::from_bits(self.gain_bits.load(Ordering::Relaxed)),
        );
        tail
    }
}
