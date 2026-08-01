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
use khive_storage::{SqlStatement, StorageCapability, StorageError};
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
fn non_finite_vector_error(idx: usize, value: f32) -> RuntimeError {
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
    Ok(create_notes_atomic_with_report(runtime, specs).await?.0)
}

/// The truncation-reporting form of [`create_notes_atomic`]. The report covers
/// every note/model embedding actually produced before the atomic commit and is
/// returned only when the whole note set commits successfully.
pub async fn create_notes_atomic_with_report(
    runtime: &KhiveRuntime,
    specs: Vec<AtomicNoteSpec<'_>>,
) -> RuntimeResult<(Vec<Note>, crate::retrieval::EmbeddingTruncationReport)> {
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
    let mut embedding_truncation = crate::retrieval::EmbeddingTruncationReport::default();

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
                    let fut = rt.embed_document_with_model_outcome_for_token(&token, &name, &text);
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
                Ok((note_idx, model_idx, Ok(outcome))) => {
                    embedding_truncation.observe(&outcome);
                    if outcome.truncated {
                        tracing::warn!(
                            model = %outcome.model_name,
                            source_bytes = outcome.source_bytes,
                            embedded_bytes = outcome.embedded_bytes,
                            "atomic note embedding input truncated; full content will be stored unchanged"
                        );
                    }
                    embeddings[note_idx][model_idx] = Some(outcome.vector);
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

    // Reject any non-finite embedding value BEFORE any plan is built — same
    // validation the canonical `VectorStore::insert` DML performs, applied
    // here so a bad custom embedding provider never reaches the raw vector
    // insert on this path (embed-first: no note, FTS document, or vector row
    // has been written yet).
    for embeddings_for_note in &embeddings {
        for embedding in embeddings_for_note.iter().flatten() {
            if let Some(idx) = non_finite_index(embedding) {
                return Err(non_finite_vector_error(idx, embedding[idx]));
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
            // Delete-then-insert upsert — mirrors `text.rs`'s
            // `upsert_document_dml` exactly, so a caller-supplied/reused note
            // id never leaves a stale/duplicate FTS document behind (the
            // note row itself is already an upsert via
            // `note_upsert_statement`).
            statements.push(PlanStatement {
                statement: khive_db::stores::text::delete_document_statement(
                    "fts_notes",
                    &note.namespace,
                    note.id,
                ),
                guard: None,
            });
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
        Ok(AtomicRunOutcome::Committed { .. }) => Ok((notes, embedding_truncation)),
        Ok(AtomicRunOutcome::RolledBack {
            failed_op_index,
            failure,
        }) => Err(RuntimeError::Internal(format!(
            "atomic multi-note write rolled back at op {failed_op_index}: {failure:?}"
        ))),
        Err(e) => Err(RuntimeError::Storage(e.0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService, MAX_TEXT_CHARS};

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
            Ok(texts.iter().map(|_| vec![0.5]).collect())
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
            1
        }
        async fn build(&self) -> RuntimeResult<std::sync::Arc<dyn EmbeddingService>> {
            Ok(std::sync::Arc::new(TruncationService))
        }
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
    async fn create_notes_atomic_with_report_preserves_truncation_outcome() {
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        runtime.register_embedder(TruncationProvider);
        let token = runtime
            .authorize(Namespace::parse("atomic-message-truncation-test").unwrap())
            .expect("authorize");
        let content = "x".repeat(MAX_TEXT_CHARS);

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

    /// Calling `create_notes_atomic` twice with the SAME caller-supplied id
    /// (different content each time) must leave exactly one FTS document for
    /// that id, reflecting the latest content — not an append of two
    /// documents (the note row is already an upsert; the FTS half must match).
    #[tokio::test]
    async fn create_notes_atomic_upserts_fts_document_for_reused_note_id() {
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        let ns = "atomic-message-fts-upsert-test";
        let token = runtime
            .authorize(Namespace::parse(ns).unwrap())
            .expect("authorize");
        let id = Uuid::new_v4();

        create_notes_atomic(
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

        create_notes_atomic(
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
}
