// Licensed under the Apache License, Version 2.0.

// FILE SIZE JUSTIFICATION: curation.rs holds entity/note/edge patch types alongside
// their update and merge implementations. The implementations share private helpers
// (merge_properties, namespace checks, dedup policy) that need pub(crate) access to
// runtime internals. Inline tests cover merge semantics that require direct access to
// those helpers. Split plan: extract patch types into `curation/patch.rs` and merge
// logic into `curation/merge.rs` once the dedup policy API stabilises.
//! Curation operations: entity update/merge and edge-list filter type.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use khive_db::SqliteError;
use khive_storage::note::Note;
use khive_storage::types::{EdgeFilter, TextDocument};
use khive_storage::{EdgeRelation, Entity, SubstrateKind};
use khive_types::EventKind;
use rusqlite::OptionalExtension;

use crate::error::{RuntimeError, RuntimeResult};
use crate::operations::canonical_edge_endpoints;
use crate::runtime::{KhiveRuntime, NamespaceToken};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Patch for `update_entity`. Only `Some(_)` fields are applied; `None` means "leave unchanged".
///
/// For `description`:
/// - `None` (outer) — leave the current description as-is
/// - `Some(None)` — clear the description (set to NULL)
/// - `Some(Some(s))` — set the description to `s`
///
/// For `properties` (deep-merge semantics):
/// - `None` — leave properties as-is
/// - `Some(value)` — deep-merge `value` into existing properties. Keys present in
///   the patch overwrite existing keys; keys absent from the patch are preserved.
///   Removing a key requires explicit replacement of the parent object (or a future
///   `unset`/`null-marker` extension).
///
/// For `tags` — replace semantics: `Some(vec)` sets tags to exactly `vec`. To add
/// a tag without losing existing tags, read the entity first, push the new tag,
/// and pass the full list back.
#[derive(Clone, Debug, Default)]
pub struct EntityPatch {
    pub name: Option<String>,
    pub description: Option<Option<String>>,
    pub properties: Option<Value>,
    pub tags: Option<Vec<String>>,
}

/// Policy used when deduplicating two entities.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EntityDedupMergePolicy {
    /// `into` values win on conflict. Tags are unioned. Properties from `from` fill in
    /// keys that `into` doesn't have. This is the default.
    #[default]
    PreferInto,
    /// `from` values win on conflict.
    PreferFrom,
    /// Deep-merge: object properties merge recursively. Scalar conflicts go to `into`.
    Union,
}

/// Strategy for merging note content when two notes are combined.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContentMergeStrategy {
    #[default]
    Append,
    PreferInto,
    PreferFrom,
}

/// Result returned by `merge_entity` / `merge_note`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MergeSummary {
    pub kept_id: Uuid,
    pub removed_id: Uuid,
    pub edges_rewired: usize,
    pub properties_merged: usize,
    pub tags_unioned: usize,
    pub content_appended: bool,
    pub dry_run: bool,
}

/// Patch for `update_edge`. Only `Some(_)` fields are applied; `None` means "leave unchanged".
///
/// For `properties` — replacement semantics (not deep merge): `Some(value)` replaces
/// the entire metadata object. `None` leaves metadata unchanged.
#[derive(Clone, Debug, Default)]
pub struct EdgePatch {
    pub relation: Option<EdgeRelation>,
    pub weight: Option<f64>,
    pub properties: Option<Value>,
}

/// Patch for `update_note`. Only `Some(_)` fields are applied; `None` means "leave unchanged".
///
/// For `salience`/`decay_factor`:
/// - `None` (outer) — leave unchanged
/// - `Some(None)` — clear the value
/// - `Some(Some(v))` — set to v
#[derive(Clone, Debug, Default)]
pub struct NotePatch {
    pub name: Option<Option<String>>,
    pub content: Option<String>,
    pub salience: Option<Option<f64>>,
    pub decay_factor: Option<Option<f64>>,
    pub properties: Option<Value>,
    pub(crate) kind_status: Option<String>,
}

impl NotePatch {
    /// Construct a `NotePatch` from the public fields only.
    /// Use this from external crates; `kind_status` is set to `None`.
    pub fn new(
        name: Option<Option<String>>,
        content: Option<String>,
        salience: Option<Option<f64>>,
        decay_factor: Option<Option<f64>>,
        properties: Option<Value>,
    ) -> Self {
        Self {
            name,
            content,
            salience,
            decay_factor,
            properties,
            kind_status: None,
        }
    }
}

/// Filter for `list_edges` / `count_edges`.
#[derive(Clone, Debug, Default)]
pub struct EdgeListFilter {
    pub source_id: Option<Uuid>,
    pub target_id: Option<Uuid>,
    /// Empty = any relation.
    pub relations: Vec<EdgeRelation>,
    pub min_weight: Option<f64>,
    pub max_weight: Option<f64>,
}

