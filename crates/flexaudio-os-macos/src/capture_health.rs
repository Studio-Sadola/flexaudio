//! Device-free capture-health policy and the callback's atomic observation mailbox.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub(crate) const ADVISORY_DETAIL: &str = "System/process capture delivered only bit-exact zero samples for five continuous seconds while an eligible process had output I/O active. System Audio Recording permission may be missing; genuine digital silence can also cause this. Check macOS System Settings > Privacy & Security > Screen & System Audio Recording, allow System Audio Recording, then restart the app and retry.";
const WINDOW: Duration = Duration::from_secs(5);
const MAX_OBSERVATION_GAP: Duration = Duration::from_millis(250);

#[derive(Default)]
pub(crate) struct SampleMailbox {
    sequence: AtomicU64,
    samples: AtomicU64,
    breaks: AtomicU64,
    callbacks: AtomicU64,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct SampleSnapshot {
    samples: u64,
    breaks: u64,
    callbacks: u64,
}

impl SampleMailbox {
    /// Called by the single sink-owning callback, without allocating or querying the OS.
    pub(crate) fn observe(&self, samples: &[f32], delivered: usize) {
        self.sequence.fetch_add(1, Ordering::SeqCst);
        if samples.is_empty()
            || delivered != samples.len()
            || samples.iter().any(|sample| sample.to_bits() != 0)
        {
            self.breaks.fetch_add(1, Ordering::SeqCst);
        }
        self.samples
            .fetch_add(samples.len() as u64, Ordering::SeqCst);
        self.callbacks.fetch_add(1, Ordering::SeqCst);
        self.sequence.fetch_add(1, Ordering::SeqCst);
    }

    /// Observation loss (including callback reentrancy) invalidates the candidate window.
    pub(crate) fn invalidate(&self) {
        self.breaks.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn snapshot(&self) -> Option<SampleSnapshot> {
        let sequence = self.sequence.load(Ordering::SeqCst);
        if sequence % 2 != 0 {
            return None;
        }
        let snapshot = SampleSnapshot {
            samples: self.samples.load(Ordering::SeqCst),
            breaks: self.breaks.load(Ordering::SeqCst),
            callbacks: self.callbacks.load(Ordering::SeqCst),
        };
        (self.sequence.load(Ordering::SeqCst) == sequence
            && self.breaks.load(Ordering::SeqCst) == snapshot.breaks)
            .then_some(snapshot)
    }
}

pub(crate) enum Selection {
    Include(Vec<u32>),
    Exclude(Vec<u32>),
}

impl Selection {
    pub(crate) fn includes(&self, object: u32, pid: u32, own_pid: u32) -> bool {
        pid != own_pid
            && match self {
                Self::Include(objects) => objects.contains(&object),
                Self::Exclude(objects) => !objects.contains(&object),
            }
    }
}

pub(crate) struct ProcessActivity {
    pub(crate) object: u32,
    pub(crate) pid: u32,
    pub(crate) output_active: Option<bool>,
    pub(crate) on_device: Option<bool>,
}

/// Unknown status/routing never qualifies as active, even if a different process is active.
pub(crate) fn eligible_activity(
    selection: &Selection,
    own_pid: u32,
    device_scoped: bool,
    processes: &[ProcessActivity],
) -> Option<bool> {
    let mut active = false;
    for process in processes {
        if !selection.includes(process.object, process.pid, own_pid) {
            continue;
        }
        if !process.output_active? {
            continue;
        }
        if device_scoped && !process.on_device? {
            continue;
        }
        active = true;
    }
    Some(active)
}

pub(crate) struct SilenceDetector {
    previous: Option<(Duration, SampleSnapshot)>,
    candidate: Option<(Duration, u64)>,
    minimum_samples: u64,
    emitted: bool,
}

impl SilenceDetector {
    pub(crate) fn new(rate: u32, channels: u16) -> Self {
        Self {
            previous: None,
            candidate: None,
            minimum_samples: u64::from(rate) * u64::from(channels) * WINDOW.as_secs(),
            emitted: false,
        }
    }

