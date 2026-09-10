//! Atomic multi-note write primitive: commits a set of notes — each with its
//! FTS document and every registered embedding model's vector row — in ONE
//! writer transaction, instead of one `create_note` call per note.
//!
//! Built for `khive-pack-comm`'s `dual_write_message` (outbound + inbound
//! copy of a `comm.send`/`comm.reply`), which previously cost roughly a
//! dozen separate writer acquisitions per send: two `create_note_inner`
//! calls (row + FTS + one vector insert per registered model each) plus a
//! root-send `thread_id` patch. Shaped as `create_notes_atomic(Vec<AtomicNoteSpec>)`
//! rather than a comm-specific pair primitive so other multi-write verbs
//! can share the same preparation. Keyed `memory.remember` appends a required
//! annotation and final key claim before committing its prepared plan.
//!
//! # Embed-first
//!
//! Embedding is slow compute (network/model calls). Every distinct content's
//! embeddings, across every registered model, are computed **before** any
//! transaction opens — the writer is held only for synchronous DML, exactly
//! like the rest of the ADR-099 atomic-unit machinery
//! (`atomic_plan`/`atomic_runner`).
//! This is the same reason `atomic_prepare::prepare_add_note` defers vector
//! indexing to a post-commit `PostCommitEffect::ReindexNote`; the difference
//! here is embeddings are computed *before* commit instead of *after*, so
//! the vector rows land in the SAME atomic unit as the note row and FTS
//! document, and no post-commit reindex is needed at all — every plan built
//! by [`create_notes_atomic`] carries `post_commit: PostCommitEffect::None`.
//!
//! # One writer acquisition
//!
//! Each note becomes its own [`AddNotePlan`]; every spec's plan is applied by
//! ONE [`crate::atomic_runner::run_atomic_unit`] commit pass — one [`khive_storage::SqlAccess::atomic_unit`],
//! one writer checkout, one WAL commit for the whole set. A failure on any
//! note's plan rolls back the ENTIRE unit (`atomic_runner`'s documented
//! guarantee: a later op's failure unwinds even an earlier op's own
//! already-`RELEASE`d `SAVEPOINT`), so a crash or guard failure partway
//! through never leaves an orphan copy — the process-crash gap the two-call
//! version of `dual_write_message` used to document is closed by
//! construction.

use std::collections::HashMap;

use serde_json::Value;
use uuid::Uuid;

use khive_storage::note::Note;
use khive_storage::types::SqlValue;
use khive_storage::{SqlStatement, StorageCapability, StorageError};
use khive_types::SubstrateKind;

use crate::atomic_plan::{AddNotePlan, AffectedRowGuard, PlanStatement, PostCommitEffect};
use crate::atomic_runner::{AtomicOpPlan, AtomicRunOutcome};
use crate::config::NamespaceToken;
use crate::curation::note_fts_document;
use crate::error::{RuntimeError, RuntimeResult};
use crate::runtime::KhiveRuntime;

/// One note to write as part of an atomic set. Mirrors the subset of
/// `create_note_inner`'s parameters `comm.send`/`comm.reply` actually use —
/// no `annotates`, `salience`, `decay_factor`, `embedding_content` override,
/// or explicit `embedding_model` pin. Keyed-memory preparation carries those
/// through internal options without changing the public multi-note spec.
pub struct AtomicNoteSpec<'a> {
    /// Namespace + actor identity this note is written under.
    pub token: &'a NamespaceToken,
    /// Caller-supplied id, when the id must be known before the write (e.g.
    /// a root send's own id becomes the canonical `thread_id` stored in
    /// BOTH copies' properties). `None` generates a fresh id, same as
    /// `Note::new`.
    pub id: Option<Uuid>,
    pub kind: &'a str,
    pub name: Option<&'a str>,
    pub content: &'a str,
    pub properties: Option<Value>,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct AtomicNoteOptions<'a> {
    pub salience: Option<f64>,
    pub decay_factor: Option<f64>,
    pub embedding_model: Option<&'a str>,
    pub embedding_content: Option<&'a str>,
    pub embed: Option<bool>,
    pub key: Option<&'a str>,
    pub fence: Option<&'a crate::note_write::NoteFences>,
}

pub(crate) struct AtomicNoteRequest<'a> {
    pub spec: AtomicNoteSpec<'a>,
    pub options: AtomicNoteOptions<'a>,
}

pub(crate) struct PreparedAtomicNotes {
    pub notes: Vec<Note>,
    pub plans: Vec<AtomicOpPlan>,
    pub embedding_truncation: crate::retrieval::EmbeddingTruncationReport,
}

/// Bit-identical to `khive-db`'s private `f32_slice_as_bytes` (native-endian
/// reinterpretation of the float buffer) but built from safe `to_ne_bytes`
/// calls instead of an unsafe pointer cast — this crate has no visibility
/// into that helper, and correctness here only requires this process's
/// writes and reads agree on layout, which native-endian byte-for-byte
/// concatenation guarantees identically to the raw cast.
fn f32_vec_to_bytes(data: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(data.len() * 4);
    for f in data {
        bytes.extend_from_slice(&f.to_ne_bytes());
    }
    bytes
}

/// Mirrors `khive-db`'s private `non_finite_index` — this crate has no
/// visibility into that helper, so the check is reproduced here to keep the
/// atomic path's embedding validation observably identical to the canonical
/// `create_note_inner` -> `VectorStore::insert` path.
fn non_finite_index(data: &[f32]) -> Option<usize> {
    data.iter().position(|v| !v.is_finite())
}

