//! Device-free generation and conservative identification of a private permission probe.

pub(crate) const AMPLITUDE: f32 = 1.0e-5;
pub(crate) const SIGNAL_SECONDS: f64 = 0.3;
pub(crate) const MAX_CAPTURE_LATENCY_SECONDS: f64 = 0.5;
const FREQUENCY: f64 = 1_000.0;
const CHIP_SECONDS: f64 = 0.01;
const MAX_CAPTURE_FRAMES: usize = 192_000 * 2;
const MAX_MATCH_SAMPLES: usize = 2_400;

pub(crate) struct ProbeSignal {
    signs: [f64; 30],
}

impl ProbeSignal {
    pub(crate) fn new(mut nonce: u64) -> Self {
        let mut signs = [1.0; 30];
        signs[15..].fill(-1.0);
        for index in (1..30).rev() {
            nonce ^= nonce << 13;
            nonce ^= nonce >> 7;
            nonce ^= nonce << 17;
            let other = (nonce % (index as u64 + 1)) as usize;
            signs.swap(index, other);
        }
        Self { signs }
    }

    fn sign(&self, seconds: f64) -> Option<f64> {
        if !(0.0..SIGNAL_SECONDS).contains(&seconds) {
            return None;
        }
        self.signs.get((seconds / CHIP_SECONDS) as usize).copied()
    }

    pub(crate) fn sample(&self, seconds: f64) -> f32 {
        let Some(sign) = self.sign(seconds) else {
            return 0.0;
        };
        let ramp = (seconds / 0.001)
            .min((SIGNAL_SECONDS - seconds) / 0.001)
            .clamp(0.0, 1.0);
        (f64::from(AMPLITUDE) * sign * ramp * (std::f64::consts::TAU * FREQUENCY * seconds).sin())
            as f32
    }

    pub(crate) fn samples(&self, rate: u32) -> Vec<f32> {
        (0..(f64::from(rate) * SIGNAL_SECONDS) as usize)
            .map(|frame| self.sample(frame as f64 / f64::from(rate)))
            .collect()
    }
}

pub(crate) struct CapturedFrame {
    pub(crate) host_time: u64,
    pub(crate) sample: f32,
    pub(crate) all_exact_zero: bool,
}

