//! Checkpoint configuration and serialization helpers.

use serde::{Deserialize, Serialize};

use crate::NodeId;

/// Sort `NodeId`s by byte representation for deterministic, reproducible ordering.
#[inline]
pub fn sort_ids(ids: &mut [NodeId]) {
    ids.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
}

/// Subset of `HnswConfig` for checkpoint compatibility; stored as simple values for forward compat.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HnswCheckpointConfig {
    /// Maximum connections per node per layer (M).
    pub m: usize,
    /// Size of dynamic candidate list during construction.
    pub ef_construction: usize,
    /// Distance metric name (e.g. `"cosine"`, `"dot"`, `"euclidean"`).
    pub metric: String,
}

impl HnswCheckpointConfig {
    /// Create a checkpoint config from the full [`super::super::config::HnswConfig`].
    pub fn from_hnsw_config(config: &super::super::config::HnswConfig) -> Self {
        Self {
            m: config.m,
            ef_construction: config.ef_construction,
            metric: metric_to_string(&config.metric),
        }
    }
}

/// Convert a [`super::super::config::DistanceMetric`] to its canonical string representation.
pub(crate) fn metric_to_string(metric: &super::super::config::DistanceMetric) -> String {
    match metric {
        super::super::config::DistanceMetric::Cosine => "cosine".to_string(),
        super::super::config::DistanceMetric::Dot => "dot".to_string(),
        super::super::config::DistanceMetric::L2 => "euclidean".to_string(),
        // Fall back to debug repr for future variants.
        other => format!("{:?}", other).to_lowercase(),
    }
}