/// Same error shape (capability, operation label, message) as `khive-db`'s
/// private `non_finite_vector_error("vec_insert", ..)` — the atomic path
/// must reject a non-finite embedding exactly as the raw `VectorStore::insert`
/// DML does, not silently write it.
pub(crate) fn non_finite_vector_error(idx: usize, value: f32) -> RuntimeError {
    RuntimeError::Storage(StorageError::InvalidInput {
        capability: StorageCapability::Vectors,
        operation: "vec_insert".into(),
        message: format!(
            "non-finite value at index {idx}: {value} \
             (NaN/Inf values corrupt distance computations)"
        ),
    })
}

/// A harmless no-op `UPDATE` with a statically zero-row predicate (`WHERE 1
/// = 0`, not a match against a specific id), guarded `exactly(1)` so it
/// always fails its guard inside the atomic unit regardless of which note
/// ids exist in the store — a fixed nil-UUID predicate would pass its guard
/// if a caller ever supplied that id. Spliced in by test-only fault
/// injection (see [`maybe_inject_fts_failure`]/[`maybe_inject_vector_failure`])
/// to prove the whole unit — not just the failing note's own plan — rolls
/// back.
///
/// `fault-injection` is an ordinary Cargo feature (see this crate's
/// `Cargo.toml`); it must never be enabled in a packaged/release build —
/// doing so would compile this fault-injection path into a shipped binary.
#[cfg(any(test, feature = "fault-injection"))]
fn injected_failure_statement(label: &str) -> PlanStatement {
    PlanStatement {
        statement: SqlStatement {
            sql: "UPDATE notes SET updated_at = updated_at WHERE 1 = 0".to_string(),
            params: vec![],
            label: Some(label.to_string()),
        },
        guard: Some(AffectedRowGuard::exactly(1)),
    }
}

#[cfg(any(test, feature = "fault-injection"))]
fn maybe_inject_fts_failure(namespace: &str, label: &str) -> Option<PlanStatement> {
    crate::operations::consume_fts_fail_fault(namespace).then(|| injected_failure_statement(label))
}
#[cfg(not(any(test, feature = "fault-injection")))]
fn maybe_inject_fts_failure(_namespace: &str, _label: &str) -> Option<PlanStatement> {
    None
}

#[cfg(any(test, feature = "fault-injection"))]
fn maybe_inject_vector_failure(namespace: &str, label: &str) -> Option<PlanStatement> {
    crate::operations::consume_vector_fail_fault(namespace)
        .then(|| injected_failure_statement(label))
}
#[cfg(not(any(test, feature = "fault-injection")))]
fn maybe_inject_vector_failure(_namespace: &str, _label: &str) -> Option<PlanStatement> {
    None
}

/// DELETE-then-INSERT-then-log statements for one (note, model) vector row.
/// This static atomic plan cannot branch on an identity-constrained DELETE's
/// affected-row count, so it retains the general delete-log scan used before
/// `khive-db::stores::vectors::replace_vector_row_dml` gained its live-
/// connection fast path. It uses the same plain-[`PlanStatement`] technique
/// as `atomic_prepare::push_index_purge_statements`. Unguarded: same
/// convention as the FTS-insert statement in
/// `atomic_prepare::prepare_add_note` (the row's existence guard is carried by
/// the plan's primary note-row statement, applied first).
#[allow(clippy::too_many_arguments)]
pub(crate) fn vector_insert_statements(
    table: &str,
    namespace: &str,
    subject_id: Uuid,
    field: &str,
    embedding_model: &str,
    embedding: &[f32],
    label_prefix: &str,
) -> Vec<PlanStatement> {
    let subject = subject_id.to_string();
    let kind_str = SubstrateKind::Note.to_string();
    let blob = f32_vec_to_bytes(embedding);
    vec![
        // Delta-log any pre-existing row this REPLACE is about to evict —
        // mirrors `log_vector_deletes`'s "replace" predicate exactly.
        PlanStatement {
            statement: SqlStatement {
                sql: format!(
                    "INSERT INTO ann_write_log \
                     (namespace, embedding_model, kind, field, subject_id, op) \
                     SELECT namespace, embedding_model, kind, field, subject_id, 'delete' \
                     FROM {table} WHERE subject_id = ?1 AND NOT \
                     (namespace = ?2 AND embedding_model = ?3 AND kind = ?4 AND field = ?5)"
                ),
                params: vec![
                    SqlValue::Text(subject.clone()),
                    SqlValue::Text(namespace.to_string()),
                    SqlValue::Text(embedding_model.to_string()),
                    SqlValue::Text(kind_str.clone()),
                    SqlValue::Text(field.to_string()),
                ],
                label: Some(format!("{label_prefix}-log-delete")),
            },
            guard: None,
        },
        PlanStatement {
            statement: SqlStatement {
                sql: format!("DELETE FROM {table} WHERE subject_id = ?1"),
                params: vec![SqlValue::Text(subject.clone())],
                label: Some(format!("{label_prefix}-delete")),
            },
            guard: None,
        },
        PlanStatement {
            statement: SqlStatement {
                sql: format!(
                    "INSERT INTO {table} \
                     (subject_id, namespace, kind, field, embedding_model, embedding) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
                ),
                params: vec![
                    SqlValue::Text(subject.clone()),
                    SqlValue::Text(namespace.to_string()),
                    SqlValue::Text(kind_str.clone()),
                    SqlValue::Text(field.to_string()),
                    SqlValue::Text(embedding_model.to_string()),
                    SqlValue::Blob(blob),
                ],
                label: Some(format!("{label_prefix}-insert")),
            },
            guard: None,
        },
        PlanStatement {
            statement: SqlStatement {
                sql: "INSERT INTO ann_write_log \
                      (namespace, embedding_model, kind, field, subject_id, op) \
                      VALUES (?1, ?2, ?3, ?4, ?5, 'upsert')"
                    .to_string(),
                params: vec![
                    SqlValue::Text(namespace.to_string()),
                    SqlValue::Text(embedding_model.to_string()),
                    SqlValue::Text(kind_str),
                    SqlValue::Text(field.to_string()),
                    SqlValue::Text(subject),
                ],
                label: Some(format!("{label_prefix}-log-upsert")),
            },
            guard: None,
        },
    ]
}