pub(crate) struct RenderEvidence {
    pub(crate) start: u64,
    pub(crate) end: u64,
    pub(crate) ticks_per_second: f64,
    pub(crate) nonzero_frames: usize,
    pub(crate) rate: u32,
    pub(crate) capture_rate: u32,
    /// Native format/timestamp validity, complete output submission, and successful teardown.
    pub(crate) valid: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SignalEvidence {
    Matched,
    ExactZeros,
    Inconclusive,
}

/// The complete observation interval includes the supported post-render capture latency.
pub(crate) fn observation_end(rendered: &RenderEvidence) -> Option<u64> {
    let latency_ticks = rendered.ticks_per_second * MAX_CAPTURE_LATENCY_SECONDS;
    if !latency_ticks.is_finite() || latency_ticks <= 0.0 || latency_ticks >= u64::MAX as f64 {
        return None;
    }
    rendered.end.checked_add(latency_ticks.ceil() as u64)
}

/// Only continuous samples in the render-plus-latency interval qualify as evidence.
/// The adapter validates continuity, format, cancellation, and native shutdown before calling.
pub(crate) fn identify(
    signal: &ProbeSignal,
    rendered: &RenderEvidence,
    captured: &[CapturedFrame],
) -> SignalEvidence {
    if !rendered.valid
        || rendered.start == 0
        || rendered.end <= rendered.start
        || !rendered.ticks_per_second.is_finite()
        || rendered.ticks_per_second <= 0.0
        || rendered.rate == 0
        || rendered.capture_rate == 0
        || captured.len() > MAX_CAPTURE_FRAMES
        || rendered.nonzero_frames < rendered.rate as usize / 10
    {
        return SignalEvidence::Inconclusive;
    }
    let Some(window_end) = observation_end(rendered) else {
        return SignalEvidence::Inconclusive;
    };
    let overlap: Vec<_> = captured
        .iter()
        .filter(|frame| frame.host_time >= rendered.start && frame.host_time < window_end)
        .collect();
    let (Some(first), Some(last)) = (overlap.first(), overlap.last()) else {
        return SignalEvidence::Inconclusive;
    };
    let expected_step = rendered.ticks_per_second / f64::from(rendered.capture_rate);
    if overlap.windows(2).any(|pair| {
        let Some(step) = pair[1].host_time.checked_sub(pair[0].host_time) else {
            return true;
        };
        (step as f64 - expected_step).abs() > expected_step * 0.1 + 2.0
    }) {
        return SignalEvidence::Inconclusive;
    }
    let span = last.host_time.saturating_sub(first.host_time) as f64 / rendered.ticks_per_second;
    if span < 0.1 || overlap.iter().any(|frame| !frame.sample.is_finite()) {
        return SignalEvidence::Inconclusive;
    }
    if overlap.iter().all(|frame| frame.all_exact_zero) {
        // A short zero prefix can precede a delayed valid signal. Absence is evidence
        // only over at least 95% of the full render-plus-500 ms latency interval.
        let covered_ticks = last.host_time.saturating_sub(first.host_time) as f64 + expected_step;
        let required_ticks = window_end.saturating_sub(rendered.start) as f64 * 0.95;
        return if covered_ticks >= required_ticks {
            SignalEvidence::ExactZeros
        } else {
            SignalEvidence::Inconclusive
        };
    }
    // Ignore chip edges, where the output device's resampler/filter can smear phase transitions.
    // Quadrature correlation permits unknown carrier phase. A balanced nonce-specific chip
    // sequence, a strict correlation threshold, and an amplitude bound reject ordinary tones.
    if span < 0.15 {
        return SignalEvidence::Inconclusive;
    }
    let stride = overlap.len().div_ceil(MAX_MATCH_SAMPLES).max(1);
    // Carrier phase rotation does not change quadrature correlation magnitude.
    // Precompute the trigonometry once: at most 2,400 sine/cosine pairs, followed
    // by 511 bounded candidate scans with only chip lookup and arithmetic.
    let samples: Vec<_> = overlap
        .iter()
        .step_by(stride)
        .map(|frame| {
            let seconds = (frame.host_time - rendered.start) as f64 / rendered.ticks_per_second;
            let sample = f64::from(frame.sample);
            let phase = std::f64::consts::TAU * FREQUENCY * seconds;
            (
                seconds,
                sample * sample,
                sample * phase.sin(),
                sample * phase.cos(),
            )
        })
        .collect();
    for lag_ms in -10..=500 {
        let lag = f64::from(lag_ms) / 1_000.0;
        let mut energy = 0.0;
        let mut sine = 0.0;
        let mut cosine = 0.0;
        let mut count = 0usize;
        let mut chips = 0u32;
        let mut positive_chips = 0u32;
        let mut negative_chips = 0u32;
        let mut first_seconds = None;
        let mut last_seconds = 0.0;
        for &(captured_seconds, sample_energy, sample_sine, sample_cosine) in &samples {
            let seconds = captured_seconds - lag;
            let Some(sign) = signal.sign(seconds) else {
                continue;
            };
            let chip_position = seconds % CHIP_SECONDS;
            if !(0.001..0.009).contains(&chip_position) {
                continue;
            }
            energy += sample_energy;
            sine += sign * sample_sine;
            cosine += sign * sample_cosine;
            count += 1;
            first_seconds.get_or_insert(seconds);
            last_seconds = seconds;
            let chip = 1u32 << (seconds / CHIP_SECONDS) as u32;
            chips |= chip;
            if sign > 0.0 {
                positive_chips |= chip;
            } else {
                negative_chips |= chip;
            }
        }
        // A balanced full waveform can have a constant-phase partial window. Require
        // meaningful evidence of both signs in the observed window, rather than treating
        // an ordinary 1 kHz tone in such a window as the nonce-specific signature.
        if count < 100
            || chips.count_ones() < 12
            || positive_chips.count_ones() < 4
            || negative_chips.count_ones() < 4
            || first_seconds.is_none_or(|first| last_seconds - first < 0.15)
            || energy <= 0.0
        {
            continue;
        }
        let rms = (energy / count as f64).sqrt();
        let correlation = (2.0 * (sine * sine + cosine * cosine) / (energy * count as f64)).sqrt();
        if (f64::from(AMPLITUDE) * 0.15..=f64::from(AMPLITUDE) * 3.0).contains(&rms)
            && correlation >= 0.98
        {
            return SignalEvidence::Matched;
        }
    }
    SignalEvidence::Inconclusive
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered() -> RenderEvidence {
        RenderEvidence {
            start: 1_000_000,
            end: 1_300_000,
            ticks_per_second: 1_000_000.0,
            nonzero_frames: 14_000,
            rate: 48_000,
            capture_rate: 48_000,
            valid: true,
        }
    }
    fn capture(samples: impl Fn(f64) -> f32) -> Vec<CapturedFrame> {
        (0..38_400)
            .map(|frame| {
                let seconds = f64::from(frame) / 48_000.0;
                let sample = samples(seconds);
                CapturedFrame {
                    host_time: 1_000_000 + (seconds * 1_000_000.0) as u64,
                    sample,
                    all_exact_zero: sample.to_bits() == 0,
                }
            })
            .collect()
    }
    #[test]
    fn generated_nonce_signature_matches() {
        let signal = ProbeSignal::new(12345);
        assert_eq!(
            identify(&signal, &rendered(), &capture(|time| signal.sample(time))),
            SignalEvidence::Matched
        );
        assert!(signal
            .samples(48_000)
            .iter()
            .all(|value| value.abs() <= AMPLITUDE));
    }
    #[test]
    fn shifted_signal_matches_but_plain_tone_and_other_nonce_do_not() {
        let signal = ProbeSignal::new(12345);
        let other = ProbeSignal::new(987654);
        assert_eq!(
            identify(
                &signal,
                &rendered(),
                &capture(|time| signal.sample(time - 0.007))
            ),
            SignalEvidence::Matched
        );
        for samples in [
            capture(|time| (std::f64::consts::TAU * FREQUENCY * time).sin() as f32 * AMPLITUDE),
            capture(|time| other.sample(time)),
        ] {
            assert_eq!(
                identify(&signal, &rendered(), &samples),
                SignalEvidence::Inconclusive
            );
        }
    }
    #[test]
    fn partial_constant_phase_window_does_not_match_plain_tone() {
        let mut signs = [1.0; 30];
        signs[15..].fill(-1.0);
        let signal = ProbeSignal { signs };
        let mut frames =
            capture(|time| (std::f64::consts::TAU * FREQUENCY * time).sin() as f32 * AMPLITUDE);
        frames.truncate(7_680);
        assert_eq!(
            identify(&signal, &rendered(), &frames),
            SignalEvidence::Inconclusive
        );
    }
    #[test]
    fn only_overlapping_exact_zeros_with_render_evidence_classify() {
        let signal = ProbeSignal::new(12345);
        assert_eq!(
            identify(&signal, &rendered(), &capture(|_| 0.0)),
            SignalEvidence::ExactZeros
        );
        assert_eq!(
            identify(&signal, &rendered(), &capture(|_| -0.0)),
            SignalEvidence::Inconclusive
        );
        let mut absent = rendered();
        absent.nonzero_frames = 0;
        assert_eq!(
            identify(&signal, &absent, &capture(|_| 0.0)),
            SignalEvidence::Inconclusive
        );
        let mut frames = capture(|_| 0.0);
        frames
            .iter_mut()
            .for_each(|frame| frame.host_time += 1_000_000);
        assert_eq!(
            identify(&signal, &rendered(), &frames),
            SignalEvidence::Inconclusive
        );
        assert_eq!(
            identify(&signal, &rendered(), &[]),
            SignalEvidence::Inconclusive
        );
    }

    #[test]
    fn delayed_capture_signal_never_classifies_as_zero_absence() {
        let signal = ProbeSignal::new(12345);
        for lag in [0.25, 0.5, -0.01] {
            assert_eq!(
                identify(
                    &signal,
                    &rendered(),
                    &capture(|time| signal.sample(time - lag)),
                ),
                SignalEvidence::Matched
            );
        }
        let unrelated = ProbeSignal::new(987654);
        assert_eq!(
            identify(
                &signal,
                &rendered(),
                &capture(|time| unrelated.sample(time - 0.25)),
            ),
            SignalEvidence::Inconclusive
        );
    }

    #[test]
    fn short_zero_prefix_and_fifty_millisecond_gap_are_inconclusive() {
        let signal = ProbeSignal::new(12345);
        let mut frames = capture(|_| 0.0);
        frames.truncate(7_200);
        assert_eq!(
            identify(&signal, &rendered(), &frames),
            SignalEvidence::Inconclusive
        );
        let mut frames = capture(|_| 0.0);
        frames.drain(9_600..12_000);
        assert_eq!(
            identify(&signal, &rendered(), &frames),
            SignalEvidence::Inconclusive
        );
    }

    #[test]
    fn zero_absence_requires_ninety_five_percent_complete_window() {
        let signal = ProbeSignal::new(12345);
        let mut frames = capture(|_| 0.0);
        frames.truncate(36_479);
        assert_eq!(
            identify(&signal, &rendered(), &frames),
            SignalEvidence::Inconclusive
        );
        let mut frames = capture(|_| 0.0);
        frames.truncate(36_481);
        assert_eq!(
            identify(&signal, &rendered(), &frames),
            SignalEvidence::ExactZeros
        );
        assert_eq!(observation_end(&rendered()), Some(1_800_000));
    }

    #[test]
    fn latency_window_and_matching_use_host_clock_tick_units() {
        let signal = ProbeSignal::new(12345);
        let mut evidence = rendered();
        evidence.start *= 24;
        evidence.end *= 24;
        evidence.ticks_per_second *= 24.0;
        let mut frames = capture(|time| signal.sample(time - 0.25));
        frames.iter_mut().for_each(|frame| frame.host_time *= 24);
        assert_eq!(observation_end(&evidence), Some(43_200_000));
        assert_eq!(
            identify(&signal, &evidence, &frames),
            SignalEvidence::Matched
        );
        frames.iter_mut().for_each(|frame| {
            frame.sample = 0.0;
            frame.all_exact_zero = true;
        });
        assert_eq!(
            identify(&signal, &evidence, &frames),
            SignalEvidence::ExactZeros
        );
        evidence.end = u64::MAX;
        assert_eq!(observation_end(&evidence), None);
    }

    #[test]
    fn late_nonmatching_nonzero_prevents_zero_absence_inference() {
        let signal = ProbeSignal::new(12345);
        let mut frames = capture(|_| 0.0);
        frames[38_000].sample = AMPLITUDE;
        frames[38_000].all_exact_zero = false;
        assert_eq!(
            identify(&signal, &rendered(), &frames),
            SignalEvidence::Inconclusive
        );
    }

    #[test]
    fn invalid_format_host_time_and_gapped_capture_are_inconclusive() {
        let signal = ProbeSignal::new(12345);
        let mut evidence = rendered();
        evidence.valid = false;
        assert_eq!(
            identify(&signal, &evidence, &capture(|_| 0.0)),
            SignalEvidence::Inconclusive
        );
        evidence = rendered();
        evidence.start = 0;
        assert_eq!(
            identify(&signal, &evidence, &capture(|_| 0.0)),
            SignalEvidence::Inconclusive
        );
        let mut frames = capture(|_| 0.0);
        frames.remove(100);
        assert_eq!(
            identify(&signal, &rendered(), &frames),
            SignalEvidence::Inconclusive
        );
        let mut frames = capture(|_| 0.0);
        frames[100].host_time = frames[99].host_time;
        assert_eq!(
            identify(&signal, &rendered(), &frames),
            SignalEvidence::Inconclusive
        );
    }
}
