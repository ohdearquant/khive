//! Fixed-point scoring: f64 → i64 at 2^32 scale for cross-platform determinism.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::{Add, Div, Mul, Sub};

/// Fixed-point score wrapping an `i64` scaled by `2^32`.
#[derive(Copy, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize))]
#[repr(transparent)]
pub struct DeterministicScore(i64);

#[cfg(feature = "serde")]
impl<'de> Deserialize<'de> for DeterministicScore {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = i64::deserialize(deserializer)?;
        DeterministicScore::from_raw_checked(raw).ok_or_else(|| {
            serde::de::Error::custom(
                "DeterministicScore raw value i64::MIN is the reserved sentinel and cannot be stored as a runtime value"
            )
        })
    }
}

impl DeterministicScore {
    const SCALE: f64 = 4_294_967_296.0; // 2^32

    /// Highest representable score (`i64::MAX`); maps to `+Infinity` in float conversion.
    pub const MAX: Self = Self(i64::MAX);
    /// Reserved sentinel at `i64::MIN`; never produced by arithmetic or float conversion.
    pub const MIN: Self = Self(i64::MIN);
    /// Lowest reachable runtime score (`i64::MIN + 1`); underflow and `-Infinity` clamp here.
    pub const NEG_INF: Self = Self(i64::MIN + 1);
    /// Score of exactly zero.
    pub const ZERO: Self = Self(0);

    /// Construct from a raw `i64` without validation. Passing `i64::MIN` creates the reserved sentinel.
    #[inline]
    pub const fn from_raw(raw: i64) -> Self {
        Self(raw)
    }

    /// Construct from a raw `i64`, mapping `i64::MIN` to [`NEG_INF`][Self::NEG_INF].
    #[inline]
    pub const fn from_raw_saturating(raw: i64) -> Self {
        if raw == i64::MIN {
            Self::NEG_INF
        } else {
            Self(raw)
        }
    }

    /// Construct from a raw `i64`, returning `None` if `raw == i64::MIN`.
    #[inline]
    pub const fn from_raw_checked(raw: i64) -> Option<Self> {
        if raw == i64::MIN {
            None
        } else {
            Some(Self(raw))
        }
    }

    /// Return the underlying `i64` fixed-point representation.
    #[inline]
    pub const fn to_raw(self) -> i64 {
        self.0
    }

    /// Convert an `f64` to a `DeterministicScore` (NaN → ZERO, ±Inf → MAX/NEG_INF).
    #[inline]
    pub fn from_f64(val: f64) -> Self {
        if val.is_nan() {
            return Self::ZERO;
        }
        if val.is_infinite() {
            return if val.is_sign_positive() {
                Self::MAX
            } else {
                Self::NEG_INF
            };
        }

        let scaled = (val * Self::SCALE).round();
        Self::from_rounded_arithmetic(scaled)
    }

    /// Convert an `f32` to a `DeterministicScore` via `from_f64`.
    #[inline]
    pub fn from_f32(val: f32) -> Self {
        Self::from_f64(val as f64)
    }

    /// Convert this score back to `f64` (sentinels map to ±Infinity).
    #[inline]
    pub fn to_f64(self) -> f64 {
        if self.0 == Self::MAX.0 {
            return f64::INFINITY;
        }
        if self.0 == Self::NEG_INF.0 {
            return f64::NEG_INFINITY;
        }
        self.0 as f64 / Self::SCALE
    }

    /// Return `true` if the score is the `MAX` or `NEG_INF` sentinel.
    #[inline]
    pub const fn is_infinite(self) -> bool {
        self.0 == Self::MAX.0 || self.0 == Self::NEG_INF.0
    }

    /// Saturating arithmetic helper: clamps to `[NEG_INF, MAX]`, never produces `MIN`.
    #[inline]
    fn from_arithmetic_raw(raw: i128) -> Self {
        if raw >= Self::MAX.0 as i128 {
            Self::MAX
        } else if raw <= Self::NEG_INF.0 as i128 {
            Self::NEG_INF
        } else {
            Self(raw as i64)
        }
    }

    /// Float conversion helper: NaN → ZERO, ±Inf → MAX/NEG_INF, finite → clamped to `[NEG_INF, MAX]`.
    #[inline]
    fn from_rounded_arithmetic(raw: f64) -> Self {
        if raw.is_nan() {
            Self::ZERO
        } else if raw.is_sign_positive() && !raw.is_finite() {
            Self::MAX
        } else if !raw.is_finite() {
            Self::NEG_INF
        } else if raw >= Self::MAX.0 as f64 {
            Self::MAX
        } else if raw <= Self::NEG_INF.0 as f64 {
            Self::NEG_INF
        } else {
            Self(raw as i64)
        }
    }
}

impl Ord for DeterministicScore {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.cmp(&other.0)
    }
}

