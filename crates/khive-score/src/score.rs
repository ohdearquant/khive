//! Core `DeterministicScore` — fixed-point integer scoring (ADR-006).
//!
//! Cross-platform deterministic by converting f64 to i64 with 2^32 scaling.
//! NaN → 0 (neutral ranking), +Inf → MAX, -Inf → NEG_INF.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::{Add, Div, Mul, Sub};

#[derive(Copy, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[repr(transparent)]
pub struct DeterministicScore(i64);

impl DeterministicScore {
    const SCALE: f64 = 4_294_967_296.0; // 2^32

    pub const MAX: Self = Self(i64::MAX);
    pub const NEG_INF: Self = Self(i64::MIN + 1);
    pub const ZERO: Self = Self(0);

    #[inline]
    pub const fn from_raw(raw: i64) -> Self {
        Self(raw)
    }

    #[inline]
    pub const fn to_raw(self) -> i64 {
        self.0
    }

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

    #[inline]
    pub fn from_f32(val: f32) -> Self {
        Self::from_f64(val as f64)
    }

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

    #[inline]
    pub const fn is_infinite(self) -> bool {
        self.0 == i64::MAX || self.0 == Self::NEG_INF.0
    }

    #[inline]
    fn from_arithmetic_raw(raw: i128) -> Self {
        if raw >= i64::MAX as i128 {
            Self::MAX
        } else if raw <= Self::NEG_INF.0 as i128 {
            Self::NEG_INF
        } else {
            Self(raw as i64)
        }
    }

    #[inline]
    fn from_rounded_arithmetic(raw: f64) -> Self {
        if raw.is_nan() {
            Self::ZERO
        } else if raw.is_sign_positive() && !raw.is_finite() {
            Self::MAX
        } else if !raw.is_finite() {
            Self::NEG_INF
        } else if raw >= i64::MAX as f64 {
            Self::MAX
        } else if raw <= i64::MIN as f64 {
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
}