impl From<EdgeListFilter> for EdgeFilter {
    fn from(f: EdgeListFilter) -> Self {
        EdgeFilter {
            source_ids: f.source_id.into_iter().collect(),
            target_ids: f.target_id.into_iter().collect(),
            relations: f.relations,
            min_weight: f.min_weight,
            max_weight: f.max_weight,
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Private types
// ---------------------------------------------------------------------------

// REASON: EdgeRow fields are populated via rusqlite row mapping. The struct is fully
// constructed even when not all fields are read back after construction. The complete
// field mapping guards against column-order bugs when the schema changes.
#[allow(dead_code)]
struct EdgeRow {
    id: Uuid,
    source_id: Uuid,
    target_id: Uuid,
    relation: String,
    weight: f64,
    created_at: i64,
    updated_at: i64,
    deleted_at: Option<i64>,
    target_backend: Option<String>,
    metadata: Option<String>,
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

impl KhiveRuntime {
    /// Patch-style entity update.
    ///
    /// Only fields set to `Some(_)` are changed. Re-indexes FTS5 (and vectors if configured)
    /// when `name` or `description` changes; skips re-indexing for property/tag-only patches.
    ///
    /// Returns `RuntimeError::NotFound` if the entity does not exist or belongs to a different
    /// namespace. Namespace isolation is enforced at the runtime layer.
    pub async fn update_entity(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        patch: EntityPatch,
    ) -> RuntimeResult<Entity> {
        // Secret gate: scan incoming text fields, properties, and tags.
        if let Some(ref name) = patch.name {
            crate::secret_gate::check(name)?;
        }
        if let Some(Some(ref desc)) = patch.description {
            crate::secret_gate::check(desc)?;
        }
        if let Some(ref props) = patch.properties {
            crate::secret_gate::check_json(props)?;
        }
        if let Some(ref tags) = patch.tags {
            crate::secret_gate::check_tags(tags)?;
        }
        let store = self.entities(token)?;
        let mut entity = store
            .get_entity(id)
            .await?
            .ok_or_else(|| RuntimeError::NotFound(format!("entity {id}")))?;

        let mut text_changed = false;
        let mut changed_fields: Vec<&'static str> = Vec::new();

        if let Some(name) = patch.name {
            text_changed |= entity.name != name;
            entity.name = name;
            changed_fields.push("name");
        }
        if let Some(desc_patch) = patch.description {
            text_changed |= entity.description != desc_patch;
            entity.description = desc_patch;
            changed_fields.push("description");
        }
        if let Some(props) = patch.properties {
            let (merged, _) = merge_properties(
                &entity.properties,
                &Some(props),
                EntityDedupMergePolicy::PreferFrom,
            );
            entity.properties = merged;
            changed_fields.push("properties");
        }
        if let Some(tags) = patch.tags {
            entity.tags = tags;
            changed_fields.push("tags");
        }

        entity.updated_at = chrono::Utc::now().timestamp_micros();
        store.upsert_entity(entity.clone()).await?;

        if text_changed {
            self.reindex_entity(token, &entity).await?;
        }

        let event_store = self.events(token)?;
        let event = khive_storage::event::Event::new(
            entity.namespace.clone(),
            "update",
            EventKind::EntityUpdated,
            SubstrateKind::Entity,
            "",
        )
        .with_target(entity.id)
        .with_payload(serde_json::json!({
            "id": entity.id,
            "namespace": entity.namespace,
            "changed_fields": changed_fields,
        }));
        event_store.append_event(event).await.map_err(|e| {
            RuntimeError::Internal(format!("update_entity: event store write failed: {e}"))
        })?;

        Ok(entity)
    }

    /// Merge `from_id` into `into_id`.
    ///
    /// All edges incident to `from_id` are rewired to `into_id`. Self-loops that would
    /// result from the rewire are dropped. Properties and tags are merged per `strategy`.
    /// `from_id` is tombstoned with merge provenance and removed from indexes. Returns a summary.
    ///
    /// If `dry_run` is true, computes and returns the planned summary without mutating any rows.
    ///
    /// Atomic: all SQL (entity reads/writes, edge rewires, FTS updates, vec-index delete)
    /// runs on a single pool connection inside one `BEGIN IMMEDIATE` transaction via
    /// `merge_entity_sql`. If embedding vectors are configured, the vector re-insert for
    /// `into_id` is performed after the transaction (requires async embedding computation).
    pub async fn merge_entity(
        &self,
        token: &NamespaceToken,
        into_id: Uuid,
        from_id: Uuid,
        strategy: EntityDedupMergePolicy,
        dry_run: bool,
    ) -> RuntimeResult<MergeSummary> {
        if into_id == from_id {
            return Err(RuntimeError::InvalidInput(
                "cannot merge an entity into itself".into(),
            ));
        }
        // H2 fix: enforce same-kind constraint at the runtime layer.
        // The handler also checks this, but any direct runtime caller (CLI, tests,
        // future SDK) would bypass the handler guard without this check here.
        {
            let into_entity = self.get_entity(token, into_id).await?;
            let from_entity = self.get_entity(token, from_id).await?;
            if into_entity.kind != from_entity.kind {
                return Err(RuntimeError::InvalidInput(format!(
                    "cannot merge entities of different kinds: into={} ({}), from={} ({}); \
                     merge requires both entities to share the same kind",
                    into_id, into_entity.kind, from_id, from_entity.kind
                )));
            }
        }
        let ns = token.namespace().as_str().to_owned();
        let fts_table = "fts_entities".to_string();
        let vec_tables: Vec<String> = self
            .registered_embedding_model_names()
            .iter()
            .map(|name| format!("vec_{}", crate::config::sanitize_key(name)))
            .collect();

        // Ensure all required tables exist before entering the transaction.
        // Each accessor applies its DDL idempotently via `CREATE TABLE IF NOT EXISTS`.
        let _ = self.entities(token)?;
        let _ = self.graph(token)?;
        let _ = self.text(token)?;
        // Prime DDL for every registered model's vec table.  Use vectors_for_model
        // (not the default-model-only self.vectors()) so that custom-only runtimes
        // (embedding_model: None but custom providers registered) work correctly.
        for model_name in &self.registered_embedding_model_names() {
            let _ = self.vectors_for_model(token, model_name)?;
        }

        let pool = self.backend().pool_arc();

        let (summary, updated_entity) = tokio::task::spawn_blocking(move || {
            let guard = pool.writer()?;
            guard.transaction(|conn| {
                merge_entity_sql(
                    conn, ns, fts_table, vec_tables, into_id, from_id, strategy, dry_run,
                )
            })
        })
        .await
        .map_err(|e| RuntimeError::Internal(e.to_string()))??;

        // If vectors are configured, reindex into_entity (requires async embedding).
        // FTS and vec-deletes for all registered models were already committed inside
        // the transaction above.
        if !dry_run && !self.registered_embedding_model_names().is_empty() {
            self.reindex_entity(token, &updated_entity).await?;
        }

        let event_store = self.events(token)?;
        // Mirror the wire-level strategy spelling from MergeParams so consumers
        // can round-trip the policy string back into a request.
        let policy_str = match strategy {
            EntityDedupMergePolicy::PreferInto => "prefer_into",
            EntityDedupMergePolicy::PreferFrom => "prefer_from",
            EntityDedupMergePolicy::Union => "union",
        };
        let event = khive_storage::event::Event::new(
            updated_entity.namespace.clone(),
            "merge",
            EventKind::EntityMerged,
            SubstrateKind::Entity,
            "",
        )
        .with_target(summary.kept_id)
        .with_payload(serde_json::json!({
            "into_id": summary.kept_id,
            "from_id": summary.removed_id,
            "policy": policy_str,
            "edges_rewired": summary.edges_rewired,
        }));
        event_store.append_event(event).await.map_err(|e| {
            RuntimeError::Internal(format!("merge_entity: event store write failed: {e}"))
        })?;

        Ok(summary)
    }

    // ---- Internal helpers ----

    /// Re-upsert FTS5 document and vector(s) for the entity across all registered models.
    ///
    /// Uses `entity.namespace` — the authoritative namespace stored on the record — rather
    /// than the caller-supplied `namespace` parameter. This prevents a cross-namespace
    /// reindex from writing the search document into the wrong namespace's FTS index.
    ///
    /// Best-effort for vectors: if embedding or inserting for a particular model fails,
    /// logs a warning and continues to the next model. The FTS step is fail-closed
    /// (propagates error). Callers (update_entity, merge_entity) have already committed
    /// the entity row, so a partial embed miss leaves a stale vector rather than
    /// rolling back the update — acceptable because SqliteVecStore::insert is an upsert
    /// (the prior vector stays intact on failure, keeping the record searchable).
    pub(crate) async fn reindex_entity(
        &self,
        token: &NamespaceToken,
        entity: &Entity,
    ) -> RuntimeResult<()> {
        // Use entity.namespace (authoritative) rather than token.namespace().as_str() (caller claim).
        let ns = entity.namespace.clone();
        let doc = entity_fts_document(entity);
        let embed_body = doc.body.clone();
        self.text(token)?.upsert_document(doc).await?;

        let embed_model_names = self.registered_embedding_model_names();
        for model_name in &embed_model_names {
            match self
                .embed_document_with_model(model_name, &embed_body)
                .await
            {
                Ok(vector) => match self.vectors_for_model(token, model_name) {
                    Ok(vs) => {
                        if let Err(e) = vs
                            .insert(
                                entity.id,
                                SubstrateKind::Entity,
                                &ns,
                                "entity.body",
                                vec![vector],
                            )
                            .await
                        {
                            tracing::warn!(
                                model = model_name,
                                id = %entity.id,
                                "reindex_entity: vector insert failed, skipping model: {e}"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            model = model_name,
                            id = %entity.id,
                            "reindex_entity: could not access vector store for model, skipping: {e}"
                        );
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        model = model_name,
                        id = %entity.id,
                        "reindex_entity: embed failed for model, skipping: {e}"
                    );
                }
            }
        }

        Ok(())
    }

    /// Remove an entity from FTS5 and vector indexes across all registered models.
    pub(crate) async fn remove_from_indexes(
        &self,
        token: &NamespaceToken,
        id: Uuid,
    ) -> RuntimeResult<()> {
        let ns = token.namespace().as_str().to_owned();
        self.text(token)?.delete_document(&ns, id).await?;
        for model_name in self.registered_embedding_model_names() {
            self.vectors_for_model(token, &model_name)?
                .delete(id)
                .await?;
        }
        Ok(())
    }

    /// Re-upsert FTS5 document and vector(s) for the note across all registered models.
    ///
    /// Best-effort for vectors: mirrors reindex_entity's warn-and-continue policy.
    pub(crate) async fn reindex_note(
        &self,
        token: &NamespaceToken,
        note: &khive_storage::note::Note,
    ) -> RuntimeResult<()> {
        self.text_for_notes(token)?
            .upsert_document(note_fts_document(note))
            .await?;

        let ns = note.namespace.clone();
        let embed_model_names = self.registered_embedding_model_names();
        for model_name in &embed_model_names {
            match self
                .embed_document_with_model(model_name, &note.content)
                .await
            {
                Ok(vector) => match self.vectors_for_model(token, model_name) {
                    Ok(vs) => {
                        if let Err(e) = vs
                            .insert(
                                note.id,
                                SubstrateKind::Note,
                                &ns,
                                "note.content",
                                vec![vector],
                            )
                            .await
                        {
                            tracing::warn!(
                                model = model_name,
                                id = %note.id,
                                "reindex_note: vector insert failed, skipping model: {e}"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            model = model_name,
                            id = %note.id,
                            "reindex_note: could not access vector store for model, skipping: {e}"
                        );
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        model = model_name,
                        id = %note.id,
                        "reindex_note: embed failed for model, skipping: {e}"
                    );
                }
            }
        }
        Ok(())
    }

    /// Patch-style note update.
    pub async fn update_note(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        patch: NotePatch,
    ) -> RuntimeResult<khive_storage::note::Note> {
        // Secret gate: scan incoming text fields and structured properties.
        if let Some(ref content) = patch.content {
            crate::secret_gate::check(content)?;
        }
        if let Some(Some(ref name)) = patch.name {
            crate::secret_gate::check(name)?;
        }
        if let Some(ref props) = patch.properties {
            crate::secret_gate::check_json(props)?;
        }
        let store = self.notes(token)?;
        let mut note = store
            .get_note(id)
            .await?
            .ok_or_else(|| RuntimeError::NotFound(format!("note {id}")))?;

        let mut text_changed = false;

        if let Some(name_patch) = patch.name {
            text_changed |= note.name != name_patch;
            note.name = name_patch;
        }
        if let Some(content) = patch.content {
            text_changed |= note.content != content;
            note.content = content;
        }
        if let Some(salience_patch) = patch.salience {
            // Reject non-finite or out-of-range salience at the runtime boundary
            // rather than silently clamping invalid caller input (coding-standards §608-622).
            if let Some(s) = salience_patch {
                if !s.is_finite() || !(0.0..=1.0).contains(&s) {
                    return Err(crate::RuntimeError::InvalidInput(format!(
                        "salience must be a finite value in [0.0, 1.0]; got {s}"
                    )));
                }
            }
            note.salience = salience_patch;
        }
        if let Some(decay_patch) = patch.decay_factor {
            // Reject non-finite or negative decay_factor at the runtime boundary.
            if let Some(d) = decay_patch {
                if !d.is_finite() || d < 0.0 {
                    return Err(crate::RuntimeError::InvalidInput(format!(
                        "decay_factor must be a finite value >= 0.0; got {d}"
                    )));
                }
            }
            note.decay_factor = decay_patch;
        }
        if let Some(props) = patch.properties {
            let (merged, _) = merge_properties(
                &note.properties,
                &Some(props),
                EntityDedupMergePolicy::PreferFrom,
            );
            note.properties = merged;
        }
        if let Some(status) = patch.kind_status {
            note.status = status;
        }

        note.updated_at = chrono::Utc::now().timestamp_micros();
        store.upsert_note(note.clone()).await?;

        if text_changed {
            self.reindex_note(token, &note).await?;
        }

        Ok(note)
    }

    /// Merge `from_id` note into `into_id` note.
    ///
    /// Both notes must exist in the namespace and have the same `kind`. Content is merged
    /// per `content_strategy`. Properties are merged per `strategy`. `from_id` is
    /// tombstoned (status='deleted', deleted_at set). Returns a summary.
    ///
    /// If `dry_run` is true, computes and returns the planned summary without mutating
    /// any rows, edges, or indexes.
    pub async fn merge_note(
        &self,
        token: &NamespaceToken,
        into_id: Uuid,
        from_id: Uuid,
        strategy: EntityDedupMergePolicy,
        content_strategy: ContentMergeStrategy,
        dry_run: bool,
    ) -> RuntimeResult<MergeSummary> {
        if into_id == from_id {
            return Err(RuntimeError::InvalidInput(
                "cannot merge a note into itself".into(),
            ));
        }
        let ns = token.namespace().as_str().to_string();
        let fts_table = "fts_notes".to_string();
        let vec_tables: Vec<String> = self
            .registered_embedding_model_names()
            .iter()
            .map(|name| format!("vec_{}", crate::config::sanitize_key(name)))
            .collect();

        let note_store = self.notes(token)?;
        let into_note = note_store
            .get_note(into_id)
            .await?
            .ok_or_else(|| RuntimeError::NotFound("not found in this namespace".into()))?;
        Self::ensure_namespace(&into_note.namespace, &ns)?;

        let from_note = note_store
            .get_note(from_id)
            .await?
            .ok_or_else(|| RuntimeError::NotFound("not found in this namespace".into()))?;
        Self::ensure_namespace(&from_note.namespace, &ns)?;

        let _ = self.graph(token)?;
        let _ = self.text_for_notes(token)?;
        // Prime DDL for every registered model's vec table (mirrors merge_entity fix).
        for model_name in &self.registered_embedding_model_names() {
            let _ = self.vectors_for_model(token, model_name)?;
        }

        let pool = self.backend().pool_arc();
        let (summary, updated_note) = tokio::task::spawn_blocking(move || {
            let guard = pool.writer()?;
            guard.transaction(|conn| {
                merge_note_sql(
                    conn,
                    ns,
                    fts_table,
                    vec_tables,
                    into_id,
                    from_id,
                    strategy,
                    content_strategy,
                    dry_run,
                )
            })
        })
        .await
        .map_err(|e| RuntimeError::Internal(e.to_string()))??;

        if !dry_run && !self.registered_embedding_model_names().is_empty() {
            self.reindex_note(token, &updated_note).await?;
        }
        Ok(summary)
    }
}

// ---------------------------------------------------------------------------
// FTS document construction
// ---------------------------------------------------------------------------

/// Build the `TextDocument` for an entity. This is the single source of truth for
/// entity FTS document shape; all write paths (create, update, merge, reindex, backfill)
/// must go through this function so search parity is guaranteed.
///
/// Body rule: when the entity has a non-empty description, prepend the name
/// (`"<name> <description>"`). Otherwise the body is just the name. This
/// matches the FTS index contract: `title` and `body` are the ranked columns;
/// `tags`, `metadata`, and `namespace` are UNINDEXED.
///
/// `updated_at` is taken from the entity's own timestamp so that backfill and
/// reindex runs record the entity's actual mutation time rather than the
/// reindex execution time.
pub fn entity_fts_document(entity: &Entity) -> TextDocument {
    let body = match &entity.description {
        Some(d) if !d.is_empty() => format!("{} {}", entity.name, d),
        _ => entity.name.clone(),
    };
    let updated_at =
        chrono::DateTime::from_timestamp_micros(entity.updated_at).unwrap_or_else(chrono::Utc::now);
    TextDocument {
        subject_id: entity.id,
        kind: SubstrateKind::Entity,
        title: Some(entity.name.clone()),
        body,
        tags: entity.tags.clone(),
        namespace: entity.namespace.clone(),
        metadata: entity.properties.clone(),
        updated_at,
    }
}

/// Build the `TextDocument` for a note. This is the single source of truth for
/// note FTS document shape; all write paths (create, update, reindex) must go
/// through this function so recall parity is guaranteed. Changes here apply to
/// every caller automatically.
///
/// Body rule: when the note has a `name`, prepend it to the content
/// (`"<name> <content>"`). This matches the FTS index contract: title and body
/// both contribute to ranking, and the name is the most salient signal.
///
/// `updated_at` is taken from the note's own timestamp (not `Utc::now()`) so
/// that backfill and reindex runs record the note's actual mutation time rather
/// than the reindex execution time.
pub fn note_fts_document(note: &Note) -> TextDocument {
    let body = match &note.name {
        Some(n) => format!("{n} {}", note.content),
        None => note.content.clone(),
    };
    let updated_at =
        chrono::DateTime::from_timestamp_micros(note.updated_at).unwrap_or_else(chrono::Utc::now);
    TextDocument {
        subject_id: note.id,
        kind: SubstrateKind::Note,
        title: note.name.clone(),
        body,
        tags: vec![],
        namespace: note.namespace.clone(),
        metadata: note.properties.clone(),
        updated_at,
    }
}

/// SQL-bind–ready scalars derived from [`note_fts_document`].
///
/// Used by `merge_note_sql` to guarantee that the raw SQL FTS INSERT stores
/// exactly what [`Fts5TextSearch::upsert_document`] would write, preventing
/// null/empty-string divergence on the `title` column for nameless notes.
pub(crate) struct NoteFtsScalars {
    /// Empty string when `note.name` is `None` — matches the `unwrap_or("")` in
    /// `Fts5TextSearch::upsert_document`.
    pub title: String,
    pub body: String,
    /// Always the JSON array `"[]"`.
    pub tags: String,
    /// Serialised `note.properties`, or `None` when properties are absent.
    pub metadata: Option<String>,
    /// `note.updated_at` converted to `DateTime<Utc>` timestamp_micros.
    pub updated_at_micros: i64,
}

/// Derive [`NoteFtsScalars`] from a [`Note`].
///
/// All values match the encoding that [`Fts5TextSearch::upsert_document`]
/// applies when given the output of [`note_fts_document`].
pub(crate) fn note_fts_scalars(note: &Note) -> NoteFtsScalars {
    let doc = note_fts_document(note);
    NoteFtsScalars {
        title: doc.title.unwrap_or_default(),
        body: doc.body,
        tags: "[]".to_string(),
        metadata: doc
            .metadata
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default()),
        updated_at_micros: doc.updated_at.timestamp_micros(),
    }
}

// ---------------------------------------------------------------------------
// Transactional merge SQL helpers
// ---------------------------------------------------------------------------

/// Read one entity row by ID within a namespace, returning `SqliteError` on missing/wrong-ns.
fn read_merge_entity(
    conn: &rusqlite::Connection,
    id: Uuid,
    namespace: &str,
) -> Result<Entity, SqliteError> {
    let id_str = id.to_string();
    let mut stmt = conn.prepare(
        "SELECT id, namespace, kind, entity_type, name, description, properties, tags, \
         created_at, updated_at, deleted_at, merged_into, merge_event_id \
         FROM entities WHERE id = ?1 AND deleted_at IS NULL",
    )?;
    let mut rows = stmt.query(rusqlite::params![id_str])?;
    let row = rows
        .next()?
        .ok_or_else(|| SqliteError::InvalidData(format!("entity {id} not found")))?;

    let id_s: String = row.get(0)?;
    let ns: String = row.get(1)?;
    let kind: String = row.get(2)?;
    let entity_type: Option<String> = row.get(3)?;
    let name: String = row.get(4)?;
    let description: Option<String> = row.get(5)?;
    let properties_str: Option<String> = row.get(6)?;
    let tags_str: String = row.get(7)?;
    let created_at: i64 = row.get(8)?;
    let updated_at: i64 = row.get(9)?;
    let deleted_at: Option<i64> = row.get(10)?;
    let merged_into_str: Option<String> = row.get(11)?;
    let merge_event_id_str: Option<String> = row.get(12)?;

    if ns != namespace {
        return Err(SqliteError::InvalidData(format!(
            "entity {id} belongs to namespace '{ns}', not '{namespace}'"
        )));
    }

    let entity_id = Uuid::parse_str(&id_s).map_err(|e| SqliteError::InvalidData(e.to_string()))?;
    let properties: Option<Value> = properties_str
        .map(|s| {
            serde_json::from_str::<Value>(&s).map_err(|e| SqliteError::InvalidData(e.to_string()))
        })
        .transpose()?;
    let tags: Vec<String> =
        serde_json::from_str(&tags_str).map_err(|e| SqliteError::InvalidData(e.to_string()))?;
    let merged_into = merged_into_str
        .as_deref()
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|e| SqliteError::InvalidData(e.to_string()))?;
    let merge_event_id = merge_event_id_str
        .as_deref()
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|e| SqliteError::InvalidData(e.to_string()))?;

    Ok(Entity {
        id: entity_id,
        namespace: ns,
        kind,
        entity_type,
        name,
        description,
        properties,
        tags,
        created_at,
        updated_at,
        deleted_at,
        merged_into,
        merge_event_id,
    })
}

/// All merge SQL on one connection inside an already-open `BEGIN IMMEDIATE` transaction.
///
/// Reads both entities, rewires/drops incident edges, merges entity fields, updates FTS,
/// deletes the `from` vec entry (if `vec_table` is Some), and tombstones `from` with merge
/// provenance.  Returns the updated `into` entity so the caller can do the async vec re-insert.
///
/// When `dry_run` is true, all reads and computations are performed but no writes are issued.
// REASON: merge requires both entity IDs, the namespace, FTS and vec table names, merge
// policy, and dry-run flag — all are load-bearing; reducing to a struct would obscure
// the sync/async boundary split that keeps this function off the async runtime.
#[allow(clippy::too_many_arguments)]
fn merge_entity_sql(
    conn: &rusqlite::Connection,
    namespace: String,
    fts_table: String,
    vec_tables: Vec<String>,
    into_id: Uuid,
    from_id: Uuid,
    strategy: EntityDedupMergePolicy,
    dry_run: bool,
) -> Result<(MergeSummary, Entity), SqliteError> {
    let into_entity = read_merge_entity(conn, into_id, &namespace)?;
    let from_entity = read_merge_entity(conn, from_id, &namespace)?;

    // --- Collect edges incident to from_id ---
    let parse_id =
        |s: String| Uuid::parse_str(&s).map_err(|e| SqliteError::InvalidData(e.to_string()));

    let from_str = from_id.to_string();

    let mut outbound: Vec<EdgeRow> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id, source_id, target_id, relation, weight, created_at, \
                    updated_at, deleted_at, target_backend, metadata \
             FROM graph_edges WHERE namespace = ?1 AND source_id = ?2",
        )?;
        let mut rows = stmt.query(rusqlite::params![&namespace, &from_str])?;
        while let Some(row) = rows.next()? {
            outbound.push(EdgeRow {
                id: parse_id(row.get(0)?)?,
                source_id: parse_id(row.get(1)?)?,
                target_id: parse_id(row.get(2)?)?,
                relation: row.get(3)?,
                weight: row.get(4)?,
                created_at: row.get(5)?,
                updated_at: row.get(6)?,
                deleted_at: row.get(7)?,
                target_backend: row.get(8)?,
                metadata: row.get(9)?,
            });
        }
    }

    let mut inbound: Vec<EdgeRow> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id, source_id, target_id, relation, weight, created_at, \
                    updated_at, deleted_at, target_backend, metadata \
             FROM graph_edges WHERE namespace = ?1 AND target_id = ?2",
        )?;
        let mut rows = stmt.query(rusqlite::params![&namespace, &from_str])?;
        while let Some(row) = rows.next()? {
            inbound.push(EdgeRow {
                id: parse_id(row.get(0)?)?,
                source_id: parse_id(row.get(1)?)?,
                target_id: parse_id(row.get(2)?)?,
                relation: row.get(3)?,
                weight: row.get(4)?,
                created_at: row.get(5)?,
                updated_at: row.get(6)?,
                deleted_at: row.get(7)?,
                target_backend: row.get(8)?,
                metadata: row.get(9)?,
            });
        }
    }

    // Deduplicate by edge ID (a self-edge from_id→from_id appears in both lists).
    let mut seen: HashSet<Uuid> = HashSet::new();
    let mut all_edges: Vec<EdgeRow> = Vec::new();
    for edge in outbound.into_iter().chain(inbound) {
        if seen.insert(edge.id) {
            all_edges.push(edge);
        }
    }

    // --- Merge entity fields ---
    let (merged_props, properties_merged) =
        merge_properties(&into_entity.properties, &from_entity.properties, strategy);
    let merged_name = merge_string_field(&into_entity.name, &from_entity.name, strategy);
    let merged_description =
        merge_option_string_field(&into_entity.description, &from_entity.description, strategy);
    let (merged_tags, tags_unioned) = union_tags(&into_entity.tags, &from_entity.tags);

    let now = chrono::Utc::now().timestamp_micros();
    let into_str = into_id.to_string();
    let props_str = merged_props
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default());
    let tags_json = serde_json::to_string(&merged_tags).unwrap_or_else(|_| "[]".to_string());

    // --- Rewire edges ---
    let mut edges_rewired = 0usize;
    if !dry_run {
        for edge in all_edges {
            let raw_src = if edge.source_id == from_id {
                into_id
            } else {
                edge.source_id
            };
            let raw_tgt = if edge.target_id == from_id {
                into_id
            } else {
                edge.target_id
            };
            // Symmetric relations must be stored with source_uuid < target_uuid.
            // Apply canonicalization so the conflict check and UPDATE both use the canonical form.
            let (new_src, new_tgt) = match edge.relation.parse::<EdgeRelation>() {
                Ok(rel) => canonical_edge_endpoints(rel, raw_src, raw_tgt),
                Err(_) => (raw_src, raw_tgt),
            };

            if new_src == new_tgt {
                conn.execute(
                    "DELETE FROM graph_edges WHERE namespace = ?1 AND id = ?2",
                    rusqlite::params![&namespace, edge.id.to_string()],
                )?;
                continue;
            }

            let now_ts = chrono::Utc::now().timestamp();
            // H3 fix: preserve the original edge ID by updating
            // source_id/target_id in-place when no conflict exists.
            //
            // Two-step approach to handle all cases while keeping the original ID:
            //   (a) No conflict (new triple): UPDATE source_id/target_id in-place.
            //       The edge retains its original UUID — callers can still get() it
            //       by the ID they received from link().
            //   (b) Conflict: into_id already has an edge with this (source,target,
            //       relation). Delete the from-edge (it is superseded) and UPDATE
            //       the existing into-edge to refresh weight/metadata/deleted_at.
            //       The surviving edge is the into-entity's original edge (correct).
            //
            // Check for a conflict: does into_id already have this natural key?
            let conflict_id: Option<String> = {
                let conflict_src = new_src.to_string();
                let conflict_tgt = new_tgt.to_string();
                conn.query_row(
                    "SELECT id FROM graph_edges \
                     WHERE namespace = ?1 AND source_id = ?2 AND target_id = ?3 \
                     AND relation = ?4 AND id != ?5",
                    rusqlite::params![
                        &namespace,
                        &conflict_src,
                        &conflict_tgt,
                        &edge.relation,
                        edge.id.to_string(),
                    ],
                    |row| row.get(0),
                )
                .optional()
                .map_err(SqliteError::Rusqlite)?
            };

            let changed = if let Some(existing_id) = conflict_id {
                // Case (b): a live or soft-deleted row already owns this natural key.
                // Delete the from-edge and refresh the existing row.
                conn.execute(
                    "DELETE FROM graph_edges WHERE namespace = ?1 AND id = ?2",
                    rusqlite::params![&namespace, edge.id.to_string()],
                )?;
                conn.execute(
                    "UPDATE graph_edges SET \
                     weight = ?1, updated_at = ?2, deleted_at = NULL, \
                     target_backend = ?3, metadata = ?4 \
                     WHERE namespace = ?5 AND id = ?6",
                    rusqlite::params![
                        edge.weight,
                        now_ts,
                        edge.target_backend,
                        edge.metadata,
                        &namespace,
                        &existing_id,
                    ],
                )?
            } else {
                // Case (a): no conflict — update source_id/target_id in-place,
                // preserving the original edge ID for callers.
                conn.execute(
                    "UPDATE graph_edges SET \
                     source_id = ?1, target_id = ?2, updated_at = ?3 \
                     WHERE namespace = ?4 AND id = ?5",
                    rusqlite::params![
                        new_src.to_string(),
                        new_tgt.to_string(),
                        now_ts,
                        &namespace,
                        edge.id.to_string(),
                    ],
                )?
            };
            if changed > 0 {
                edges_rewired += 1;
            }
        }

        // --- Upsert merged entity ---
        conn.execute(
            "INSERT OR REPLACE INTO entities \
             (id, namespace, kind, name, description, properties, tags, \
              created_at, updated_at, deleted_at, merged_into, merge_event_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                &into_str,
                &namespace,
                &into_entity.kind,
                &merged_name,
                &merged_description,
                &props_str,
                &tags_json,
                into_entity.created_at,
                now,
                into_entity.deleted_at,
                Option::<String>::None,
                Option::<String>::None,
            ],
        )?;

        // --- Reindex into_id in FTS (delete existing, insert updated) ---
        // Body formula mirrors entity_fts_document (the canonical constructor) —
        // this path is sync/spawn_blocking so it cannot call entity_fts_document
        // directly, but must stay field-identical.
        let fts_body = match &merged_description {
            Some(d) if !d.is_empty() => format!("{} {}", merged_name, d),
            _ => merged_name.clone(),
        };
        let kind_str = SubstrateKind::Entity.to_string();

        conn.execute(
            &format!(
                "DELETE FROM {} WHERE namespace = ?1 AND subject_id = ?2",
                fts_table
            ),
            rusqlite::params![&namespace, &into_str],
        )?;
        conn.execute(
            &format!(
                "INSERT INTO {} \
                 (subject_id, kind, title, body, tags, namespace, metadata, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                fts_table
            ),
            rusqlite::params![
                &into_str,
                &kind_str,
                &merged_name,
                &fts_body,
                &tags_json,
                &namespace,
                &props_str,
                now,
            ],
        )?;

        // --- Delete from_id from FTS ---
        conn.execute(
            &format!(
                "DELETE FROM {} WHERE namespace = ?1 AND subject_id = ?2",
                fts_table
            ),
            rusqlite::params![&namespace, &from_str],
        )?;

        // --- Delete from_id from all registered model vector tables ---
        for vec_tbl in &vec_tables {
            conn.execute(
                &format!(
                    "DELETE FROM {} WHERE subject_id = ?1 AND namespace = ?2",
                    vec_tbl
                ),
                rusqlite::params![&from_str, &namespace],
            )?;
        }

        // --- Tombstone from entity (soft-delete with provenance) ---
        let merge_event_id = Uuid::new_v4();
        conn.execute(
            "UPDATE entities \
             SET deleted_at = ?1, merged_into = ?2, merge_event_id = ?3, updated_at = ?1 \
             WHERE namespace = ?4 AND id = ?5 AND deleted_at IS NULL",
            rusqlite::params![
                now,
                into_str,
                merge_event_id.to_string(),
                &namespace,
                &from_str,
            ],
        )?;
    }

    let updated_entity = Entity {
        id: into_id,
        namespace,
        kind: into_entity.kind,
        entity_type: into_entity.entity_type,
        name: merged_name,
        description: merged_description,
        properties: merged_props,
        tags: merged_tags,
        created_at: into_entity.created_at,
        updated_at: now,
        deleted_at: into_entity.deleted_at,
        merged_into: None,
        merge_event_id: None,
    };

    Ok((
        MergeSummary {
            kept_id: into_id,
            removed_id: from_id,
            edges_rewired,
            properties_merged,
            tags_unioned,
            content_appended: false,
            dry_run,
        },
        updated_entity,
    ))
}

