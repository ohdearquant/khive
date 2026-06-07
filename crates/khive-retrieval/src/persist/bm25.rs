//! BM25-specific persistence methods.

use khive_bm25::Bm25Index;

use super::shadow::{log_validation_result, should_sample};
use super::{
    PersistError, RetrievalPersistence, ShadowMetrics, ShadowValidationConfig,
    ShadowValidationResult,
};

impl RetrievalPersistence {
    /// Persist a BM25 index to SQLite.
    ///
    /// The entire index is serialized (it already has Serde derives).
    pub async fn persist_bm25_index(&self, index: &Bm25Index) -> Result<(), PersistError> {
        self.persist_snapshot("bm25", index).await
    }

    /// Load the latest BM25 index from SQLite.
    ///
    /// Returns `None` if no snapshot exists for this namespace.
    /// Rebuilds the fast-path `doc_lengths_vec` from the deserialized HashMap.
    pub async fn load_bm25_index(&self) -> Result<Option<Bm25Index>, PersistError> {
        let mut index = self.load_snapshot::<Bm25Index>("bm25").await?;
        if let Some(ref mut idx) = index {
            idx.ensure_doc_lengths_vec();
        }
        Ok(index)
    }

    /// Persist a BM25 index with optional shadow validation.
    ///
    /// If shadow validation is enabled, the index is immediately loaded
    /// back and compared to verify integrity. Discrepancies are logged but
    /// do not block the persist operation.
    pub async fn persist_bm25_with_validation(
        &self,
        index: &Bm25Index,
        config: &ShadowValidationConfig,
    ) -> Result<Option<ShadowValidationResult>, PersistError> {
        // Always persist first
        self.persist_bm25_index(index).await?;

        // Skip validation if disabled or not sampled
        if !config.enabled || !should_sample(config.sample_rate) {
            return Ok(None);
        }

        // Capture expected metrics
        let expected = ShadowMetrics {
            item_count: index.doc_count(),
            tombstone_count: 0, // BM25 doesn't have tombstones
            snapshot_size: 0,
        };

        // Perform shadow validation
        let result = self.validate_bm25_snapshot(expected).await;

        // Log result (non-blocking)
        log_validation_result(&result);

        Ok(Some(result))
    }

    /// Validate a BM25 snapshot by loading it back and comparing metrics.
    pub(crate) async fn validate_bm25_snapshot(
        &self,
        expected: ShadowMetrics,
    ) -> ShadowValidationResult {
        let mut result = ShadowValidationResult {
            passed: false,
            index_type: "bm25".to_string(),
            expected: expected.clone(),
            actual: None,
            discrepancies: Vec::new(),
        };

        // Try to load the snapshot back
        match self.load_bm25_index().await {
            Ok(Some(index)) => {
                let actual = ShadowMetrics {
                    item_count: index.doc_count(),
                    tombstone_count: 0,
                    snapshot_size: 0,
                };

                // Compare metrics
                if actual.item_count != expected.item_count {
                    result.discrepancies.push(format!(
                        "doc_count mismatch: expected {}, got {}",
                        expected.item_count, actual.item_count
                    ));
                }

                result.actual = Some(actual);
                result.passed = result.discrepancies.is_empty();
            }
            Ok(None) => {
                result
                    .discrepancies
                    .push("index not found after persist".to_string());
            }
            Err(e) => {
                result
                    .discrepancies
                    .push(format!("failed to load index: {e}"));
            }
        }

        result
    }
}
