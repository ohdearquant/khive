//! Note storage capability — temporal-referential record CRUD.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::types::{BatchWriteSummary, DeleteMode, Page, PageRequest, SqlValue, StorageResult};

/// A storage-level note record. Flat, SQL-friendly representation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Note {
    pub id: Uuid,
    pub namespace: String,
    pub kind: String,
    pub status: String,
    pub name: Option<String>,
    pub content: String,
    pub salience: Option<f64>,
    pub decay_factor: Option<f64>,
    pub expires_at: Option<i64>,
    pub properties: Option<Value>,
    pub created_at: i64,
    pub updated_at: i64,
    pub deleted_at: Option<i64>,
}

impl Note {
    pub fn new(
        namespace: impl Into<String>,
        kind: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        let now = chrono::Utc::now().timestamp_micros();
        Self {
            id: Uuid::new_v4(),
            namespace: namespace.into(),
            kind: kind.into(),
            status: "active".to_string(),
            name: None,
            content: content.into(),
            salience: None,
            decay_factor: None,
            expires_at: None,
            properties: None,
            created_at: now,
            updated_at: now,
            deleted_at: None,
        }
    }

    pub fn with_name(mut self, n: impl Into<String>) -> Self {
        self.name = Some(n.into());
        self
    }

    pub fn with_salience(mut self, s: f64) -> Self {
        debug_assert!(s.is_finite(), "salience must be finite, got {s}");
        self.salience = Some(s.clamp(0.0, 1.0));
        self
    }

    pub fn with_decay(mut self, d: f64) -> Self {
        debug_assert!(d.is_finite(), "decay_factor must be finite, got {d}");
        self.decay_factor = Some(d.max(0.0));
        self
    }

    pub fn with_properties(mut self, p: Value) -> Self {
        self.properties = Some(p);
        self
    }
}

/// Sort direction for filtered note queries.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortDir {
    Asc,
    Desc,
}

/// Comparison operator for a [`PropertyFilter`] on a JSON path.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterOp {
    Eq,
    /// Matches rows where the JSON field equals the value OR the field is absent/NULL.
    /// Used for properties that may be missing in legacy rows (e.g. `$.read`).
    EqOrMissing,
    Ne,
    Lt,
    Lte,
    Gt,
    Gte,
    /// Matches rows where `json_type(properties, path) = value`.
    /// Value must be a SQLite json_type string literal: 'true', 'false', 'integer',
    /// 'real', 'text', 'array', 'object', or 'null'.
    JsonTypeEq,
    /// Matches rows where the json_type is absent (NULL) OR differs from value.
    /// Equivalent to `json_type IS NULL OR json_type != value`.
    /// Used for unread filter: matches any `$.read` that is NOT the JSON boolean true.
    JsonTypeNeMissing,
}

/// A single `json_extract(properties, '$.field') op value` predicate.
///
/// Callers import this as `khive_storage::note::PropertyFilter` to avoid
/// collision with the vector-metadata `PropertyFilter` in `khive_storage::types`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PropertyFilter {
    pub json_path: String,
    pub op: FilterOp,
    pub value: SqlValue,
}

/// Filter + sort options for [`NoteStore::query_notes_filtered`].
///
/// Designed for general property-based filtering on any JSON field, not
/// schedule-specific, so D9 and future packs can reuse the same API.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct NoteFilter {
    pub kind: Option<String>,
    #[serde(default)]
    pub property_filters: Vec<PropertyFilter>,
    /// `(json_path, direction)` — `None` defaults to `created_at DESC`.
    pub order_by: Option<(String, SortDir)>,
}

#[async_trait]
pub trait NoteStore: Send + Sync + 'static {
    async fn upsert_note(&self, note: Note) -> StorageResult<()>;
    async fn upsert_notes(&self, notes: Vec<Note>) -> StorageResult<BatchWriteSummary>;
    async fn get_note(&self, id: Uuid) -> StorageResult<Option<Note>>;
    async fn delete_note(&self, id: Uuid, mode: DeleteMode) -> StorageResult<bool>;
    async fn query_notes(
        &self,
        namespace: &str,
        kind: Option<&str>,
        page: PageRequest,
    ) -> StorageResult<Page<Note>>;
    async fn query_notes_filtered(
        &self,
        namespace: &str,
        filter: &NoteFilter,
        page: PageRequest,
    ) -> StorageResult<Page<Note>>;
    async fn count_notes(&self, namespace: &str, kind: Option<&str>) -> StorageResult<u64>;

    async fn get_notes_batch(&self, ids: &[Uuid]) -> StorageResult<Vec<Note>> {
        let mut out = Vec::with_capacity(ids.len());
        for &id in ids {
            if let Some(n) = self.get_note(id).await? {
                out.push(n);
            }
        }
        Ok(out)
    }
}