// ---------------------------------------------------------------------------
// Note merge SQL helpers
// ---------------------------------------------------------------------------

/// Read one note row by ID within a namespace, returning `SqliteError` on missing/wrong-ns.
fn read_merge_note(
    conn: &rusqlite::Connection,
    id: Uuid,
    namespace: &str,
) -> Result<khive_storage::note::Note, SqliteError> {
    use khive_storage::note::Note;
    let id_str = id.to_string();
    let mut stmt = conn.prepare(
        "SELECT id, namespace, kind, status, name, content, salience, decay_factor, \
         expires_at, properties, created_at, updated_at, deleted_at \
         FROM notes WHERE id = ?1 AND deleted_at IS NULL",
    )?;
    let mut rows = stmt.query(rusqlite::params![id_str])?;
    let row = rows
        .next()?
        .ok_or_else(|| SqliteError::InvalidData(format!("note {id} not found")))?;

    let id_s: String = row.get(0)?;
    let ns: String = row.get(1)?;
    let kind: String = row.get(2)?;
    let status: String = row.get(3)?;
    let name: Option<String> = row.get(4)?;
    let content: String = row.get(5)?;
    let salience: Option<f64> = row.get(6)?;
    let decay_factor: Option<f64> = row.get(7)?;
    let expires_at: Option<i64> = row.get(8)?;
    let properties_str: Option<String> = row.get(9)?;
    let created_at: i64 = row.get(10)?;
    let updated_at: i64 = row.get(11)?;
    let deleted_at: Option<i64> = row.get(12)?;

    if ns != namespace {
        return Err(SqliteError::InvalidData(format!(
            "note {id} belongs to namespace '{ns}', not '{namespace}'"
        )));
    }

    let note_id = Uuid::parse_str(&id_s).map_err(|e| SqliteError::InvalidData(e.to_string()))?;
    let properties: Option<serde_json::Value> = properties_str
        .map(|s| serde_json::from_str(&s).map_err(|e| SqliteError::InvalidData(e.to_string())))
        .transpose()?;

    Ok(Note {
        id: note_id,
        namespace: ns,
        kind,
        status,
        name,
        content,
        salience,
        decay_factor,
        expires_at,
        properties,
        created_at,
        updated_at,
        deleted_at,
    })
}

