//! Integrated VAD owns its clock; gap finalization always uses the pre-gap anchor.
use super::{vad_event_to_js_abs, CoreVad, JsVadEvent, VadError, VadEvent};

pub(super) trait Engine {
    fn position(&self) -> u64;
    fn process(
        &mut self,
        samples: &[f32],
        rate: u32,
        channels: u16,
    ) -> Result<Vec<VadEvent>, VadError>;
    fn flush(&mut self) -> Result<Vec<VadEvent>, VadError>;
    fn reset(&mut self) -> Result<(), VadError>;
}

impl Engine for CoreVad {
    fn position(&self) -> u64 {
        self.converted_sample_position()
    }
    fn process(
        &mut self,
        samples: &[f32],
        rate: u32,
        channels: u16,
    ) -> Result<Vec<VadEvent>, VadError> {
        self.process_pcm(samples, rate, channels)
    }
    fn flush(&mut self) -> Result<Vec<VadEvent>, VadError> {
        self.flush()
    }
    fn reset(&mut self) -> Result<(), VadError> {
        self.reset()
    }
}

pub(super) struct IntegratedVad<E = CoreVad> {
    engine: E,
    rate: u32,
    last_dropped: u32,
    anchor_sample: u64,
    pub anchor_pts: i64,
}

impl<E: Engine> IntegratedVad<E> {
    pub fn new(engine: E, rate: u32) -> Self {
        Self {
            engine,
            rate,
            last_dropped: 0,
            anchor_sample: 0,
            anchor_pts: 0,
        }
    }

    fn marshal(&self, events: Vec<VadEvent>) -> Result<Vec<JsVadEvent>, VadError> {
        events
            .into_iter()
            .map(|event| {
                let sample = match event {
                    VadEvent::SpeechStart { at_sample } | VadEvent::SpeechEnd { at_sample } => {
                        at_sample
                    }
                };
                // Signed *wide* arithmetic only for time offsets; the JS counter remains u64.
                let delta = i128::from(sample) - i128::from(self.anchor_sample);
                let pts =
                    i128::from(self.anchor_pts) + delta * 1_000_000_000 / i128::from(self.rate);
                let pts = i64::try_from(pts).map_err(|_| {
                    VadError::InvalidFormat("VAD timestamp exceeds the signed time range".into())
                })?;
                Ok(vad_event_to_js_abs(event, Some(pts)))
            })
            .collect()
    }

    pub fn process(
        &mut self,
        samples: &[f32],
        rate: u32,
        channels: u16,
        pts: i64,
        discontinuity: bool,
        dropped: u32,
    ) -> Result<Vec<JsVadEvent>, VadError> {
        let gap = discontinuity || dropped > self.last_dropped;
        self.last_dropped = dropped;
        let mut before_gap = Vec::new();
        if gap {
            let flushed = self.engine.flush();
            // Attempt recovery even when flush fails, but never process this affected chunk.
            let reset = self.engine.reset();
            before_gap = self.marshal(flushed?)?;
            reset?;
        }
        self.anchor_sample = self.engine.position();
        self.anchor_pts = pts;
        let events = self.engine.process(samples, rate, channels)?;
        before_gap.extend(self.marshal(events)?);
        Ok(before_gap)
    }

    pub fn flush(&mut self) -> Result<Vec<JsVadEvent>, VadError> {
        let events = self.engine.flush()?;
        self.marshal(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Probe {
        position: u64,
        calls: Vec<&'static str>,
        fail_flush: bool,
    }
    impl Engine for Probe {
        fn position(&self) -> u64 {
            self.position
        }
        fn process(&mut self, _: &[f32], _: u32, _: u16) -> Result<Vec<VadEvent>, VadError> {
            self.calls.push("process");
            let start = self.position;
            self.position += 320;
            Ok(vec![VadEvent::SpeechStart { at_sample: start }])
        }
        fn flush(&mut self) -> Result<Vec<VadEvent>, VadError> {
            self.calls.push("flush");
            if self.fail_flush {
                return Err(VadError::Reset("injected flush failure".into()));
            }
            Ok(vec![VadEvent::SpeechEnd {
                at_sample: self.position,
            }])
        }
        fn reset(&mut self) -> Result<(), VadError> {
            self.calls.push("reset");
            self.position = 0;
            Ok(())
        }
    }

    #[test]
    fn vad_discontinuity_flushes_before_reset() {
        for (discontinuity, dropped) in [(true, 0), (false, 1)] {
            let mut vad = IntegratedVad::new(
                Probe {
                    position: 0,
                    calls: Vec::new(),
                    fail_flush: false,
                },
                16_000,
            );
            vad.process(&[], 16_000, 1, 1_000_000_000, false, 0)
                .unwrap();
            let events = vad
                .process(&[], 16_000, 1, 5_000_000_000, discontinuity, dropped)
                .unwrap();
            assert_eq!(vad.engine.calls, ["process", "flush", "reset", "process"]);
            assert_eq!(events[0].kind, "speechEnd");
            assert_eq!(events[0].at_ns, Some(1_020_000_000));
            assert_eq!(events[1].kind, "speechStart");
            assert_eq!(events[1].at_ns, Some(5_000_000_000));
        }
    }

    #[test]
    fn vad_discontinuity_failed_flush_control() {
        let mut vad = IntegratedVad::new(
            Probe {
                position: 320,
                calls: Vec::new(),
                fail_flush: true,
            },
            16_000,
        );
        for (discontinuity, dropped) in [(true, 0), (false, 1)] {
            let error = vad
                .process(&[], 16_000, 1, 5_000_000_000, discontinuity, dropped)
                .unwrap_err();
            assert!(error.to_string().contains("injected flush failure"));
            assert_eq!(vad.engine.calls, ["flush", "reset"]);
            assert_eq!(vad.anchor_pts, 0);
            assert_eq!(vad.engine.position, 0);
            vad.engine.calls.clear();
        }
    }
}
