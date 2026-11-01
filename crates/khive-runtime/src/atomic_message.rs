//! Atomic multi-note write primitive: commits a set of notes — each with its
//! FTS document and its kind-selected embedding model rows — in ONE
//! writer transaction, instead of one `create_note` call per note.
//!
//! Built for `khive-pack-comm`'s `dual_write_message` (outbound + inbound
//! copy of a `comm.send`/`comm.reply`), which previously cost roughly a
//! dozen separate writer acquisitions per send: two `create_note_inner`
//! calls (row + FTS + one vector insert per registered model each) plus a
//! root-send `thread_id` patch. Shaped as `create_notes_atomic_with_report(Vec<AtomicNoteSpec>)`
//! rather than a comm-specific pair primitive so other multi-write verbs
//! can share the same preparation. Keyed `memory.remember` appends a required
//! annotation and final key claim before committing its prepared plan.
//!
//! # Embed-first
//!
//! Embedding is slow compute (network/model calls). Every distinct content's
//! embeddings, across the selected models, are computed **before** any
//! transaction opens — the writer is held only for synchronous DML, exactly
//! like the rest of the ADR-099 atomic-unit machinery
//! (`atomic_plan`/`atomic_runner`).
//! This is the same reason `atomic_prepare::prepare_add_note` defers vector
//! indexing to a post-commit `PostCommitEffect::ReindexNote`; the difference
//! here is embeddings are computed *before* commit instead of *after*, so
//! the vector rows land in the SAME atomic unit as the note row and FTS
//! document, and no post-commit reindex is needed at all — every plan built
//! by [`create_notes_atomic_with_report`] carries `post_commit: PostCommitEffect::None`.
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
    /// Keyed memory only: persist a zero-model receipt header and each model's
    /// exact log-upsert rowid in this same atomic unit.
    pub memory_visibility_receipt: bool,
    /// Ask the writer transaction to compare a live key holder against this
    /// candidate and report the result (ADR-172 Amendment 6). Every other
    /// keyed route defaults this to `false` and gets today's Details shape
    /// on a key conflict, with no comparison outcome in it at all.
    pub replay_receipt: bool,
    pub fence: Option<&'a crate::note_write::NoteFences>,
    /// `spec.properties` is already the fully validator-derived value, so
    /// skip calling the note-write validator again below (ADR-172 Amendment
    /// 6's keyed singleton-create staging derives properties once, before
    /// its initial holder check, and reuses that same value through
    /// preparation and any final-check fallback; the installed validator is
    /// not guaranteed idempotent, and a kg regression test counts its calls,
    /// so deriving twice for one request is an observably different
    /// candidate, not a redundant no-op). Every other caller leaves this
    /// `false` and keeps deriving from raw caller-supplied properties here.
    pub properties_already_derived: bool,
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

/// Mirrors `khive-db`'s private `non_finite_index` — this crate has no
/// visibility into that helper, so the check is reproduced here to keep the
/// atomic path's embedding validation observably identical to the canonical
/// `create_note_inner` -> `VectorStore::insert` path.
pub(crate) fn non_finite_index(data: &[f32]) -> Option<usize> {
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
    vector_insert_statements_for_substrate(
        table,
        namespace,
        subject_id,
        SubstrateKind::Note,
        field,
        embedding_model,
        embedding,
        label_prefix,
        None,
    )
}

/// Build atomic vector and ANN-log writes for a typed substrate. The caller
/// chooses whether the two single-row inserts require affected-row evidence.
#[allow(clippy::too_many_arguments)]
pub(crate) fn vector_insert_statements_for_substrate(
    table: &str,
    namespace: &str,
    subject_id: Uuid,
    substrate: SubstrateKind,
    field: &str,
    embedding_model: &str,
    embedding: &[f32],
    label_prefix: &str,
    insert_guard: Option<AffectedRowGuard>,
) -> Vec<PlanStatement> {
    let subject = subject_id.to_string();
    let model_key = table
        .strip_prefix("vec_")
        .expect("runtime vector tables use the vec_ prefix");
    let kind_str = substrate.to_string();
    let blob = khive_storage::encode_f32_native(embedding);
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
        // A replacement through this raw atomic plan does not know the source
        // text, even if its new vector bytes happen to equal the old bytes.
        PlanStatement {
            statement: SqlStatement {
                sql: "DELETE FROM vector_provenance \
                      WHERE model_key = ?1 AND subject_id = ?2"
                    .to_string(),
                params: vec![
                    SqlValue::Text(model_key.to_string()),
                    SqlValue::Text(subject.clone()),
                ],
                label: Some(format!("{label_prefix}-clear-provenance")),
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
            guard: insert_guard,
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
            guard: insert_guard,
        },
    ]
}

