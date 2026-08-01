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
//! (`memory.remember`, `gtd.assign`) can adopt it later without a new type.
//!
//! # Embed-first
//!
//! Embedding is slow compute (network/model calls). Every note's embeddings,
//! across every registered model, are computed **before** any transaction
//! opens — the writer is held only for synchronous DML, exactly like the
//! rest of the ADR-099 atomic-unit machinery (`atomic_plan`/`atomic_runner`).
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
//! ONE [`run_atomic_unit`] call — one [`khive_storage::SqlAccess::atomic_unit`],
//! one writer checkout, one WAL commit for the whole set. A failure on any
//! note's plan rolls back the ENTIRE unit (`atomic_runner`'s documented
//! guarantee: a later op's failure unwinds even an earlier op's own
//! already-`RELEASE`d `SAVEPOINT`), so a crash or guard failure partway
//! through never leaves an orphan copy — the process-crash gap the two-call
//! version of `dual_write_message` used to document is closed by
//! construction.

use serde_json::Value;
use uuid::Uuid;

use khive_storage::note::Note;
use khive_storage::types::SqlValue;
use khive_storage::SqlStatement;
use khive_types::SubstrateKind;

use crate::atomic_plan::{AddNotePlan, AffectedRowGuard, PlanStatement, PostCommitEffect};
use crate::atomic_runner::{run_atomic_unit, AtomicOpPlan, AtomicRunOutcome};
use crate::config::NamespaceToken;
use crate::curation::note_fts_document;
use crate::error::{RuntimeError, RuntimeResult};
use crate::runtime::KhiveRuntime;

/// One note to write as part of an atomic set. Mirrors the subset of
/// `create_note_inner`'s parameters `comm.send`/`comm.reply` actually use —
/// no `annotates`, `salience`, `decay_factor`, `embedding_content` override,
/// or explicit `embedding_model` pin. A future caller needing those can grow
/// this struct; none of khive-pack-comm's call sites need them today.
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