impl PartialOrd for DeterministicScore {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Hash for DeterministicScore {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl Default for DeterministicScore {
    fn default() -> Self {
        Self::ZERO
    }
}

impl Add for DeterministicScore {
    type Output = Self;
    #[inline]
    fn add(self, rhs: Self) -> Self::Output {
        Self::from_arithmetic_raw(self.0 as i128 + rhs.0 as i128)
    }
}

impl Sub for DeterministicScore {
    type Output = Self;
    #[inline]
    fn sub(self, rhs: Self) -> Self::Output {
        Self::from_arithmetic_raw(self.0 as i128 - rhs.0 as i128)
    }
}

impl Mul<i64> for DeterministicScore {
    type Output = Self;
    #[inline]
    fn mul(self, rhs: i64) -> Self::Output {
        let result = (self.0 as i128).saturating_mul(rhs as i128);
        Self::from_arithmetic_raw(result)
    }
}

impl Mul<f64> for DeterministicScore {
    type Output = Self;
    #[inline]
    fn mul(self, rhs: f64) -> Self::Output {
        if rhs.is_nan() {
            return Self::ZERO;
        }
        let product = (self.0 as f64) * rhs;
        Self::from_rounded_arithmetic(product.round())
    }
}

impl Div<i64> for DeterministicScore {
    type Output = Self;
    #[inline]
    fn div(self, rhs: i64) -> Self::Output {
        if rhs == 0 {
            return if self.0 == 0 {
                Self::ZERO
            } else if self.0 > 0 {
                Self::MAX
            } else {
                Self::NEG_INF
            };
        }
        Self::from_arithmetic_raw(self.0.saturating_div(rhs) as i128)
    }
}

impl fmt::Debug for DeterministicScore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if *self == Self::MAX {
            write!(f, "DeterministicScore(+Inf)")
        } else if *self == Self::NEG_INF {
            write!(f, "DeterministicScore(-Inf)")
        } else {
            write!(f, "DeterministicScore({:.9})", self.to_f64())
        }
    }
}

impl fmt::Display for DeterministicScore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if *self == Self::MAX {
            write!(f, "+Inf")
        } else if *self == Self::NEG_INF {
            write!(f, "-Inf")
        } else {
            write!(f, "{:.6}", self.to_f64())
        }
    }
}

impl From<f64> for DeterministicScore {
    fn from(val: f64) -> Self {
        Self::from_f64(val)
    }
}

impl From<f32> for DeterministicScore {
    fn from(val: f32) -> Self {
        Self::from_f32(val)
    }
}

