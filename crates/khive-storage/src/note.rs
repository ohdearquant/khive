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
    /// Create a new note with a generated UUID and current timestamp.
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

    /// Set the note display name.
    pub fn with_name(mut self, n: impl Into<String>) -> Self {
        self.name = Some(n.into());
        self
    }

    /// Set salience (infallible). Rejects non-finite values by returning `self`
    /// unchanged; clamps finite values to `[0.0, 1.0]`. Prefer
    /// [`try_with_salience`](Self::try_with_salience) at public boundaries.
    pub fn with_salience(mut self, s: f64) -> Self {
        if !s.is_finite() {
            return self;
        }
        self.salience = Some(s.clamp(0.0, 1.0));
        self
    }

    /// Set decay factor (infallible). Rejects non-finite values by returning
    /// `self` unchanged; floors finite values at `0.0`. Prefer
    /// [`try_with_decay`](Self::try_with_decay) at public boundaries.
    pub fn with_decay(mut self, d: f64) -> Self {
        if !d.is_finite() {
            return self;
        }
        self.decay_factor = Some(d.max(0.0));
        self
    }

    /// Set salience with validation. Returns an error for non-finite or
    /// out-of-range `[0.0, 1.0]` values.
    pub fn try_with_salience(mut self, s: f64) -> Result<Self, String> {
        if !s.is_finite() {
            return Err(format!("salience must be finite, got {s}"));
        }
        if !(0.0..=1.0).contains(&s) {
            return Err(format!("salience must be in [0.0, 1.0], got {s}"));
        }
        self.salience = Some(s);
        Ok(self)
    }

    /// Set decay factor with validation. Returns an error for non-finite or
    /// negative values.
    pub fn try_with_decay(mut self, d: f64) -> Result<Self, String> {
        if !d.is_finite() {
            return Err(format!("decay_factor must be finite, got {d}"));
        }
        if d < 0.0 {
            return Err(format!("decay_factor must be >= 0.0, got {d}"));
        }
        self.decay_factor = Some(d);
        Ok(self)
    }

    /// Set the note properties JSON blob.
    pub fn with_properties(mut self, p: Value) -> Self {
        self.properties = Some(p);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_note() -> Note {
        Note::new("ns:test", "memory", "hello world")
    }

    // -- with_salience --

    #[test]
    fn with_salience_clamps_to_range() {
        let n = base_note().with_salience(1.5);
        assert_eq!(n.salience, Some(1.0));
        let n = base_note().with_salience(-0.1);
        assert_eq!(n.salience, Some(0.0));
        let n = base_note().with_salience(0.7);
        assert_eq!(n.salience, Some(0.7));
    }

    #[test]
    fn with_salience_ignores_nan() {
        let n = base_note().with_salience(f64::NAN);
        assert_eq!(n.salience, None, "NaN must not set salience");
    }

    #[test]
    fn with_salience_ignores_inf() {
        let n = base_note().with_salience(f64::INFINITY);
        assert_eq!(n.salience, None, "+Inf must not set salience");
        let n = base_note().with_salience(f64::NEG_INFINITY);
        assert_eq!(n.salience, None, "-Inf must not set salience");
    }

    // -- with_decay --

    #[test]
    fn with_decay_floors_at_zero() {
        let n = base_note().with_decay(-1.0);
        assert_eq!(n.decay_factor, Some(0.0));
        let n = base_note().with_decay(0.5);
        assert_eq!(n.decay_factor, Some(0.5));
    }

    #[test]
    fn with_decay_ignores_nan() {
        let n = base_note().with_decay(f64::NAN);
        assert_eq!(n.decay_factor, None, "NaN must not set decay_factor");
    }

    #[test]
    fn with_decay_ignores_inf() {
        let n = base_note().with_decay(f64::INFINITY);
        assert_eq!(n.decay_factor, None, "+Inf must not set decay_factor");
    }

    // -- try_with_salience --

    #[test]
    fn try_with_salience_accepts_valid_range() {
        let n = base_note().try_with_salience(0.0).unwrap();
        assert_eq!(n.salience, Some(0.0));
        let n = base_note().try_with_salience(1.0).unwrap();
        assert_eq!(n.salience, Some(1.0));
        let n = base_note().try_with_salience(0.85).unwrap();
        assert_eq!(n.salience, Some(0.85));
    }

    #[test]
    fn try_with_salience_rejects_nan() {
        let err = base_note().try_with_salience(f64::NAN).unwrap_err();
        assert!(err.contains("finite"), "error must mention finite: {err}");
    }

    #[test]
    fn try_with_salience_rejects_out_of_range() {
        let err = base_note().try_with_salience(1.1).unwrap_err();
        assert!(err.contains("1.0"), "error must mention bound: {err}");
        let err = base_note().try_with_salience(-0.01).unwrap_err();
        assert!(err.contains("0.0"), "error must mention bound: {err}");
    }

    // -- try_with_decay --

    #[test]
    fn try_with_decay_accepts_valid_values() {
        let n = base_note().try_with_decay(0.0).unwrap();
        assert_eq!(n.decay_factor, Some(0.0));
        let n = base_note().try_with_decay(2.5).unwrap();
        assert_eq!(n.decay_factor, Some(2.5));
    }

    #[test]
    fn try_with_decay_rejects_nan() {
        let err = base_note().try_with_decay(f64::NAN).unwrap_err();
        assert!(err.contains("finite"), "error must mention finite: {err}");
    }

    #[test]
    fn try_with_decay_rejects_negative() {
        let err = base_note().try_with_decay(-0.1).unwrap_err();
        assert!(err.contains("0.0"), "error must mention bound: {err}");
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

/// Temporal-referential note CRUD over the notes substrate table.
#[async_trait]
pub trait NoteStore: Send + Sync + 'static {
    /// Insert or update a single note.
    async fn upsert_note(&self, note: Note) -> StorageResult<()>;
    /// Insert or update a batch of notes.
    async fn upsert_notes(&self, notes: Vec<Note>) -> StorageResult<BatchWriteSummary>;
    /// Fetch a note by UUID, returning `None` if absent.
    async fn get_note(&self, id: Uuid) -> StorageResult<Option<Note>>;
    /// Delete a note by UUID using the specified delete mode.
    async fn delete_note(&self, id: Uuid, mode: DeleteMode) -> StorageResult<bool>;
    /// Query notes by namespace and optional kind with pagination.
    async fn query_notes(
        &self,
        namespace: &str,
        kind: Option<&str>,
        page: PageRequest,
    ) -> StorageResult<Page<Note>>;
    /// Query notes with property-based filtering and custom sort.
    async fn query_notes_filtered(
        &self,
        namespace: &str,
        filter: &NoteFilter,
        page: PageRequest,
    ) -> StorageResult<Page<Note>>;
    /// Count notes in a namespace, optionally filtered by kind.
    async fn count_notes(&self, namespace: &str, kind: Option<&str>) -> StorageResult<u64>;

    /// Fetch multiple notes by UUID in a single call.
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
