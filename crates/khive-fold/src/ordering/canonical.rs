//! Canonical float normalization functions

/// Canonical quiet NaN bit pattern for f64.
pub(crate) const CANONICAL_NAN_F64: u64 = 0x7ff8_0000_0000_0000;

/// Canonical quiet NaN bit pattern for f32.
pub(crate) const CANONICAL_NAN_F32: u32 = 0x7fc0_0000;

const ABS_MASK_F64: u64 = 0x7fff_ffff_ffff_ffff;
const EXP_MASK_F64: u64 = 0x7ff0_0000_0000_0000;

const ABS_MASK_F32: u32 = 0x7fff_ffff;
const EXP_MASK_F32: u32 = 0x7f80_0000;

/// Normalize an f64 value to canonical form for comparison.
///
/// Ensures that:
/// - All NaN variants map to the same canonical quiet NaN
/// - Negative zero (-0.0) maps to positive zero (+0.0)
/// - All other values are preserved exactly
///
/// The implementation is branchless and intended to lower to predictable
/// mask/`setcc`/`cmov` style code on modern CPUs.
#[inline]
pub fn canonical_f64(x: f64) -> f64 {
    let bits = x.to_bits();
    let abs = bits & ABS_MASK_F64;

    let nan_mask = ((abs > EXP_MASK_F64) as u64).wrapping_neg();
    let zero_mask = ((abs == 0) as u64).wrapping_neg();

    let zero_normalized = bits & !zero_mask;
    f64::from_bits((zero_normalized & !nan_mask) | (CANONICAL_NAN_F64 & nan_mask))
}

/// Normalize an f32 value to canonical form for comparison.
///
/// Ensures that:
/// - All NaN variants map to the same canonical quiet NaN
/// - Negative zero (-0.0) maps to positive zero (+0.0)
/// - All other values are preserved exactly
#[inline]
pub fn canonical_f32(x: f32) -> f32 {
    let bits = x.to_bits();
    let abs = bits & ABS_MASK_F32;

    let nan_mask = ((abs > EXP_MASK_F32) as u32).wrapping_neg();
    let zero_mask = ((abs == 0) as u32).wrapping_neg();

    let zero_normalized = bits & !zero_mask;
    f32::from_bits((zero_normalized & !nan_mask) | (CANONICAL_NAN_F32 & nan_mask))
}