impl From<DeterministicScore> for f64 {
    fn from(score: DeterministicScore) -> Self {
        score.to_f64()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_f64() {
        let s = DeterministicScore::from_f64(0.5);
        assert!((s.to_f64() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn nan_maps_to_zero() {
        let s = DeterministicScore::from_f64(f64::NAN);
        assert_eq!(s, DeterministicScore::ZERO);
        assert!((s.to_f64() - 0.0).abs() < 1e-15);
    }

    #[test]
    fn infinity() {
        assert_eq!(
            DeterministicScore::from_f64(f64::INFINITY),
            DeterministicScore::MAX
        );
        assert_eq!(
            DeterministicScore::from_f64(f64::NEG_INFINITY),
            DeterministicScore::NEG_INF
        );
    }

    #[test]
    fn ordering() {
        let a = DeterministicScore::from_f64(0.1);
        let b = DeterministicScore::from_f64(0.5);
        assert!(a < b);
    }

    #[test]
    fn arithmetic() {
        let a = DeterministicScore::from_f64(0.3);
        let b = DeterministicScore::from_f64(0.4);
        let sum = a + b;
        assert!((sum.to_f64() - 0.7).abs() < 1e-9);
    }

    #[test]
    fn scaling_by_f64() {
        let s = DeterministicScore::from_f64(0.5);
        let scaled = s * 2.0;
        assert!((scaled.to_f64() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn div_by_zero() {
        let pos = DeterministicScore::from_f64(0.5);
        assert_eq!(pos / 0, DeterministicScore::MAX);
        let neg = DeterministicScore::from_f64(-0.5);
        assert_eq!(neg / 0, DeterministicScore::NEG_INF);
        assert_eq!(DeterministicScore::ZERO / 0, DeterministicScore::ZERO);
    }

    #[test]
    fn raw_scale_known_value() {
        assert_eq!(
            DeterministicScore::from_f64(1.0).to_raw(),
            4_294_967_296_i64
        );
    }

    #[test]
    fn display_formatting() {
        let s = format!("{}", DeterministicScore::from_f64(0.1234567));
        assert_eq!(s, "0.123457");
        assert_eq!(format!("{}", DeterministicScore::MAX), "+Inf");
        assert_eq!(format!("{}", DeterministicScore::NEG_INF), "-Inf");
    }

    #[test]
    fn debug_formatting() {
        assert_eq!(
            format!("{:?}", DeterministicScore::MAX),
            "DeterministicScore(+Inf)"
        );
        assert_eq!(
            format!("{:?}", DeterministicScore::NEG_INF),
            "DeterministicScore(-Inf)"
        );
        let s = format!("{:?}", DeterministicScore::from_f64(0.5));
        assert!(
            s.starts_with("DeterministicScore(0.5"),
            "unexpected debug output: {s}"
        );
    }

    #[test]
    fn add_saturates_at_max() {
        assert_eq!(
            DeterministicScore::MAX + DeterministicScore::from_raw(1),
            DeterministicScore::MAX
        );
    }

    #[test]
    fn sub_saturates_at_neg_inf() {
        assert_eq!(
            DeterministicScore::NEG_INF - DeterministicScore::from_raw(1),
            DeterministicScore::NEG_INF
        );
    }

    #[test]
    fn mul_i64_saturates_at_max() {
        let large = DeterministicScore::from_raw(i64::MAX / 2);
        assert_eq!(large * 3_i64, DeterministicScore::MAX);
    }

    #[test]
    fn mul_f64_nan_yields_zero() {
        let s = DeterministicScore::from_f64(1.0);
        assert_eq!(s * f64::NAN, DeterministicScore::ZERO);
    }

    // NEG_INF = i64::MIN + 1; MIN (i64::MIN) is reserved sentinel (Lean: `MIN`)
    #[test]
    fn neg_inf_is_i64_min_plus_one() {
        assert_eq!(DeterministicScore::NEG_INF.to_raw(), i64::MIN + 1);
    }

    #[test]
    fn min_sentinel_is_i64_min() {
        assert_eq!(DeterministicScore::MIN.to_raw(), i64::MIN);
    }

    #[test]
    fn min_sentinel_distinct_from_neg_inf() {
        assert_ne!(DeterministicScore::MIN, DeterministicScore::NEG_INF);
        assert!(DeterministicScore::MIN < DeterministicScore::NEG_INF);
    }

    #[test]
    fn neg_infinity_maps_to_neg_inf() {
        assert_eq!(
            DeterministicScore::from_f64(f64::NEG_INFINITY),
            DeterministicScore::NEG_INF
        );
    }

    #[test]
    fn underflow_clamps_to_neg_inf_not_min() {
        // Arithmetic must clamp at NEG_INF (= i64::MIN + 1), never produce MIN.
        let result = DeterministicScore::from_raw(i64::MIN + 1) - DeterministicScore::from_raw(1);
        assert_eq!(result, DeterministicScore::NEG_INF);
        assert_ne!(result, DeterministicScore::MIN);
    }

    // ── from_raw_saturating / from_raw_checked ────────────────────────────────

    #[test]
    fn from_raw_saturating_min_maps_to_neg_inf() {
        let s = DeterministicScore::from_raw_saturating(i64::MIN);
        assert_eq!(
            s,
            DeterministicScore::NEG_INF,
            "i64::MIN must saturate to NEG_INF, not the reserved MIN sentinel"
        );
    }

    #[test]
    fn from_raw_saturating_neg_inf_raw_is_identity() {
        let s = DeterministicScore::from_raw_saturating(i64::MIN + 1);
        assert_eq!(s, DeterministicScore::NEG_INF);
    }

    #[test]
    fn from_raw_saturating_normal_value_is_identity() {
        let s = DeterministicScore::from_raw_saturating(42);
        assert_eq!(s.to_raw(), 42);
    }

    #[test]
    fn from_raw_checked_min_returns_none() {
        assert!(
            DeterministicScore::from_raw_checked(i64::MIN).is_none(),
            "i64::MIN should be rejected by from_raw_checked"
        );
    }

    #[test]
    fn from_raw_checked_valid_value_returns_some() {
        let s = DeterministicScore::from_raw_checked(i64::MIN + 1).unwrap();
        assert_eq!(s, DeterministicScore::NEG_INF);
    }

    #[test]
    fn from_raw_checked_zero_returns_some() {
        let s = DeterministicScore::from_raw_checked(0).unwrap();
        assert_eq!(s, DeterministicScore::ZERO);
    }

    // ── serde: custom Deserialize rejects i64::MIN ────────────────────────────

    #[cfg(feature = "serde")]
    #[test]
    fn serde_deserialize_rejects_i64_min() {
        let raw_json = format!("{}", i64::MIN);
        let result: Result<DeterministicScore, _> = serde_json::from_str(&raw_json);
        assert!(
            result.is_err(),
            "deserializing i64::MIN must fail: got {:?}",
            result
        );
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_deserialize_accepts_neg_inf_raw() {
        let raw_json = format!("{}", i64::MIN + 1);
        let s: DeterministicScore = serde_json::from_str(&raw_json).unwrap();
        assert_eq!(s, DeterministicScore::NEG_INF);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_roundtrip_zero() {
        let original = DeterministicScore::ZERO;
        let json = serde_json::to_string(&original).unwrap();
        let restored: DeterministicScore = serde_json::from_str(&json).unwrap();
        assert_eq!(original, restored);
    }
}
