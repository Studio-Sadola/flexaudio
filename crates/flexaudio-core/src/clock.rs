//! Helpers for translating timestamps to process-monotonic `i64` nanoseconds.
//!
//! Built-in microphone and loopback backends on every OS pass monotonic arrival times
//! to `RawSink::push`, which discards them: the raw ring carries samples only. The
//! facade assigns a fresh arrival time at ingest and translates it with
//! [`ClockNormalizer`]. Device timestamps are not used by the capture pipeline;
//! these labels do not provide media-clock or cross-stream synchronization.

use std::time::Instant;

/// Returns the current process monotonic clock value in nanoseconds.
///
/// A monotonic value based on [`Instant`]. It is not an absolute wall-clock time and
/// is meaningful only for differences and ordering within the same process.
pub fn monotonic_now_ns() -> i64 {
    monotonic_base().elapsed().as_nanos() as i64
}

/// Process-wide monotonic clock origin, initialized on first use.
fn monotonic_base() -> Instant {
    use std::sync::OnceLock;
    static BASE: OnceLock<Instant> = OnceLock::new();
    *BASE.get_or_init(Instant::now)
}

/// Translates input timestamps to the process-monotonic clock.
///
/// Records the input timestamp and monotonic clock origins on the first call.
/// Later calls return `device_pts - device_origin + monotonic_origin`, preserving
/// input deltas. Despite the parameter names, the facade supplies ingest arrival
/// times ([`monotonic_now_ns`]) for both microphone and loopback, not device PTS.
#[derive(Debug, Clone)]
pub struct ClockNormalizer {
    /// Input timestamp origin recorded on the first call (ns).
    device_origin_ns: Option<i64>,
    /// Monotonic clock origin recorded on the first sample (ns).
    monotonic_origin_ns: i64,
}

impl ClockNormalizer {
    /// Creates a normalizer with unset origins, which are fixed by the first [`normalize`](Self::normalize).
    pub fn new() -> Self {
        Self {
            device_origin_ns: None,
            monotonic_origin_ns: 0,
        }
    }

    /// Returns whether the origins are still unset (the first sample has not arrived).
    pub fn is_unset(&self) -> bool {
        self.device_origin_ns.is_none()
    }

    /// Maps an input timestamp (ns) to the process-monotonic timeline (ns).
    ///
    /// The first call fixes the origins and uses the current [`monotonic_now_ns`] as
    /// the monotonic origin. Later calls preserve input timestamp deltas.
    /// Input timestamp rollover uses wrapping subtraction; a translated value outside i64
    /// saturates at the nearest boundary rather than panicking or wrapping the timeline.
    pub fn normalize(&mut self, device_pts_ns: i64) -> i64 {
        match self.device_origin_ns {
            Some(origin) => self
                .monotonic_origin_ns
                .saturating_add(device_pts_ns.wrapping_sub(origin)),
            None => {
                self.device_origin_ns = Some(device_pts_ns);
                self.monotonic_origin_ns = monotonic_now_ns();
                self.monotonic_origin_ns
            }
        }
    }
}

impl Default for ClockNormalizer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_is_non_decreasing() {
        let a = monotonic_now_ns();
        let b = monotonic_now_ns();
        assert!(b >= a);
    }

    #[test]
    fn first_sample_sets_origin_and_preserves_deltas() {
        let mut n = ClockNormalizer::new();
        assert!(n.is_unset());
        // On the first call, fix the device origin and return the monotonic origin.
        let t0 = n.normalize(1_000_000); // device pts 1ms
        assert!(!n.is_unset());
        // Preserve device deltas: +5ms produces a normalized value that is also +5ms.
        let t1 = n.normalize(6_000_000);
        assert_eq!(t1 - t0, 5_000_000);
        // Preserve backward movement as a negative delta for gap detection by callers.
        let t2 = n.normalize(4_000_000);
        assert_eq!(t2 - t0, 3_000_000);
    }

    /// `normalize` computes deltas with `wrapping_sub`. If `device_pts` overflows near
    /// `i64::MAX`, it does not panic; the wrapped delta is added to the monotonic origin.
    /// Verify that +1 from origin `i64::MAX` wraps to `i64::MIN`, and `wrapping_sub`
    /// yields `i64::MIN - i64::MAX = +1` (wrapped).
    #[test]
    fn normalize_wrapping_sub_handles_i64_boundary() {
        let mut n = ClockNormalizer::new();
        // Fix the origin at i64::MAX (the monotonic origin is the current monotonic_now_ns).
        let base = n.normalize(i64::MAX);
        // Set device_pts to i64::MIN to simulate clock overflow.
        // wrapping_sub(i64::MAX) wraps to +1 (i64::MIN - i64::MAX = 1 mod 2^64).
        let next = n.normalize(i64::MIN);
        assert_eq!(
            next.wrapping_sub(base),
            1,
            "wrapping_sub boundary: MIN - MAX should wrap to +1"
        );
    }

    /// Verify that the first normalize changes `is_unset` to false on a fresh normalizer.
    /// Even for a large negative device_pts, the first call returns the current monotonic
    /// origin without calculating a delta.
    #[test]
    fn first_normalize_ignores_device_value_for_origin() {
        let mut n = ClockNormalizer::new();
        // The first call returns monotonic_now_ns regardless of the device value; it only fixes the origins.
        let before = monotonic_now_ns();
        let t0 = n.normalize(i64::MIN);
        let after = monotonic_now_ns();
        assert!(
            t0 >= before && t0 <= after,
            "first call should return the monotonic origin: {before} <= {t0} <= {after}"
        );
        assert!(!n.is_unset());
    }

    /// `Default` creates the same unset state as `new`.
    #[test]
    fn default_is_unset_like_new() {
        let n = ClockNormalizer::default();
        assert!(n.is_unset());
    }
}

#[cfg(test)]
mod saturation_tests {
    use super::*;
    #[test]
    fn translated_clock_saturates_instead_of_panicking_or_wrapping() {
        let mut clock = ClockNormalizer {
            device_origin_ns: Some(0),
            monotonic_origin_ns: 10,
        };
        assert_eq!(clock.normalize(i64::MAX), i64::MAX);
        clock.monotonic_origin_ns = -10;
        assert_eq!(clock.normalize(i64::MIN), i64::MIN);
    }
}
