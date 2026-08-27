//! Note storage capability — temporal-referential record CRUD.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::types::{
    BatchWriteSummary, DeleteMode, Page, PageRequest, SeekCursor, SeekPage, SqlValue, StorageResult,
};

/// A storage-level note record. Flat, SQL-friendly representation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
    /// Matches rows where a JSON text field equals the value, while treating
    /// every missing or non-text value as that same value. The SQL adapter
    /// emits `CASE WHEN json_type(...) = 'text' THEN json_extract(...) ELSE
    /// value END = value`, mirroring callers whose read model assigns one
    /// textual default to absent, JSON-null, and malformed legacy values.
    TextEqOrNonText,
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
    /// Matches rows where `json_extract(properties, path)` equals any value in
    /// the set. A row with a missing/NULL property does not match — use
    /// `NotInOrMissing` with the complementary set when "absent" should count
    /// as included. `PropertyFilter.value` is unused for this op; the set
    /// lives in the variant itself.
    In(Vec<SqlValue>),
    /// Matches rows where the property is missing/NULL OR its value is not in
    /// the set. Used for "exclude a small closed set of terminal values, but
    /// treat a still-unset property as included" (e.g. GTD default task
    /// listing excludes `done`/`cancelled` while a task with no `status` yet
    /// still counts as `inbox`, i.e. included). `PropertyFilter.value` is
    /// unused for this op; the set lives in the variant itself.
    NotInOrMissing(Vec<SqlValue>),
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
    /// When non-empty, restricts results to any of these namespaces using
    /// `namespace IN (...)`. Takes precedence over the `namespace` string
    /// parameter passed to `query_notes_filtered`. When empty the
    /// caller-supplied `namespace` parameter is used (backward-compatible).
    #[serde(default)]
    pub namespaces: Vec<String>,
    /// Restrict to notes where `created_at >= min_created_at` (microseconds epoch).
    /// `None` applies no lower-bound constraint.
    pub min_created_at: Option<i64>,
}

