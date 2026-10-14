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
#[path = "band_tests.rs"]
mod tests;