    /// `now` and all observations are injected; a new instance represents a capture generation.
    pub(crate) fn update(
        &mut self,
        now: Duration,
        snapshot: Option<SampleSnapshot>,
        activity: Option<bool>,
    ) -> bool {
        let Some(snapshot) = snapshot else {
            self.candidate = None;
            self.previous = None;
            return false;
        };
        let continuous = self.previous.is_some_and(|(time, previous)| {
            now.checked_sub(time)
                .is_some_and(|gap| gap <= MAX_OBSERVATION_GAP)
                && previous.breaks == snapshot.breaks
                && previous.callbacks < snapshot.callbacks
                && previous.samples < snapshot.samples
        });
        self.previous = Some((now, snapshot));
        if self.emitted || activity != Some(true) || !continuous {
            self.candidate = None;
            return false;
        }
        let (since, samples) = *self.candidate.get_or_insert((now, snapshot.samples));
        if now
            .checked_sub(since)
            .is_some_and(|elapsed| elapsed >= WINDOW)
            && snapshot.samples.saturating_sub(samples) >= self.minimum_samples
        {
            self.emitted = true;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tick(
        detector: &mut SilenceDetector,
        mailbox: &SampleMailbox,
        time: u64,
        activity: Option<bool>,
    ) -> bool {
        mailbox.observe(&[0.0; 10], 10);
        detector.update(
            Duration::from_millis(time * 100),
            mailbox.snapshot(),
            activity,
        )
    }

    #[test]
    fn active_exact_zeros_emit_once_and_new_generation_can_emit() {
        let mailbox = SampleMailbox::default();
        for _ in 0..2 {
            let mut detector = SilenceDetector::new(100, 1);
            let events = (0..100)
                .filter(|time| tick(&mut detector, &mailbox, *time, Some(true)))
                .count();
            assert_eq!(events, 1);
        }
    }

    #[test]
    fn idle_and_unknown_activity_never_emit() {
        for activity in [Some(false), None] {
            let mailbox = SampleMailbox::default();
            let mut detector = SilenceDetector::new(100, 1);
            for time in 0..100 {
                assert!(!tick(&mut detector, &mailbox, time, activity));
            }
        }
    }

    #[test]
    fn negative_zero_nonzero_nan_and_subnormal_break_window() {
        for sample in [-0.0, 0.1, f32::NAN, f32::from_bits(1)] {
            let mailbox = SampleMailbox::default();
            let mut detector = SilenceDetector::new(100, 1);
            for time in 0..50 {
                assert!(!tick(&mut detector, &mailbox, time, Some(true)));
            }
            mailbox.observe(&[sample], 1);
            for time in 50..100 {
                assert!(!tick(&mut detector, &mailbox, time, Some(true)));
            }
            assert!((100..110).any(|time| tick(&mut detector, &mailbox, time, Some(true))));
        }
    }

    #[test]
    fn missing_empty_lost_or_unknown_observation_resets_window() {
        for scenario in 0..5 {
            let mailbox = SampleMailbox::default();
            let mut detector = SilenceDetector::new(100, 1);
            for time in 0..50 {
                assert!(!tick(&mut detector, &mailbox, time, Some(true)));
            }
            match scenario {
                0 => {
                    mailbox.observe(&[], 0);
                }
                1 => {
                    mailbox.observe(&[0.0], 0);
                }
                2 => {
                    mailbox.invalidate();
                }
                3 => {
                    detector.update(Duration::from_secs(5), None, Some(true));
                }
                _ => {
                    detector.update(Duration::from_secs(5), mailbox.snapshot(), None);
                }
            }
            for time in 50..100 {
                assert!(!tick(&mut detector, &mailbox, time, Some(true)));
            }
        }
    }

    #[test]
    fn stall_or_owner_gap_resets_window() {
        let mailbox = SampleMailbox::default();
        let mut detector = SilenceDetector::new(100, 1);
        for time in 0..50 {
            assert!(!tick(&mut detector, &mailbox, time, Some(true)));
        }
        assert!(!detector.update(Duration::from_secs(5), mailbox.snapshot(), Some(true)));
        for time in 100..150 {
            assert!(!tick(&mut detector, &mailbox, time, Some(true)));
        }
    }

    #[test]
    fn five_seconds_requires_native_sample_duration_too() {
        let mailbox = SampleMailbox::default();
        let mut detector = SilenceDetector::new(48_000, 2);
        for time in 0..100 {
            assert!(!tick(&mut detector, &mailbox, time, Some(true)));
        }
    }

    fn process(
        object: u32,
        pid: u32,
        output_active: Option<bool>,
        on_device: Option<bool>,
    ) -> ProcessActivity {
        ProcessActivity {
            object,
            pid,
            output_active,
            on_device,
        }
    }

    #[test]
    fn include_uses_only_target_and_excludes_ourselves() {
        let processes = [
            process(1, 10, Some(false), None),
            process(2, 20, Some(true), None),
        ];
        assert_eq!(
            eligible_activity(&Selection::Include(vec![1]), 30, false, &processes),
            Some(false)
        );
        assert_eq!(
            eligible_activity(&Selection::Include(vec![2]), 20, false, &processes),
            Some(false)
        );
        assert_eq!(
            eligible_activity(&Selection::Include(vec![2]), 30, false, &processes),
            Some(true)
        );
    }

    #[test]
    fn exclude_obeys_exclusions_host_and_device_routing() {
        let processes = [
            process(1, 10, Some(true), Some(true)),
            process(2, 20, Some(true), Some(false)),
            process(3, 30, Some(true), Some(true)),
        ];
        assert_eq!(
            eligible_activity(&Selection::Exclude(vec![1]), 30, true, &processes),
            Some(false)
        );
        assert_eq!(
            eligible_activity(&Selection::Exclude(vec![1]), 30, false, &processes),
            Some(true)
        );
        assert_eq!(
            eligible_activity(&Selection::Exclude(vec![]), 30, true, &processes),
            Some(true)
        );
    }

    #[test]
    fn query_failure_and_unknown_routing_disable_inference() {
        let selection = Selection::Exclude(vec![]);
        let processes = [
            process(1, 10, Some(true), Some(true)),
            process(2, 20, None, None),
        ];
        assert_eq!(eligible_activity(&selection, 30, false, &processes), None);
        assert_eq!(
            eligible_activity(&selection, 30, true, &[process(1, 10, Some(true), None)]),
            None
        );
        assert_eq!(eligible_activity(&selection, 30, false, &[]), Some(false));
    }
}
