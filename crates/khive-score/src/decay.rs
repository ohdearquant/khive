//! Checked scalar decay with caller-supplied time units.

use crate::{ArithmeticError, UnitScore};

/// A finite, strictly positive half-life, in the caller's time unit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HalfLife(f64);

impl HalfLife {
    /// Accept positive finite values, including positive subnormals.
    pub fn try_new(value: f64) -> Result<Self, ArithmeticError> {
        require_finite(value, "half_life")?;
        if value <= 0.0 {
            return Err(ArithmeticError::OutOfRange {
                parameter: "half_life",
            });
        }
        Ok(Self(value))
    }

    /// Return the half-life in its original unit.
    pub fn value(self) -> f64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum DecayParameterKind {
    Rate(f64),
    HalfLife(HalfLife),
}

/// A checked rate or retained half-life for exponential or hyperbolic decay.
///
/// Keeping these representations separate avoids overflowing reciprocal rates
/// and does not claim bit equality between rate and half-life evaluation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DecayParameter(DecayParameterKind);

impl DecayParameter {
    /// Accept a finite nonnegative rate, including either signed zero.
    pub fn try_from_rate(rate: f64) -> Result<Self, ArithmeticError> {
        require_finite(rate, "rate")?;
        if rate < 0.0 {
            return Err(ArithmeticError::OutOfRange { parameter: "rate" });
        }
        Ok(Self(DecayParameterKind::Rate(rate)))
    }

    /// Retain the half-life without converting it to a reciprocal rate.
    pub fn from_half_life(half_life: HalfLife) -> Self {
        Self(DecayParameterKind::HalfLife(half_life))
    }
}

/// Scalar decay with validated parameters and no built-in time-unit policy.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Decay {
    /// `exp(-rate * elapsed)`, or `2^(-elapsed / half_life)`.
    Exponential(DecayParameter),
    /// `1 / (1 + rate * elapsed)`, or `1 / (1 + elapsed / half_life)`.
    Hyperbolic(DecayParameter),
    /// Rational exponent-one decay, retaining scalar multiplication order.
    PowerLaw(HalfLife),
    /// Identity for every finite value and elapsed time.
    None,
}

impl Decay {
    /// Construct exponential decay from a finite nonnegative rate.
    pub fn exponential(rate: f64) -> Result<Self, ArithmeticError> {
        Ok(Self::Exponential(DecayParameter::try_from_rate(rate)?))
    }

    /// Construct hyperbolic decay from a finite nonnegative rate.
    pub fn hyperbolic(rate: f64) -> Result<Self, ArithmeticError> {
        Ok(Self::Hyperbolic(DecayParameter::try_from_rate(rate)?))
    }

    /// Construct exponential decay while retaining the supplied half-life.
    pub fn exponential_with_half_life(half_life: HalfLife) -> Self {
        Self::Exponential(DecayParameter::from_half_life(half_life))
    }

    /// Construct hyperbolic decay while retaining the supplied half-life.
    pub fn hyperbolic_with_half_life(half_life: HalfLife) -> Self {
        Self::Hyperbolic(DecayParameter::from_half_life(half_life))
    }

    /// Apply decay to any finite scalar, including signed and nonunit values.
    ///
    /// Both inputs are validated before identity shortcuts. Nonpositive finite
    /// time, zero input and zero rate preserve the input bits. For positive time,
    /// PowerLaw retains `value * h / (h + elapsed)` when its intermediates are
    /// finite; only overflow selects `value / (1 + elapsed / h)`. Finite underflow
    /// is retained, so this is not equivalent to multiplying by [`Self::factor`].
    pub fn apply(self, value: f64, elapsed: f64) -> Result<f64, ArithmeticError> {
        require_finite(value, "value")?;
        require_finite(elapsed, "elapsed")?;
        if elapsed <= 0.0 || value == 0.0 {
            return Ok(value);
        }
        Ok(match self {
            Self::Exponential(DecayParameter(DecayParameterKind::Rate(rate))) => {
                if rate == 0.0 {
                    value
                } else {
                    value * (-rate * elapsed).exp()
                }
            }
            Self::Exponential(DecayParameter(DecayParameterKind::HalfLife(half_life))) => {
                value * 2.0_f64.powf(-elapsed / half_life.value())
            }
            Self::Hyperbolic(DecayParameter(DecayParameterKind::Rate(rate))) => {
                if rate == 0.0 {
                    value
                } else {
                    value / (1.0 + rate * elapsed)
                }
            }
            Self::Hyperbolic(DecayParameter(DecayParameterKind::HalfLife(half_life))) => {
                value / (1.0 + elapsed / half_life.value())
            }
            Self::PowerLaw(half_life) => {
                let h = half_life.value();
                let numerator = value * h;
                let denominator = h + elapsed;
                if numerator.is_finite() && denominator.is_finite() {
                    numerator / denominator
                } else {
                    value / (1.0 + elapsed / h)
                }
            }
            Self::None => value,
        })
    }

    /// Return a factor in `[0, 1]`, rejecting nonfinite elapsed time.
    ///
    /// A positive product or ratio overflow yields the limiting factor zero.
    /// This factor does not reproduce PowerLaw scalar rounding when multiplied
    /// by an arbitrary value; use [`Self::apply`] for that evaluation contract.
    pub fn factor(self, elapsed: f64) -> Result<f64, ArithmeticError> {
        require_finite(elapsed, "elapsed")?;
        if elapsed <= 0.0 {
            return Ok(1.0);
        }
        Ok(match self {
            Self::Exponential(DecayParameter(DecayParameterKind::Rate(rate))) => {
                (-rate * elapsed).exp()
            }
            Self::Exponential(DecayParameter(DecayParameterKind::HalfLife(half_life))) => {
                2.0_f64.powf(-elapsed / half_life.value())
            }
            Self::Hyperbolic(DecayParameter(DecayParameterKind::Rate(rate))) => {
                1.0 / (1.0 + rate * elapsed)
            }
            Self::Hyperbolic(DecayParameter(DecayParameterKind::HalfLife(half_life))) => {
                1.0 / (1.0 + elapsed / half_life.value())
            }
            Self::PowerLaw(half_life) => {
                let h = half_life.value();
                let denominator = h + elapsed;
                if denominator.is_finite() {
                    h / denominator
                } else {
                    1.0 / (1.0 + elapsed / h)
                }
            }
            Self::None => 1.0,
        })
    }

    /// Apply scalar decay to a unit score without introducing a policy clamp.
    pub fn apply_unit(self, value: UnitScore, elapsed: f64) -> Result<UnitScore, ArithmeticError> {
        UnitScore::try_new(self.apply(value.value(), elapsed)?)
    }
}

fn require_finite(value: f64, parameter: &'static str) -> Result<(), ArithmeticError> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(ArithmeticError::NonFinite { parameter })
    }
}

#[cfg(test)]
#[path = "decay_tests.rs"]
mod tests;
