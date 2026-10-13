//! Integrated VAD owns its clock; gap finalization always uses the pre-gap anchor.
use super::{vad_event_to_js_abs, CoreVad, JsVadEvent, VadError, VadEvent};

use flexaudio_vad::{process_capture_chunk, VadCaptureEngine};

pub(super) trait Engine: VadCaptureEngine {
    fn process(
        &mut self,
        samples: &[f32],
        rate: u32,
        channels: u16,
    ) -> Result<Vec<VadEvent>, VadError>;
}

impl Engine for CoreVad {
    fn process(
        &mut self,
        samples: &[f32],
        rate: u32,
        channels: u16,
    ) -> Result<Vec<VadEvent>, VadError> {
        self.process_pcm(samples, rate, channels)
    }
}

pub(super) struct IntegratedVad<E = CoreVad> {
    engine: E,
    rate: u32,
    last_dropped: u32,
    anchor_sample: u64,
    has_anchor: bool,
    pub anchor_pts: i64,
    pending_events: Vec<JsVadEvent>,
}

impl<E: Engine> IntegratedVad<E> {
    pub fn new(engine: E, rate: u32) -> Self {
        Self {
            engine,
            rate,
            last_dropped: 0,
            anchor_sample: 0,
            has_anchor: false,
            anchor_pts: 0,
            pending_events: Vec::new(),
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
        let outcome = process_capture_chunk(&mut self.engine, gap, E::flush, |engine| {
            engine.process(samples, rate, channels)
        });
        // Map old events before installing the post-gap clock. Retention happens before
        // propagating either reset or processing failure, so a later poll/stop can deliver them.
        let flushed = self.marshal(outcome.flushed)?;
        self.pending_events.extend(flushed);
        if gap {
            // Even a failed flush attempts reset. Its next accepted input needs a fresh origin.
            self.has_anchor = false;
        }
        let processed = outcome.processed?;
        if !self.has_anchor {
            if let Some(position) = outcome.process_start_sample {
                self.anchor_sample = position;
                self.anchor_pts = pts;
                self.has_anchor = true;
            }
        }
        // Keep one origin for the whole VAD epoch. Reanchoring historical boundaries to
        // later, rounded chunk PTS can shift SpeechStart(0) below recording zero.
        let events = self.marshal(processed)?;
        self.pending_events.extend(events);
        Ok(self.take_pending_events())
    }

    /// Drain events already finalized on their original clock, including after an error.
    pub fn take_pending_events(&mut self) -> Vec<JsVadEvent> {
        std::mem::take(&mut self.pending_events)
    }

    pub fn flush(&mut self) -> Result<Vec<JsVadEvent>, VadError> {
        let flushed = self.engine.flush();
        let events = match flushed {
            Ok(events) => events,
            Err(error) => {
                let retained = self.engine.take_pending_events();
                let mapped = self.marshal(retained)?;
                self.pending_events.extend(mapped);
                return Err(error);
            }
        };
        let mapped = self.marshal(events);
        // A successful flush reset the engine; map its events on the old clock first.
        self.has_anchor = false;
        self.pending_events.extend(mapped?);
        Ok(self.take_pending_events())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Probe {
        position: u64,
        calls: Vec<&'static str>,
        fail_flush: bool,
        fail_process: bool,
        open: bool,
    }
    impl VadCaptureEngine for Probe {
        fn position(&self) -> u64 {
            self.position
        }
        fn flush(&mut self) -> Result<Vec<VadEvent>, VadError> {
            self.calls.push("flush");
            if self.fail_flush {
                return Err(VadError::Reset("injected flush failure".into()));
            }
            let position = self.position;
            self.position = 0;
            Ok(if std::mem::take(&mut self.open) {
                vec![VadEvent::SpeechEnd {
                    at_sample: position,
                }]
            } else {
                Vec::new()
            })
        }
        fn reset(&mut self) -> Result<(), VadError> {
            self.calls.push("reset");
            self.position = 0;
            Ok(())
        }
    }

    impl Engine for Probe {
        fn process(&mut self, _: &[f32], _: u32, _: u16) -> Result<Vec<VadEvent>, VadError> {
            self.calls.push("process");
            if std::mem::take(&mut self.fail_process) {
                return Err(VadError::Inference(
                    "injected one-shot processing failure".into(),
                ));
            }
            self.open = true;
            let start = self.position;
            self.position += 320;
            Ok(vec![VadEvent::SpeechStart { at_sample: start }])
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
                    fail_process: false,
                    open: false,
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
    fn destructive_flush_events_survive_one_shot_process_failure() -> Result<(), VadError> {
        for use_flush in [false, true] {
            let mut vad = IntegratedVad::new(
                Probe {
                    position: 0,
                    calls: Vec::new(),
                    fail_flush: false,
                    fail_process: false,
                    open: false,
                },
                16_000,
            );
            vad.process(&[], 16_000, 1, 1_000_000_000, false, 0)?;
            vad.engine.fail_process = true;
            let error = vad.process(&[], 16_000, 1, 5_000_000_000, true, 0);
            assert!(
                matches!(error, Err(VadError::Inference(_))),
                "processing failure must still be reported"
            );
            let recovered = if use_flush {
                vad.flush()?
            } else {
                vad.process(&[], 16_000, 1, 6_000_000_000, false, 0)?
            };
            assert_eq!(
                recovered
                    .iter()
                    .filter(|event| event.kind == "speechEnd")
                    .count(),
                1,
                "a successful destructive flush must not lose SpeechEnd when processing fails"
            );
            assert_eq!(recovered[0].at_sample.get_u64(), (false, 320, true));
            assert_eq!(
                recovered[0].at_ns,
                Some(1_020_000_000),
                "retain the pre-gap clock"
            );
            if !use_flush {
                assert_eq!(recovered[1].kind, "speechStart");
                assert_eq!(recovered[1].at_ns, Some(6_000_000_000));
            }
            let next = vad.flush()?;
            assert!(
                next.iter().all(|event| event.at_ns != Some(1_020_000_000)),
                "retained events must be delivered only once"
            );
        }
        Ok(())
    }

    struct DelayedBoundaryEngine {
        position: u64,
        open: bool,
    }
    impl VadCaptureEngine for DelayedBoundaryEngine {
        fn position(&self) -> u64 {
            self.position
        }
        fn flush(&mut self) -> Result<Vec<VadEvent>, VadError> {
            let events = if std::mem::take(&mut self.open) {
                vec![
                    VadEvent::SpeechStart { at_sample: 0 },
                    VadEvent::SpeechEnd {
                        at_sample: self.position,
                    },
                ]
            } else {
                Vec::new()
            };
            self.position = 0;
            Ok(events)
        }
        fn reset(&mut self) -> Result<(), VadError> {
            self.position = 0;
            Ok(())
        }
    }
    impl Engine for DelayedBoundaryEngine {
        fn process(&mut self, _: &[f32], _: u32, _: u16) -> Result<Vec<VadEvent>, VadError> {
            self.open = true;
            self.position += 320;
            Ok(Vec::new())
        }
    }

    #[test]
    fn late_finalization_keeps_recording_origin_despite_rounded_chunk_pts() -> Result<(), VadError>
    {
        let mut vad = IntegratedVad::new(
            DelayedBoundaryEngine {
                position: 0,
                open: false,
            },
            16_000,
        );
        assert!(vad.process(&[], 16_000, 1, 0, false, 0)?.is_empty());
        // Caller-visible tap timestamps can round a later 20 ms sample offset by 160 ns.
        // SpeechStart(0) belongs to the first input, not this later chunk's rounded clock.
        assert!(vad
            .process(&[], 16_000, 1, 19_999_840, false, 0)?
            .is_empty());
        let events = vad.flush()?;
        assert_eq!(events[0].at_ns, Some(0), "historical SpeechStart must stay at recording zero; per-chunk reanchoring produces -160 ns");
        assert_eq!(
            events[1].at_ns,
            Some(40_000_000),
            "map the whole segment on its original sample clock"
        );
        assert!(
            vad.flush()?.is_empty(),
            "closing events must be delivered only once"
        );
        // Explicit flush resets the VAD sample clock; the next input defines a new origin.
        vad.process(&[], 16_000, 1, 80_000_000, false, 0)?;
        let restarted = vad.flush()?;
        assert_eq!(restarted[0].at_ns, Some(80_000_000));
        assert_eq!(restarted[1].at_ns, Some(100_000_000));
        Ok(())
    }

    #[test]
    fn vad_discontinuity_failed_flush_control() {
        let mut vad = IntegratedVad::new(
            Probe {
                position: 320,
                calls: Vec::new(),
                fail_flush: true,
                fail_process: false,
                open: true,
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