/// Build and commit `specs` as one atomic unit. Returns the persisted
/// [`Note`]s in the same order as `specs`. On any failure — pre-write
/// validation, embedding, or the atomic commit pass itself — NO note, FTS
/// document, or vector row from any spec is left behind (embed failures
/// occur before any write is attempted; commit-pass failures roll back the
/// whole unit per [`crate::atomic_runner::run_atomic_unit`]'s guarantee).
#[cfg(test)]
pub(crate) async fn create_notes_atomic(
    runtime: &KhiveRuntime,
    specs: Vec<AtomicNoteSpec<'_>>,
) -> RuntimeResult<Vec<Note>> {
    Ok(create_notes_atomic_with_report(runtime, specs).await?.0)
}

/// Build and commit notes with an embedding-truncation report. The report keeps
/// logical per-note/model accounting even when identical content shares one
/// provider result, and is returned only when the whole note set commits
/// successfully.
pub async fn create_notes_atomic_with_report(
    runtime: &KhiveRuntime,
    specs: Vec<AtomicNoteSpec<'_>>,
) -> RuntimeResult<(Vec<Note>, crate::retrieval::EmbeddingTruncationReport)> {
    create_notes_atomic_with_attachments(runtime, specs, &[]).await
}

/// Create notes and their role-keyed attachments in the same writer transaction.
/// The existing note, FTS, vector and revision accounting is preserved.
pub async fn create_notes_atomic_with_attachments(
    runtime: &KhiveRuntime,
    specs: Vec<AtomicNoteSpec<'_>>,
    attachments: &[khive_storage::NewAttachment],
) -> RuntimeResult<(Vec<Note>, crate::retrieval::EmbeddingTruncationReport)> {
    create_notes_atomic_with_extra_statements(runtime, specs, attachments, Vec::new()).await
}