/// Temporal-referential note CRUD over the notes substrate table.
#[async_trait]
pub trait NoteStore: Send + Sync + 'static {
    /// Insert or update a single note.
    async fn upsert_note(&self, note: Note) -> StorageResult<()>;
    /// Replace a note only when the persisted row still matches the caller's
    /// read snapshot.
    ///
    /// `expected_updated_at` is the snapshot revision and
    /// `expected_deleted_at` closes the soft-delete race (legacy soft-delete
    /// paths may change `deleted_at` without changing `updated_at`). The
    /// replacement note's `updated_at` must be strictly greater than that
    /// persisted revision. Returns `false` when the row disappeared, changed,
    /// or was supplied a non-advancing replacement revision. This is the
    /// full-note compare-and-swap seam used when a pack hook derives coupled
    /// fields from that snapshot before persistence. The default returns
    /// `Unsupported` rather than falling back to an unguarded upsert and
    /// reintroducing the stale-snapshot race.
    async fn replace_note_if_unchanged(
        &self,
        _note: Note,
        _expected_updated_at: i64,
        _expected_deleted_at: Option<i64>,
    ) -> StorageResult<bool> {
        Err(crate::StorageError::Unsupported {
            capability: crate::StorageCapability::Notes,
            operation: "replace_note_if_unchanged".into(),
            message: "this backend does not implement guarded note replacement".into(),
        })
    }
    /// Insert a note only if no row already holds its id, reporting whether
    /// this call is the one that inserted it.
    ///
    /// This closes the other half of the read-modify-write race that
    /// [`NoteStore::replace_note_if_unchanged`] closes. That one protects a
    /// caller who read an existing row; this one protects a caller who read
    /// *no* row. Where the id is derived rather than freshly generated — a
    /// deterministic per-subject id — two callers can both read absence and
    /// both write, and an upsert resolves that by overwriting, so the first
    /// caller's write is lost with no error on either side. Returns `false`
    /// when a row already existed, which the caller maps to a conflict rather
    /// than to success.
    ///
    /// The pre-existing row is left exactly as it is: this must not be
    /// implemented as an upsert, since overwriting is the behaviour being
    /// avoided. The default returns `Unsupported` for that reason, rather
    /// than falling back to `upsert_note` and silently reintroducing the
    /// race under a name that promises otherwise.
    async fn insert_note_if_absent(&self, _note: Note) -> StorageResult<bool> {
        Err(crate::StorageError::Unsupported {
            capability: crate::StorageCapability::Notes,
            operation: "insert_note_if_absent".into(),
            message: "this backend does not implement guarded note insertion".into(),
        })
    }
    /// Insert or update a batch of notes.
    async fn upsert_notes(&self, notes: Vec<Note>) -> StorageResult<BatchWriteSummary>;
    /// Fetch a note by UUID, returning `None` if absent.
    async fn get_note(&self, id: Uuid) -> StorageResult<Option<Note>>;
    /// Fetch a note by UUID regardless of soft-deletion state.
    ///
    /// Returns the note row even when `deleted_at` is set. Callers use this
    /// to distinguish "soft-deleted" from "never existed".
    async fn get_note_including_deleted(&self, id: Uuid) -> StorageResult<Option<Note>>;
    /// Delete a note by UUID using the specified delete mode.
    async fn delete_note(&self, id: Uuid, mode: DeleteMode) -> StorageResult<bool>;
    /// Patch `properties`/`updated_at` on an existing note in place via a real
    /// `UPDATE`, leaving every other column (including the row's `rowid`)
    /// untouched.
    ///
    /// Unlike `upsert_note`, which writes the complete note shape, this leaves
    /// every non-property column untouched. It also never churns the row's
    /// implicit `rowid`, which is required by callers relying on stable row
    /// identity (#780).
    /// Returns `true` when a live (non-soft-deleted) row with this `id` was
    /// found and updated, `false` otherwise.
    async fn update_note_properties(
        &self,
        id: Uuid,
        properties: Option<Value>,
        updated_at: i64,
    ) -> StorageResult<bool>;
    /// Atomically set one top-level key in a note's JSON `properties` object.
    ///
    /// The backend must perform the read/modify/write as one storage operation
    /// so concurrent writes to different keys cannot overwrite each other.
    /// `value` keeps its JSON type, including explicit JSON `null`. A SQL-NULL
    /// property document is initialized as an empty object. A live row whose
    /// stored document is a non-object is not modified and returns `false`, as
    /// do missing and soft-deleted rows. Keys containing U+0000 must be
    /// rejected: SQLite JSON-path labels cannot address them without risking
    /// mutation of a shorter sibling key.
    async fn set_note_property(
        &self,
        id: Uuid,
        key: &str,
        value: Value,
        updated_at: i64,
    ) -> StorageResult<bool>;
    /// Atomically patch a single `properties` JSON key on a note, but only
    /// when the row's *current* state (re-evaluated inside this same
    /// statement, not a snapshot the caller fetched earlier) still satisfies
    /// `filter`'s namespace/kind/property_filters.
    ///
    /// Unlike `set_note_property` (which patches unconditionally once the row
    /// is live) or `update_note_properties` (which replaces the whole
    /// `properties` column with a value the caller already computed — safe
    /// only when nothing else can have written to the row since the caller's
    /// read), this also rechecks `filter` against the row's live state before
    /// writing, so a target that stopped matching an eligibility predicate
    /// between validation and this call is not mutated. Any other property
    /// written concurrently between the caller's read and this call survives
    /// untouched either way. A live row whose stored `properties` document is
    /// a non-object (scalar, array, or otherwise) is not modified and returns
    /// `false`, mirroring `set_note_property`. Returns `Ok(false)` — not an
    /// error — when no live row currently matches `filter` (id not found,
    /// soft-deleted, an eligibility property changed since the caller last
    /// validated it, or the stored document is not a JSON object); the
    /// caller degrades that the same way as `update_note_properties`'s
    /// `Ok(false)`.
    async fn try_patch_note_property(
        &self,
        id: Uuid,
        namespace: &str,
        filter: &NoteFilter,
        json_path: &str,
        value: Value,
        updated_at: i64,
    ) -> StorageResult<bool>;
    /// Atomically patch one JSON property on every supplied note.
    ///
    /// Each target is rechecked against `namespace` and `filter` inside the
    /// same transaction. The operation commits only when every distinct id
    /// matches exactly one live object-valued row; a missing, soft-deleted, or
    /// no-longer-eligible target rolls the entire unit back. Other property
    /// keys are preserved by the same storage-side `json_set` operation as
    /// [`Self::try_patch_note_property`]. Backends without a transactional
    /// multi-note implementation retain the default `Unsupported` result.
    async fn patch_note_property_atomic(
        &self,
        _ids: Vec<Uuid>,
        _namespace: &str,
        _filter: &NoteFilter,
        _json_path: &str,
        _value: Value,
        _updated_at: i64,
    ) -> StorageResult<()> {
        Err(crate::StorageError::Unsupported {
            capability: crate::StorageCapability::Notes,
            operation: "patch_note_property_atomic".into(),
            message: "this backend does not implement atomic multi-note property patches".into(),
        })
    }
    /// Query notes by namespace and optional kind with pagination.
    /// The returned total and page items must come from one consistent
    /// backend snapshot.
    async fn query_notes(
        &self,
        namespace: &str,
        kind: Option<&str>,
        page: PageRequest,
    ) -> StorageResult<Page<Note>>;
    /// Query notes with property-based filtering and custom sort.
    /// The returned total and page items must come from one consistent
    /// backend snapshot.
    async fn query_notes_filtered(
        &self,
        namespace: &str,
        filter: &NoteFilter,
        page: PageRequest,
    ) -> StorageResult<Page<Note>>;
    /// Resolve a note id to its immutable insertion sequence.
    async fn note_sequence(&self, _id: Uuid) -> StorageResult<Option<i64>> {
        Err(crate::StorageError::Unsupported {
            capability: crate::StorageCapability::Notes,
            operation: "note_sequence".into(),
            message: "this backend does not implement note insertion sequences".into(),
        })
    }
    /// Query an immutable insertion-sequence keyset page with the same
    /// predicates as [`Self::query_notes_filtered`].
    async fn query_notes_filtered_after(
        &self,
        _namespace: &str,
        _filter: &NoteFilter,
        _after: Option<SeekCursor>,
        _limit: u32,
    ) -> StorageResult<SeekPage<Note>> {
        Err(crate::StorageError::Unsupported {
            capability: crate::StorageCapability::Notes,
            operation: "query_notes_filtered_after".into(),
            message: "this backend does not implement note seek pagination".into(),
        })
    }
    /// Fetch up to `max_rows + 1` notes matching `filter` in a single
    /// deterministically-ordered SQL statement, with no separate `COUNT(*)`
    /// and no pagination loop.
    ///
    /// A single statement observes one consistent snapshot for its entire
    /// execution, so the result cannot be split across a concurrent insert
    /// the way a `COUNT(*)` followed by independent `LIMIT`/`OFFSET` pages
    /// can. Callers detect the over-bound case by checking whether the
    /// returned `Vec` has more than `max_rows` items — that means at least
    /// `max_rows + 1` rows matched and the caller must reject the query
    /// rather than silently return a truncated, possibly priority-incomplete
    /// set.
    async fn query_notes_filtered_bounded(
        &self,
        namespace: &str,
        filter: &NoteFilter,
        max_rows: u32,
    ) -> StorageResult<Vec<Note>>;
    /// Count notes in a namespace, optionally filtered by kind.
    async fn count_notes(&self, namespace: &str, kind: Option<&str>) -> StorageResult<u64>;
    /// Count notes across the given namespaces, optionally filtered by kind.
    /// The default preserves compatibility by summing the existing
    /// single-namespace operation; SQL backends should override this with one
    /// `IN` aggregate.
    async fn count_notes_in_namespaces(
        &self,
        namespaces: &[String],
        kind: Option<&str>,
    ) -> StorageResult<u64> {
        let mut total = 0;
        for namespace in namespaces {
            total += self.count_notes(namespace, kind).await?;
        }
        Ok(total)
    }

    /// Attempt to insert a note without overwriting an existing row.
    ///
    /// Returns `true` when the row was newly written.  Returns `false` only
    /// when a live note with the same non-empty `external_id` already exists in
    /// the same namespace and kind (confirmed dedup hit).  Any other constraint
    /// violation (e.g. a primary key collision) is surfaced as a `StorageError`
    /// so that callers do not misinterpret unexpected failures as deduplication.
    async fn try_insert_note(&self, note: Note) -> StorageResult<bool>;

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
