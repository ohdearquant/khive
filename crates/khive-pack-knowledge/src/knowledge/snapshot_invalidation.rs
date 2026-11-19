//! Legacy Vamana snapshot invalidation shared by the knowledge pack and kkernel reindex.

use khive_storage::types::{SqlStatement, SqlValue};

/// Execute legacy Vamana snapshot invalidation through an already-open writer.
/// Returns the affected-row count or the original storage error. The caller owns
/// writer acquisition, statement labeling, missing-table tolerance and logging.
pub async fn invalidate_legacy_vamana_snapshots(
    writer: &mut dyn khive_storage::SqlWriter,
    namespace: &str,
    label: &'static str,
) -> khive_storage::StorageResult<u64> {
    let pattern = format!("{}::vamana::%", khive_types::escape_like_literal(namespace));
    writer
        .execute(SqlStatement {
            sql: khive_runtime::sql!("knowledge_legacy_snapshots_invalidate").into(),
            params: vec![SqlValue::Text(pattern)],
            label: Some(label.into()),
        })
        .await
}
