//! Shadow validation types and helpers for persistence integrity checking.
//!
//! Shadow validation (Issue #628) verifies persisted snapshots can be correctly
//! restored without blocking production operations. Discrepancies are logged only.

use rand::Rng;

// ---------------------------------------------------------------------------
// Shadow Validation (Issue #628)
// ---------------------------------------------------------------------------

/// Configuration for shadow validation.
///
/// Shadow validation verifies persisted snapshots can be correctly restored
/// without blocking production operations. Discrepancies are logged only.
#[derive(Debug, Clone)]
pub struct ShadowValidationConfig {
    /// Whether shadow validation is enabled.
    pub enabled: bool,
    /// Sample rate for validation (0.0 to 1.0).
    /// Set to 1.0 to validate every persist operation.
    pub sample_rate: f64,
}

impl Default for ShadowValidationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sample_rate: 0.1, // 10% sample rate by default
        }
    }
}

impl ShadowValidationConfig {
    /// Enable shadow validation with full coverage.
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            sample_rate: 1.0,
        }
    }

    /// Enable shadow validation with a specific sample rate.
    pub fn with_sample_rate(rate: f64) -> Self {
        Self {
            enabled: true,
            sample_rate: rate.clamp(0.0, 1.0),
        }
    }
}

/// Result of shadow validation.
#[derive(Debug, Clone)]
pub struct ShadowValidationResult {
    /// Whether validation passed.
    pub passed: bool,
    /// Index type that was validated.
    pub index_type: String,
    /// Expected metrics from the original index.
    pub expected: ShadowMetrics,
    /// Actual metrics from the restored snapshot.
    pub actual: Option<ShadowMetrics>,
    /// Discrepancies found (empty if validation passed).
    pub discrepancies: Vec<String>,
}

/// Metrics captured for shadow validation comparison.
#[derive(Debug, Clone, Default)]
pub struct ShadowMetrics {
    /// Total number of items in the index.
    pub item_count: usize,
    /// Number of tombstoned/deleted items (HNSW only).
    pub tombstone_count: usize,
    /// Snapshot size in bytes.
    pub snapshot_size: usize,
}

/// Determine whether to sample this operation for validation.
pub(crate) fn should_sample(rate: f64) -> bool {
    if rate >= 1.0 {
        return true;
    }
    if rate <= 0.0 {
        return false;
    }
    rand::thread_rng().gen::<f64>() < rate
}

/// Log the validation result (logging-only, non-blocking).
///
/// This function logs discrepancies but never blocks or returns errors.
/// In production, this should integrate with the application's logging
/// infrastructure (e.g., tracing crate).
pub(crate) fn log_validation_result(result: &ShadowValidationResult) {
    // Only log failures - successful validations are silent by default
    // to avoid log noise. The result is still returned to callers who
    // may want to record metrics or take other actions.
    if !result.passed {
        tracing::warn!(
            index_type = %result.index_type,
            discrepancies = ?result.discrepancies,
            "Shadow validation failed"
        );
    }
}