/// Like [`create_notes_atomic_with_attachments`], and also commit `extra_statements` in the same
/// writer transaction, after the last note's plan. Each statement must affect exactly one row;
/// if any statement fails, no note and no extra row is left behind.
pub async fn create_notes_atomic_with_extra_statements(
    runtime: &KhiveRuntime,
    specs: Vec<AtomicNoteSpec<'_>>,
    attachments: &[khive_storage::NewAttachment],
    extra_statements: Vec<khive_storage::SqlStatement>,
) -> RuntimeResult<(Vec<Note>, crate::retrieval::EmbeddingTruncationReport)> {
    validate_note_attachments(runtime, attachments)?;
    let mut prepared = prepare_atomic_notes(runtime, specs, AtomicNoteOptions::default()).await?;
    append_note_attachments(&mut prepared, attachments)?;
    append_extra_statements(&mut prepared, extra_statements)?;
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

pub(crate) fn validate_note_attachments(
    runtime: &KhiveRuntime,
    attachments: &[khive_storage::NewAttachment],
) -> RuntimeResult<()> {
    if !attachments.is_empty() {
        // Note plans execute on runtime.sql(); attachment liveness belongs to
        // main. Refuse a split placement before preparing any write.
        runtime.attachments()?;
    }
    for attachment in attachments {
        attachment.validate()?;
    }
    Ok(())
}

pub(crate) fn append_note_attachments(
    prepared: &mut PreparedAtomicNotes,
    attachments: &[khive_storage::NewAttachment],
) -> RuntimeResult<()> {
    for (plan, note) in prepared.plans.iter_mut().zip(&prepared.notes) {
        let AtomicOpPlan::AddNote(plan) = plan else {
            return Err(RuntimeError::Internal(
                "expected prepared attachment owner note".into(),
            ));
        };
        for attachment in attachments {
            let row = khive_storage::Attachment::from_new(
                note.id,
                khive_storage::AttachmentSubstrate::Note,
                attachment.clone(),
                note.created_at,
            );
            // Append after the note's complete index plan, preserving adjacent
            // last_insert_rowid-dependent FTS statements and their counters.
            plan.statements.push(PlanStatement {
                statement: khive_db::stores::attachment::attachment_upsert_statement(&row)?,
                guard: Some(AffectedRowGuard::exactly(1)),
            });
        }
    }
    Ok(())
}

/// Append caller-built statements to the last note's plan so they commit in the same unit. Each
/// statement must affect exactly one row.
pub(crate) fn append_extra_statements(
    prepared: &mut PreparedAtomicNotes,
    statements: Vec<khive_storage::SqlStatement>,
) -> RuntimeResult<()> {
    if statements.is_empty() {
        return Ok(());
    }
    let Some(AtomicOpPlan::AddNote(last)) = prepared.plans.last_mut() else {
        return Err(RuntimeError::Internal(
            "expected prepared note for extra statements".into(),
        ));
    };
    for statement in statements {
        last.statements.push(PlanStatement {
            statement,
            guard: Some(AffectedRowGuard::exactly(1)),
        });
    }
    Ok(())
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
            runtime.embedding_models_for_note_kind(request.spec.kind)
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
    for (index, request) in requests.iter().enumerate() {
        let spec = &request.spec;
        let options = &request.options;
        let record = format!("note[{index}]");
        runtime.validate_note_kind(spec.kind)?;
        // Same owned-identity derivation every other note-write site runs
        // (`operations.rs`'s create funnel, `atomic_prepare::prepare_add_note`):
        // derive from the token BEFORE the secret scan and note construction,
        // so a caller-supplied `from_actor`/`thread_id`/etc. in `spec.properties`
        // never reaches storage verbatim on this writer either. Skipped when
        // the caller already derived (`properties_already_derived`): the
        // validator is not guaranteed idempotent, so a second call for the
        // same request is a different candidate, not a no-op.
        let properties = if options.properties_already_derived {
            spec.properties.clone()
        } else {
            runtime.derive_note_write_properties(spec.kind, spec.token, spec.properties.clone())?
        };
        crate::secret_gate::reject_reserved_secret_gate_property(properties.as_ref())?;
        crate::secret_gate::check_note_fields_at(
            spec.content,
            spec.name,
            properties.as_ref(),
            &record,
        )?;

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

    // Deferred key publication clears the temporary Note.key; its private receipt
    // option is the original keyed-memory intent. Generic and stream routes
    // retain options.key. Kind is always the canonical prepared note kind.
    let memory_receipts: Vec<bool> = notes
        .iter()
        .zip(&requests)
        .map(|(note, request)| {
            note.kind == "memory"
                && (request.options.key.is_some() || request.options.memory_visibility_receipt)
        })
        .collect();
    if memory_receipts.iter().any(|capture| *capture) {
        runtime.require_visibility_cutover()?;
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
            join_set.spawn(crate::runtime::inherit_request_embedder_scope(async move {
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
            }));
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

        if memory_receipts[note_idx] {
            statements.push(PlanStatement {
                statement: SqlStatement {
                    sql:
                        "INSERT INTO memory_visibility_receipts (namespace, note_id, model_count) \
                          VALUES (?1, ?2, ?3)"
                            .into(),
                    params: vec![
                        SqlValue::Text(note.namespace.clone()),
                        SqlValue::Text(note.id.to_string()),
                        SqlValue::Integer(note_models[note_idx].len() as i64),
                    ],
                    label: Some("memory-visibility-receipt".into()),
                },
                guard: Some(AffectedRowGuard::exactly(1)),
            });
        }

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
                if memory_receipts[note_idx] {
                    // The preceding statement is the model's ann_write_log
                    // upsert. Keep this insert adjacent: last_insert_rowid()
                    // reads that exact transaction-local sequence, never a
                    // later MAX(seq) vulnerable to concurrency/compaction.
                    statements.push(PlanStatement {
                        statement: SqlStatement {
                            sql: "INSERT INTO memory_visibility_fences \
                                  (namespace, note_id, model, ann_write_log_seq) \
                                  VALUES (?1, ?2, ?3, last_insert_rowid())"
                                .into(),
                            params: vec![
                                SqlValue::Text(note.namespace.clone()),
                                SqlValue::Text(note.id.to_string()),
                                SqlValue::Text(model_name.clone()),
                            ],
                            label: Some("memory-visibility-fence".into()),
                        },
                        guard: Some(AffectedRowGuard::exactly(1)),
                    });
                }
            }
        }

        // After every log/fence pair: inserting the independent marker must
        // never disturb either ANN or FTS last_insert_rowid adjacency.
        if memory_receipts[note_idx] {
            statements.push(PlanStatement {
                statement: SqlStatement {
                    sql: "INSERT INTO memory_visibility_epochs (note_id, namespace, epoch) \
                          VALUES (?1, ?2, 'modern')"
                        .into(),
                    params: vec![
                        SqlValue::Text(note.id.to_string()),
                        SqlValue::Text(note.namespace.clone()),
                    ],
                    label: Some("memory-visibility-epoch".into()),
                },
                guard: Some(AffectedRowGuard::exactly(1)),
            });
        }

        plans.push(AtomicOpPlan::AddNote(Box::new(AddNotePlan {
            note_guard: Some(crate::note_write::NoteWriteGuard {
                namespace: requests[note_idx].spec.token.namespace().as_str().into(),
                target_id: note.id,
                expected_version: None,
                fence: requests[note_idx].options.fence.cloned(),
                create_key: note
                    .key
                    .as_ref()
                    .map(|key| crate::note_write::CreateKeyClaim {
                        kind: note.kind.clone(),
                        key: key.clone(),
                        content: note.content.clone(),
                        properties: note.properties.clone(),
                        replay_signal: requests[note_idx].options.replay_receipt,
                    }),
            }),
            note_id: note.id,
            statements,
            post_commit: PostCommitEffect::None,
        })));
    }

    Ok(PreparedAtomicNotes {
        notes,
        plans,
        embedding_truncation,
    })
}

#[cfg(test)]
#[path = "atomic_message_tests.rs"]
mod tests;
