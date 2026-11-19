//! Fixed-point scoring: f64 → i64 at 2^32 scale for cross-platform determinism.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::{Add, Div, Mul, Sub};

/// Fixed-point `i64` score scaled by `2^32`, with saturating deterministic arithmetic.
///
/// The raw `i64::MIN` value is reserved. See
/// `crates/khive-score/docs/api/deterministic-score.md`.
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
#[path = "score_tests.rs"]
mod tests;
