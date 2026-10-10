//! Runtime-owned embedding registry records (ADR-071 §5).

use khive_storage::{SqlRow, SqlValue};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// One persisted embedding model identity and its lifecycle history.
///
/// Registry reads preserve the stored UUID, canonical key and timestamp values;
/// they do not reconstruct identity or fill in missing history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingModelRecord {
    pub id: Uuid,
    pub engine_name: String,
    pub model_id: String,
    pub key_version: String,
    pub dim: u32,
    pub output_dim: Option<u32>,
    pub status: EmbeddingModelStatus,
    pub activated_at: Option<i64>,
    pub superseded_at: Option<i64>,
    pub superseded_by: Option<Uuid>,
    pub canonical_key: Vec<u8>,
    pub created_at: i64,
}

/// The lifecycle states admitted by the embedding registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingModelStatus {
    Pending,
    Active,
    Superseded,
    Archived,
}

impl EmbeddingModelStatus {
    /// The stable registry and serialized spelling, also used by CLI projections.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Superseded => "superseded",
            Self::Archived => "archived",
        }
    }
}

impl EmbeddingModelRecord {
    /// Keep the established registry read policy: warn and skip malformed required
    /// values. Newly projected non-NULL optional values must also be valid; they
    /// never acquire a fabricated default. Legacy optional timestamps retain their
    /// existing compatibility behavior: a non-integer value decodes as None.
    pub(crate) fn from_sql_row(row: &SqlRow) -> Option<Self> {
        let required_text = |column: &str| match row.get(column) {
            Some(SqlValue::Text(value)) => Some(value.clone()),
            other => {
                tracing::warn!(target: "khive_runtime::runtime", column, value = ?other, "skipping registry row: unexpected type");
                None
            }
        };
        let required_blob = |column: &str| match row.get(column) {
            Some(SqlValue::Blob(value)) => Some(value.as_slice()),
            _ => {
                tracing::warn!(target: "khive_runtime::runtime", column, "skipping registry row: expected blob");
                None
            }
        };
        let uuid = |column: &str, bytes: &[u8]| match Uuid::from_slice(bytes) {
            Ok(value) => Some(value),
            Err(_) => {
                tracing::warn!(target: "khive_runtime::runtime", column, "skipping registry row: invalid UUID bytes");
                None
            }
        };
        let dimension = |column: &str, value: i64| match u32::try_from(value) {
            Ok(value) => Some(value),
            Err(_) => {
                tracing::warn!(target: "khive_runtime::runtime", column, dim = value, "skipping registry row: dim out of u32 range");
                None
            }
        };

        let engine_name = required_text("engine_name")?;
        let model_id = required_text("model_id")?;
        let key_version = required_text("key_version")?;
        let dim = match row.get("dim") {
            Some(SqlValue::Integer(value)) => dimension("dim", *value)?,
            other => {
                tracing::warn!(target: "khive_runtime::runtime", column = "dim", value = ?other, "skipping registry row: unexpected type");
                return None;
            }
        };
        let status = match required_text("status")?.as_str() {
            "pending" => EmbeddingModelStatus::Pending,
            "active" => EmbeddingModelStatus::Active,
            "superseded" => EmbeddingModelStatus::Superseded,
            "archived" => EmbeddingModelStatus::Archived,
            _ => {
                tracing::warn!(target: "khive_runtime::runtime", "skipping registry row: unknown lifecycle status");
                return None;
            }
        };
        let id = uuid("id", required_blob("id")?)?;
        let canonical_key = required_blob("canonical_key")?.to_vec();
        let created_at = match row.get("created_at") {
            Some(SqlValue::Integer(value)) => *value,
            _ => {
                tracing::warn!(target: "khive_runtime::runtime", column = "created_at", "skipping registry row: expected integer");
                return None;
            }
        };
        let output_dim = match row.get("output_dim") {
            Some(SqlValue::Null) => None,
            Some(SqlValue::Integer(value)) => Some(dimension("output_dim", *value)?),
            _ => {
                tracing::warn!(target: "khive_runtime::runtime", column = "output_dim", "skipping registry row: expected nullable dimension");
                return None;
            }
        };
        let superseded_by = match row.get("superseded_by") {
            Some(SqlValue::Null) => None,
            Some(SqlValue::Blob(value)) => Some(uuid("superseded_by", value)?),
            _ => {
                tracing::warn!(target: "khive_runtime::runtime", column = "superseded_by", "skipping registry row: expected nullable UUID bytes");
                return None;
            }
        };
        let optional_timestamp = |column: &str| match row.get(column) {
            Some(SqlValue::Integer(value)) => Some(*value),
            _ => None,
        };

        Some(Self {
            id,
            engine_name,
            model_id,
            key_version,
            dim,
            output_dim,
            status,
            activated_at: optional_timestamp("activated_at"),
            superseded_at: optional_timestamp("superseded_at"),
            superseded_by,
            canonical_key,
            created_at,
        })
    }
}
