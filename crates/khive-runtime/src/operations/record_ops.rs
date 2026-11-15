use khive_storage::note::{Note, NoteFilter, NoteStatusSelector};
use khive_storage::types::{DeleteMode, Page, PageRequest, SeekCursor};
use khive_storage::{Entity, EntityFilter, Event, EventFilter};
use khive_types::KhiveError;
use uuid::Uuid;

#[cfg(doc)]
use super::EntityCreateSpec;
#[cfg(any(test, feature = "fault-injection"))]
use super::{consume_fault, ENTITY_COMPENSATION_FAIL_NS};
use super::{entity_stats_counts, EntityStatsCounts};
use crate::atomic_runner::AtomicOpPlan;
use crate::error::{RuntimeError, RuntimeResult};
use crate::runtime::{KhiveRuntime, NamespaceToken};

/// One note item for [`KhiveRuntime::prepare_bulk_note_plan`], the note
/// analogue of [`EntityCreateSpec`]. The caller has already run the kind's
/// own preparation hook, so owner-kind policy such as the memory pack's
/// creation refusal happens before this point.
#[derive(Clone, Debug)]
pub struct NoteCreateSpec {
    pub kind: String,
    pub name: Option<String>,
    pub content: String,
    pub salience: Option<f64>,
    pub properties: Option<serde_json::Value>,
}

impl KhiveRuntime {
    // ---- Entity operations ----

    pub(super) async fn compensate_entity_create(
        &self,
        token: &NamespaceToken,
        entity_id: Uuid,
        namespace: &str,
        vector_models: &[String],
    ) -> Vec<String> {
        let mut cleanup_errors = Vec::new();

        #[cfg(any(test, feature = "fault-injection"))]
        let entity_delete_injected = consume_fault(&ENTITY_COMPENSATION_FAIL_NS, namespace);
        #[cfg(not(any(test, feature = "fault-injection")))]
        let entity_delete_injected = false;

        if entity_delete_injected {
            cleanup_errors.push("entity row delete: injected compensation failure".to_string());
        } else {
            match self.entities(token) {
                Ok(store) => {
                    if let Err(error) = store.delete_entity(entity_id, DeleteMode::Hard).await {
                        cleanup_errors.push(format!("entity row delete: {error}"));
                    }
                }
                Err(error) => cleanup_errors.push(format!("entity store access: {error}")),
            }
        }

        match self.text(token) {
            Ok(fts) => {
                if let Err(error) = fts.delete_document(namespace, entity_id).await {
                    cleanup_errors.push(format!("FTS document delete: {error}"));
                }
            }
            Err(error) => cleanup_errors.push(format!("FTS store access: {error}")),
        }

        for model_name in vector_models {
            match self.vectors_for_model(token, model_name) {
                Ok(vectors) => {
                    if let Err(error) = vectors.delete(entity_id).await {
                        cleanup_errors
                            .push(format!("vector delete for model {model_name}: {error}"));
                    }
                }
                Err(error) => cleanup_errors.push(format!(
                    "vector store access for model {model_name}: {error}"
                )),
            }
        }

        cleanup_errors
    }

    pub(super) fn entity_create_failure(
        entity_id: Uuid,
        primary: RuntimeError,
        cleanup_errors: Vec<String>,
    ) -> RuntimeError {
        if cleanup_errors.is_empty() {
            primary
        } else {
            RuntimeError::Khive(KhiveError::internal(format!(
                "create_entity indexing failed for record {entity_id}; primary failure: \
                 {primary}; compensation failure(s): {}; partial persistence is possible; \
                 inspect and reconcile this record before retrying",
                cleanup_errors.join("; ")
            )))
        }
    }

    /// Retrieve an entity by ID.
    ///
    /// UUID v4 is globally unique: no namespace filter on by-ID ops.
    ///
    /// Interim identifier-continuity disclosure (precedes the full transitive
    /// redirect chase): a miss is probed once against the tombstone row. If
    /// the id was consumed by `merge(into_id, from_id)` — `merged_into` set —
    /// the `NotFound` message names the kept id so the caller can requery it
    /// directly. Single-level only: it does not chase a chain of merges and
    /// does not return the kept entity in place of the miss. The probe only
    /// runs after the live-row lookup misses, so the happy path pays no
    /// extra query.
    pub async fn get_entity(&self, token: &NamespaceToken, id: Uuid) -> RuntimeResult<Entity> {
        let store = self.entities(token)?;
        if let Some(entity) = store.get_entity(id).await? {
            return Ok(entity);
        }
        if let Some(tombstone) = store.get_entity_including_deleted(id).await? {
            if let Some(kept_id) = tombstone.merged_into {
                return Err(RuntimeError::NotFound(format!(
                    "{id} was merged into {kept_id}; query the kept id"
                )));
            }
        }
        Err(RuntimeError::NotFound(format!("entity {id}")))
    }

