//! Min-max arithmetic with explicit floating-point degeneracy policy.

use crate::{ArithmeticError, DeterministicScore};

/// How a floating-point band handles a span at or below its supplied tolerance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DegenerateSpan {
    /// Return the supplied ceiling directly.
    Ceiling,
    /// Evaluate `floor + (ceiling - floor)` with two separately rounded operations.
    UpperEndpoint,
    /// Clamp the maximum input score to the supplied band.
    ClampMaximum,
}

macro_rules! float_band {
    ($float:ty, $name:ident) => {
        /// Normalize finite scores in their original order and precision.
        ///
        /// Bounds must be finite and ordered; tolerance must be finite and
        /// nonnegative. Configuration is checked even for empty input. For a
        /// nonempty input, every arithmetic intermediate must be representable.
        /// No row filtering, signal adjustment or implicit unit clamp is applied.
        pub fn $name(
            scores: &[$float],
            floor: $float,
            ceiling: $float,
            tolerance: $float,
            degenerate: DegenerateSpan,
        ) -> Result<Vec<$float>, ArithmeticError> {
            fn finite(value: $float, operation: &'static str) -> Result<$float, ArithmeticError> {
                if value.is_finite() {
                    Ok(value)
                } else {
                    Err(ArithmeticError::Overflow { operation })
                }
            }
            for (parameter, value) in [
                ("floor", floor),
                ("ceiling", ceiling),
                ("tolerance", tolerance),
            ] {
                if !value.is_finite() {
                    return Err(ArithmeticError::NonFinite { parameter });
                }
            }
            if floor > ceiling {
                return Err(ArithmeticError::InvalidBounds);
            }
            if tolerance < 0.0 {
                return Err(ArithmeticError::OutOfRange {
                    parameter: "tolerance",
                });
            }
            if scores.is_empty() {
                return Ok(Vec::new());
            }
            if scores.iter().any(|score| !score.is_finite()) {
                return Err(ArithmeticError::NonFinite {
                    parameter: "scores",
                });
            }
            let min = scores
                .iter()
                .copied()
                .fold(<$float>::INFINITY, <$float>::min);
            let max = scores
                .iter()
                .copied()
                .fold(<$float>::NEG_INFINITY, <$float>::max);
            let span = finite(max - min, "band span")?;
            let range = finite(ceiling - floor, "band range")?;
            if span <= tolerance {
                let value = match degenerate {
                    DegenerateSpan::Ceiling => ceiling,
                    DegenerateSpan::UpperEndpoint => finite(floor + range, "band upper endpoint")?,
                    DegenerateSpan::ClampMaximum => max.clamp(floor, ceiling),
                };
                return Ok(vec![value; scores.len()]);
            }
            scores
                .iter()
                .map(|score| {
                    let difference = finite(*score - min, "band difference")?;
                    let percentile = finite(difference / span, "band division")?.clamp(0.0, 1.0);
                    let scaled = finite(percentile * range, "band product")?;
                    finite(floor + scaled, "band sum")
                })
                .collect()
        }
    };
}

float_band!(f64, min_max_band);
float_band!(f32, min_max_band_f32);

