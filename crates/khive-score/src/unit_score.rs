//! Checked unit-interval values and scalar adjustment arithmetic.

use crate::{ArithmeticError, DeterministicScore};

/// A finite `f64` in `[0, 1]`, retaining either input sign of zero.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct UnitScore(f64);

impl UnitScore {
    /// Exactly positive zero.
    pub const ZERO: Self = Self(0.0);
    /// Exactly one.
    pub const ONE: Self = Self(1.0);

    /// Reject nonfinite values and values outside the unit interval.
    pub fn try_new(value: f64) -> Result<Self, ArithmeticError> {
        if !value.is_finite() {
            return Err(ArithmeticError::NonFinite { parameter: "value" });
        }
        if !(0.0..=1.0).contains(&value) {
            return Err(ArithmeticError::OutOfRange { parameter: "value" });
        }
        Ok(Self(value))
    }

    /// Return the validated value without changing its precision or zero sign.
    pub const fn value(self) -> f64 {
        self.0
    }

    /// Interpret `[0, 1]` as a fraction and `(1, 100]` as a percentage.
    pub fn from_fraction_or_percent(value: f64) -> Result<Self, ArithmeticError> {
        if !value.is_finite() {
            return Err(ArithmeticError::NonFinite { parameter: "value" });
        }
        if !(0.0..=100.0).contains(&value) {
            return Err(ArithmeticError::OutOfRange { parameter: "value" });
        }
        Self::try_new(if value <= 1.0 { value } else { value / 100.0 })
    }

    /// Clamp finite input to `[0, 1]`; nonfinite input is an error.
    pub fn clamp01(value: f64) -> Result<Self, ArithmeticError> {
        if !value.is_finite() {
            return Err(ArithmeticError::NonFinite { parameter: "value" });
        }
        Ok(Self(value.clamp(0.0, 1.0)))
    }

    /// Apply `self * (1 + weight * x)` and then one terminal unit clamp.
    ///
    /// This is not an intermediate step in a longer adjustment chain. Every
    /// input and intermediate must be finite, even when `self` is zero.
    pub fn boost(self, weight: f64, x: f64) -> Result<Self, ArithmeticError> {
        let result = self.0 * boost_factor(weight, x)?;
        if !result.is_finite() {
            return Err(ArithmeticError::Overflow {
                operation: "unit boost",
            });
        }
        Self::clamp01(result)
    }
}

impl From<UnitScore> for DeterministicScore {
    fn from(value: UnitScore) -> Self {
        Self::from_f64(value.value())
    }
}

impl TryFrom<DeterministicScore> for UnitScore {
    type Error = ArithmeticError;

    fn try_from(value: DeterministicScore) -> Result<Self, Self::Error> {
        if !(0..=4_294_967_296).contains(&value.to_raw()) {
            return Err(ArithmeticError::OutOfRange { parameter: "score" });
        }
        Self::try_new(value.to_f64())
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for UnitScore {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <f64 as serde::Deserialize>::deserialize(deserializer)?;
        Self::try_new(value).map_err(serde::de::Error::custom)
    }
}

macro_rules! scalar_helpers {
    ($float:ty, $boost:ident, $affine:ident) => {
        /// Compute `1 + weight * x` in the input precision, without clamping.
        pub fn $boost(weight: $float, x: $float) -> Result<$float, ArithmeticError> {
            for (parameter, value) in [("weight", weight), ("x", x)] {
                if !value.is_finite() {
                    return Err(ArithmeticError::NonFinite { parameter });
                }
            }
            let product = weight * x;
            if !product.is_finite() {
                return Err(ArithmeticError::Overflow {
                    operation: "boost product",
                });
            }
            let result = 1.0 + product;
            if !result.is_finite() {
                return Err(ArithmeticError::Overflow {
                    operation: "boost sum",
                });
            }
            Ok(result)
        }

        /// Compute `clamp(1 + weight * (mean - 0.5), lo, hi)` without widening.
        ///
        /// Bounds must be finite and ordered. Intermediate overflow is refused
        /// before clamping; no posterior policy or absent-value default is chosen.
        pub fn $affine(
            mean: $float,
            weight: $float,
            lo: $float,
            hi: $float,
        ) -> Result<$float, ArithmeticError> {
            for (parameter, value) in [("mean", mean), ("weight", weight), ("lo", lo), ("hi", hi)] {
                if !value.is_finite() {
                    return Err(ArithmeticError::NonFinite { parameter });
                }
            }
            if lo > hi {
                return Err(ArithmeticError::InvalidBounds);
            }
            let centered = mean - 0.5;
            if !centered.is_finite() {
                return Err(ArithmeticError::Overflow {
                    operation: "affine subtraction",
                });
            }
            let product = weight * centered;
            if !product.is_finite() {
                return Err(ArithmeticError::Overflow {
                    operation: "affine product",
                });
            }
            let result = 1.0 + product;
            if !result.is_finite() {
                return Err(ArithmeticError::Overflow {
                    operation: "affine sum",
                });
            }
            Ok(result.clamp(lo, hi))
        }
    };
}

scalar_helpers!(f64, boost_factor, affine_clamped);
scalar_helpers!(f32, boost_factor_f32, affine_clamped_f32);

#[cfg(test)]
#[path = "unit_score_tests.rs"]
mod tests;