    /// Retrieve an entity by ID including soft-deleted rows.
    ///
    /// UUID v4 is globally unique: no namespace filter on by-ID ops.
    pub async fn get_entity_including_deleted(
        &self,
        token: &NamespaceToken,
        id: Uuid,
    ) -> RuntimeResult<Option<Entity>> {
        self.entities(token)?
            .get_entity_including_deleted(id)
            .await
            .map_err(Into::into)
    }

    /// Retrieve a note by ID including soft-deleted rows.
    ///
    /// UUID v4 is globally unique: no namespace filter on by-ID ops.
    pub async fn get_note_including_deleted(
        &self,
        token: &NamespaceToken,
        id: Uuid,
    ) -> RuntimeResult<Option<khive_storage::note::Note>> {
        self.notes(token)?
            .get_note_including_deleted(id)
            .await
            .map_err(Into::into)
    }

    /// Fetch multiple entities by ID, returning only those that exist in the
    /// caller's namespace.  Missing or namespace-mismatched IDs are silently
    /// omitted so that batch lookups don't abort on a single stale reference.
    pub async fn get_entities_by_ids(
        &self,
        token: &NamespaceToken,
        ids: &[Uuid],
    ) -> RuntimeResult<Vec<Entity>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let filter = EntityFilter {
            ids: ids.to_vec(),
            ..Default::default()
        };
        let page = self
            .entities(token)?
            .query_entities(
                token.namespace().as_str(),
                filter,
                PageRequest {
                    offset: 0,
                    limit: ids.len() as u32,
                },
            )
            .await?;
        Ok(page.items)
    }

    /// Like `get_entities_by_ids` but scoped to the token's full visible-namespace
    /// set (`primary ∪ extra_visible`) instead of primary only.
    ///
    /// Graph expansion (`neighbors`, `traverse`) iterates over all visible
    /// namespaces, so enrichment must use the same scope — otherwise neighbors
    /// or path nodes whose entities live in an extra-visible namespace are left
    /// with `name = None`, `kind = None`.  Missing or out-of-scope IDs are
    /// silently omitted (best-effort, same as `get_entities_by_ids`).
    pub(super) async fn get_entities_by_ids_visible(
        &self,
        token: &NamespaceToken,
        ids: &[Uuid],
    ) -> RuntimeResult<Vec<Entity>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let namespaces: Vec<String> = token
            .visible_namespaces()
            .iter()
            .map(|ns| ns.as_str().to_owned())
            .collect();
        let filter = EntityFilter {
            ids: ids.to_vec(),
            namespaces,
            ..Default::default()
        };
        let page = self
            .entities(token)?
            .query_entities(
                token.namespace().as_str(),
                filter,
                PageRequest {
                    offset: 0,
                    limit: ids.len() as u32,
                },
            )
            .await?;
        Ok(page.items)
    }

    /// Enforce that `record_ns` is within the caller's visible namespace set.
    ///
    /// Returns `Err(NotFound)` when the record namespace is not in the visible
    /// set — wrong-namespace and absent UUIDs must be indistinguishable
    /// externally (no existence oracle).
    ///
    /// When the visible set is a single entry equal to `caller_primary_ns`, this
    /// is identical to the former strict-equality check (backward-compatible).
    pub(crate) fn ensure_namespace(record_ns: &str, caller_primary_ns: &str) -> RuntimeResult<()> {
        if record_ns == caller_primary_ns {
            return Ok(());
        }
        Err(RuntimeError::NotFound("not found in this namespace".into()))
    }

    /// Enforce that `record_ns` is a member of the token's visible namespace set.
    ///
    /// This is the multi-namespace-aware variant used when the token carries an
    /// extended visibility set. For single-namespace tokens (visible == [primary])
    /// this degenerates to the same strict-equality check as `ensure_namespace`.
    pub(crate) fn ensure_namespace_visible(
        record_ns: &str,
        token: &NamespaceToken,
    ) -> RuntimeResult<()> {
        for ns in token.visible_namespaces() {
            if record_ns == ns.as_str() {
                return Ok(());
            }
        }
        Err(RuntimeError::NotFound("not found in this namespace".into()))
    }

    /// List entities visible to the token, optionally filtered by kind and entity_type.
    /// A null entity_type falls back to a string properties.type for filtering only.
    ///
    /// When the token carries a multi-namespace visible set, entities from all
    /// visible namespaces are returned. When the visible set is `[primary]`
    /// (the default) this behaves identically to the pre-visibility behaviour.
    pub async fn list_entities(
        &self,
        token: &NamespaceToken,
        kind: Option<&str>,
        entity_type: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> RuntimeResult<Vec<Entity>> {
        let filter = EntityFilter {
            kinds: kind
                .map(|value| vec![value.to_string()])
                .unwrap_or_default(),
            entity_types: entity_type
                .map(|value| vec![value.to_string()])
                .unwrap_or_default(),
            legacy_entity_type_fallback: true,
            ..Default::default()
        };
        self.list_entities_filtered(token, filter, limit, offset)
            .await
    }

    /// Apply a composed entity predicate before offset pagination. Namespace
    /// visibility is supplied by the token, just as for the scalar list API.
    pub async fn list_entities_filtered(
        &self,
        token: &NamespaceToken,
        mut filter: EntityFilter,
        limit: u32,
        offset: u32,
    ) -> RuntimeResult<Vec<Entity>> {
        filter.namespaces = token
            .visible_namespaces()
            .iter()
            .map(|namespace| namespace.as_str().to_owned())
            .collect();
        let page = self
            .entities(token)?
            .query_entities_count_free(
                token.namespace().as_str(),
                filter,
                PageRequest {
                    offset: offset.into(),
                    limit,
                },
            )
            .await?;
        Ok(page.items)
    }

    /// List an immutable insertion-sequence page of visible entities.
    ///
    /// The public cursor remains the UUID of the last returned entity. We
    /// resolve its immutable database-assigned sequence before querying so callers do
    /// not need to serialize storage details. A missing or out-of-scope cursor
    /// fails explicitly instead of silently resuming from the wrong boundary.
    pub async fn list_entities_after(
        &self,
        token: &NamespaceToken,
        kind: Option<&str>,
        entity_type: Option<&str>,
        tags_any: &[String],
        after: Option<Uuid>,
        limit: u32,
    ) -> RuntimeResult<(Vec<Entity>, Option<Uuid>)> {
        let filter = EntityFilter {
            kinds: kind
                .map(|value| vec![value.to_string()])
                .unwrap_or_default(),
            entity_types: entity_type
                .map(|value| vec![value.to_string()])
                .unwrap_or_default(),
            legacy_entity_type_fallback: true,
            tags_any: tags_any.to_vec(),
            ..Default::default()
        };
        self.list_entities_after_filtered(token, filter, after, limit)
            .await
    }

    /// Apply a composed entity predicate before insertion-sequence pagination,
    /// preserving the scalar API's cursor validation and token visibility.
    pub async fn list_entities_after_filtered(
        &self,
        token: &NamespaceToken,
        mut filter: EntityFilter,
        after: Option<Uuid>,
        limit: u32,
    ) -> RuntimeResult<(Vec<Entity>, Option<Uuid>)> {
        let store = self.entities(token)?;
        let after = match after {
            Some(id) => {
                let entity = self
                    .get_entity_including_deleted(token, id)
                    .await?
                    .ok_or_else(|| RuntimeError::NotFound(format!("entity cursor {id}")))?;
                Self::ensure_namespace_visible(&entity.namespace, token)?;
                let sequence = store.entity_sequence(id).await?.ok_or_else(|| {
                    RuntimeError::Internal(format!(
                        "entity cursor {id} has no insertion-sequence ledger row"
                    ))
                })?;
                Some(SeekCursor { sequence, id })
            }
            None => None,
        };
        filter.namespaces = token
            .visible_namespaces()
            .iter()
            .map(|namespace| namespace.as_str().to_owned())
            .collect();
        let page = store
            .query_entities_after(token.namespace().as_str(), filter, after, limit)
            .await?;
        Ok((page.items, page.next_after.map(|cursor| cursor.id)))
    }

    /// List entities filtered by kind, optional domain tag, limit, and offset.
    ///
    /// When `domain_tag` is Some, the query is restricted at the storage layer via
    /// `EntityFilter::tags_any` so the page result already reflects the domain
    /// constraint.  This avoids the silent truncation that occurs when filtering
    /// post-page (K-3). Multi-namespace visibility from the token is applied.
    pub async fn list_entities_tagged(
        &self,
        token: &NamespaceToken,
        kind: Option<&str>,
        domain_tag: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> RuntimeResult<Vec<Entity>> {
        let ns_strs: Vec<String> = token
            .visible_namespaces()
            .iter()
            .map(|ns| ns.as_str().to_owned())
            .collect();
        let filter = EntityFilter {
            kinds: match kind {
                Some(k) => vec![k.to_string()],
                None => vec![],
            },
            tags_any: match domain_tag {
                Some(t) if !t.is_empty() => vec![t.to_string()],
                _ => vec![],
            },
            namespaces: ns_strs,
            ..Default::default()
        };
        let page = self
            .entities(token)?
            .query_entities_count_free(
                token.namespace().as_str(),
                filter,
                PageRequest {
                    offset: offset.into(),
                    limit,
                },
            )
            .await?;
        Ok(page.items)
    }

    /// Count entities filtered by kind and optional domain tag.
    ///
    /// Used to report a meaningful `total` alongside a paginated listing (K-6).
    /// Multi-namespace visibility from the token is applied.
    pub async fn count_entities_tagged(
        &self,
        token: &NamespaceToken,
        kind: Option<&str>,
        domain_tag: Option<&str>,
    ) -> RuntimeResult<u64> {
        let ns_strs: Vec<String> = token
            .visible_namespaces()
            .iter()
            .map(|ns| ns.as_str().to_owned())
            .collect();
        let filter = EntityFilter {
            kinds: match kind {
                Some(k) => vec![k.to_string()],
                None => vec![],
            },
            tags_any: match domain_tag {
                Some(t) if !t.is_empty() => vec![t.to_string()],
                _ => vec![],
            },
            namespaces: ns_strs,
            ..Default::default()
        };
        Ok(self
            .entities(token)?
            .count_entities(token.namespace().as_str(), filter)
            .await?)
    }

    /// List events in the namespace proven by the caller token.
    pub async fn list_events(
        &self,
        token: &NamespaceToken,
        filter: EventFilter,
        page: PageRequest,
    ) -> RuntimeResult<Page<Event>> {
        self.events(token)?
            .query_events(filter, page)
            .await
            .map_err(Into::into)
    }

    /// List active notes visible to the token, optionally filtered by kind.
    ///
    /// When the token carries a multi-namespace visible set, notes from all
    /// visible namespaces are returned. When the visible set is `[primary]`
    /// (the default), reads stay in the primary namespace.
    pub async fn list_notes(
        &self,
        token: &NamespaceToken,
        kind: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> RuntimeResult<Vec<Note>> {
        let visible = token.visible_namespaces();
        if visible.len() == 1 {
            // Keep the primary-only route, applying status before pagination.
            let filter = NoteFilter {
                kind: kind.map(str::to_owned),
                ..Default::default()
            }
            .with_default_note_status(NoteStatusSelector::Active);
            let page = self
                .notes(token)?
                .query_notes_filtered_count_free(
                    token.namespace().as_str(),
                    &filter,
                    PageRequest {
                        offset: offset.into(),
                        limit,
                    },
                )
                .await?;
            return Ok(page.items);
        }
        // Multi-namespace path: use query_notes_filtered with the visible set.
        let ns_strs: Vec<String> = visible.iter().map(|ns| ns.as_str().to_owned()).collect();
        let filter = NoteFilter {
            kind: kind.map(|k| k.to_string()),
            namespaces: ns_strs,
            ..Default::default()
        }
        .with_default_note_status(NoteStatusSelector::Active);
        let page = self
            .notes(token)?
            .query_notes_filtered_count_free(
                token.namespace().as_str(),
                &filter,
                PageRequest {
                    offset: offset.into(),
                    limit,
                },
            )
            .await?;
        Ok(page.items)
    }

    /// List an immutable insertion-sequence page of active visible notes.
    ///
    /// Soft-deleting the prior page's last note does not invalidate the
    /// cursor because the boundary is resolved including tombstones. A hard
    /// deletion makes the cursor unresolvable and returns an explicit error.
    pub async fn list_notes_after(
        &self,
        token: &NamespaceToken,
        kind: Option<&str>,
        after: Option<Uuid>,
        limit: u32,
    ) -> RuntimeResult<(Vec<Note>, Option<Uuid>)> {
        let store = self.notes(token)?;
        let after = match after {
            Some(id) => {
                let note = self
                    .get_note_including_deleted(token, id)
                    .await?
                    .ok_or_else(|| RuntimeError::NotFound(format!("note cursor {id}")))?;
                Self::ensure_namespace_visible(&note.namespace, token)?;
                let sequence = store.note_sequence(id).await?.ok_or_else(|| {
                    RuntimeError::Internal(format!(
                        "note cursor {id} has no insertion-sequence ledger row"
                    ))
                })?;
                Some(SeekCursor { sequence, id })
            }
            None => None,
        };
        let filter = khive_storage::note::NoteFilter {
            kind: kind.map(str::to_string),
            namespaces: token
                .visible_namespaces()
                .iter()
                .map(|namespace| namespace.as_str().to_owned())
                .collect(),
            ..Default::default()
        }
        .with_default_note_status(NoteStatusSelector::Active);
        let page = store
            .query_notes_filtered_after(token.namespace().as_str(), &filter, after, limit)
            .await?;
        Ok((page.items, page.next_after.map(|cursor| cursor.id)))
    }

    /// Count notes matching `kind` across the caller's visible namespaces.
    pub async fn count_notes(
        &self,
        token: &NamespaceToken,
        kind: Option<&str>,
    ) -> RuntimeResult<u64> {
        let namespaces: Vec<String> = token
            .visible_namespaces()
            .iter()
            .map(|namespace| namespace.as_str().to_owned())
            .collect();
        Ok(self
            .notes(token)?
            .count_notes_in_namespaces(&namespaces, kind)
            .await?)
    }

    /// Count entities in a namespace, optionally filtered.
    pub async fn count_entities(
        &self,
        token: &NamespaceToken,
        kind: Option<&str>,
    ) -> RuntimeResult<u64> {
        let ns_strs: Vec<String> = token
            .visible_namespaces()
            .iter()
            .map(|ns| ns.as_str().to_owned())
            .collect();
        let filter = EntityFilter {
            kinds: match kind {
                Some(k) => vec![k.to_string()],
                None => vec![],
            },
            namespaces: ns_strs,
            ..Default::default()
        };
        Ok(self
            .entities(token)?
            .count_entities(token.namespace().as_str(), filter)
            .await?)
    }

    /// Return the coupled entity total and optional type counts for `stats`.
    pub async fn entity_stats_counts(
        &self,
        token: &NamespaceToken,
    ) -> RuntimeResult<EntityStatsCounts> {
        entity_stats_counts(self.entities(token)?.as_ref(), token).await
    }

    /// Validate and prepare one note item for a bulk `create(items=[...])`
    /// write. The note goes through the preparation a singleton note create
    /// uses (`validate_head`, then `prepare_atomic_notes`: kind validation,
    /// owned-identity derivation, secret gate, salience range, row and FTS
    /// statements) with embedding switched off, so the plan writes the row
    /// and its FTS document and no vector. A later `reindex` backfills the
    /// vector, as it does for bulk entities. The caller commits the plan,
    /// alone or joined with its siblings in one `run_atomic_unit` call.
    pub async fn prepare_bulk_note_plan(
        &self,
        token: &NamespaceToken,
        spec: NoteCreateSpec,
    ) -> RuntimeResult<(Note, AtomicOpPlan)> {
        let mut candidate = Note::new(token.namespace().as_str(), &spec.kind, &spec.content);
        candidate.name = spec.name.clone();
        candidate.properties = spec.properties.clone();
        crate::note_write::validate_head(&candidate)?;
        let mut prepared = crate::atomic_message::prepare_atomic_notes(
            self,
            vec![crate::atomic_message::AtomicNoteSpec {
                token,
                id: None,
                kind: &spec.kind,
                name: spec.name.as_deref(),
                content: &spec.content,
                properties: spec.properties,
            }],
            crate::atomic_message::AtomicNoteOptions {
                salience: spec.salience,
                embed: Some(false),
                ..Default::default()
            },
        )
        .await?;
        match (prepared.notes.pop(), prepared.plans.pop()) {
            (Some(note), Some(plan)) if prepared.notes.is_empty() && prepared.plans.is_empty() => {
                Ok((note, plan))
            }
            _ => Err(RuntimeError::Internal(
                "bulk note preparation must yield exactly one note and one plan".into(),
            )),
        }
    }
}
