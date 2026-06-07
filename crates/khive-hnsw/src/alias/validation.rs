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
mod tests {
    use super::*;
    use crate::HnswIndex;

    fn make_test_index() -> HnswIndex {
        let mut index = HnswIndex::new(4);
        // Insert 10 vectors
        for i in 0..10u8 {
            let id = NodeId::new([i; 16]);
            let vec = vec![i as f32; 4];
            index.insert(id, vec).unwrap();
        }
        index
    }

    #[test]
    fn test_noop_validator() {
        let index = make_test_index();
        let validator = NoopValidator;
        assert!(validator.validate(&index).is_ok());
    }

    #[test]
    fn test_recall_validator_empty_golden() {
        let index = make_test_index();
        let validator = RecallValidator::new(vec![], 5, 0.95);
        assert!(validator.validate(&index).is_ok());
    }

    #[test]
    fn test_recall_validator_perfect_recall() {
        let index = make_test_index();

        // Search for the actual results first, then use those as golden truth
        let query = vec![5.0f32; 4];
        let results = index.search(&query, 3).unwrap();
        let expected: Vec<NodeId> = results.iter().map(|(id, _)| *id).collect();
        assert!(!expected.is_empty(), "search should return results");

        // Validator with the actual results as golden truth should pass
        let validator = RecallValidator::new(vec![(query, expected)], 3, 0.95);
        assert!(validator.validate(&index).is_ok());
    }

    #[test]
    fn test_recall_validator_fails_low_recall() {
        let index = make_test_index();

        // Expect IDs that don't exist in the index
        let query = vec![5.0f32; 4];
        let fake_ids: Vec<NodeId> = (100..110u8).map(|i| NodeId::new([i; 16])).collect();

        let validator = RecallValidator::new(vec![(query, fake_ids)], 5, 0.95);
        let result = validator.validate(&index);
        assert!(result.is_err());

        match result.unwrap_err() {
            AliasError::ValidationFailed {
                recall, min_recall, ..
            } => {
                assert_eq!(recall, Some(0.0));
                assert_eq!(min_recall, Some(0.95));
            }
            other => panic!("Expected ValidationFailed, got: {other:?}"),
        }
    }
}