/// Build and commit `specs` as one atomic unit. Returns the persisted
/// [`Note`]s in the same order as `specs`. On any failure — pre-write
/// validation, embedding, or the atomic commit pass itself — NO note, FTS
/// document, or vector row from any spec is left behind (embed failures
/// occur before any write is attempted; commit-pass failures roll back the
/// whole unit per [`crate::atomic_runner::run_atomic_unit`]'s guarantee).
pub async fn create_notes_atomic(
    runtime: &KhiveRuntime,
    specs: Vec<AtomicNoteSpec<'_>>,
) -> RuntimeResult<Vec<Note>> {
    Ok(create_notes_atomic_with_report(runtime, specs).await?.0)
}

/// The truncation-reporting form of [`create_notes_atomic`]. The report keeps
/// logical per-note/model accounting even when identical content shares one
/// provider result, and is returned only when the whole note set commits
/// successfully.
pub async fn create_notes_atomic_with_report(
    runtime: &KhiveRuntime,
    specs: Vec<AtomicNoteSpec<'_>>,
) -> RuntimeResult<(Vec<Note>, crate::retrieval::EmbeddingTruncationReport)> {
    let mut prepared = prepare_atomic_notes(runtime, specs, AtomicNoteOptions::default()).await?;
    match crate::atomic_runner::run_atomic_unit_with_note_versions(
        runtime.sql().as_ref(),
        prepared.plans,
        true,
    )
    .await
    {
        Ok((AtomicRunOutcome::Committed { .. }, versions)) => {
            assert_eq!(
                prepared.notes.len(),
                versions.len(),
                "one revision receipt per prepared note"
            );
            for (note, version) in prepared.notes.iter_mut().zip(versions) {
                note.version = version;
            }
            Ok((prepared.notes, prepared.embedding_truncation))
        }
        Ok((
            AtomicRunOutcome::RolledBack {
                failed_op_index,
                failure,
            },
            _,
        )) => Err(RuntimeError::Internal(format!(
            "atomic multi-note write rolled back at op {failed_op_index}: {failure:?}"
        ))),
        Err(e) => Err(RuntimeError::Storage(e.0)),
    }
}

pub(crate) async fn prepare_atomic_notes(
    runtime: &KhiveRuntime,
    specs: Vec<AtomicNoteSpec<'_>>,
    options: AtomicNoteOptions<'_>,
) -> RuntimeResult<PreparedAtomicNotes> {
    validate_atomic_note_options(&options)?;
    if options.embed != Some(false) {
        if let Some(model) = options.embedding_model {
            runtime.resolve_embedding_model(Some(model))?;
        }
    }
    prepare_atomic_note_requests(
        runtime,
        specs
            .into_iter()
            .map(|spec| AtomicNoteRequest { spec, options })
            .collect(),
    )
    .await
}

fn validate_atomic_note_options(options: &AtomicNoteOptions<'_>) -> RuntimeResult<()> {
    if let Some(value) = options.salience {
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            return Err(RuntimeError::InvalidInput(
                "salience must be a finite value in [0.0, 1.0]".into(),
            ));
        }
    }
    if let Some(value) = options.decay_factor {
        if !value.is_finite() || value < 0.0 {
            return Err(RuntimeError::InvalidInput(
                "decay_factor must be a finite value >= 0.0".into(),
            ));
        }
    }
    Ok(())
}

