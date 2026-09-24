//! Helpers that normalize device PTS / the monotonic clock to a common `i64` in nanoseconds.
//!
//! Loopback paths carry PTS derived from the device clock, while the microphone (cpal) path only
//! has relative, opaque timestamps. The core maps both onto a common monotonic clock using "an
//! origin offset captured at open time". Strict cross-stream synchronization is best-effort.

use std::time::Instant;

/// Returns the current value of the process monotonic clock in nanoseconds.
///
/// A monotonic value based on [`Instant`]. It is not absolute wall-clock time; it is meaningful
/// only for differences and ordering within the same process.
pub fn monotonic_now_ns() -> i64 {
    monotonic_base().elapsed().as_nanos() as i64
}

/// Origin of the monotonic clock, fixed exactly once at process startup.
fn monotonic_base() -> Instant {
    use std::sync::OnceLock;
    static BASE: OnceLock<Instant> = OnceLock::new();
    *BASE.get_or_init(Instant::now)
}

/// Normalizes device PTS onto the common monotonic clock.
///
/// On the first sample it records "the device PTS origin" and "the monotonic clock origin", and
/// thereafter returns `device_pts - device_origin + monotonic_origin`, translating onto the
/// monotonic clock axis while preserving the device clock's rate.
///
/// For paths without device PTS, such as the microphone, pass each sample's arrival time
/// ([`monotonic_now_ns`]) as device_pts.
#[derive(Debug, Clone)]
pub struct ClockNormalizer {
    /// Device PTS origin (ns), recorded on the first sample.
    device_origin_ns: Option<i64>,
    /// Monotonic clock origin (ns), recorded on the first sample.
    monotonic_origin_ns: i64,
}

impl ClockNormalizer {
    /// Creates a new normalizer. The origin is not yet fixed (it is fixed by the first
    /// [`normalize`](Self::normalize)).
    pub fn new() -> Self {
        Self {
            device_origin_ns: None,
            monotonic_origin_ns: 0,
        }
    }

    /// Whether the origin is not yet fixed (the first sample has not arrived).
    pub fn is_unset(&self) -> bool {
        self.device_origin_ns.is_none()
    }

    /// Maps a device PTS (ns) to a normalized monotonic PTS (ns).
    ///
    /// The first call fixes the origin and adopts the [`monotonic_now_ns`] at that moment as the
    /// monotonic origin. Thereafter it preserves device clock differences.
    pub fn normalize(&mut self, device_pts_ns: i64) -> i64 {
        match self.device_origin_ns {
            Some(origin) => self.monotonic_origin_ns + device_pts_ns.wrapping_sub(origin),
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
        // The first call fixes the device origin and returns the monotonic origin.
        let t0 = n.normalize(1_000_000); // device pts 1ms
        assert!(!n.is_unset());
        // Thereafter device differences are preserved: +5ms -> the normalized value is also +5ms.
        let t1 = n.normalize(6_000_000);
        assert_eq!(t1 - t0, 5_000_000);
        // Going backwards keeps the delta as negative (the caller uses it for gap detection).
        let t2 = n.normalize(4_000_000);
        assert_eq!(t2 - t0, 3_000_000);
    }

    /// The difference in `normalize` comes from `wrapping_sub`. Even if device_pts overflows
    /// around `i64::MAX`, it does not panic, and the wrapped difference is added to the monotonic
    /// origin. Verifies that going +1 from the origin `i64::MAX` (wrapping to `i64::MIN`) yields
    /// a delta of `i64::MIN - i64::MAX = +1` (wrapped) via `wrapping_sub`.
    #[test]
    fn normalize_wrapping_sub_handles_i64_boundary() {
        let mut n = ClockNormalizer::new();
        // Fix the origin at i64::MAX (the monotonic origin is monotonic_now_ns at that time).
        let base = n.normalize(i64::MAX);
        // Move device_pts to i64::MIN (simulating a wall-clock digit overflow).
        // wrapping_sub(i64::MAX) wraps to +1 (i64::MIN - i64::MAX = 1 mod 2^64).
        let next = n.normalize(i64::MIN);
        assert_eq!(
            next.wrapping_sub(base),
            1,
            "wrapping_sub boundary: MIN - MAX should wrap to +1"
        );
    }

    /// Every time a normalizer with an unfixed origin is created, the first normalize sets
    /// `is_unset` to false. Even with a large negative device_pts, the first call returns the
    /// monotonic origin at that moment (no difference is computed).
    #[test]
    fn first_normalize_ignores_device_value_for_origin() {
        let mut n = ClockNormalizer::new();
        // The first call returns monotonic_now_ns regardless of the device value (it only fixes
        // the origin).
        let before = monotonic_now_ns();
        let t0 = n.normalize(i64::MIN);
        let after = monotonic_now_ns();
        assert!(
            t0 >= before && t0 <= after,
            "the first call should return the monotonic origin: {before} <= {t0} <= {after}"
        );
        assert!(!n.is_unset());
    }

    /// `Default` creates the same unfixed state as `new`.
    #[test]
    fn default_is_unset_like_new() {
        let n = ClockNormalizer::default();
        assert!(n.is_unset());
    }
}
