//! HNSW-specific persistence methods.

use khive_hnsw::HnswIndex;
use khive_hnsw::HnswSnapshot;

use super::shadow::{log_validation_result, should_sample};
use super::{
    PersistError, RetrievalPersistence, ShadowMetrics, ShadowValidationConfig,
    ShadowValidationResult,
};

impl RetrievalPersistence {
    /// Persist an HNSW index snapshot to SQLite.
    ///
    /// Creates a snapshot of the index and stores it as a serialized BLOB.
    pub async fn persist_hnsw_snapshot(&self, index: &HnswIndex) -> Result<(), PersistError> {
        let snapshot = index.snapshot();
        self.persist_snapshot("hnsw", &snapshot).await
    }

    /// Load the latest HNSW snapshot from SQLite.
    ///
    /// Returns `None` if no snapshot exists for this namespace.
    pub async fn load_hnsw_snapshot(&self) -> Result<Option<HnswSnapshot>, PersistError> {
        self.load_snapshot::<HnswSnapshot>("hnsw").await
    }

    /// Persist an HNSW snapshot with optional shadow validation.
    ///
    /// If shadow validation is enabled, the snapshot is immediately loaded
    /// back and compared to verify integrity. Discrepancies are logged but
    /// do not block the persist operation.
    pub async fn persist_hnsw_with_validation(
        &self,
        index: &HnswIndex,
        config: &ShadowValidationConfig,
    ) -> Result<Option<ShadowValidationResult>, PersistError> {
        // Always persist first
        self.persist_hnsw_snapshot(index).await?;

        // Skip validation if disabled or not sampled
        if !config.enabled || !should_sample(config.sample_rate) {
            return Ok(None);
        }

        // Capture expected metrics
        let expected = ShadowMetrics {
            item_count: index.len(),
            tombstone_count: index.tombstone_stats().tombstone_count,
            snapshot_size: 0, // Will be filled by stats
        };

        // Perform shadow validation
        let result = self.validate_hnsw_snapshot(expected).await;

        // Log result (non-blocking)
        log_validation_result(&result);

        Ok(Some(result))
    }

    /// Validate an HNSW snapshot by loading it back and comparing metrics.
    pub(crate) async fn validate_hnsw_snapshot(
        &self,
        expected: ShadowMetrics,
    ) -> ShadowValidationResult {
        let mut result = ShadowValidationResult {
            passed: false,
            index_type: "hnsw".to_string(),
            expected: expected.clone(),
            actual: None,
            discrepancies: Vec::new(),
        };

        // Try to load the snapshot back
        match self.load_hnsw_snapshot().await {
            Ok(Some(snapshot)) => {
                // Issue #867: Deep verification using HnswSnapshot::verify()
                // This checks internal consistency beyond just count comparison:
                // - Count consistency: total_nodes == live_nodes + tombstone_count
                // - ID count integrity: indexed_ids.len() == total_nodes
                // - Tombstone containment: all tombstoned IDs exist in indexed_ids
                if let Err(e) = snapshot.verify() {
                    result
                        .discrepancies
                        .push(format!("Snapshot verification failed: {e}"));
                }

                let actual = ShadowMetrics {
                    item_count: snapshot.total_nodes,
                    tombstone_count: snapshot.tombstone_count,
                    snapshot_size: 0, // Not easily available without re-serializing
                };

                // Compare metrics
                if actual.item_count != expected.item_count {
                    result.discrepancies.push(format!(
                        "item_count mismatch: expected {}, got {}",
                        expected.item_count, actual.item_count
                    ));
                }

                if actual.tombstone_count != expected.tombstone_count {
                    result.discrepancies.push(format!(
                        "tombstone_count mismatch: expected {}, got {}",
                        expected.tombstone_count, actual.tombstone_count
                    ));
                }

                result.actual = Some(actual);
                result.passed = result.discrepancies.is_empty();
            }
            Ok(None) => {
                result
                    .discrepancies
                    .push("snapshot not found after persist".to_string());
            }
            Err(e) => {
                result
                    .discrepancies
                    .push(format!("failed to load snapshot: {e}"));
            }
        }

        result
    }
}