/// A harmless no-op `UPDATE` targeting a row that can never exist, guarded
/// `exactly(1)` so it always fails its guard inside the atomic unit. Spliced
/// in by test-only fault injection (see [`maybe_inject_fts_failure`]/
/// [`maybe_inject_vector_failure`]) to prove the whole unit — not just the
/// failing note's own plan — rolls back.
#[cfg(any(test, feature = "fault-injection"))]
fn injected_failure_statement(label: &str) -> PlanStatement {
    PlanStatement {
        statement: SqlStatement {
            sql: "UPDATE notes SET updated_at = updated_at \
                  WHERE id = '00000000-0000-0000-0000-000000000000'"
                .to_string(),
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

/// DELETE-then-INSERT-then-log statements for one (note, model) vector row,
/// replaying `khive-db::stores::vectors::replace_vector_row_dml`'s DML shape
/// as plain [`PlanStatement`]s rather than a live `rusqlite::Connection`
/// call — the same technique `atomic_prepare::push_index_purge_statements`
/// uses for delete. Unguarded: same convention as the FTS-insert statement
/// in `atomic_prepare::prepare_add_note` (the row's existence guard is
/// carried by the plan's primary note-row statement, applied first).
#[allow(clippy::too_many_arguments)]
fn vector_insert_statements(
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
/// whole unit per [`run_atomic_unit`]'s guarantee).
pub async fn create_notes_atomic(
    runtime: &KhiveRuntime,
    specs: Vec<AtomicNoteSpec<'_>>,
) -> RuntimeResult<Vec<Note>> {
    // ---- 1. Validate + build Note objects (all pre-write checks, same as
    // create_note_inner, before any embedding or DML is attempted). ----
    let mut notes: Vec<Note> = Vec::with_capacity(specs.len());
    for spec in &specs {
        runtime.validate_note_kind(spec.kind)?;
        crate::secret_gate::check(spec.content)?;
        if let Some(n) = spec.name {
            crate::secret_gate::check(n)?;
        }
        if let Some(ref p) = spec.properties {
            crate::secret_gate::check_json(p)?;
        }

        let ns = spec.token.namespace().as_str();
        let mut note = Note::new(ns, spec.kind, spec.content);
        if let Some(id) = spec.id {
            note.id = id;
        }
        if let Some(n) = spec.name {
            note = note.with_name(n);
        }
        if let Some(p) = spec.properties.clone() {
            note = note.with_properties(p);
        }
        notes.push(note);
    }

    // ---- 2. Embed every (note, model) pair in parallel, BEFORE opening any
    // transaction. Any failure aborts here — no write has been attempted. ----
    let embed_model_names = runtime.registered_embedding_model_names();
    // (note_idx, model_idx) -> embedding, filled in as embed tasks complete.
    let mut embeddings: Vec<Vec<Option<Vec<f32>>>> = notes
        .iter()
        .map(|_| vec![None; embed_model_names.len()])
        .collect();

    if !embed_model_names.is_empty() {
        // Ensure every model's vector table exists before the commit pass —
        // the same lazy-create side effect `vectors_for_model` performs on
        // the non-atomic path, done once per model rather than per note.
        for model_name in &embed_model_names {
            runtime.vectors_for_model(specs[0].token, model_name)?;
        }

        let usage_ctx = crate::usage::current();
        let mut join_set = tokio::task::JoinSet::new();
        for (note_idx, (spec, note)) in specs.iter().zip(notes.iter()).enumerate() {
            let text = crate::curation::note_embedding_text(note);
            for (model_idx, model_name) in embed_model_names.iter().enumerate() {
                let rt = runtime.clone();
                let token = spec.token.clone();
                let name = model_name.clone();
                let text = text.clone();
                let ctx = usage_ctx.clone();
                join_set.spawn(async move {
                    let fut = rt.embed_document_with_model_for_token(&token, &name, &text);
                    let result = match ctx {
                        Some(ctx) => crate::usage::scope(ctx, fut).await,
                        None => fut.await,
                    };
                    (note_idx, model_idx, result)
                });
            }
        }

        while let Some(joined) = join_set.join_next().await {
            match joined {
                Ok((note_idx, model_idx, Ok(vector))) => {
                    embeddings[note_idx][model_idx] = Some(vector);
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

    // ---- 3. Build one AddNotePlan per spec (row + FTS + vector-insert
    // statements), all pre-computed embeddings already in hand. ----
    let mut plans: Vec<AtomicOpPlan> = Vec::with_capacity(notes.len());
    for (note, embeddings_for_note) in notes.iter().zip(embeddings) {
        let mut statements = vec![PlanStatement {
            statement: khive_db::stores::note::note_upsert_statement(note),
            guard: Some(AffectedRowGuard::exactly(1)),
        }];

        if let Some(fault) = maybe_inject_fts_failure(&note.namespace, "fault-injected-fts") {
            statements.push(fault);
        } else {
            statements.push(PlanStatement {
                statement: khive_db::stores::text::insert_document_statement(
                    "fts_notes",
                    &note_fts_document(note),
                ),
                guard: None,
            });
        }

        if let Some(fault) = maybe_inject_vector_failure(&note.namespace, "fault-injected-vector") {
            statements.push(fault);
        } else {
            for (model_name, embedding) in embed_model_names.iter().zip(embeddings_for_note) {
                let embedding = embedding.expect("every model index observed exactly once");
                let table = format!("vec_{}", crate::config::sanitize_key(model_name));
                statements.extend(vector_insert_statements(
                    &table,
                    &note.namespace,
                    note.id,
                    "note.content",
                    model_name,
                    &embedding,
                    &format!("atomic-message-vec-{table}-{}", note.id),
                ));
            }
        }

        plans.push(AtomicOpPlan::AddNote(AddNotePlan {
            note_id: note.id,
            statements,
            post_commit: PostCommitEffect::None,
        }));
    }

    // ---- 4. One writer acquisition for the whole set. ----
    match run_atomic_unit(runtime.sql().as_ref(), plans).await {
        Ok(AtomicRunOutcome::Committed { .. }) => Ok(notes),
        Ok(AtomicRunOutcome::RolledBack {
            failed_op_index,
            failure,
        }) => Err(RuntimeError::Internal(format!(
            "atomic multi-note write rolled back at op {failed_op_index}: {failure:?}"
        ))),
        Err(e) => Err(RuntimeError::Storage(e.0)),
    }
}
