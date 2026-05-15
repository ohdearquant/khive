//! Vector similarity metric.

/// Distance metric for vector similarity search.
///
/// # Variants
/// - `Cosine`: `1 - cosine_similarity`. Value in [0, 2] for unit vectors.
/// - `Dot`: dot product (negated for min-heap; higher dot = lower distance).
/// - `L2`: Euclidean (L2) distance.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum DistanceMetric {
    Cosine,
    Dot,
    L2,
}

impl Default for DistanceMetric {
    #[inline]
    fn default() -> Self {
        Self::Cosine
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_cosine() {
        assert_eq!(DistanceMetric::default(), DistanceMetric::Cosine);
    }
}
