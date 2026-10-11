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
mod tests {
    use super::*;

    fn variants(half_life: f64) -> [Decay; 6] {
        let h = HalfLife::try_new(half_life).unwrap();
        [
            Decay::exponential(0.25).unwrap(),
            Decay::hyperbolic(0.25).unwrap(),
            Decay::exponential_with_half_life(h),
            Decay::hyperbolic_with_half_life(h),
            Decay::PowerLaw(h),
            Decay::None,
        ]
    }

    #[test]
    fn constructors_reject_invalid_domains_without_losing_subnormal_half_lives() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                HalfLife::try_new(value),
                Err(ArithmeticError::NonFinite {
                    parameter: "half_life"
                })
            );
            assert_eq!(
                DecayParameter::try_from_rate(value),
                Err(ArithmeticError::NonFinite { parameter: "rate" })
            );
        }
        for value in [0.0, -0.0, -1.0] {
            assert_eq!(
                HalfLife::try_new(value),
                Err(ArithmeticError::OutOfRange {
                    parameter: "half_life"
                })
            );
        }
        assert_eq!(
            DecayParameter::try_from_rate(-1.0),
            Err(ArithmeticError::OutOfRange { parameter: "rate" })
        );
        for value in [f64::from_bits(1), 1.0, f64::MAX] {
            assert_eq!(
                HalfLife::try_new(value).unwrap().value().to_bits(),
                value.to_bits()
            );
        }
    }

    #[test]
    fn nonpositive_time_and_zero_inputs_preserve_bits_in_every_form() {
        for decay in variants(f64::from_bits(1)) {
            for value in [-0.0_f64, 0.0, 0.3, 1.5, -2.0, f64::MAX] {
                for elapsed in [-f64::MAX, -1.0, -0.0, 0.0] {
                    assert_eq!(
                        decay.apply(value, elapsed).unwrap().to_bits(),
                        value.to_bits()
                    );
                    assert_eq!(decay.factor(elapsed).unwrap(), 1.0);
                }
            }
            for value in [-0.0_f64, 0.0] {
                assert_eq!(
                    decay.apply(value, f64::MAX).unwrap().to_bits(),
                    value.to_bits()
                );
            }
        }
        for rate in [-0.0, 0.0] {
            for decay in [
                Decay::exponential(rate).unwrap(),
                Decay::hyperbolic(rate).unwrap(),
            ] {
                assert_eq!(decay.apply(-1.5, f64::MAX).unwrap(), -1.5);
                assert_eq!(decay.factor(f64::MAX).unwrap(), 1.0);
            }
        }
    }

    #[test]
    fn nonfinite_inputs_refuse_before_identity_shortcuts() {
        let mut decays = variants(1.0).to_vec();
        decays.extend([
            Decay::exponential(0.0).unwrap(),
            Decay::hyperbolic(0.0).unwrap(),
        ]);
        for decay in decays {
            for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
                assert_eq!(
                    decay.apply(invalid, 0.0),
                    Err(ArithmeticError::NonFinite { parameter: "value" })
                );
                assert_eq!(
                    decay.apply(0.0, invalid),
                    Err(ArithmeticError::NonFinite {
                        parameter: "elapsed"
                    })
                );
                assert_eq!(
                    decay.factor(invalid),
                    Err(ArithmeticError::NonFinite {
                        parameter: "elapsed"
                    })
                );
            }
        }
    }

    #[test]
    fn supplied_half_lives_halve_factors_even_at_finite_extremes() {
        for value in [f64::from_bits(1), 4.0, f64::MAX] {
            let h = HalfLife::try_new(value).unwrap();
            for decay in [
                Decay::exponential_with_half_life(h),
                Decay::hyperbolic_with_half_life(h),
                Decay::PowerLaw(h),
            ] {
                assert_eq!(decay.factor(value).unwrap(), 0.5);
                assert_eq!(decay.apply(1.0, value).unwrap(), 0.5);
            }
            assert_eq!(Decay::None.factor(value).unwrap(), 1.0);
        }
        let rate = 0.25;
        assert!(
            (Decay::exponential(rate)
                .unwrap()
                .factor(std::f64::consts::LN_2 / rate)
                .unwrap()
                - 0.5)
                .abs()
                < 1e-12
        );
        assert!((Decay::hyperbolic(rate).unwrap().factor(1.0 / rate).unwrap() - 0.5).abs() < 1e-12);
    }

    #[test]
    fn power_law_retains_ordinary_scalar_bits_and_is_not_exponential() {
        let decay = Decay::PowerLaw(HalfLife::try_new(0.3).unwrap());
        let actual = decay.apply(0.1, 3.0).unwrap();
        assert_eq!(actual.to_bits(), 0x3f82_9e41_29e4_129e);
        assert_ne!(actual.to_bits(), (0.1_f64 / (1.0 + 3.0 / 0.3)).to_bits());
        assert_ne!(
            actual.to_bits(),
            (0.1 * decay.factor(3.0).unwrap()).to_bits()
        );

        let h = HalfLife::try_new(1.0).unwrap();
        assert_eq!(Decay::PowerLaw(h).factor(3.0).unwrap(), 0.25);
        assert_eq!(
            Decay::exponential_with_half_life(h).factor(3.0).unwrap(),
            0.125
        );
    }

    #[test]
    fn power_law_keeps_finite_underflow_and_uses_fallback_only_for_overflow() {
        let tiny = f64::from_bits(1);
        let decay = Decay::PowerLaw(HalfLife::try_new(tiny).unwrap());
        assert_eq!(decay.apply(0.5, tiny).unwrap(), 0.0);
        assert_eq!(decay.factor(tiny).unwrap(), 0.5);
        assert_eq!(decay.apply(0.5, 0.0).unwrap(), 0.5);

        let decay = Decay::PowerLaw(HalfLife::try_new(3.0 * tiny).unwrap());
        assert_eq!(decay.apply(0.2, tiny).unwrap(), 0.25);
        assert_eq!(decay.factor(tiny).unwrap(), 0.75);

        let decay = Decay::PowerLaw(HalfLife::try_new(f64::MAX).unwrap());
        assert_eq!(decay.apply(0.75, f64::MAX).unwrap(), 0.375);
        assert_eq!(decay.apply(-0.75, f64::MAX).unwrap(), -0.375);
        let numerator_overflow = Decay::PowerLaw(HalfLife::try_new(2.0).unwrap());
        assert_eq!(
            numerator_overflow.apply(f64::MAX, 1.0).unwrap(),
            f64::MAX / 1.5
        );
    }

    #[test]
    fn overflow_limits_preserve_signed_zero_and_nonunit_values_stay_nonunit() {
        let tiny = HalfLife::try_new(f64::from_bits(1)).unwrap();
        for decay in [
            Decay::exponential(f64::MAX).unwrap(),
            Decay::hyperbolic(f64::MAX).unwrap(),
            Decay::exponential_with_half_life(tiny),
            Decay::hyperbolic_with_half_life(tiny),
            Decay::PowerLaw(tiny),
        ] {
            assert_eq!(decay.factor(f64::MAX).unwrap(), 0.0);
            assert_eq!(
                decay.apply(-1.5, f64::MAX).unwrap().to_bits(),
                (-0.0_f64).to_bits()
            );
        }
        let h = HalfLife::try_new(4.0).unwrap();
        for decay in [
            Decay::exponential_with_half_life(h),
            Decay::hyperbolic_with_half_life(h),
            Decay::PowerLaw(h),
        ] {
            assert_eq!(decay.apply(1.5, 4.0).unwrap(), 0.75);
            assert_eq!(decay.apply(-1.5, 4.0).unwrap(), -0.75);
        }
        assert_eq!(Decay::None.apply(1.5, f64::MAX).unwrap(), 1.5);
    }

    #[test]
    fn unit_convenience_preserves_zero_bits_and_rejects_nonfinite_time() {
        for decay in variants(f64::MAX) {
            for raw in [-0.0_f64, 0.0, 0.1, 1.0] {
                let unit = UnitScore::try_new(raw).unwrap();
                assert_eq!(
                    decay.apply_unit(unit, 0.0).unwrap().value().to_bits(),
                    raw.to_bits()
                );
                let result = decay.apply_unit(unit, f64::MAX).unwrap().value();
                assert!(result.is_finite() && (0.0..=1.0).contains(&result));
                if raw == 0.0 {
                    assert_eq!(result.to_bits(), raw.to_bits());
                }
                assert_eq!(
                    decay.apply_unit(unit, f64::NAN),
                    Err(ArithmeticError::NonFinite {
                        parameter: "elapsed"
                    })
                );
            }
        }
    }
}
