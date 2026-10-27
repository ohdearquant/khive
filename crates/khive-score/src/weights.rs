//! Checked ordered weights and keyed means with explicit floating-point order.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use crate::ArithmeticError;

/// Finite, nonnegative ordered weights with at least one positive value.
///
/// Construction preserves the raw values, order, and signed-zero bits. A
/// normalized view is derived separately, without requiring a finite raw sum.
#[derive(Clone, Debug)]
pub struct ValidatedWeights {
    raw: Vec<f64>,
}

impl ValidatedWeights {
    /// Validate an owned sequence without normalizing or reordering it.
    ///
    /// Empty, nonfinite, negative, and all-zero inputs are rejected.
    pub fn try_new(weights: Vec<f64>) -> Result<Self, ArithmeticError> {
        if weights.is_empty() {
            return Err(ArithmeticError::EmptyWeights);
        }
        let mut has_positive = false;
        for &weight in &weights {
            validate_weight(weight)?;
            has_positive |= weight > 0.0;
        }
        if !has_positive {
            return Err(ArithmeticError::NoPositiveWeight);
        }
        Ok(Self { raw: weights })
    }

    /// Borrow the original ordered weights.
    pub fn as_slice(&self) -> &[f64] {
        &self.raw
    }

    /// Derive weights normalized by a max-scaled sum in the original order.
    ///
    /// No epsilon replaces positive weights. Ratios may round to zero; the
    /// positive maximum always contributes one to the scaled denominator.
    pub fn normalized(&self) -> Vec<f64> {
        let maximum = self.raw.iter().copied().fold(0.0, f64::max);
        let mut scaled_sum = 0.0;
        for &weight in &self.raw {
            scaled_sum += weight / maximum;
        }
        self.raw
            .iter()
            .map(|&weight| (weight / maximum) / scaled_sum)
            .collect()
    }
}

/// Finite, nonnegative configured weights for caller-ordered feature means.
///
/// Empty and all-zero maps are valid. The map constructor has already resolved
/// any duplicate configured keys; duplicate keys in a feature sequence refuse.
#[derive(Clone, Debug)]
pub struct NamedWeights<K> {
    weights: HashMap<K, f64>,
}

impl<K: Eq + Hash> NamedWeights<K> {
    /// Validate every configured weight, including keys a caller may not use.
    ///
    /// If several entries are invalid, the error order is unspecified.
    pub fn try_new(weights: HashMap<K, f64>) -> Result<Self, ArithmeticError> {
        for &weight in weights.values() {
            validate_weight(weight)?;
        }
        Ok(Self { weights })
    }

    /// Compute a mean over unique keys in the supplied feature order.
    ///
    /// Missing weights are zero. Unknown configured keys affect neither scale
    /// nor denominator. Zero-weight feature values are ignored, even if they
    /// are nonfinite; every raw positive-weight feature must be finite, even
    /// when its scaled weight would round to zero. No active features yields
    /// zero. Duplicate feature keys are rejected before arithmetic validation.
    ///
    /// Weight and feature scaling avoid overflow from finite inputs. The
    /// normalized intermediate is clamped to `[-1, 1]`, then rescaled; the final
    /// result is not a unit score. Accumulation follows the declared order with
    /// separate multiply and add operations, not map iteration or fused math.
    pub fn weighted_mean(&self, features: &[(K, f64)]) -> Result<f64, ArithmeticError> {
        let mut declared = HashSet::with_capacity(features.len());
        for (key, _) in features {
            if !declared.insert(key) {
                return Err(ArithmeticError::DuplicateKey);
            }
        }

        scaled_weighted_mean_with(
            features
                .iter()
                .map(|(key, value)| (self.weights.get(key).copied().unwrap_or(0.0), *value)),
            |value| {
                if !value.is_finite() {
                    Err(ArithmeticError::NonFinite {
                        parameter: "feature",
                    })
                } else {
                    Ok(())
                }
            },
        )
    }
}

/// Compute a max-scaled mean of `(weight, value)` pairs without validation.
///
/// Callers own weight and feature validation. For finite results, weights must be
/// finite and nonnegative, and values with nonzero weights must be finite. Raw
/// zero weights ignore their values. A zero maximum weight returns `0.0`.
/// Negative or nonfinite inputs are not rejected or sanitized; the denominator
/// includes only positive scaled weights, preserving unchecked arithmetic use.
///
/// Each iterator clone must replay the same pairs in the same order without
/// side effects. One ordered pass finds the weight maximum and active feature-
/// magnitude maximum (at least one); a second accumulates the scaled mean.
/// Multiplication and addition remain separate; the normalized intermediate is
/// clamped to `[-1, 1]` before rescaling, without clamping the final score.
///
/// The core allocates no heap storage of its own. Cloning and evaluating the
/// supplied iterator may have caller-defined costs; array and slice iterators
/// keep the arithmetic allocation-free with constant auxiliary space.
pub fn scaled_weighted_mean<I>(weighted_features: I) -> f64
where
    I: Iterator<Item = (f64, f64)> + Clone,
{
    match scaled_weighted_mean_with(
        weighted_features,
        |_| Ok::<(), std::convert::Infallible>(()),
    ) {
        Ok(mean) => mean,
        Err(never) => match never {},
    }
}

// Validate raw nonzero-weight values while finding both independent scales.
// Keeping the callback private leaves checked and unchecked policy at callers.
fn scaled_weighted_mean_with<I, E>(
    weighted_features: I,
    mut validate: impl FnMut(f64) -> Result<(), E>,
) -> Result<f64, E>
where
    I: Iterator<Item = (f64, f64)> + Clone,
{
    let mut scale = 0.0_f64;
    let mut feature_scale = 1.0_f64;
    for (weight, value) in weighted_features.clone() {
        if weight != 0.0 {
            validate(value)?;
        }
        scale = scale.max(weight);
        if weight != 0.0 {
            feature_scale = feature_scale.max(value.abs());
        }
    }
    if scale == 0.0 {
        return Ok(0.0);
    }
    let mut numerator = 0.0_f64;
    let mut weight_sum = 0.0_f64;
    for (weight, feature_value) in weighted_features {
        if weight == 0.0 {
            continue;
        }
        let normalized_weight = weight / scale;
        numerator += normalized_weight * (feature_value / feature_scale);
        if normalized_weight > 0.0 {
            weight_sum += normalized_weight;
        }
    }
    Ok((numerator / weight_sum).clamp(-1.0, 1.0) * feature_scale)
}

fn validate_weight(weight: f64) -> Result<(), ArithmeticError> {
    if !weight.is_finite() {
        Err(ArithmeticError::NonFinite {
            parameter: "weight",
        })
    } else if weight < 0.0 {
        Err(ArithmeticError::OutOfRange {
            parameter: "weight",
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[path = "weights_tests.rs"]
mod tests;
