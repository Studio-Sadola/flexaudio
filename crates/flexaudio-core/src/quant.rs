//! Signed 16-bit PCM quantization, shared by every output path.
//!
//! A single canonical quantizer keeps the CLI, the encoders and the language
//! bindings byte-identical. Sample values are host-native `i16`; serializing to
//! a little-endian wire format (s16le) is the consumer's responsibility (this
//! crate returns values, not byte-order-specific buffers).

/// Quantize one f32 sample to signed 16-bit PCM (no dither).
///
/// The scale factor 32768 uses the full negative range (`-1.0` maps to
/// `-32768`); `+1.0` cannot be represented and clamps to `32767`. Inputs
/// outside `[-1.0, 1.0]` saturate to the range bounds. Non-finite inputs
/// saturate through the `as` cast (`NaN -> 0`, `+inf -> 32767`,
/// `-inf -> -32768`). Rounding is round-half-away-from-zero.
#[inline]
pub fn quantize_i16(x: f32) -> i16 {
    (x * 32768.0).round().clamp(-32768.0, 32767.0) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantizes_reference_values() {
        assert_eq!(quantize_i16(0.0), 0);
        assert_eq!(quantize_i16(0.25), 8192);
        // Full negative scale; positive full-scale clamps one LSB below.
        assert_eq!(quantize_i16(-1.0), -32768);
        assert_eq!(quantize_i16(1.0), 32767);
    }

    #[test]
    fn saturates_out_of_range_and_non_finite() {
        // Out-of-range inputs clamp to the range bounds.
        assert_eq!(quantize_i16(2.0), 32767);
        assert_eq!(quantize_i16(-2.0), -32768);
        // Non-finite inputs saturate.
        assert_eq!(quantize_i16(f32::NAN), 0);
        assert_eq!(quantize_i16(f32::INFINITY), 32767);
        assert_eq!(quantize_i16(f32::NEG_INFINITY), -32768);
    }

    #[test]
    fn rounds_half_away_from_zero() {
        // 1.5 / 32768 is exactly representable in binary; rounds to 2.
        assert_eq!(quantize_i16(1.5 / 32768.0), 2);
        assert_eq!(quantize_i16(-1.5 / 32768.0), -2);
    }
}