fn max_option_f64(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
    }
}

fn append_merge_history(props: Option<Value>, entry: Value) -> Result<Option<Value>, SqliteError> {
    use serde_json::{json, Map};
    let mut obj: Map<String, Value> = match props {
        Some(Value::Object(m)) => m,
        Some(other) => {
            let mut m = Map::new();
            m.insert("_value".into(), other);
            m
        }
        None => Map::new(),
    };
    let history = obj
        .entry("_merge_history".to_string())
        .or_insert_with(|| json!([]));
    if let Value::Array(arr) = history {
        arr.push(entry);
    }
    Ok(Some(Value::Object(obj)))
}

/// All note merge SQL on one connection inside a `BEGIN IMMEDIATE` transaction.
///
/// Reads both notes (must have same `kind`), rewires/drops incident edges, merges content
/// per `content_strategy`, tombstones `from`. Returns the updated `into` note for async
/// re-embedding.
///
/// When `dry_run` is true, all reads and computations are performed but no writes are issued.
// REASON: note merge additionally requires a content_strategy parameter versus entity merge;
// same sync/async boundary rationale as merge_entity_sql applies here.
#[allow(clippy::too_many_arguments)]
fn merge_note_sql(
    conn: &rusqlite::Connection,
    namespace: String,
    fts_table: String,
    vec_tables: Vec<String>,
    into_id: Uuid,
    from_id: Uuid,
    strategy: EntityDedupMergePolicy,
    content_strategy: ContentMergeStrategy,
    dry_run: bool,
) -> Result<(MergeSummary, khive_storage::note::Note), SqliteError> {
    let into_note = read_merge_note(conn, into_id, &namespace)?;
    let from_note = read_merge_note(conn, from_id, &namespace)?;

    if into_note.kind != from_note.kind {
        return Err(SqliteError::InvalidData(format!(
            "cannot merge notes of different kinds: {} vs {}",
            into_note.kind, from_note.kind
        )));
    }

    let now = chrono::Utc::now().timestamp_micros();
    let into_str = into_id.to_string();
    let from_str = from_id.to_string();

    // Collect edges incident to from_id.
    let parse_id =
        |s: String| Uuid::parse_str(&s).map_err(|e| SqliteError::InvalidData(e.to_string()));

    let mut outbound: Vec<EdgeRow> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id, source_id, target_id, relation, weight, created_at, updated_at, deleted_at, target_backend, metadata \
             FROM graph_edges WHERE namespace = ?1 AND source_id = ?2",
        )?;
        let mut rows = stmt.query(rusqlite::params![&namespace, &from_str])?;
        while let Some(row) = rows.next()? {
            outbound.push(EdgeRow {
                id: parse_id(row.get(0)?)?,
                source_id: parse_id(row.get(1)?)?,
                target_id: parse_id(row.get(2)?)?,
                relation: row.get(3)?,
                weight: row.get(4)?,
                created_at: row.get(5)?,
                updated_at: row.get(6)?,
                deleted_at: row.get(7)?,
                target_backend: row.get(8)?,
                metadata: row.get(9)?,
            });
        }
    }
    let mut inbound: Vec<EdgeRow> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id, source_id, target_id, relation, weight, created_at, updated_at, deleted_at, target_backend, metadata \
             FROM graph_edges WHERE namespace = ?1 AND target_id = ?2",
        )?;
        let mut rows = stmt.query(rusqlite::params![&namespace, &from_str])?;
        while let Some(row) = rows.next()? {
            inbound.push(EdgeRow {
                id: parse_id(row.get(0)?)?,
                source_id: parse_id(row.get(1)?)?,
                target_id: parse_id(row.get(2)?)?,
                relation: row.get(3)?,
                weight: row.get(4)?,
                created_at: row.get(5)?,
                updated_at: row.get(6)?,
                deleted_at: row.get(7)?,
                target_backend: row.get(8)?,
                metadata: row.get(9)?,
            });
        }
    }
    let mut seen: HashSet<Uuid> = HashSet::new();
    let mut all_edges: Vec<EdgeRow> = Vec::new();
    for edge in outbound.into_iter().chain(inbound) {
        if seen.insert(edge.id) {
            all_edges.push(edge);
        }
    }

    // Merge note fields.
    let (merged_content, content_appended) = match content_strategy {
        ContentMergeStrategy::Append => {
            if from_note.content.is_empty() {
                (into_note.content.clone(), false)
            } else {
                (
                    format!("{}\n\n---\n\n{}", into_note.content, from_note.content),
                    true,
                )
            }
        }
        ContentMergeStrategy::PreferInto => (into_note.content.clone(), false),
        ContentMergeStrategy::PreferFrom => (from_note.content.clone(), false),
    };

    let merged_name = match strategy {
        EntityDedupMergePolicy::PreferFrom => from_note.name.clone().or(into_note.name.clone()),
        _ => into_note.name.clone().or(from_note.name.clone()),
    };

    let (merged_props, properties_merged) =
        merge_properties(&into_note.properties, &from_note.properties, strategy);

    // Append merge history to properties.
    let merge_history_entry = serde_json::json!({
        "merged_from": from_id.to_string(),
        "merged_at": now,
        "strategy": format!("{:?}", strategy),
        "content_strategy": format!("{:?}", content_strategy),
    });
    let merged_props = append_merge_history(merged_props, merge_history_entry)?;

    let merged_salience = max_option_f64(into_note.salience, from_note.salience);
    let merged_expires_at = match (into_note.expires_at, from_note.expires_at) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };

    let props_str = merged_props
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default());

    let mut edges_rewired = 0usize;
    if !dry_run {
        // Rewire and upsert.
        for edge in all_edges {
            let raw_src = if edge.source_id == from_id {
                into_id
            } else {
                edge.source_id
            };
            let raw_tgt = if edge.target_id == from_id {
                into_id
            } else {
                edge.target_id
            };
            // Canonicalize symmetric relations before conflict check + UPDATE.
            let (new_src, new_tgt) = match edge.relation.parse::<EdgeRelation>() {
                Ok(rel) => canonical_edge_endpoints(rel, raw_src, raw_tgt),
                Err(_) => (raw_src, raw_tgt),
            };
            if new_src == new_tgt {
                conn.execute(
                    "DELETE FROM graph_edges WHERE namespace = ?1 AND id = ?2",
                    rusqlite::params![&namespace, edge.id.to_string()],
                )?;
                continue;
            }
            let now_ts = chrono::Utc::now().timestamp();
            // Same two-step approach as entity merge rewire: preserve original edge ID
            // when no conflict, merge into existing row when conflict exists.
            let conflict_id: Option<String> = {
                let conflict_src = new_src.to_string();
                let conflict_tgt = new_tgt.to_string();
                conn.query_row(
                    "SELECT id FROM graph_edges \
                     WHERE namespace = ?1 AND source_id = ?2 AND target_id = ?3 \
                     AND relation = ?4 AND id != ?5",
                    rusqlite::params![
                        &namespace,
                        &conflict_src,
                        &conflict_tgt,
                        &edge.relation,
                        edge.id.to_string(),
                    ],
                    |row| row.get(0),
                )
                .optional()
                .map_err(SqliteError::Rusqlite)?
            };

            let changed = if let Some(existing_id) = conflict_id {
                conn.execute(
                    "DELETE FROM graph_edges WHERE namespace = ?1 AND id = ?2",
                    rusqlite::params![&namespace, edge.id.to_string()],
                )?;
                conn.execute(
                    "UPDATE graph_edges SET \
                     weight = ?1, updated_at = ?2, deleted_at = NULL, \
                     target_backend = ?3, metadata = ?4 \
                     WHERE namespace = ?5 AND id = ?6",
                    rusqlite::params![
                        edge.weight,
                        now_ts,
                        edge.target_backend,
                        edge.metadata,
                        &namespace,
                        &existing_id,
                    ],
                )?
            } else {
                conn.execute(
                    "UPDATE graph_edges SET \
                     source_id = ?1, target_id = ?2, updated_at = ?3 \
                     WHERE namespace = ?4 AND id = ?5",
                    rusqlite::params![
                        new_src.to_string(),
                        new_tgt.to_string(),
                        now_ts,
                        &namespace,
                        edge.id.to_string(),
                    ],
                )?
            };
            if changed > 0 {
                edges_rewired += 1;
            }
        }

        // Upsert merged into-note.
        conn.execute(
            "INSERT OR REPLACE INTO notes \
             (id, namespace, kind, status, name, content, salience, decay_factor, \
              expires_at, properties, created_at, updated_at, deleted_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            rusqlite::params![
                &into_str,
                &namespace,
                &into_note.kind,
                &into_note.status,
                &merged_name,
                &merged_content,
                merged_salience,
                into_note.decay_factor,
                merged_expires_at,
                &props_str,
                into_note.created_at,
                now,
                into_note.deleted_at,
            ],
        )?;

        // Update FTS for into-note.
        conn.execute(
            &format!(
                "DELETE FROM {} WHERE namespace = ?1 AND subject_id = ?2",
                fts_table
            ),
            rusqlite::params![&namespace, &into_str],
        )?;
        // Derive FTS scalars through the shared constructor so this raw SQL
        // path is field-identical to TextSearch::upsert_document.  Critically,
        // `title` is an empty string (not SQL NULL) for nameless notes —
        // matching the unwrap_or("") in Fts5TextSearch::upsert_document and
        // allowing get_document to round-trip None ↔ "" correctly.
        let fts_merged = {
            let mut merged_note = Note::new(&namespace, &*into_note.kind, &*merged_content);
            merged_note.id = into_id;
            merged_note.name = merged_name.clone();
            merged_note.properties = merged_props.clone();
            merged_note.updated_at = now;
            note_fts_scalars(&merged_note)
        };
        conn.execute(
            &format!(
                "INSERT INTO {} \
                 (subject_id, kind, title, body, tags, namespace, metadata, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                fts_table
            ),
            rusqlite::params![
                &into_str,
                SubstrateKind::Note.to_string(),
                &fts_merged.title,
                &fts_merged.body,
                &fts_merged.tags,
                &namespace,
                &fts_merged.metadata,
                fts_merged.updated_at_micros,
            ],
        )?;

        // Delete from-note from FTS.
        conn.execute(
            &format!(
                "DELETE FROM {} WHERE namespace = ?1 AND subject_id = ?2",
                fts_table
            ),
            rusqlite::params![&namespace, &from_str],
        )?;

        // Delete from-note from all registered model vector tables.
        for vec_tbl in &vec_tables {
            conn.execute(
                &format!(
                    "DELETE FROM {} WHERE subject_id = ?1 AND namespace = ?2",
                    vec_tbl
                ),
                rusqlite::params![&from_str, &namespace],
            )?;
        }

        // Tombstone the from-note.
        conn.execute(
            "UPDATE notes SET status = 'deleted', deleted_at = ?1, updated_at = ?1 \
             WHERE namespace = ?2 AND id = ?3 AND deleted_at IS NULL",
            rusqlite::params![now, &namespace, &from_str],
        )?;
    }

    let updated_note = khive_storage::note::Note {
        id: into_id,
        namespace: namespace.clone(),
        kind: into_note.kind.clone(),
        status: into_note.status.clone(),
        name: merged_name,
        content: merged_content,
        salience: merged_salience,
        decay_factor: into_note.decay_factor,
        expires_at: merged_expires_at,
        properties: merged_props,
        created_at: into_note.created_at,
        updated_at: now,
        deleted_at: into_note.deleted_at,
    };

    Ok((
        MergeSummary {
            kept_id: into_id,
            removed_id: from_id,
            edges_rewired,
            properties_merged,
            tags_unioned: 0,
            content_appended,
            dry_run,
        },
        updated_note,
    ))
}