pub(crate) async fn prepare_atomic_note_requests(
    runtime: &KhiveRuntime,
    requests: Vec<AtomicNoteRequest<'_>>,
) -> RuntimeResult<PreparedAtomicNotes> {
    let mut embed_model_names = Vec::new();
    let mut note_models = Vec::with_capacity(requests.len());
    for request in &requests {
        let options = &request.options;
        validate_atomic_note_options(options)?;
        let models = if options.embed == Some(false) {
            Vec::new()
        } else if let Some(model) = options.embedding_model {
            runtime.resolve_embedding_model(Some(model))?;
            vec![model.to_owned()]
        } else {
            runtime.registered_embedding_model_names()
        };
        let mut indices = Vec::with_capacity(models.len());
        for model in models {
            let index = match embed_model_names.iter().position(|name| name == &model) {
                Some(index) => index,
                None => {
                    embed_model_names.push(model);
                    embed_model_names.len() - 1
                }
            };
            indices.push(index);
        }
        note_models.push(indices);
    }
    // ---- 1. Validate + build Note objects (all pre-write checks, same as
    // create_note_inner, before any embedding or DML is attempted). ----
    let mut notes: Vec<Note> = Vec::with_capacity(requests.len());
    for request in &requests {
        let spec = &request.spec;
        let options = &request.options;
        runtime.validate_note_kind(spec.kind)?;
        // Same owned-identity derivation every other note-write site runs
        // (`operations.rs`'s create funnel, `atomic_prepare::prepare_add_note`):
        // derive from the token BEFORE the secret scan and note construction,
        // so a caller-supplied `from_actor`/`thread_id`/etc. in `spec.properties`
        // never reaches storage verbatim on this writer either.
        let properties =
            runtime.derive_note_write_properties(spec.kind, spec.token, spec.properties.clone())?;
        crate::secret_gate::reject_reserved_secret_gate_property(properties.as_ref())?;
        crate::secret_gate::check(spec.content)?;
        if let Some(n) = spec.name {
            crate::secret_gate::check(n)?;
        }
        if let Some(ref p) = properties {
            crate::secret_gate::check_json(p)?;
        }

        let ns = spec.token.namespace().as_str();
        let mut note = Note::new(ns, spec.kind, spec.content);
        note.key = options.key.map(str::to_owned);
        if let Some(salience) = options.salience {
            note = note.with_salience(salience);
        }
        if let Some(decay_factor) = options.decay_factor {
            note = note.with_decay(decay_factor);
        }
        if let Some(id) = spec.id {
            note.id = id;
        }
        if let Some(n) = spec.name {
            note = note.with_name(n);
        }
        if let Some(p) = properties {
            note = note.with_properties(p);
        }
        notes.push(note);
    }

    // ---- 2. Batch distinct content per model in parallel, BEFORE
    // opening any transaction. Any failure aborts here — no write has been
    // attempted. Identical note siblings reuse the same computed vector. ----
    let mut content_group_by_text: HashMap<&str, usize> = HashMap::new();
    let mut content_groups: Vec<Vec<usize>> = Vec::new();
    let mut note_content_groups: Vec<usize> = Vec::with_capacity(notes.len());
    for (note_idx, note) in notes.iter().enumerate() {
        let text = requests[note_idx]
            .options
            .embedding_content
            .unwrap_or_else(|| crate::curation::note_embedding_text_ref(note));
        let content_group_idx = match content_group_by_text.get(text) {
            Some(&idx) => {
                content_groups[idx].push(note_idx);
                idx
            }
            None => {
                let idx = content_groups.len();
                content_group_by_text.insert(text, idx);
                content_groups.push(vec![note_idx]);
                idx
            }
        };
        note_content_groups.push(content_group_idx);
    }

    // (content_group_idx, model_idx) -> outcome, filled as tasks complete.
    let mut embedding_outcomes: Vec<Vec<Option<crate::retrieval::DocumentEmbeddingOutcome>>> =
        content_groups
            .iter()
            .map(|_| vec![None; embed_model_names.len()])
            .collect();
    let mut embedding_truncation = crate::retrieval::EmbeddingTruncationReport::default();

    // `requests` is empty when a caller prepares a note set every member of
    // which was refused before preparation: there is nothing to embed, no
    // token to read, and the lazy vector-table create belongs to a write
    // that is not going to happen.
    if !embed_model_names.is_empty() {
        // Ensure every model's vector table exists before the commit pass —
        // the same lazy-create side effect as vector-store construction, with
        // all model tables sharing one schema-writer acquisition.
        let models = embed_model_names
            .iter()
            .map(|name| {
                runtime
                    .vector_model_metadata(name)
                    .map(|(name, dimensions)| (crate::config::sanitize_key(&name), dimensions))
            })
            .collect::<RuntimeResult<Vec<_>>>()?;
        let model_specs: Vec<_> = models
            .iter()
            .map(|(key, dimensions)| (key.as_str(), *dimensions))
            .collect();
        runtime.backend().ensure_vector_tables(&model_specs)?;

        let usage_ctx = crate::usage::current();
        let mut join_set = tokio::task::JoinSet::new();
        for (model_idx, model_name) in embed_model_names.iter().enumerate() {
            let mut groups = Vec::new();
            let mut texts = Vec::new();
            let mut token = None;
            for (group_idx, note_indices) in content_groups.iter().enumerate() {
                let Some(&note_idx) = note_indices
                    .iter()
                    .find(|&&index| note_models[index].contains(&model_idx))
                else {
                    continue;
                };
                token.get_or_insert_with(|| requests[note_idx].spec.token.clone());
                groups.push(group_idx);
                texts.push(
                    requests[note_idx]
                        .options
                        .embedding_content
                        .unwrap_or_else(|| {
                            crate::curation::note_embedding_text_ref(&notes[note_idx])
                        })
                        .to_owned(),
                );
            }
            let rt = runtime.clone();
            let token = token.expect("a selected model has at least one note");
            let name = model_name.clone();
            let ctx = usage_ctx.clone();
            join_set.spawn(async move {
                let fut = async {
                    let mut outcomes = Vec::with_capacity(texts.len());
                    for chunk in texts.chunks(lattice_embed::DEFAULT_MAX_BATCH_SIZE) {
                        outcomes.extend(
                            rt.embed_document_batch_with_model_outcomes_for_token(
                                &token, &name, chunk,
                            )
                            .await?,
                        );
                    }
                    Ok::<_, RuntimeError>(outcomes)
                };
                let result = match ctx {
                    Some(ctx) => crate::usage::scope(ctx, fut).await,
                    None => fut.await,
                };
                (groups, model_idx, result)
            });
        }

        while let Some(joined) = join_set.join_next().await {
            match joined {
                Ok((groups, model_idx, Ok(outcomes))) => {
                    for (group_idx, outcome) in groups.into_iter().zip(outcomes) {
                        embedding_outcomes[group_idx][model_idx] = Some(outcome);
                    }
                }
                Ok((_, _, Err(e))) => {
                    join_set.abort_all();
                    return Err(e);
                }
                Err(join_err) => {
                    join_set.abort_all();
                    return Err(RuntimeError::Internal(format!(
                        "embed task panicked: {join_err}"
                    )));
                }
            }
        }
    }

    // Preserve the report's logical note/model accounting even when identical
    // siblings share one provider invocation.
    for (note_idx, &content_group_idx) in note_content_groups.iter().enumerate() {
        for &model_idx in &note_models[note_idx] {
            let outcome = embedding_outcomes[content_group_idx][model_idx]
                .as_ref()
                .expect("every requested embedding was observed");
            embedding_truncation.observe(outcome);
            if outcome.truncated {
                tracing::warn!(
                    model = %outcome.model_name,
                    source_bytes = outcome.source_bytes,
                    embedded_bytes = outcome.embedded_bytes,
                    "atomic note embedding input truncated; full content will be stored unchanged"
                );
            }
        }
    }

    // Reject any non-finite embedding value BEFORE any plan is built — same
    // validation the canonical `VectorStore::insert` DML performs, applied
    // here so a bad custom embedding provider never reaches the raw vector
    // insert on this path (embed-first: no note, FTS document, or vector row
    // has been written yet).
    for outcomes_for_content in &embedding_outcomes {
        for outcome in outcomes_for_content.iter().flatten() {
            if let Some(idx) = non_finite_index(&outcome.vector) {
                return Err(non_finite_vector_error(idx, outcome.vector[idx]));
            }
        }
    }

    // ---- 3. Build one AddNotePlan per spec (row + FTS + vector-insert
    // statements), all pre-computed embeddings already in hand. ----
    let mut plans: Vec<AtomicOpPlan> = Vec::with_capacity(notes.len());
    for (note_idx, note) in notes.iter().enumerate() {
        let outcomes_for_note = &embedding_outcomes[note_content_groups[note_idx]];
        let mut statements = vec![PlanStatement {
            statement: if note.key.is_some() {
                khive_db::stores::note::note_insert_keyed_statement(note)
            } else {
                khive_db::stores::note::note_upsert_statement(note)
            },
            guard: Some(AffectedRowGuard::exactly(1)),
        }];

        if let Some(fault) = maybe_inject_fts_failure(&note.namespace, "fault-injected-fts") {
            statements.push(fault);
        } else {
            // Delete-then-insert upsert — mirrors `text.rs`'s
            // `upsert_document_dml` exactly, so a caller-supplied/reused note
            // id never leaves a stale/duplicate FTS document behind (the
            // note row itself is already an upsert via
            // `note_upsert_statement`). The delete alone is enough on the
            // sidecar rowid-map side too — the insert pair's `INSERT OR
            // REPLACE` below overwrites the map row rather than requiring a
            // prior delete of it (see `delete_document_statement`'s doc
            // comment).
            statements.push(PlanStatement {
                statement: khive_db::stores::text::delete_document_statement(
                    "fts_notes",
                    &note.namespace,
                    note.id,
                ),
                guard: None,
            });
            // Order-sensitive pair: the map upsert's `last_insert_rowid()`
            // must read back the FTS insert immediately before it, with
            // nothing else written to this connection in between — pushed
            // here as the array `insert_document_statements` returns so the
            // two can never be separated by an edit to this call site.
            for statement in khive_db::stores::text::insert_document_statements(
                "fts_notes",
                &note_fts_document(note),
            ) {
                statements.push(PlanStatement {
                    statement,
                    guard: None,
                });
            }
        }

        if let Some(fault) = maybe_inject_vector_failure(&note.namespace, "fault-injected-vector") {
            statements.push(fault);
        } else {
            for &model_idx in &note_models[note_idx] {
                let model_name = &embed_model_names[model_idx];
                let outcome = outcomes_for_note[model_idx]
                    .as_ref()
                    .expect("every model index observed exactly once");
                let table = format!("vec_{}", crate::config::sanitize_key(model_name));
                statements.extend(vector_insert_statements(
                    &table,
                    &note.namespace,
                    note.id,
                    "note.content",
                    model_name,
                    &outcome.vector,
                    &format!("atomic-message-vec-{table}-{}", note.id),
                ));
            }
        }

        plans.push(AtomicOpPlan::AddNote(AddNotePlan {
            note_guard: Some(crate::note_write::NoteWriteGuard {
                namespace: requests[note_idx].spec.token.namespace().as_str().into(),
                target_id: note.id,
                expected_version: None,
                fence: requests[note_idx].options.fence.cloned(),
                create_key: note
                    .key
                    .as_ref()
                    .map(|key| (note.kind.clone(), key.clone())),
            }),
            note_id: note.id,
            statements,
            post_commit: PostCommitEffect::None,
        }));
    }

    Ok(PreparedAtomicNotes {
        notes,
        plans,
        embedding_truncation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService, MAX_TEXT_BYTES};
    use std::sync::{Arc, Mutex};

    use khive_types::Namespace;

    use crate::embedder_registry::EmbedderProvider;

    const NAN_MODEL: &str = "atomic-message-nan-model";
    const NAN_DIMS: usize = 4;

    struct NanService;
    #[async_trait]
    impl EmbeddingService for NanService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts.iter().map(|_| vec![f32::NAN; NAN_DIMS]).collect())
        }
        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }
        fn name(&self) -> &'static str {
            NAN_MODEL
        }
    }
    struct NanProvider;
    #[async_trait]
    impl EmbedderProvider for NanProvider {
        fn name(&self) -> &str {
            NAN_MODEL
        }
        fn dimensions(&self) -> usize {
            NAN_DIMS
        }
        async fn build(&self) -> RuntimeResult<std::sync::Arc<dyn EmbeddingService>> {
            Ok(std::sync::Arc::new(NanService))
        }
    }

    struct TruncationService;
    #[async_trait]
    impl EmbeddingService for TruncationService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts
                .iter()
                .map(|_| vec![0.5; EmbeddingModel::MultilingualE5Base.dimensions()])
                .collect())
        }
        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }
        fn name(&self) -> &'static str {
            "multilingual-e5-base"
        }
    }

    struct TruncationProvider;
    #[async_trait]
    impl EmbedderProvider for TruncationProvider {
        fn name(&self) -> &str {
            "multilingual-e5-base"
        }
        fn dimensions(&self) -> usize {
            EmbeddingModel::MultilingualE5Base.dimensions()
        }
        async fn build(&self) -> RuntimeResult<std::sync::Arc<dyn EmbeddingService>> {
            Ok(std::sync::Arc::new(TruncationService))
        }
    }

    const DEDUP_DIMS: usize = 4;

    struct DedupService {
        name: &'static str,
    }

    #[async_trait]
    impl EmbeddingService for DedupService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts.iter().map(|_| vec![0.25; DEDUP_DIMS]).collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            self.name
        }
    }

    struct DedupProvider {
        name: &'static str,
    }

    #[async_trait]
    impl EmbedderProvider for DedupProvider {
        fn name(&self) -> &str {
            self.name
        }

        fn dimensions(&self) -> usize {
            DEDUP_DIMS
        }

        async fn build(&self) -> RuntimeResult<std::sync::Arc<dyn EmbeddingService>> {
            Ok(std::sync::Arc::new(DedupService { name: self.name }))
        }
    }

    struct BatchCountingService {
        name: &'static str,
        calls: Arc<Mutex<Vec<Vec<String>>>>,
    }

    #[async_trait]
    impl EmbeddingService for BatchCountingService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> Result<Vec<Vec<f32>>, EmbedError> {
            self.calls.lock().unwrap().push(texts.to_vec());
            Ok(texts.iter().map(|_| vec![0.25; DEDUP_DIMS]).collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            self.name
        }
    }

    struct BatchCountingProvider {
        name: &'static str,
        calls: Arc<Mutex<Vec<Vec<String>>>>,
    }

    #[async_trait]
    impl EmbedderProvider for BatchCountingProvider {
        fn name(&self) -> &str {
            self.name
        }

        fn dimensions(&self) -> usize {
            DEDUP_DIMS
        }

        async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
            Ok(Arc::new(BatchCountingService {
                name: self.name,
                calls: Arc::clone(&self.calls),
            }))
        }
    }

    #[tokio::test]
    async fn heterogeneous_notes_batch_models_with_one_schema_writer() {
        let runtime = KhiveRuntime::memory().unwrap();
        let token = runtime
            .authorize(Namespace::parse("batch-notes").unwrap())
            .unwrap();
        let names = ["batch-notes-a", "batch-notes-b"];
        let mut counters = Vec::new();
        for name in names {
            let calls = Arc::new(Mutex::new(Vec::new()));
            runtime.register_embedder(BatchCountingProvider {
                name,
                calls: Arc::clone(&calls),
            });
            runtime.embedder_with_token(&token, name).await.unwrap();
            counters.push(calls);
        }
        let inputs = [
            ("first", "shared text", true, 0.2),
            ("second", "shared text", true, 0.4),
            ("third", "different text", true, 0.6),
            ("disabled", "shared text", false, 0.8),
        ];
        let requests = inputs
            .iter()
            .map(|&(key, content, embed, salience)| AtomicNoteRequest {
                spec: AtomicNoteSpec {
                    token: &token,
                    id: None,
                    kind: "observation",
                    name: None,
                    content,
                    properties: Some(serde_json::json!({"key": key})),
                },
                options: AtomicNoteOptions {
                    key: Some(key),
                    salience: Some(salience),
                    embed: Some(embed),
                    ..Default::default()
                },
            })
            .collect();
        let before = runtime.backend().pool().writer_acquisition_snapshot();
        let prepared = prepare_atomic_note_requests(&runtime, requests)
            .await
            .unwrap();
        let after = runtime.backend().pool().writer_acquisition_snapshot();
        assert_eq!(after.acquisitions - before.acquisitions, 1);
        for calls in &counters {
            assert_eq!(
                *calls.lock().unwrap(),
                vec![vec!["shared text".to_owned(), "different text".to_owned()]],
            );
        }
        for (note, &(key, content, _, salience)) in prepared.notes.iter().zip(&inputs) {
            assert_eq!(note.key.as_deref(), Some(key));
            assert_eq!(note.content, content);
            assert_eq!(note.salience, Some(salience));
            assert_eq!(note.properties.as_ref().unwrap()["key"], key);
        }
        assert!(matches!(
            crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), prepared.plans)
                .await
                .unwrap(),
            AtomicRunOutcome::Committed { .. }
        ));
        for name in names {
            assert_eq!(
                runtime
                    .vectors_for_model(&token, name)
                    .unwrap()
                    .count()
                    .await
                    .unwrap(),
                3,
                "disabled notes must not inherit a sibling's shared vector",
            );
        }
    }

    #[tokio::test]
    async fn atomic_note_batch_chunks_only_above_provider_limit() {
        let runtime = KhiveRuntime::memory().unwrap();
        let token = runtime.authorize(Namespace::local()).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        runtime.register_embedder(BatchCountingProvider {
            name: "batch-notes-large",
            calls: Arc::clone(&calls),
        });
        let contents: Vec<_> = (0..=lattice_embed::DEFAULT_MAX_BATCH_SIZE)
            .map(|index| format!("distinct document {index}"))
            .collect();
        let specs = contents
            .iter()
            .map(|content| AtomicNoteSpec {
                token: &token,
                id: None,
                kind: "observation",
                name: None,
                content,
                properties: None,
            })
            .collect();
        let prepared = prepare_atomic_notes(&runtime, specs, AtomicNoteOptions::default())
            .await
            .unwrap();
        assert_eq!(prepared.notes.len(), contents.len());
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].len(), lattice_embed::DEFAULT_MAX_BATCH_SIZE);
        assert_eq!(calls[1].len(), 1);
        assert_eq!(calls.concat(), contents);
    }

    /// A minimal note-write validator standing in for a pack's real one (e.g.
    /// `khive-pack-comm`'s `derive_message_identity`): unconditionally stamps
    /// `from_actor` with the caller's token-derived actor id, discarding
    /// whatever the caller's own `properties` named for that key.
    fn stamp_from_actor(
        _kind: &str,
        actor_id: &str,
        properties: Option<Value>,
    ) -> RuntimeResult<Option<Value>> {
        let mut props = match properties {
            Some(Value::Object(map)) => map,
            _ => serde_json::Map::new(),
        };
        props.insert(
            "from_actor".to_string(),
            Value::String(actor_id.to_string()),
        );
        Ok(Some(Value::Object(props)))
    }

    async fn fts_row_count(runtime: &KhiveRuntime, namespace: &str) -> i64 {
        let mut reader = runtime.sql().reader().await.expect("sql reader");
        match reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM fts_notes WHERE namespace = ?1".to_string(),
                params: vec![SqlValue::Text(namespace.to_string())],
                label: None,
            })
            .await
            .expect("fts count query")
        {
            Some(SqlValue::Integer(n)) => n,
            other => panic!("unexpected fts count result: {other:?}"),
        }
    }

    async fn ann_write_log_count(runtime: &KhiveRuntime, namespace: &str, model: &str) -> i64 {
        let mut reader = runtime.sql().reader().await.expect("sql reader");
        match reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM ann_write_log \
                      WHERE namespace = ?1 AND embedding_model = ?2"
                    .to_string(),
                params: vec![
                    SqlValue::Text(namespace.to_string()),
                    SqlValue::Text(model.to_string()),
                ],
                label: None,
            })
            .await
            .expect("ann_write_log count query")
        {
            Some(SqlValue::Integer(n)) => n,
            other => panic!("unexpected ann_write_log count result: {other:?}"),
        }
    }

    /// A custom embedding provider returning NaN must be rejected BEFORE any
    /// plan is built — same validation `VectorStore::insert` performs on the
    /// canonical `create_note_inner` path. No note row, FTS document, vector
    /// row, or `ann_write_log` row may be committed.
    #[tokio::test]
    async fn create_notes_atomic_rejects_non_finite_embedding_before_any_write() {
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        runtime.register_embedder(NanProvider);
        let ns = "atomic-message-nan-test";
        let token = runtime
            .authorize(Namespace::parse(ns).unwrap())
            .expect("authorize");

        let result = create_notes_atomic(
            &runtime,
            vec![AtomicNoteSpec {
                token: &token,
                id: None,
                kind: "observation",
                name: None,
                content: "nan embedding content",
                properties: None,
            }],
        )
        .await;

        assert!(
            result.is_err(),
            "a non-finite embedding must be rejected; got {result:?}"
        );

        let alive = runtime
            .list_notes(&token, Some("observation"), 100, 0)
            .await
            .expect("list_notes")
            .into_iter()
            .filter(|n| n.deleted_at.is_none())
            .count();
        assert_eq!(
            alive, 0,
            "no note row may be committed when an embedding is non-finite"
        );

        assert_eq!(
            fts_row_count(&runtime, ns).await,
            0,
            "no FTS document may be committed when an embedding is non-finite"
        );

        let vs = runtime
            .vectors_for_model(&token, NAN_MODEL)
            .expect("vec store");
        assert_eq!(
            vs.count().await.expect("count"),
            0,
            "no vector row may be committed when an embedding is non-finite"
        );

        assert_eq!(
            ann_write_log_count(&runtime, ns, NAN_MODEL).await,
            0,
            "no ann_write_log row may be committed when an embedding is non-finite"
        );
    }

    #[tokio::test]
    async fn create_notes_atomic_rejects_reserved_secret_gate_key() {
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        let ns = "atomic-message-reserved-key-test";
        let token = runtime
            .authorize(Namespace::parse(ns).unwrap())
            .expect("authorize");

        let result = create_notes_atomic(
            &runtime,
            vec![AtomicNoteSpec {
                token: &token,
                id: None,
                kind: "observation",
                name: None,
                content: "atomic message reservation target",
                properties: Some(
                    serde_json::json!({"khive:secret_gate": "exempted:content-sha256-manifest-v1"}),
                ),
            }],
        )
        .await;

        let err = result.expect_err("caller-supplied reserved key must be rejected");
        assert!(
            matches!(err, RuntimeError::InvalidInput(ref msg) if msg.contains("khive:secret_gate")),
            "unexpected error: {err:?}"
        );

        let alive = runtime
            .list_notes(&token, Some("observation"), 100, 0)
            .await
            .expect("list_notes")
            .into_iter()
            .filter(|n| n.deleted_at.is_none())
            .count();
        assert_eq!(
            alive, 0,
            "no note row may be committed on reservation rejection"
        );
    }

    #[tokio::test]
    async fn create_notes_atomic_with_report_preserves_truncation_outcome() {
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        runtime.register_embedder(TruncationProvider);
        let token = runtime
            .authorize(Namespace::parse("atomic-message-truncation-test").unwrap())
            .expect("authorize");
        let content = "x".repeat(MAX_TEXT_BYTES);

        let (notes, report) = create_notes_atomic_with_report(
            &runtime,
            vec![AtomicNoteSpec {
                token: &token,
                id: None,
                kind: "observation",
                name: None,
                content: &content,
                properties: None,
            }],
        )
        .await
        .expect("atomic note write");

        assert_eq!(notes.len(), 1);
        assert_eq!(report.truncated, 1);
        assert_eq!(
            report.discarded_bytes,
            "passage: ".len() as u64,
            "E5 document-prefix reservation must be reflected in the returned report"
        );
    }

    #[tokio::test]
    async fn create_notes_atomic_reuses_identical_content_once_per_model() {
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        for name in ["atomic-dedup-model-a", "atomic-dedup-model-b"] {
            runtime.register_embedder(DedupProvider { name });
        }
        let outbound_token = runtime
            .authorize(Namespace::parse("atomic-dedup-outbound").unwrap())
            .expect("authorize outbound");
        let inbound_token = runtime
            .authorize(Namespace::parse("atomic-dedup-inbound").unwrap())
            .expect("authorize inbound");
        let content = "byte-identical outbound and inbound message content";

        let usage = crate::usage::UsageContext::new();
        let notes = crate::usage::scope(usage.clone(), async {
            create_notes_atomic(
                &runtime,
                vec![
                    AtomicNoteSpec {
                        token: &outbound_token,
                        id: None,
                        kind: "observation",
                        name: Some("shared subject"),
                        content,
                        properties: Some(serde_json::json!({"direction": "outbound"})),
                    },
                    AtomicNoteSpec {
                        token: &inbound_token,
                        id: None,
                        kind: "observation",
                        name: Some("shared subject"),
                        content,
                        properties: Some(serde_json::json!({"direction": "inbound"})),
                    },
                ],
            )
            .await
        })
        .await
        .expect("atomic note pair");

        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].content, content);
        assert_eq!(notes[1].content, content);
        assert_eq!(
            usage.snapshot()["embed_calls"],
            2,
            "two identical notes across two models must issue one embed per model"
        );
        for model in ["atomic-dedup-model-a", "atomic-dedup-model-b"] {
            for (direction, token) in [("outbound", &outbound_token), ("inbound", &inbound_token)] {
                assert_eq!(
                    runtime
                        .vectors_for_model(token, model)
                        .expect("vector store")
                        .count()
                        .await
                        .expect("vector count"),
                    1,
                    "the {direction} note must retain its vector row for {model}"
                );
            }
        }
    }

    /// Calling `create_notes_atomic` twice with the SAME caller-supplied id
    /// (different content each time) must leave exactly one FTS document for
    /// that id, reflecting the latest content — not an append of two
    /// documents (the note row is already an upsert; the FTS half must match).
    #[tokio::test]
    async fn version_create_notes_atomic_upserts_fts_document_for_reused_note_id() {
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        let ns = "atomic-message-fts-upsert-test";
        let token = runtime
            .authorize(Namespace::parse(ns).unwrap())
            .expect("authorize");
        let id = Uuid::new_v4();

        let first = create_notes_atomic(
            &runtime,
            vec![AtomicNoteSpec {
                token: &token,
                id: Some(id),
                kind: "observation",
                name: None,
                content: "first content",
                properties: None,
            }],
        )
        .await
        .expect("first write with supplied id");
        assert_eq!(first[0].version, 1);

        let second = create_notes_atomic(
            &runtime,
            vec![AtomicNoteSpec {
                token: &token,
                id: Some(id),
                kind: "observation",
                name: None,
                content: "second content replacing the first",
                properties: None,
            }],
        )
        .await
        .expect("second write reusing the same id");
        assert_eq!(second[0].version, 2);
        assert_eq!(
            runtime
                .notes(&token)
                .unwrap()
                .get_note(id)
                .await
                .unwrap()
                .unwrap()
                .version,
            2
        );
        let repeated = create_notes_atomic(
            &runtime,
            (0..2)
                .map(|_| AtomicNoteSpec {
                    token: &token,
                    id: Some(id),
                    kind: "observation",
                    name: None,
                    content: "second content replacing the first",
                    properties: None,
                })
                .collect(),
        )
        .await
        .expect("equal-value upserts in the same batch");
        assert_eq!(
            repeated.iter().map(|note| note.version).collect::<Vec<_>>(),
            vec![3, 4]
        );
        assert_eq!(
            runtime
                .notes(&token)
                .unwrap()
                .get_note(id)
                .await
                .unwrap()
                .unwrap()
                .version,
            4
        );

        assert_eq!(
            fts_row_count(&runtime, ns).await,
            1,
            "exactly one FTS document may exist for the reused id"
        );

        let text = runtime.text_for_notes(&token).expect("text store");
        let doc = text
            .get_document(ns, id)
            .await
            .expect("get_document")
            .expect("document exists for the reused id");
        assert!(
            doc.body.contains("second content"),
            "the surviving FTS document must reflect the latest content; got {:?}",
            doc.body
        );
    }

    /// ADR-124 regression: `create_notes_atomic_with_report` must run
    /// `derive_note_write_properties` per spec, the same as every other
    /// note-write site (`operations.rs`'s create funnel,
    /// `atomic_prepare::prepare_add_note`). A caller holding a valid token
    /// for one actor must not be able to plant an arbitrary `from_actor` (or
    /// any other owner-established property) via `AtomicNoteSpec::properties`
    /// — the installed validator's derived value must win.
    ///
    /// This test fails if the `derive_note_write_properties` call in
    /// `create_notes_atomic_with_report`'s spec loop is removed: verified by
    /// deleting that call in a scratch copy of this file and re-running —
    /// the assertion below turns red because the stored `from_actor` reverts
    /// to the forged `"forged-actor"` value instead of the token's actor id.
    #[tokio::test]
    async fn create_notes_atomic_derives_from_actor_overwriting_a_forged_value() {
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        runtime.install_note_write_validator(std::sync::Arc::new(stamp_from_actor));
        let ns = "atomic-message-identity-guard-test";
        let token = runtime
            .authorize(Namespace::parse(ns).unwrap())
            .expect("authorize");
        let true_actor = token.actor().id.clone();

        let notes = create_notes_atomic(
            &runtime,
            vec![AtomicNoteSpec {
                token: &token,
                id: None,
                kind: "observation",
                name: None,
                content: "atomic writer identity guard probe",
                properties: Some(serde_json::json!({"from_actor": "forged-actor"})),
            }],
        )
        .await
        .expect("atomic note write must succeed — the guard derives, it does not refuse");

        assert_eq!(notes.len(), 1);
        assert_eq!(
            notes[0]
                .properties
                .as_ref()
                .and_then(|p| p.get("from_actor"))
                .and_then(|v| v.as_str()),
            Some(true_actor.as_str()),
            "the atomic writer must store the token-derived from_actor, not the \
             caller-supplied forged value"
        );
    }
}