/// Normalize raw fixed-point scores to `[0, 1]` using exact i128 intermediates.
///
/// Empty input stays empty; equal and singleton inputs map to one. All raw i64
/// values, including sentinels, retain the existing fusion normalization behavior.
/// No floating-point conversion, input filtering or score reordering occurs.
pub fn min_max_band_fixed(scores: &[DeterministicScore]) -> Vec<DeterministicScore> {
    const SCALE: i128 = 4_294_967_296;
    let Some(min) = scores.iter().map(|score| score.to_raw()).min() else {
        return Vec::new();
    };
    let max = scores.iter().map(|score| score.to_raw()).max().unwrap();
    let span = i128::from(max) - i128::from(min);
    if span <= 0 {
        return vec![DeterministicScore::from_raw(SCALE as i64); scores.len()];
    }
    scores
        .iter()
        .map(|score| {
            let numerator = (i128::from(score.to_raw()) - i128::from(min)) * SCALE;
            let raw = (numerator / span).clamp(0, i128::from(i64::MAX));
            DeterministicScore::from_raw(raw as i64)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bands_keep_order_cardinality_and_explicit_degenerate_policies() {
        assert!(min_max_band(&[], 0.0, 1.0, 0.0, DegenerateSpan::Ceiling)
            .unwrap()
            .is_empty());
        assert_eq!(
            min_max_band(&[9.0, 1.0, 5.0], -2.0, 2.0, 0.0, DegenerateSpan::Ceiling).unwrap(),
            [2.0, -2.0, 0.0]
        );
        for policy in [DegenerateSpan::Ceiling, DegenerateSpan::UpperEndpoint] {
            assert_eq!(
                min_max_band(&[0.2, 0.2], 0.1, 0.5, 0.0, policy).unwrap(),
                [0.5, 0.5]
            );
        }
        assert_eq!(
            min_max_band(&[0.2], 0.1, 0.5, 0.0, DegenerateSpan::ClampMaximum).unwrap(),
            [0.2]
        );
        assert_eq!(
            min_max_band(
                &[0.2, 0.2 + f64::EPSILON],
                0.1,
                0.5,
                f64::EPSILON,
                DegenerateSpan::Ceiling
            )
            .unwrap(),
            [0.5, 0.5]
        );
    }

    #[test]
    fn f32_upper_endpoint_is_not_simplified_or_widened() {
        let upper = min_max_band_f32(
            &[0.2],
            0.15,
            0.82,
            f32::EPSILON,
            DegenerateSpan::UpperEndpoint,
        )
        .unwrap();
        let ceiling =
            min_max_band_f32(&[0.2], 0.15, 0.82, f32::EPSILON, DegenerateSpan::Ceiling).unwrap();
        assert_eq!(upper[0].to_bits(), 0x3f51_eb84);
        assert_eq!(ceiling[0].to_bits(), 0x3f51_eb85);
        assert_eq!(
            min_max_band_f32(&[2.0, 1.0], -1.0, 1.0, 0.0, DegenerateSpan::Ceiling).unwrap(),
            [1.0, -1.0]
        );
        let ordinary =
            min_max_band_f32(&[0.0, 1.0, 9.0], 0.15, 0.9, 0.0, DegenerateSpan::Ceiling).unwrap();
        assert_eq!(ordinary[1].to_bits(), 0x3e6e_eef0);
        let fused = (1.0f32 / 9.0).mul_add(0.9f32 - 0.15, 0.15);
        assert_eq!(fused.to_bits(), 0x3e6e_eeef);
        assert_eq!(
            min_max_band_f32(
                &[0.01],
                0.5,
                0.82,
                f32::EPSILON,
                DegenerateSpan::ClampMaximum
            )
            .unwrap(),
            [0.5]
        );
    }

    #[test]
    fn invalid_inputs_and_overflow_refuse_the_whole_band() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(min_max_band(&[0.0, value], 0.0, 1.0, 0.0, DegenerateSpan::Ceiling).is_err());
            assert!(min_max_band(&[], value, 1.0, 0.0, DegenerateSpan::Ceiling).is_err());
            assert!(min_max_band(&[0.0], 0.0, 1.0, value, DegenerateSpan::Ceiling).is_err());
        }
        assert_eq!(
            min_max_band(&[0.0], 1.0, 0.0, 0.0, DegenerateSpan::Ceiling),
            Err(ArithmeticError::InvalidBounds)
        );
        assert!(min_max_band(&[0.0], 0.0, 1.0, -1.0, DegenerateSpan::Ceiling).is_err());
        assert_eq!(
            min_max_band(
                &[-f64::MAX, f64::MAX],
                0.0,
                1.0,
                0.0,
                DegenerateSpan::Ceiling
            ),
            Err(ArithmeticError::Overflow {
                operation: "band span"
            })
        );
        assert_eq!(
            min_max_band(&[0.0], -f64::MAX, f64::MAX, 0.0, DegenerateSpan::Ceiling),
            Err(ArithmeticError::Overflow {
                operation: "band range"
            })
        );
        assert!(min_max_band_f32(
            &[-f32::MAX, f32::MAX],
            0.0,
            1.0,
            0.0,
            DegenerateSpan::Ceiling
        )
        .is_err());
    }

    #[test]
    fn fixed_band_preserves_integer_division_and_extreme_raw_behavior() {
        let normalize = |raw: &[i64]| {
            min_max_band_fixed(
                &raw.iter()
                    .copied()
                    .map(DeterministicScore::from_raw)
                    .collect::<Vec<_>>(),
            )
            .into_iter()
            .map(DeterministicScore::to_raw)
            .collect::<Vec<_>>()
        };
        assert!(normalize(&[]).is_empty());
        assert_eq!(normalize(&[0, 1, 3]), [0, 1_431_655_765, 4_294_967_296]);
        assert_eq!(normalize(&[-3, -2, 0]), [0, 1_431_655_765, 4_294_967_296]);
        assert_eq!(
            normalize(&[i64::MAX, i64::MIN, i64::MIN + 1]),
            [4_294_967_296, 0, 0]
        );
        assert_eq!(normalize(&[i64::MIN]), [4_294_967_296]);
        assert_eq!(normalize(&[9, 9]), [4_294_967_296, 4_294_967_296]);
    }
}