// ---------------------------------------------------------------------------
// Merge helpers (pure functions — easier to unit test)
// ---------------------------------------------------------------------------

fn merge_string_field(into: &str, from: &str, strategy: EntityDedupMergePolicy) -> String {
    match strategy {
        EntityDedupMergePolicy::PreferInto | EntityDedupMergePolicy::Union => into.to_string(),
        EntityDedupMergePolicy::PreferFrom => from.to_string(),
    }
}

fn merge_option_string_field(
    into: &Option<String>,
    from: &Option<String>,
    strategy: EntityDedupMergePolicy,
) -> Option<String> {
    match strategy {
        EntityDedupMergePolicy::PreferInto => {
            if into.is_some() {
                into.clone()
            } else {
                from.clone()
            }
        }
        EntityDedupMergePolicy::PreferFrom => {
            if from.is_some() {
                from.clone()
            } else {
                into.clone()
            }
        }
        EntityDedupMergePolicy::Union => {
            // Keep into's description; if empty, append from's.
            match (into, from) {
                (Some(a), _) if !a.is_empty() => Some(a.clone()),
                (_, Some(b)) => Some(b.clone()),
                _ => None,
            }
        }
    }
}

/// Merge two property objects. Returns (merged, count_of_fields_from_from_that_were_added).
fn merge_properties(
    into: &Option<Value>,
    from: &Option<Value>,
    strategy: EntityDedupMergePolicy,
) -> (Option<Value>, usize) {
    match (into, from) {
        (None, None) => (None, 0),
        (Some(a), None) => (Some(a.clone()), 0),
        (None, Some(b)) => {
            let count = if let Value::Object(m) = b { m.len() } else { 1 };
            (Some(b.clone()), count)
        }
        (Some(into_val), Some(from_val)) => {
            let (merged, added) = merge_json(into_val, from_val, strategy);
            (Some(merged), added)
        }
    }
}

/// Deep-merge two JSON values per strategy. Returns (merged, keys_contributed_by_from).
fn merge_json(into: &Value, from: &Value, strategy: EntityDedupMergePolicy) -> (Value, usize) {
    match (into, from, strategy) {
        (Value::Object(a), Value::Object(b), EntityDedupMergePolicy::Union) => {
            let mut result = a.clone();
            let mut added = 0usize;
            for (k, v_from) in b {
                if let Some(v_into) = a.get(k) {
                    let (merged, sub_added) =
                        merge_json(v_into, v_from, EntityDedupMergePolicy::Union);
                    result.insert(k.clone(), merged);
                    added += sub_added;
                } else {
                    result.insert(k.clone(), v_from.clone());
                    added += 1;
                }
            }
            (Value::Object(result), added)
        }
        (Value::Object(a), Value::Object(b), EntityDedupMergePolicy::PreferInto) => {
            let mut result = a.clone();
            let mut added = 0usize;
            for (k, v) in b {
                if !a.contains_key(k) {
                    result.insert(k.clone(), v.clone());
                    added += 1;
                }
            }
            (Value::Object(result), added)
        }
        (Value::Object(a), Value::Object(b), EntityDedupMergePolicy::PreferFrom) => {
            let mut result = a.clone();
            let mut added = 0usize;
            for (k, v) in b {
                result.insert(k.clone(), v.clone());
                if !a.contains_key(k) {
                    added += 1;
                }
            }
            (Value::Object(result), added)
        }
        // Non-object scalars: apply strategy directly.
        (_into_val, from_val, EntityDedupMergePolicy::PreferFrom) => (from_val.clone(), 1),
        _ => (into.clone(), 0),
    }
}

fn union_tags(into: &[String], from: &[String]) -> (Vec<String>, usize) {
    let mut seen: HashSet<&str> = into.iter().map(|s| s.as_str()).collect();
    let mut result: Vec<String> = into.to_vec();
    let mut added = 0usize;
    for tag in from {
        if seen.insert(tag.as_str()) {
            result.push(tag.clone());
            added += 1;
        }
    }
    (result, added)
}

