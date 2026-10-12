//! Pre-swap validation for index migrations.

use super::error::AliasError;
use crate::HnswIndex;
use crate::NodeId;

/// Validate an index before an alias swap; implementations should be stateless or cheaply cloneable.
pub trait IndexValidator: Send + Sync {
    /// Validate the new index. Return `Ok(())` to proceed with the swap,
    /// or `Err(AliasError::ValidationFailed)` to abort.
    fn validate(&self, new_index: &HnswIndex) -> Result<(), AliasError>;
}

/// Validates recall@k against golden queries before an alias swap.
///
/// Swap is approved when `mean_recall >= min_recall` across all golden queries.
pub struct RecallValidator {
    /// Golden queries: `(query_vector, expected_nearest_ids)`.
    pub golden_queries: Vec<(Vec<f32>, Vec<NodeId>)>,
    /// Number of results to retrieve per query.
    pub k: usize,
    /// Minimum acceptable mean recall (e.g., 0.95 for 95%).
    pub min_recall: f32,
}

impl RecallValidator {
    /// Create a new recall validator.
    pub fn new(golden_queries: Vec<(Vec<f32>, Vec<NodeId>)>, k: usize, min_recall: f32) -> Self {
        Self {
            golden_queries,
            k,
            min_recall,
        }
    }
}

impl IndexValidator for RecallValidator {
    fn validate(&self, new_index: &HnswIndex) -> Result<(), AliasError> {
        if self.golden_queries.is_empty() {
            return Ok(());
        }

        let mut total_recall = 0.0f64;
        let mut query_count = 0usize;

        for (query, expected) in &self.golden_queries {
            let results = new_index
                .search(query, self.k)
                .map_err(|e| AliasError::IndexError(e.to_string()))?;

            let returned_ids: std::collections::HashSet<NodeId> =
                results.iter().map(|(id, _)| *id).collect();

            let hits = expected
                .iter()
                .filter(|id| returned_ids.contains(id))
                .count();

            let recall = if expected.is_empty() {
                1.0
            } else {
                hits as f64 / expected.len() as f64
            };

            total_recall += recall;
            query_count += 1;
        }

        let mean_recall = (total_recall / query_count as f64) as f32;

        if mean_recall < self.min_recall {
            return Err(AliasError::ValidationFailed {
                reason: format!(
                    "recall@{} = {mean_recall:.4} < {:.4}",
                    self.k, self.min_recall
                ),
                recall: Some(mean_recall),
                min_recall: Some(self.min_recall),
            });
        }

        Ok(())
    }
}

/// A validator that always passes; useful for testing or when validation is not needed.
pub struct NoopValidator;

impl IndexValidator for NoopValidator {
    fn validate(&self, _new_index: &HnswIndex) -> Result<(), AliasError> {
        Ok(())
    }
}

#[cfg(test)]
#[path = "validation/tests.rs"]
mod tests;
