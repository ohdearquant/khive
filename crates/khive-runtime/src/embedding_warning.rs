//! Response advisory for embedding inputs the service had to truncate.

use crate::retrieval::{EmbeddingTruncationReport, EMBEDDING_INPUT_TRUNCATED_WARNING};

/// Add the embedding-input advisory when the report records truncation.
///
/// An object response receives the existing one-element warnings array, replacing
/// any previous warnings. Other response values and untruncated reports are unchanged.
pub fn add_embedding_truncation_warning(
    response: &mut serde_json::Value,
    report: &EmbeddingTruncationReport,
) {
    if !report.any_truncated() {
        return;
    }
    if let Some(object) = response.as_object_mut() {
        object.insert(
            "warnings".to_string(),
            serde_json::json!([EMBEDDING_INPUT_TRUNCATED_WARNING]),
        );
    }
}