// ---------------------------------------------------------------------------
// INLINE TEST JUSTIFICATION: tests here exercise patch/merge helpers and the
// update_note/update_entity paths that share private merge_properties logic.
// Moving them to tests/ would require pub-exporting merge_properties, which is
// an internal invariant not suitable for the public API surface. Broad
// behavioral curation tests live in tests/integration.rs.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{KhiveRuntime, NamespaceToken};
    use khive_storage::types::{Direction, TextFilter, TextQueryMode, TextSearchRequest};

    fn rt() -> KhiveRuntime {
        KhiveRuntime::memory().unwrap()
    }

    // Helper: search FTS5 for `query` in a runtime namespace.
    async fn fts_hit(rt: &KhiveRuntime, token: &NamespaceToken, query: &str) -> Vec<Uuid> {
        let ns = token.namespace().as_str().to_string();
        rt.text(token)
            .unwrap()
            .search(TextSearchRequest {
                query: query.to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(TextFilter {
                    namespaces: vec![ns],
                    ..Default::default()
                }),
                top_k: 50,
                snippet_chars: 100,
            })
            .await
            .unwrap()
            .into_iter()
            .map(|h| h.subject_id)
            .collect()
    }

    #[tokio::test]
    async fn update_entity_patch_changes_only_specified_fields() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let entity = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "OriginalName",
                Some("orig desc"),
                Some(serde_json::json!({"k":"v"})),
                vec![],
            )
            .await
            .unwrap();

        let updated = rt
            .update_entity(
                &tok,
                entity.id,
                EntityPatch {
                    description: Some(Some("new desc".to_string())),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(updated.name, "OriginalName");
        assert_eq!(updated.description.as_deref(), Some("new desc"));
        assert_eq!(updated.properties, Some(serde_json::json!({"k":"v"})));
    }

    #[tokio::test]
    async fn update_entity_clear_description_with_some_none() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let entity = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "ClearDesc",
                Some("has description"),
                None,
                vec![],
            )
            .await
            .unwrap();

        let updated = rt
            .update_entity(
                &tok,
                entity.id,
                EntityPatch {
                    description: Some(None),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert!(
            updated.description.is_none(),
            "description should be cleared"
        );
    }

    #[tokio::test]
    async fn update_entity_reindexes_when_name_changes() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let entity = rt
            .create_entity(&tok, "concept", None, "OldName", None, None, vec![])
            .await
            .unwrap();

        // Old name is findable.
        let hits_before = fts_hit(&rt, &tok, "OldName").await;
        assert!(
            hits_before.contains(&entity.id),
            "entity should be findable by old name"
        );

        rt.update_entity(
            &tok,
            entity.id,
            EntityPatch {
                name: Some("NewName".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let hits_old = fts_hit(&rt, &tok, "OldName").await;
        let hits_new = fts_hit(&rt, &tok, "NewName").await;

        // After rename, old name no longer matches this entity (FTS index updated).
        assert!(
            !hits_old.contains(&entity.id),
            "old name should no longer match after rename"
        );
        assert!(
            hits_new.contains(&entity.id),
            "new name should be findable after rename"
        );
    }

    #[tokio::test]
    async fn update_entity_properties_merges_preserving_existing_keys() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let entity = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "MergeProps",
                None,
                Some(serde_json::json!({
                    "domain": "inference",
                    "repo": "lattice",
                    "status": "researched",
                })),
                vec![],
            )
            .await
            .unwrap();

        let updated = rt
            .update_entity(
                &tok,
                entity.id,
                EntityPatch {
                    properties: Some(serde_json::json!({"status": "implemented"})),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let props = updated.properties.expect("properties should remain set");
        assert_eq!(props["domain"], "inference", "domain key must be preserved");
        assert_eq!(props["repo"], "lattice", "repo key must be preserved");
        assert_eq!(
            props["status"], "implemented",
            "status key must be updated by patch"
        );
    }

    #[tokio::test]
    async fn update_entity_skips_reindex_when_only_properties_change() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let entity = rt
            .create_entity(&tok, "concept", None, "StableIndexed", None, None, vec![])
            .await
            .unwrap();

        // Verify it's in the index before.
        let hits_before = fts_hit(&rt, &tok, "StableIndexed").await;
        assert!(hits_before.contains(&entity.id));

        // Only patch properties — text index should be untouched (still findable).
        rt.update_entity(
            &tok,
            entity.id,
            EntityPatch {
                properties: Some(serde_json::json!({"new": "prop"})),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let hits_after = fts_hit(&rt, &tok, "StableIndexed").await;
        assert!(
            hits_after.contains(&entity.id),
            "still findable after props-only patch"
        );
    }

    #[tokio::test]
    async fn merge_entity_rewires_edges() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let c = rt
            .create_entity(&tok, "concept", None, "C", None, None, vec![])
            .await
            .unwrap();
        let d = rt
            .create_entity(&tok, "concept", None, "D", None, None, vec![])
            .await
            .unwrap();

        // A→B and C→B; merge B into D → should become A→D and C→D.
        rt.link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        rt.link(&tok, c.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();

        let summary = rt
            .merge_entity(&tok, d.id, b.id, EntityDedupMergePolicy::PreferInto, false)
            .await
            .unwrap();

        assert_eq!(summary.kept_id, d.id);
        assert_eq!(summary.removed_id, b.id);
        assert_eq!(summary.edges_rewired, 2);

        // Verify edges now point to D.
        let a_neighbors = rt
            .neighbors(&tok, a.id, Direction::Out, None, None)
            .await
            .unwrap();
        assert_eq!(a_neighbors.len(), 1);
        assert_eq!(a_neighbors[0].node_id, d.id);

        let c_neighbors = rt
            .neighbors(&tok, c.id, Direction::Out, None, None)
            .await
            .unwrap();
        assert_eq!(c_neighbors.len(), 1);
        assert_eq!(c_neighbors[0].node_id, d.id);
    }

    #[tokio::test]
    async fn merge_entity_self_merge_rejected() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let err = rt
            .merge_entity(&tok, a.id, a.id, EntityDedupMergePolicy::PreferInto, false)
            .await
            .unwrap_err();
        assert!(
            format!("{err:?}").contains("cannot merge an entity into itself"),
            "expected self-merge rejection, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn merge_entity_prefer_into_strategy() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "Into",
                None,
                Some(serde_json::json!({"a": 1})),
                vec![],
            )
            .await
            .unwrap();
        let from = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "From",
                None,
                Some(serde_json::json!({"a": 2, "b": 3})),
                vec![],
            )
            .await
            .unwrap();

        rt.merge_entity(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            false,
        )
        .await
        .unwrap();

        let kept = rt.get_entity(&tok, into.id).await.unwrap();
        let props = kept.properties.unwrap();
        // a stays as 1 (into wins), b is added from from.
        assert_eq!(props["a"], 1);
        assert_eq!(props["b"], 3);
    }

    #[tokio::test]
    async fn merge_entity_prefer_from_strategy() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "Into",
                None,
                Some(serde_json::json!({"a": 1})),
                vec![],
            )
            .await
            .unwrap();
        let from = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "From",
                None,
                Some(serde_json::json!({"a": 2, "b": 3})),
                vec![],
            )
            .await
            .unwrap();

        rt.merge_entity(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferFrom,
            false,
        )
        .await
        .unwrap();

        let kept = rt.get_entity(&tok, into.id).await.unwrap();
        let props = kept.properties.unwrap();
        // from wins on a, b also from from.
        assert_eq!(props["a"], 2);
        assert_eq!(props["b"], 3);
    }

    #[tokio::test]
    async fn merge_entity_union_strategy() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "Into",
                None,
                Some(serde_json::json!({"a": 1})),
                vec![],
            )
            .await
            .unwrap();
        let from = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "From",
                None,
                Some(serde_json::json!({"a": 2, "b": 3})),
                vec![],
            )
            .await
            .unwrap();

        rt.merge_entity(&tok, into.id, from.id, EntityDedupMergePolicy::Union, false)
            .await
            .unwrap();

        let kept = rt.get_entity(&tok, into.id).await.unwrap();
        let props = kept.properties.unwrap();
        // Scalar conflict: into wins → a=1. b added from from.
        assert_eq!(props["a"], 1);
        assert_eq!(props["b"], 3);
    }

    #[tokio::test]
    async fn merge_entity_unions_tags() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "Into",
                None,
                None,
                vec!["x".to_string(), "y".to_string()],
            )
            .await
            .unwrap();
        let from = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "From",
                None,
                None,
                vec!["y".to_string(), "z".to_string()],
            )
            .await
            .unwrap();

        rt.merge_entity(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            false,
        )
        .await
        .unwrap();

        let kept = rt.get_entity(&tok, into.id).await.unwrap();
        let mut tags = kept.tags.clone();
        tags.sort();
        assert_eq!(tags, vec!["x", "y", "z"]);
    }

    #[tokio::test]
    async fn merge_entity_drops_self_loops() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();

        // A `extends` B — merging B into A would produce A `extends` A → drop it.
        rt.link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();

        let summary = rt
            .merge_entity(&tok, a.id, b.id, EntityDedupMergePolicy::PreferInto, false)
            .await
            .unwrap();

        assert_eq!(
            summary.edges_rewired, 0,
            "self-loop should be dropped, not rewired"
        );

        let a_out = rt
            .neighbors(&tok, a.id, Direction::Out, None, None)
            .await
            .unwrap();
        assert!(a_out.is_empty(), "no self-loop should remain");
    }

    // ---- merge helper unit tests ----

    #[test]
    fn union_tags_deduplicates() {
        let (tags, added) = union_tags(
            &["x".to_string(), "y".to_string()],
            &["y".to_string(), "z".to_string()],
        );
        let mut sorted = tags.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["x", "y", "z"]);
        assert_eq!(added, 1);
    }

    #[test]
    fn merge_properties_prefer_into_fills_missing_keys() {
        let a = serde_json::json!({"a": 1});
        let b = serde_json::json!({"a": 99, "b": 2});
        let (merged, added) =
            merge_properties(&Some(a), &Some(b), EntityDedupMergePolicy::PreferInto);
        let m = merged.unwrap();
        assert_eq!(m["a"], 1);
        assert_eq!(m["b"], 2);
        assert_eq!(added, 1);
    }

    // ---- tombstone and note merge tests ----

    #[tokio::test]
    async fn merge_entity_tombstones_source_with_provenance() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_entity(&tok, "concept", None, "Into", None, None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_entity(&tok, "concept", None, "From", None, None, vec![])
            .await
            .unwrap();
        let from_id = from.id;

        rt.merge_entity(
            &tok,
            into.id,
            from_id,
            EntityDedupMergePolicy::PreferInto,
            false,
        )
        .await
        .unwrap();

        // After merge, get_entity returns an error (soft-deleted rows are excluded).
        assert!(
            rt.get_entity(&tok, from_id).await.is_err(),
            "tombstoned source should not be returned by get_entity"
        );

        // Verify the source row still exists in SQL with provenance.
        let pool = rt.backend().pool_arc();
        let (deleted_at, merged_into): (Option<i64>, Option<String>) =
            tokio::task::spawn_blocking(move || {
                let guard = pool.writer().unwrap();
                guard
                    .conn()
                    .query_row(
                        "SELECT deleted_at, merged_into FROM entities WHERE id = ?1",
                        [from_id.to_string()],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .unwrap()
            })
            .await
            .unwrap();
        assert!(
            deleted_at.is_some(),
            "tombstoned entity must have deleted_at set"
        );
        assert_eq!(
            merged_into.as_deref(),
            Some(into.id.to_string().as_str()),
            "merged_into must point to into_id"
        );
    }

    #[tokio::test]
    async fn merge_note_same_kind_appends_content() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_note(
                &tok,
                "observation",
                None,
                "Into content",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        let from = rt
            .create_note(
                &tok,
                "observation",
                None,
                "From content",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        let from_id = from.id;

        let summary = rt
            .merge_note(
                &tok,
                into.id,
                from_id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
            )
            .await
            .unwrap();

        assert_eq!(summary.kept_id, into.id);
        assert_eq!(summary.removed_id, from_id);
        assert!(summary.content_appended);
        assert!(!summary.dry_run);

        // Source is no longer findable.
        let from_store = rt.notes(&tok).unwrap();
        assert!(
            from_store.get_note(from_id).await.unwrap().is_none(),
            "merged-from note should be soft-deleted"
        );
    }

    #[tokio::test]
    async fn merge_note_different_kinds_rejected() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_note(&tok, "observation", None, "Into", None, None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_note(&tok, "decision", None, "From", None, None, vec![])
            .await
            .unwrap();

        let result = rt
            .merge_note(
                &tok,
                into.id,
                from.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
            )
            .await;
        assert!(result.is_err(), "merging different note kinds must fail");
    }

    #[tokio::test]
    async fn merge_note_dry_run_leaves_notes_unchanged() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_note(
                &tok,
                "observation",
                None,
                "Into content",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        let from = rt
            .create_note(
                &tok,
                "observation",
                None,
                "From content",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        let into_id = into.id;
        let from_id = from.id;

        let summary = rt
            .merge_note(
                &tok,
                into_id,
                from_id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                true,
            )
            .await
            .unwrap();

        assert!(summary.dry_run);

        // Both notes still exist unchanged.
        let store = rt.notes(&tok).unwrap();
        let into_after = store.get_note(into_id).await.unwrap().unwrap();
        let from_after = store.get_note(from_id).await.unwrap().unwrap();
        assert_eq!(
            into_after.content, "Into content",
            "dry_run must not mutate into-note"
        );
        assert_eq!(
            from_after.content, "From content",
            "dry_run must not mutate from-note"
        );
    }

    // Regression: merge two NAMELESS notes with no embedding model configured.
    // Before this fix, the raw SQL FTS INSERT bound &merged_name directly — for a
    // nameless note that is SQL NULL, while Fts5TextSearch::upsert_document stores
    // an empty string.  The mismatch caused get_document to diverge (or fail) for
    // nameless merged notes.  After the fix, note_fts_scalars drives every scalar
    // and the round-trip is field-identical.
    #[tokio::test]
    async fn merge_nameless_notes_fts_document_is_parity_correct() {
        use khive_storage::types::TextSearchRequest;

        let rt = rt(); // in-memory runtime — no embedding model configured
        let tok = NamespaceToken::local();

        let into = rt
            .create_note(
                &tok,
                "observation",
                None,
                "intosentinelzxq body",
                None,
                Some(serde_json::json!({"src": "into"})),
                vec![],
            )
            .await
            .expect("create into-note");
        let from = rt
            .create_note(
                &tok,
                "observation",
                None,
                "fromsentinelzxq body",
                None,
                None,
                vec![],
            )
            .await
            .expect("create from-note");

        let into_id = into.id;
        let from_id = from.id;

        rt.merge_note(
            &tok,
            into_id,
            from_id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        )
        .await
        .expect("merge_note must succeed");

        // Fetch the merged note from the note store to get its current state.
        let note_store = rt.notes(&tok).expect("note store");
        let merged_note = note_store
            .get_note(into_id)
            .await
            .expect("get_note")
            .expect("merged note must exist");

        // Compute the expected FTS document via the shared constructor.
        let expected = note_fts_document(&merged_note);

        // Fetch the stored FTS document and verify field parity.
        let fts = rt.text_for_notes(&tok).expect("FTS store");
        let stored = fts
            .get_document("local", into_id)
            .await
            .expect("get_document must not error")
            .expect("FTS document must exist after merge");

        // Core parity: subject_id, title (must be None, not Some("")), body.
        assert_eq!(stored.subject_id, expected.subject_id, "subject_id");
        assert_eq!(
            stored.title, expected.title,
            "title (None for nameless note)"
        );
        assert_eq!(stored.body, expected.body, "body");
        assert_eq!(stored.namespace, expected.namespace, "namespace");
        assert_eq!(stored.kind, expected.kind, "kind");

        // The merged note is nameless — title must be None, matching the shared path.
        assert!(
            stored.title.is_none(),
            "nameless merged note must have title=None in FTS (was NULL before fix)"
        );

        // The merged note must be searchable by a unique token from the into-note body.
        let hits = fts
            .search(TextSearchRequest {
                query: "intosentinelzxq".to_string(),
                mode: khive_storage::types::TextQueryMode::Plain,
                filter: None,
                top_k: 10,
                snippet_chars: 0,
            })
            .await
            .expect("search");
        assert!(
            hits.iter().any(|h| h.subject_id == into_id),
            "merged note must be searchable by into-note content"
        );
    }

    #[tokio::test]
    async fn update_edge_updates_properties() {
        use khive_storage::EdgeRelation;
        let rt = rt();
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let edge = rt
            .link(&tok, a.id, b.id, EdgeRelation::Extends, 0.5, None)
            .await
            .unwrap();
        let edge_id: Uuid = edge.id.into();

        let updated = rt
            .update_edge(
                &tok,
                edge_id,
                EdgePatch {
                    properties: Some(serde_json::json!({"source": "manual"})),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(updated.metadata.as_ref().unwrap()["source"], "manual");
        assert!((updated.weight - 0.5).abs() < 0.001, "weight unchanged");
    }

    // scenario-kg-maintenance C1 regression: merge must not crash when both
    // entities share a common third-party edge (duplicate triple after rewire).
    // Before the fix, the double-ON-CONFLICT INSERT raised a UNIQUE constraint
    // error at the SQLite layer and the merge aborted mid-transaction.
    #[tokio::test]
    async fn merge_entity_survives_shared_edge_to_third_party() {
        use khive_storage::EdgeRelation;
        let rt = rt();
        let tok = NamespaceToken::local();

        // Create three entities: A and B will be merged; shared is the common target.
        // Use `extends` (concept→concept) which is a valid endpoint combination.
        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let shared = rt
            .create_entity(&tok, "concept", None, "Shared", None, None, vec![])
            .await
            .unwrap();

        // Both A and B extend the same shared concept — this creates a duplicate
        // triple (A/B → shared, extends) that triggers the crash on rewire.
        rt.link(&tok, a.id, shared.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        rt.link(&tok, b.id, shared.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();

        // Before the fix this would return Err with "UNIQUE constraint failed".
        let summary = rt
            .merge_entity(
                &tok,
                a.id,
                b.id,
                crate::EntityDedupMergePolicy::PreferInto,
                false,
            )
            .await
            .expect(
                "C1: merge must succeed even when both entities share an edge to a third party",
            );

        assert_eq!(summary.kept_id, a.id);
        assert_eq!(summary.removed_id, b.id);
        // A already had the Extends edge to shared; when B→shared is rewired to
        // A→shared, the ON CONFLICT DO UPDATE refreshes the existing row (clears
        // deleted_at, updates weight). rusqlite reports this as 1 change, so
        // edges_rewired will be >= 0. The important invariant is that the merge
        // did NOT crash and exactly one live edge A→shared remains.

        // One live edge A→shared must exist after merge.
        let a_edges = rt
            .list_edges(
                &tok,
                crate::EdgeListFilter {
                    source_id: Some(a.id),
                    target_id: Some(shared.id),
                    relations: vec![EdgeRelation::Extends],
                    ..Default::default()
                },
                10,
            )
            .await
            .unwrap();
        assert_eq!(
            a_edges.len(),
            1,
            "C1: exactly one live A→shared Extends edge must exist after merge; got: {a_edges:?}"
        );

        // Tombstone check: B must be soft-deleted after successful merge (C3).
        // get_entity filters deleted_at IS NULL, so a tombstoned entity returns None.
        let b_after = rt.entities(&tok).unwrap().get_entity(b.id).await.unwrap();
        assert!(
            b_after.is_none(),
            "C3: from_entity must be tombstoned (get_entity returns None for deleted) after merge; got: {b_after:?}"
        );
    }

    // H2 regression: merge_entity at the runtime level must reject cross-kind merges.
    // Before the H2 fix, only the pack handler had this guard; a direct runtime caller
    // could still merge concept+project, silently tombstoning the source entity.
    #[tokio::test]
    async fn merge_entity_cross_kind_rejected_at_runtime() {
        let rt = rt();
        let tok = NamespaceToken::local();

        let concept = rt
            .create_entity(&tok, "concept", None, "H2Concept", None, None, vec![])
            .await
            .unwrap();
        let project = rt
            .create_entity(&tok, "project", None, "H2Project", None, None, vec![])
            .await
            .unwrap();

        // Cross-kind merge must return InvalidInput at the runtime level.
        let err = rt
            .merge_entity(
                &tok,
                concept.id,
                project.id,
                crate::EntityDedupMergePolicy::PreferInto,
                false,
            )
            .await
            .expect_err("H2: cross-kind merge must be rejected by runtime");
        assert!(
            matches!(err, crate::RuntimeError::InvalidInput(_)),
            "H2: expected InvalidInput, got: {err:?}"
        );

        // Both entities must survive the failed merge attempt with no tombstone.
        let concept_after = rt.get_entity(&tok, concept.id).await;
        let project_after = rt.get_entity(&tok, project.id).await;
        assert!(
            concept_after.is_ok(),
            "H2: concept must remain live after rejected merge; got: {concept_after:?}"
        );
        assert!(
            project_after.is_ok(),
            "H2: project must remain live after rejected merge; got: {project_after:?}"
        );
    }

    // scenario-kg-maintenance C2 regression: same-kind merge must succeed.
    #[tokio::test]
    async fn merge_entity_same_kind_succeeds() {
        let rt = rt();
        let tok = NamespaceToken::local();

        let c1 = rt
            .create_entity(&tok, "concept", None, "Concept1", None, None, vec![])
            .await
            .unwrap();
        let c2 = rt
            .create_entity(&tok, "concept", None, "Concept2", None, None, vec![])
            .await
            .unwrap();

        let summary = rt
            .merge_entity(
                &tok,
                c1.id,
                c2.id,
                crate::EntityDedupMergePolicy::PreferInto,
                false,
            )
            .await
            .expect("same-kind merge must succeed");
        assert_eq!(summary.kept_id, c1.id);
        assert_eq!(summary.removed_id, c2.id);

        // c2 must be tombstoned.
        let c2_after = rt.entities(&tok).unwrap().get_entity(c2.id).await.unwrap();
        assert!(c2_after.is_none(), "from_entity must be tombstoned");
    }

    // ── #567 regression: cross-namespace merge_note must be denied on either ID ──

    #[tokio::test]
    async fn merge_note_cross_namespace_either_id_returns_not_found() {
        use crate::error::RuntimeError;
        use crate::Namespace;

        let rt = rt();
        let ns_a = NamespaceToken::for_namespace(Namespace::parse("ns-a").unwrap());
        let ns_b = NamespaceToken::for_namespace(Namespace::parse("ns-b").unwrap());

        let into_a = rt
            .create_note(&ns_a, "observation", None, "Into A", None, None, vec![])
            .await
            .unwrap();
        let from_a = rt
            .create_note(&ns_a, "observation", None, "From A", None, None, vec![])
            .await
            .unwrap();
        let note_b = rt
            .create_note(&ns_b, "observation", None, "Note B", None, None, vec![])
            .await
            .unwrap();

        // foreign into_id: note_b belongs to ns_b, caller token is ns_a
        let foreign_into = rt
            .merge_note(
                &ns_a,
                note_b.id,
                from_a.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
            )
            .await;
        assert!(
            matches!(foreign_into, Err(RuntimeError::NotFound(_))),
            "foreign into_id must be denied before merge, got {foreign_into:?}"
        );

        // foreign from_id: note_b belongs to ns_b, caller token is ns_a
        let foreign_from = rt
            .merge_note(
                &ns_a,
                into_a.id,
                note_b.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
            )
            .await;
        assert!(
            matches!(foreign_from, Err(RuntimeError::NotFound(_))),
            "foreign from_id must be denied before merge, got {foreign_from:?}"
        );
    }

    // ADR-007 PR-A1: cross-namespace update now succeeds (shared-brain model).

    #[tokio::test]
    async fn update_entity_cross_namespace_succeeds() {
        use crate::Namespace;

        let rt = rt();
        let ns_a = NamespaceToken::for_namespace(Namespace::parse("ns-a").unwrap());
        let ns_b = NamespaceToken::for_namespace(Namespace::parse("ns-b").unwrap());

        let entity = rt
            .create_entity(
                &ns_a,
                "concept",
                None,
                "Alpha",
                Some("original"),
                None,
                vec![],
            )
            .await
            .unwrap();

        // ADR-007 PR-A1: update from ns_b token must now succeed on an ns_a entity.
        let result = rt
            .update_entity(
                &ns_b,
                entity.id,
                EntityPatch {
                    name: Some("Updated".into()),
                    ..Default::default()
                },
            )
            .await;

        assert!(
            result.is_ok(),
            "cross-namespace update must succeed in shared-brain OSS; got {result:?}"
        );
        assert_eq!(result.unwrap().name, "Updated");
    }

    // merge_entity still requires both entities to be in the same namespace as
    // the token's write namespace (enforced at the SQL transaction layer, not the
    // runtime layer).  This is a merge-semantic constraint, not tenant isolation.
    #[tokio::test]
    async fn merge_entity_cross_namespace_ids_fail_at_sql_layer() {
        use crate::Namespace;

        let rt = rt();
        let ns_a = NamespaceToken::for_namespace(Namespace::parse("ns-a").unwrap());
        let ns_b = NamespaceToken::for_namespace(Namespace::parse("ns-b").unwrap());

        let into_a = rt
            .create_entity(&ns_a, "concept", None, "Into A", None, None, vec![])
            .await
            .unwrap();
        let from_a = rt
            .create_entity(&ns_a, "concept", None, "From A", None, None, vec![])
            .await
            .unwrap();
        let foreign_b = rt
            .create_entity(&ns_b, "concept", None, "Foreign B", None, None, vec![])
            .await
            .unwrap();

        // foreign into_id: SQL read_merge_entity checks ns matches token namespace.
        let foreign_into = rt
            .merge_entity(
                &ns_a,
                foreign_b.id,
                from_a.id,
                EntityDedupMergePolicy::PreferInto,
                false,
            )
            .await;
        assert!(
            foreign_into.is_err(),
            "cross-namespace into_id must still fail at SQL layer; got {foreign_into:?}"
        );

        // foreign from_id: same SQL constraint.
        let foreign_from = rt
            .merge_entity(
                &ns_a,
                into_a.id,
                foreign_b.id,
                EntityDedupMergePolicy::PreferInto,
                false,
            )
            .await;
        assert!(
            foreign_from.is_err(),
            "cross-namespace from_id must still fail at SQL layer; got {foreign_from:?}"
        );

        // All three entities survive the failed merges.
        assert!(rt.get_entity(&ns_a, into_a.id).await.is_ok());
        assert!(rt.get_entity(&ns_a, from_a.id).await.is_ok());
        assert!(rt.get_entity(&ns_b, foreign_b.id).await.is_ok());
    }

    // Parity: entity_fts_document must produce the same body/title as the
    // create_entity and update_entity FTS write paths.
    #[test]
    fn entity_fts_document_with_description() {
        let mut entity = Entity::new("local", "concept", "MyEntity");
        entity = entity.with_description("some description text");
        let doc = entity_fts_document(&entity);
        assert_eq!(doc.subject_id, entity.id);
        assert_eq!(doc.namespace, "local");
        assert_eq!(doc.title.as_deref(), Some("MyEntity"));
        assert_eq!(doc.body, "MyEntity some description text");
        assert_eq!(doc.kind, khive_types::SubstrateKind::Entity);
    }

    #[test]
    fn entity_fts_document_without_description() {
        let entity = Entity::new("local", "concept", "NameOnly");
        let doc = entity_fts_document(&entity);
        assert_eq!(doc.title.as_deref(), Some("NameOnly"));
        assert_eq!(doc.body, "NameOnly");
    }

    #[test]
    fn entity_fts_document_empty_description_uses_name_only() {
        let mut entity = Entity::new("local", "concept", "TitleOnly");
        entity = entity.with_description("");
        let doc = entity_fts_document(&entity);
        assert_eq!(
            doc.body, "TitleOnly",
            "empty description must not be appended"
        );
    }

    // Cross-path equality: an entity created through the runtime (operations.rs
    // create_entity path) must produce a stored FTS document field-identical to
    // entity_fts_document() called on the same Entity.
    #[tokio::test]
    async fn entity_fts_document_matches_runtime_create_path() {
        let rt = rt();
        let tok = NamespaceToken::local();

        let entity = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "CrossPathTitle",
                Some("cross path description body"),
                Some(serde_json::json!({"key": "val"})),
                vec!["tag1".to_string()],
            )
            .await
            .expect("create_entity");

        let fts = rt.text(&tok).expect("FTS store");
        let stored = fts
            .get_document("local", entity.id)
            .await
            .expect("get_document")
            .expect("document must exist after create_entity");

        let expected = entity_fts_document(&entity);

        assert_eq!(stored.subject_id, expected.subject_id, "subject_id");
        assert_eq!(stored.kind, expected.kind, "kind");
        assert_eq!(stored.title, expected.title, "title");
        assert_eq!(stored.body, expected.body, "body");
        assert_eq!(stored.namespace, expected.namespace, "namespace");
    }

    // Cross-path equality: update_entity must produce a stored FTS document
    // field-identical to entity_fts_document() on the updated Entity.
    #[tokio::test]
    async fn entity_fts_document_matches_runtime_update_path() {
        let rt = rt();
        let tok = NamespaceToken::local();

        let entity = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "OldName",
                Some("old desc"),
                None,
                vec![],
            )
            .await
            .expect("create_entity");

        let updated = rt
            .update_entity(
                &tok,
                entity.id,
                EntityPatch {
                    name: Some("NewName".to_string()),
                    description: Some(Some("new desc".to_string())),
                    ..Default::default()
                },
            )
            .await
            .expect("update_entity");

        let fts = rt.text(&tok).expect("FTS store");
        let stored = fts
            .get_document("local", updated.id)
            .await
            .expect("get_document")
            .expect("document must exist after update_entity");

        let expected = entity_fts_document(&updated);

        assert_eq!(stored.title, expected.title, "title after update");
        assert_eq!(stored.body, expected.body, "body after update");
    }

    // ── Fix #135 regression tests ────────────────────────────────────────────
    //
    // Verify that merge_entity / merge_note delete from_id vectors from ALL
    // registered model vec tables, not just the default-model table.
    //
    // Uses the same ConstVecProvider/ConstVecService pattern as operations.rs
    // so no real model files are required.

    struct MergeTestVecService {
        dims: usize,
    }

    #[async_trait::async_trait]
    impl lattice_embed::EmbeddingService for MergeTestVecService {
        async fn embed(
            &self,
            texts: &[String],
            _model: lattice_embed::EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
            Ok(texts.iter().map(|_| vec![1.0_f32; self.dims]).collect())
        }

        fn supports_model(&self, _model: lattice_embed::EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "merge-test-const-vec"
        }
    }

    struct MergeTestVecProvider {
        provider_name: String,
        dims: usize,
    }

    impl MergeTestVecProvider {
        fn new(name: &str, dims: usize) -> Self {
            Self {
                provider_name: name.to_owned(),
                dims,
            }
        }
    }

    #[async_trait::async_trait]
    impl crate::embedder_registry::EmbedderProvider for MergeTestVecProvider {
        fn name(&self) -> &str {
            &self.provider_name
        }

        fn dimensions(&self) -> usize {
            self.dims
        }

        async fn build(
            &self,
        ) -> crate::error::RuntimeResult<std::sync::Arc<dyn lattice_embed::EmbeddingService>>
        {
            Ok(std::sync::Arc::new(MergeTestVecService { dims: self.dims }))
        }
    }

    /// merge_entity must delete from_id vectors from ALL registered model tables.
    ///
    /// Two custom embedders ("merge-vec-a", "merge-vec-b") are registered.  Both
    /// entities are embedded so each has a row in both model tables.  After merge,
    /// from_id must have zero surviving rows in either table.
    #[tokio::test]
    async fn merge_entity_clears_vectors_from_all_registered_models() {
        const DIMS: usize = 4;
        let rt = KhiveRuntime::memory().unwrap();
        rt.register_embedder(MergeTestVecProvider::new("merge-vec-a", DIMS));
        rt.register_embedder(MergeTestVecProvider::new("merge-vec-b", DIMS));

        let ns_str = "merge-entity-vec-cleanup";
        let ns = crate::Namespace::parse(ns_str).unwrap();
        let tok = NamespaceToken::for_namespace(ns);

        let into_e = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "IntoVecEntity",
                Some("desc a"),
                None,
                vec![],
            )
            .await
            .expect("create into");
        let from_e = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "FromVecEntity",
                Some("desc b"),
                None,
                vec![],
            )
            .await
            .expect("create from");

        // Confirm both entities have vectors in both model tables before merge.
        let vs_a = rt.vectors_for_model(&tok, "merge-vec-a").unwrap();
        let vs_b = rt.vectors_for_model(&tok, "merge-vec-b").unwrap();
        use khive_storage::types::VectorSearchRequest;
        let query = vec![1.0_f32; DIMS];
        let pre_a = vs_a
            .search(VectorSearchRequest {
                query_vectors: vec![query.clone()],
                top_k: 100,
                namespace: Some(ns_str.to_string()),
                kind: Some(khive_types::SubstrateKind::Entity),
                embedding_model: Some("merge-vec-a".to_string()),
                filter: None,
                backend_hints: None,
            })
            .await
            .unwrap();
        assert!(
            pre_a.iter().any(|h| h.subject_id == into_e.id)
                && pre_a.iter().any(|h| h.subject_id == from_e.id),
            "both entities must be in model-a before merge; got {pre_a:?}"
        );

        // model-b must ALSO hold both entities pre-merge, else the post-merge
        // model-b emptiness check below is vacuous (nothing to delete).
        let pre_b = vs_b
            .search(VectorSearchRequest {
                query_vectors: vec![query.clone()],
                top_k: 100,
                namespace: Some(ns_str.to_string()),
                kind: Some(khive_types::SubstrateKind::Entity),
                embedding_model: Some("merge-vec-b".to_string()),
                filter: None,
                backend_hints: None,
            })
            .await
            .unwrap();
        assert!(
            pre_b.iter().any(|h| h.subject_id == into_e.id)
                && pre_b.iter().any(|h| h.subject_id == from_e.id),
            "both entities must be in model-b before merge; got {pre_b:?}"
        );

        rt.merge_entity(
            &tok,
            into_e.id,
            from_e.id,
            EntityDedupMergePolicy::PreferInto,
            false,
        )
        .await
        .expect("merge_entity");

        // from_id must have ZERO rows in EITHER model table after merge.
        let post_a = vs_a
            .search(VectorSearchRequest {
                query_vectors: vec![query.clone()],
                top_k: 100,
                namespace: Some(ns_str.to_string()),
                kind: Some(khive_types::SubstrateKind::Entity),
                embedding_model: Some("merge-vec-a".to_string()),
                filter: None,
                backend_hints: None,
            })
            .await
            .unwrap();
        let from_ids_a: Vec<_> = post_a
            .iter()
            .filter(|h| h.subject_id == from_e.id)
            .collect();
        assert!(
            from_ids_a.is_empty(),
            "from_id must have no vectors in model-a after merge; got {from_ids_a:?}"
        );

        let post_b = vs_b
            .search(VectorSearchRequest {
                query_vectors: vec![query],
                top_k: 100,
                namespace: Some(ns_str.to_string()),
                kind: Some(khive_types::SubstrateKind::Entity),
                embedding_model: Some("merge-vec-b".to_string()),
                filter: None,
                backend_hints: None,
            })
            .await
            .unwrap();
        let from_ids_b: Vec<_> = post_b
            .iter()
            .filter(|h| h.subject_id == from_e.id)
            .collect();
        assert!(
            from_ids_b.is_empty(),
            "from_id must have no vectors in model-b after merge; got {from_ids_b:?}"
        );
    }

    /// merge_note must delete from_id vectors from ALL registered model tables.
    ///
    /// Two custom embedders ("merge-note-vec-a", "merge-note-vec-b") are registered.
    /// Both notes are embedded so each has a row in both model tables.  After merge,
    /// from_id must have zero surviving rows in either table.
    #[tokio::test]
    async fn merge_note_clears_vectors_from_all_registered_models() {
        const DIMS: usize = 4;
        let rt = KhiveRuntime::memory().unwrap();
        rt.register_embedder(MergeTestVecProvider::new("merge-note-vec-a", DIMS));
        rt.register_embedder(MergeTestVecProvider::new("merge-note-vec-b", DIMS));

        let ns_str = "merge-note-vec-cleanup";
        let ns = crate::Namespace::parse(ns_str).unwrap();
        let tok = NamespaceToken::for_namespace(ns);

        let into_n = rt
            .create_note(
                &tok,
                "observation",
                None,
                "IntoVecNote content",
                None,
                None,
                vec![],
            )
            .await
            .expect("create into note");
        let from_n = rt
            .create_note(
                &tok,
                "observation",
                None,
                "FromVecNote content",
                None,
                None,
                vec![],
            )
            .await
            .expect("create from note");

        let vs_a = rt.vectors_for_model(&tok, "merge-note-vec-a").unwrap();
        let vs_b = rt.vectors_for_model(&tok, "merge-note-vec-b").unwrap();
        use khive_storage::types::VectorSearchRequest;
        let query = vec![1.0_f32; DIMS];

        let pre_a = vs_a
            .search(VectorSearchRequest {
                query_vectors: vec![query.clone()],
                top_k: 100,
                namespace: Some(ns_str.to_string()),
                kind: Some(khive_types::SubstrateKind::Note),
                embedding_model: Some("merge-note-vec-a".to_string()),
                filter: None,
                backend_hints: None,
            })
            .await
            .unwrap();
        assert!(
            pre_a.iter().any(|h| h.subject_id == into_n.id)
                && pre_a.iter().any(|h| h.subject_id == from_n.id),
            "both notes must be in model-a before merge; got {pre_a:?}"
        );

        // model-b must ALSO hold both notes pre-merge, else the post-merge
        // model-b emptiness check below is vacuous (nothing to delete).
        let pre_b = vs_b
            .search(VectorSearchRequest {
                query_vectors: vec![query.clone()],
                top_k: 100,
                namespace: Some(ns_str.to_string()),
                kind: Some(khive_types::SubstrateKind::Note),
                embedding_model: Some("merge-note-vec-b".to_string()),
                filter: None,
                backend_hints: None,
            })
            .await
            .unwrap();
        assert!(
            pre_b.iter().any(|h| h.subject_id == into_n.id)
                && pre_b.iter().any(|h| h.subject_id == from_n.id),
            "both notes must be in model-b before merge; got {pre_b:?}"
        );

        rt.merge_note(
            &tok,
            into_n.id,
            from_n.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::PreferInto,
            false,
        )
        .await
        .expect("merge_note");

        // from_id must have ZERO rows in EITHER model table after merge.
        let post_a = vs_a
            .search(VectorSearchRequest {
                query_vectors: vec![query.clone()],
                top_k: 100,
                namespace: Some(ns_str.to_string()),
                kind: Some(khive_types::SubstrateKind::Note),
                embedding_model: Some("merge-note-vec-a".to_string()),
                filter: None,
                backend_hints: None,
            })
            .await
            .unwrap();
        let from_ids_a: Vec<_> = post_a
            .iter()
            .filter(|h| h.subject_id == from_n.id)
            .collect();
        assert!(
            from_ids_a.is_empty(),
            "from_id must have no vectors in model-a after merge; got {from_ids_a:?}"
        );

        let post_b = vs_b
            .search(VectorSearchRequest {
                query_vectors: vec![query],
                top_k: 100,
                namespace: Some(ns_str.to_string()),
                kind: Some(khive_types::SubstrateKind::Note),
                embedding_model: Some("merge-note-vec-b".to_string()),
                filter: None,
                backend_hints: None,
            })
            .await
            .unwrap();
        let from_ids_b: Vec<_> = post_b
            .iter()
            .filter(|h| h.subject_id == from_n.id)
            .collect();
        assert!(
            from_ids_b.is_empty(),
            "from_id must have no vectors in model-b after merge; got {from_ids_b:?}"
        );
    }

    // Cross-path equality: merge_entity must produce a stored FTS document for
    // the kept entity that is field-identical to entity_fts_document().
    #[tokio::test]
    async fn entity_fts_document_matches_runtime_merge_path() {
        let rt = rt();
        let tok = NamespaceToken::local();

        let into_e = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "IntoEntity",
                Some("into desc"),
                None,
                vec![],
            )
            .await
            .expect("create into");
        let from_e = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "FromEntity",
                Some("from desc"),
                None,
                vec![],
            )
            .await
            .expect("create from");

        let summary = rt
            .merge_entity(
                &tok,
                into_e.id,
                from_e.id,
                EntityDedupMergePolicy::PreferInto,
                false,
            )
            .await
            .expect("merge_entity");

        let kept = rt
            .get_entity(&tok, summary.kept_id)
            .await
            .expect("get kept");

        let fts = rt.text(&tok).expect("FTS store");
        let stored = fts
            .get_document("local", kept.id)
            .await
            .expect("get_document")
            .expect("FTS document must exist for kept entity after merge");

        let expected = entity_fts_document(&kept);

        assert_eq!(stored.title, expected.title, "title after merge");
        assert_eq!(stored.body, expected.body, "body after merge");
    }
}
