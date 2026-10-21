use std::collections::{HashMap, HashSet};

use khive_runtime::{
    hex_prefix_to_uuid_pattern, KhiveRuntime, Namespace, NamespaceToken, RuntimeError,
};
use khive_storage::types::{SqlStatement, SqlValue};
use serde_json::{json, Value};
use uuid::Uuid;

use super::{
    apply_status_multipliers, atom_embed_text, atom_from_row,
    attach_body_lines_timeout_degradation, attach_hydration_degradation,
    attach_lexical_timeout_degradation, attach_member_sizing_timeout_degradation,
    candidate_fallback, deprecated_allowed_by_status_policy, deser, enforce_min_score_floor,
    filter_hits_by_status, filter_hits_by_type, format_kg_entities_markdown, fuse_ann_hits,
    is_stop, load_atom_body_line_counts, load_domain_by_id_or_slug, load_domain_member_token_sizes,
    pack_compose_markdown, parse_domain_members, rerank_search_with_stored_vectors,
    rerank_text_items, rerank_with_embeddings, row_i64, run_candidate_stages, search_core,
    search_decomposed, search_kg_entities, sql_err, status_values, trim_kg_entities_to_budget,
    vamana, AnnAvailability, Atom, CandidateStageOptions, CandidateStageOutcome, ComposeParams,
    Domain, FtsTermBudget, KnowledgeHandlers, LexicalPass, QueryEmbeddingCache,
    RoleSpecificEmbedding, ScoredTextItem, SearchCoreOutcome, SearchCtx, SearchParams,
    SuggestParams, Weights, CHARS_PER_TOKEN, D_SUGGEST_RERANK_ALPHA, KG_BLEND_CAP, MIN_TERM_LEN,
};
#[cfg(test)]
use super::{compose_read_tests, MEMBER_SIZING_READ_TIMEOUT};

async fn apply_atom_weights(
    sql: &dyn khive_storage::SqlAccess,
    namespace: &str,
    hits: &mut [super::ScoredHit],
) -> (HashMap<Uuid, f32>, Option<&'static str>) {
    use khive_retrieval::persist::PersistError;
    use khive_storage::{RequestReadStopReason, StorageError};

    let atom_ids: Vec<_> = hits
        .iter()
        .filter(|hit| !hit.is_domain)
        .filter_map(|hit| Uuid::parse_str(&hit.id).ok())
        .collect();
    if atom_ids.is_empty() {
        return (HashMap::new(), None);
    }
    let outcome = khive_storage::await_request_read_phase(
        "knowledge.atom_weights",
        khive_retrieval::weights::batch_load_weights(sql, namespace, &atom_ids),
    )
    .await;
    let weights = match outcome {
        Ok(Ok(weights)) => weights,
        refused => {
            let reason = match khive_storage::capture_request_read_context().stop_reason() {
                Some(RequestReadStopReason::Cancelled) => "cancelled",
                Some(RequestReadStopReason::Deadline) => "timeout",
                None => match refused {
                    Err(StorageError::Timeout { .. } | StorageError::AdmissionTimeout { .. })
                    | Ok(Err(PersistError::Storage(
                        StorageError::Timeout { .. } | StorageError::AdmissionTimeout { .. },
                    ))) => "timeout",
                    _ => "storage",
                },
            };
            return (HashMap::new(), Some(reason));
        }
    };
    for hit in hits.iter_mut().filter(|hit| !hit.is_domain) {
        let weight = Uuid::parse_str(&hit.id)
            .ok()
            .and_then(|id| weights.get(&id).copied())
            .unwrap_or(1.0);
        hit.score *= weight;
    }
    (weights, None)
}

#[cfg(test)]
mod atom_weight_tests {
    use super::super::{ScoreProvenance, ScoredHit};
    use super::apply_atom_weights;
    use khive_storage::types::SqlColumn;
    use khive_storage::{
        AtomicUnitOp, SqlAccess, SqlReader, SqlRow, SqlStatement, SqlValue, SqlWriter,
        StorageError, StorageResult,
    };
    use std::any::Any;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use uuid::Uuid;

    #[derive(Clone, Copy)]
    enum Refusal {
        Timeout,
        Admission,
        Storage,
    }

    impl Refusal {
        fn error(self) -> StorageError {
            match self {
                Self::Timeout => StorageError::Timeout {
                    operation: "weights fixture".into(),
                },
                Self::Admission => StorageError::AdmissionTimeout {
                    operation: "weights fixture".into(),
                    timeout_ms: 37,
                    pool_identity: Some("weights-pool".into()),
                },
                Self::Storage => StorageError::Pool {
                    operation: "weights fixture".into(),
                    message: "injected refusal".into(),
                },
            }
        }
    }

    #[derive(Clone)]
    struct RecordingSql {
        readers: Arc<AtomicUsize>,
        statements: Arc<Mutex<Vec<SqlStatement>>>,
        rows: Vec<SqlRow>,
        refusal: Option<Refusal>,
        refuse_reader: bool,
        query_started: Option<Arc<tokio::sync::Notify>>,
    }

    impl RecordingSql {
        fn new() -> Self {
            Self {
                readers: Arc::new(AtomicUsize::new(0)),
                statements: Arc::new(Mutex::new(Vec::new())),
                rows: vec![weight_row(1, 1.5), weight_row(2, 5.0)],
                refusal: None,
                refuse_reader: false,
                query_started: None,
            }
        }
    }

    #[async_trait::async_trait]
    impl SqlAccess for RecordingSql {
        async fn reader(&self) -> StorageResult<Box<dyn SqlReader>> {
            self.readers.fetch_add(1, Ordering::SeqCst);
            if self.refuse_reader {
                return Err(self.refusal.expect("reader refusal").error());
            }
            Ok(Box::new(self.clone()))
        }

        async fn writer(&self) -> StorageResult<Box<dyn SqlWriter>> {
            panic!("weights must not acquire a writer")
        }

        async fn atomic_unit(&self, _: AtomicUnitOp) -> StorageResult<Box<dyn Any + Send>> {
            panic!("weights must not enter a write transaction")
        }
    }

    #[async_trait::async_trait]
    impl SqlReader for RecordingSql {
        async fn query_all(&mut self, statement: SqlStatement) -> StorageResult<Vec<SqlRow>> {
            assert_eq!(statement.label.as_deref(), Some("weights.batch_load"));
            self.statements.lock().unwrap().push(statement);
            if let Some(started) = &self.query_started {
                started.notify_one();
                std::future::pending::<()>().await;
            }
            match self.refusal {
                Some(refusal) => Err(refusal.error()),
                None => Ok(self.rows.clone()),
            }
        }

        async fn query_row(&mut self, _: SqlStatement) -> StorageResult<Option<SqlRow>> {
            panic!("weights must use a batch read")
        }

        async fn query_scalar(&mut self, _: SqlStatement) -> StorageResult<Option<SqlValue>> {
            panic!("weights must use a batch read")
        }

        async fn explain(&mut self, _: SqlStatement) -> StorageResult<Vec<SqlRow>> {
            panic!("weights must not explain the query")
        }
    }

    fn weight_row(id: u128, weight: f64) -> SqlRow {
        SqlRow {
            columns: vec![
                SqlColumn {
                    name: "atom_id".into(),
                    value: SqlValue::Text(Uuid::from_u128(id).to_string()),
                },
                SqlColumn {
                    name: "weight".into(),
                    value: SqlValue::Float(weight),
                },
            ],
        }
    }

    fn hit(id: u128, is_domain: bool) -> ScoredHit {
        ScoredHit {
            id: Uuid::from_u128(id).to_string(),
            slug: format!("atom-{id:03}"),
            name: format!("atom {id}"),
            content: None,
            tags: None,
            atom_embed_text: None,
            finalized: true,
            is_domain,
            status: Some("reviewed".into()),
            score: 0.8,
            provenance: ScoreProvenance::lexical(),
        }
    }

    #[tokio::test]
    async fn atom_weights_batch_candidates_once_and_preserve_defaults_and_domains() {
        let sql = RecordingSql::new();
        let mut hits: Vec<_> = (1..=200).map(|id| hit(id, false)).collect();
        hits.push(hit(201, true));
        let (weights, unavailable) = apply_atom_weights(&sql, "request-ns", &mut hits).await;
        assert_eq!(unavailable, None);
        assert_eq!(weights.len(), 2);
        assert_eq!(hits[0].score.to_bits(), (0.8_f32 * 1.5).to_bits());
        assert_eq!(hits[1].score, 4.0, "learned scores are not clamped");
        assert!(hits[2..]
            .iter()
            .all(|hit| hit.score.to_bits() == 0.8_f32.to_bits()));
        assert_eq!(sql.readers.load(Ordering::SeqCst), 1);
        let statements = sql.statements.lock().unwrap();
        assert_eq!(statements.len(), 1);
        assert_eq!(statements[0].params.len(), 201);
        assert!(matches!(&statements[0].params[0], SqlValue::Text(ns) if ns == "request-ns"));
        let ids: Vec<_> = statements[0].params[1..]
            .iter()
            .map(|value| match value {
                SqlValue::Text(id) => id.clone(),
                _ => panic!("atom ID must be text"),
            })
            .collect();
        assert_eq!(
            ids,
            (1..=200)
                .map(|id| Uuid::from_u128(id).to_string())
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn atom_weights_empty_or_domain_only_candidates_do_not_read() {
        let sql = RecordingSql::new();
        for mut hits in [Vec::new(), vec![hit(1, true)]] {
            let (weights, unavailable) = apply_atom_weights(&sql, "local", &mut hits).await;
            assert!(weights.is_empty());
            assert_eq!(unavailable, None);
            assert!(hits.iter().all(|hit| hit.score == 0.8));
        }
        assert_eq!(sql.readers.load(Ordering::SeqCst), 0);
        assert!(sql.statements.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn atom_weights_reader_and_query_failures_keep_nonempty_hits_unweighted() {
        for refuse_reader in [false, true] {
            for (refusal, expected) in [
                (Refusal::Timeout, "timeout"),
                (Refusal::Admission, "timeout"),
                (Refusal::Storage, "storage"),
            ] {
                let mut sql = RecordingSql::new();
                sql.refusal = Some(refusal);
                sql.refuse_reader = refuse_reader;
                let mut hits = vec![hit(1, false), hit(2, false), hit(3, true)];
                let (weights, unavailable) = apply_atom_weights(&sql, "local", &mut hits).await;
                assert!(weights.is_empty());
                assert_eq!(unavailable, Some(expected));
                assert_eq!(hits.len(), 3);
                assert!(hits
                    .iter()
                    .all(|hit| hit.score.to_bits() == 0.8_f32.to_bits()));
                assert_eq!(sql.readers.load(Ordering::SeqCst), 1);
                assert_eq!(
                    sql.statements.lock().unwrap().len(),
                    usize::from(!refuse_reader)
                );
            }
        }
    }

    #[tokio::test]
    async fn atom_weights_cancelled_request_keeps_nonempty_hits_without_admission() {
        let sql = RecordingSql::new();
        let mut hits = vec![hit(1, false)];
        let (_sender, receiver) = tokio::sync::watch::channel(true);
        let (weights, unavailable) = khive_storage::scope_request_read_cancellation(
            receiver,
            apply_atom_weights(&sql, "local", &mut hits),
        )
        .await;
        assert!(weights.is_empty());
        assert_eq!(unavailable, Some("cancelled"));
        assert_eq!(hits[0].score, 0.8);
        assert_eq!(sql.readers.load(Ordering::SeqCst), 0);
        assert!(sql.statements.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn atom_weights_expired_request_keeps_nonempty_hits_without_new_deadline() {
        let sql = RecordingSql::new();
        let mut hits = vec![hit(1, false)];
        let (weights, unavailable) = khive_storage::scope_request_read_deadline(
            Duration::ZERO,
            apply_atom_weights(&sql, "local", &mut hits),
        )
        .await;
        assert!(weights.is_empty());
        assert_eq!(unavailable, Some("timeout"));
        assert_eq!(hits[0].score, 0.8);
        assert_eq!(sql.readers.load(Ordering::SeqCst), 0);
        assert!(sql.statements.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn atom_weights_cancellation_during_query_keeps_nonempty_hits_without_retry() {
        let mut sql = RecordingSql::new();
        let started = Arc::new(tokio::sync::Notify::new());
        sql.query_started = Some(started.clone());
        let mut hits = vec![hit(1, false), hit(2, false)];
        let (sender, receiver) = tokio::sync::watch::channel(false);
        let read = khive_storage::scope_request_read_cancellation(
            receiver,
            apply_atom_weights(&sql, "local", &mut hits),
        );
        let cancel = async {
            started.notified().await;
            sender.send(true).expect("active request receiver");
        };
        // Test-only virtual-time guard: a context-ignoring adapter must not hang the call.
        let ((weights, unavailable), ()) =
            tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(read, cancel) })
                .await
                .expect("ambient cancellation must stop the pending read");
        assert!(weights.is_empty());
        assert_eq!(unavailable, Some("cancelled"));
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|hit| hit.score == 0.8));
        assert_eq!(sql.readers.load(Ordering::SeqCst), 1);
        assert_eq!(sql.statements.lock().unwrap().len(), 1);
    }
}

// ─── handler impls ────────────────────────────────────────────────────────────

impl KnowledgeHandlers {
    pub(crate) async fn search(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
        ann: &vamana::SharedAnn,
    ) -> Result<Value, RuntimeError> {
        khive_storage::ensure_request_read_active("knowledge.search")?;
        let p: SearchParams = deser(params)?;
        let raw_query = p.query.trim().to_string();
        if raw_query.is_empty() {
            return Err(RuntimeError::InvalidInput("query must not be empty".into()));
        }

        if let Some(ms) = p.min_score {
            if !ms.is_finite() {
                return Err(RuntimeError::InvalidInput(
                    "min_score must be a finite number".into(),
                ));
            }
        }
        if let Some(ib) = p.intersection_bonus {
            if !ib.is_finite() {
                return Err(RuntimeError::InvalidInput(
                    "intersection_bonus must be a finite number".into(),
                ));
            }
        }
        if let Some(ra) = p.rerank_alpha {
            if !ra.is_finite() {
                return Err(RuntimeError::InvalidInput(
                    "rerank_alpha must be a finite number".into(),
                ));
            }
        }
        if let Some(ref w) = p.weights {
            let pairs: &[(&str, Option<f64>)] = &[
                ("w_exact_name", w.w_exact_name),
                ("w_name", w.w_name),
                ("w_tags", w.w_tags),
                ("w_content", w.w_content),
                ("expand_discount", w.expand_discount),
                ("coverage_alpha", w.coverage_alpha),
                ("w_bigram", w.w_bigram),
            ];
            for (name, val) in pairs {
                if let Some(v) = val {
                    if !v.is_finite() {
                        return Err(RuntimeError::InvalidInput(format!(
                            "weights.{name} must be a finite number"
                        )));
                    }
                }
            }
        }

        let limit = p.limit.unwrap_or(10).clamp(1, 100);
        let min_score = p.min_score.unwrap_or(0.0) as f32;
        let admission_min_score = min_score.min(0.0);
        let w = Weights::from_opts(&p);
        if let Some(kind) = p.kind.as_deref() {
            if !matches!(kind, "atom" | "domain") {
                return Err(RuntimeError::InvalidInput(format!(
                    "kind must be one of: atom, domain; got {kind:?}"
                )));
            }
        }
        let type_filter = p.kind.as_deref();
        let do_decompose = p.decompose.unwrap_or(false);
        let decompose_threshold = p.decompose_threshold.unwrap_or(4);
        let intersection_bonus = p.intersection_bonus.unwrap_or(0.25) as f32;
        let requested_rerank = p.rerank.unwrap_or(true);
        let do_rerank = requested_rerank && !runtime.default_embedder_name().is_empty();
        let rerank_alpha = p.rerank_alpha.unwrap_or(0.7) as f32;
        // Weighting must see candidates beyond the requested output limit.
        let fetch_limit = 100;

        let non_stop_count = raw_query
            .split_whitespace()
            .filter(|w| w.len() >= MIN_TERM_LEN && !is_stop(&w.to_lowercase()))
            .count();

        let ns = token.namespace().as_str().to_owned();
        if let Some(status) = p.status.as_ref() {
            let valid_shape = match status {
                Value::String(_) => true,
                Value::Array(items) => items.iter().all(Value::is_string),
                _ => false,
            };
            if !valid_shape {
                return Err(RuntimeError::InvalidInput(
                    "status must be a string or an array of strings".into(),
                ));
            }
        }
        let requested_statuses = status_values(p.status.as_ref());

        // Normalize exclude_status once: trim whitespace, treat blank as absent.
        // This single normalized value feeds both the SQL predicate (via SearchCtx)
        // and the ANN post-hydration filter, ensuring both result sources see the
        // identical exclusion set regardless of how the caller formatted the value.
        let exclude_status_normalized: Option<&str> = p
            .exclude_status
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(status) = exclude_status_normalized {
            if !matches!(status, "draft" | "reviewed" | "deprecated") {
                return Err(RuntimeError::InvalidInput(format!(
                    "exclude_status must be one of: draft, reviewed, deprecated; got {status:?}"
                )));
            }
        }

        // Precedence (highest to lowest, matches ADR-047 §Status filtering):
        //   1. explicit status=  → no exclusion; SQL and hydrated ANN use the allowlist
        //   2. no status=, explicit exclude_status= (non-blank) → use that exclusion
        //   3. no status=, include_drafts=true → exclude only deprecated
        //   4. default (no status params / blank exclude_status) → exclude draft and deprecated
        let effective_exclude_statuses: Vec<&str> = if !requested_statuses.is_empty() {
            // Caller specified exact status; the shared allowlist wins.
            vec![]
        } else if let Some(ex) = exclude_status_normalized {
            vec![ex]
        } else {
            let include_drafts = p.include_drafts.unwrap_or(false);
            if include_drafts {
                vec!["deprecated"]
            } else {
                vec!["draft", "deprecated"]
            }
        };
        // The zero multiplier for deprecated is also a final eligibility gate.
        // Resolve its override from the same precedence policy used before FTS
        // caps and ANN refill; otherwise explicit exclude_status can admit a
        // row early only for the multiplier stage to remove it later.
        let allow_deprecated =
            deprecated_allowed_by_status_policy(&requested_statuses, &effective_exclude_statuses);

        let term_budget = FtsTermBudget::new();
        let ctx = SearchCtx {
            runtime,
            ns: &ns,
            role: p.role.as_deref(),
            type_filter,
            min_score: admission_min_score,
            w: &w,
            fetch_limit,
            statuses: &requested_statuses,
            exclude_statuses: &effective_exclude_statuses,
            term_budget: &term_budget,
        };

        let ann_k = fetch_limit.max(20);
        let (
            SearchCoreOutcome {
                mut hits,
                lexical_timeouts,
                lexical_state,
            },
            CandidateStageOutcome {
                ann_hits,
                ann_availability,
                hydration_failures,
                mut query_embedding,
            },
        ) = run_candidate_stages(
            &ctx,
            token,
            ann,
            &raw_query,
            CandidateStageOptions {
                query_embedding: QueryEmbeddingCache::default(),
                ann_target: ann_k,
                ann_initial_k: ann_k,
                operation: "knowledge.search",
            },
            async {
                if do_decompose && non_stop_count >= decompose_threshold {
                    search_decomposed(&ctx, &raw_query, intersection_bonus).await
                } else {
                    search_core(&ctx, &raw_query, LexicalPass::Full).await
                }
            },
        )
        .await?;

        let mut ann_unavailable = false;
        if !ann_hits.is_empty() {
            fuse_ann_hits(&mut hits, &ann_hits, admission_min_score);
        }
        // FTS hits remain valid partial results. Preserve the existing
        // advisory only for a non-empty corpus with no lexical fallback.
        if matches!(
            ann_availability,
            Some(AnnAvailability::WarmingTimedOut {
                corpus_non_empty: true
            })
        ) && hits.is_empty()
        {
            ann_unavailable = true;
        }
        // Apply shared eligibility unconditionally so every source observes the
        // same final status and kind contract even when ANN did not run.
        filter_hits_by_status(&mut hits, &requested_statuses, &effective_exclude_statuses);
        filter_hits_by_type(&mut hits, type_filter);

        // The lexical stage now owns its own budget (issue #1930 Amendment
        // 2), so `lexical_timeout` no longer implies the request read
        // deadline is spent — only that stage's narrower budget is. Gate on
        // the live ambient deadline instead: a lexical-only degradation
        // with request time left to spare still gets its embedding rerank.
        let mut rerank_provenance = None;
        if do_rerank && !hits.is_empty() && !khive_storage::request_read_is_cancelled() {
            rerank_provenance = rerank_search_with_stored_vectors(
                runtime,
                token,
                &ns,
                &raw_query,
                &mut query_embedding,
                &mut hits,
                rerank_alpha,
            )
            .await?;
        }

        apply_status_multipliers(&mut hits, allow_deprecated);
        let (atom_weights, atom_weights_unavailable) =
            apply_atom_weights(runtime.sql().as_ref(), &ns, &mut hits).await;
        enforce_min_score_floor(&mut hits, min_score);
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.slug.cmp(&b.slug))
        });
        hits.truncate(limit);

        let atom_ids: Vec<String> = hits
            .iter()
            .filter(|hit| !hit.is_domain)
            .map(|hit| hit.id.clone())
            .collect();
        let mut body_lines_timed_out = false;
        let body_line_counts = if khive_storage::request_read_is_cancelled() {
            None
        } else {
            match load_atom_body_line_counts(runtime, &ns, &atom_ids).await? {
                Some(counts) => Some(counts),
                None => {
                    body_lines_timed_out = true;
                    None
                }
            }
        };

        let results: Vec<Value> = hits
            .iter()
            .map(|h| {
                let atom_weight = if h.is_domain {
                    1.0
                } else {
                    Uuid::parse_str(&h.id)
                        .ok()
                        .and_then(|id| atom_weights.get(&id).copied())
                        .unwrap_or(1.0)
                };
                let body_lines = if h.is_domain {
                    None
                } else {
                    body_line_counts
                        .as_ref()
                        .and_then(|counts| counts.get(&h.id).copied())
                };
                json!({
                    "id": h.id,
                    "slug": h.slug,
                    "name": h.name,
                    "content": h.content,
                    "body_lines": body_lines,
                    "tags": h.tags,
                    "status": h.status,
                    "finalized": h.finalized,
                    "kind": if h.is_domain { "domain" } else { "atom" },
                    "score": h.score,
                    "atom_weight": atom_weight,
                    "score_provenance": h.provenance.to_json(),
                })
            })
            .collect();
        let count = results.len();

        let mut out = json!({
            "results": results,
            "total": count,
            "candidate_provenance": {
                "lexical": lexical_state.as_str(),
                "fallback": candidate_fallback(&hits),
                "terms_truncated": term_budget.truncated(),
            },
        });
        if let Some(provenance) = rerank_provenance {
            out["rerank_provenance"] = provenance;
        }
        if ann_unavailable {
            out["ann_unavailable"] = json!(true);
        }
        attach_lexical_timeout_degradation(&mut out, &lexical_timeouts);
        if body_lines_timed_out {
            attach_body_lines_timeout_degradation(&mut out);
        }
        attach_hydration_degradation(&mut out, hydration_failures);
        if let Some(reason) = atom_weights_unavailable {
            if !out.get("degraded").is_some_and(Value::is_object) {
                out["degraded"] = json!({});
            }
            out["degraded"]["atom_weights_unavailable"] = json!(reason);
        }
        // The lexical stage's own budget no longer implies the request
        // deadline is spent (issue #1930 Amendment 2), so this last check
        // re-reads the live ambient deadline rather than the stage-local
        // flags: skip only when the request has actually stopped, never a
        // verb-level error for a degradation already reported above.
        if !khive_storage::request_read_is_cancelled() {
            khive_storage::ensure_request_read_active("knowledge.search")?;
        }
        Ok(out)
    }

    /// Suggest domains with measured compose-member costs and live member counts.
    /// Present domains without live members have size zero and members zero.
    /// Every unmeasured domain is withheld under the stable
    /// `degraded.member_sizing_timeout.excluded` key, whether sizing timed out or not.
    pub(crate) async fn suggest(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
        ann: &vamana::SharedAnn,
    ) -> Result<Value, RuntimeError> {
        let (out, _) = Self::suggest_with_query_embedding(
            runtime,
            token,
            params,
            ann,
            QueryEmbeddingCache::default(),
        )
        .await?;
        Ok(out)
    }

    /// Internal suggest path that accepts and returns the request's cached
    /// query vector. Auto-compose calls this directly so its suggest, atom,
    /// section, and KG stages all share one successful query embedding.
    async fn suggest_with_query_embedding(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
        ann: &vamana::SharedAnn,
        query_embedding: QueryEmbeddingCache,
    ) -> Result<(Value, QueryEmbeddingCache), RuntimeError> {
        khive_storage::ensure_request_read_active("knowledge.suggest")?;
        let p: SuggestParams = deser(params)?;
        let raw_query = p.query.trim().to_string();
        if raw_query.is_empty() {
            return Err(RuntimeError::InvalidInput("query must not be empty".into()));
        }
        let word_count = raw_query.split_whitespace().count();
        if word_count < 5 {
            return Err(RuntimeError::InvalidInput(format!(
                "suggest query must be at least 5 words for meaningful domain matching \
                 (got {word_count}). Use knowledge.search for short keyword queries."
            )));
        }
        let limit = p.limit.unwrap_or(8).clamp(1, 100);
        let ns = token.namespace().as_str().to_owned();

        // Exclude draft and deprecated domain atoms by default — same quality
        // default as knowledge.search.  Draft domain atoms are incomplete and
        // should not drive auto-compose or agent orientation.
        const SUGGEST_EXCLUDE: &[&str] = &["draft", "deprecated"];

        let term_budget = FtsTermBudget::new();
        let ctx = SearchCtx {
            runtime,
            ns: &ns,
            role: p.role.as_deref(),
            type_filter: Some("domain"),
            min_score: 0.0,
            w: &Weights::default(),
            fetch_limit: limit * 3,
            statuses: &[],
            exclude_statuses: SUGGEST_EXCLUDE,
            term_budget: &term_budget,
        };

        // Over-fetch aggressively: the corpus is ~27% domains / ~73% atoms, so
        // limit*3 would return mostly atoms that all get dropped after type filtering.
        // 50× over-fetch (floor 200) gives domains a fair chance to appear in the
        // top ANN neighbors before the type gate discards atom hits.
        let ann_k = (limit * 50).max(200);
        let (
            SearchCoreOutcome {
                mut hits,
                lexical_timeouts,
                ..
            },
            CandidateStageOutcome {
                ann_hits,
                ann_availability,
                hydration_failures,
                mut query_embedding,
            },
        ) = run_candidate_stages(
            &ctx,
            token,
            ann,
            &raw_query,
            CandidateStageOptions {
                query_embedding,
                ann_target: ctx.fetch_limit,
                ann_initial_k: ann_k,
                operation: "knowledge.suggest",
            },
            search_core(&ctx, &raw_query, LexicalPass::Full),
        )
        .await?;

        let mut ann_unavailable = false;
        if !ann_hits.is_empty() {
            fuse_ann_hits(&mut hits, &ann_hits, 0.0);
        }
        // Suggest always reports degraded candidate recall for a
        // non-empty corpus, even when lexical candidates survived.
        if let Some(AnnAvailability::WarmingTimedOut { corpus_non_empty }) = ann_availability {
            ann_unavailable = corpus_non_empty;
        }

        filter_hits_by_status(&mut hits, &[], SUGGEST_EXCLUDE);
        filter_hits_by_type(&mut hits, Some("domain"));

        // The lexical stage now owns its own budget (issue #1930 Amendment
        // 2), so `lexical_timeout` no longer implies the request read
        // deadline is spent — only that stage's narrower budget is. Gate on
        // the live ambient deadline instead: a lexical-only degradation
        // with request time left to spare still gets its embedding rerank.
        let fresh_rerank_applied = if khive_storage::request_read_is_cancelled() {
            false
        } else {
            rerank_with_embeddings(
                runtime,
                &raw_query,
                &mut query_embedding,
                &mut hits,
                D_SUGGEST_RERANK_ALPHA,
            )
            .await?
        };

        // Safety net: retain only domain hits in case any non-domain survived above.
        hits.retain(|h| h.is_domain);
        hits.truncate(limit);

        let domain_ids: Vec<String> = hits.iter().map(|h| h.id.clone()).collect();
        // A cancelled ambient deadline skips the call the same way an
        // internal timeout inside it does — both leave every domain in this
        // batch unmeasured, never a measured member cost
        // (issue #2396 fix 3).
        let (member_token_sizes, member_sizing_timed_out) =
            if khive_storage::request_read_is_cancelled() {
                (HashMap::new(), !domain_ids.is_empty())
            } else {
                #[cfg(test)]
                let measured = if MEMBER_SIZING_READ_TIMEOUT.try_with(|_| ()).is_ok() {
                    khive_storage::scope_request_read_deadline(
                        std::time::Duration::ZERO,
                        load_domain_member_token_sizes(runtime, &ns, &domain_ids),
                    )
                    .await
                } else {
                    load_domain_member_token_sizes(runtime, &ns, &domain_ids).await
                };
                #[cfg(not(test))]
                let measured = load_domain_member_token_sizes(runtime, &ns, &domain_ids).await;
                measured?
            };
        if !khive_storage::request_read_is_cancelled() {
            khive_storage::ensure_request_read_active("knowledge.suggest")?;
        }
        // A domain the sizing pass returned no row for is unmeasured as well:
        // a missing entry never defaults to a fabricated zero.
        let unmeasured_domain_ids: HashSet<&str> = domain_ids
            .iter()
            .map(String::as_str)
            .filter(|id| member_sizing_timed_out || !member_token_sizes.contains_key(*id))
            .collect();

        // Price the member atom bodies that compose expands, not the much smaller
        // domain mirror description used for retrieval. The batched join keeps the
        // suggest -> fold budget in compose's estimated-token unit without an N+1
        // hydration pass. A domain whose members were not measured is withheld
        // from `results` entirely and reported under
        // `degraded.member_sizing_timeout.excluded` instead — `suggest`'s
        // documented contract (issue #105) is that `results` feeds
        // `knowledge.fold`'s `candidates` unmodified, and `FoldCandidate::size`
        // is a non-optional `usize`, so leaving an unpriced item in `results`
        // (as `size: null`, issue #2396 fix 3) turned one unmeasured domain into
        // a hard parse error for the whole fold request. Exclusion keeps the
        // passthrough valid while still refusing to let an unpriced domain enter
        // a budgeted fold selection for free.
        let results: Vec<Value> = hits
            .iter()
            .filter(|h| !unmeasured_domain_ids.contains(h.id.as_str()))
            .filter_map(|h| {
                let sizing = member_token_sizes.get(&h.id)?;
                Some(json!({
                    "id": h.id,
                    "name": h.name,
                    "score": h.score,
                    "size": sizing.tokens,
                    "members": sizing.live_members,
                }))
            })
            .collect();
        let count = results.len();
        let excluded: Vec<Value> = hits
            .iter()
            .enumerate()
            .filter(|(_, h)| unmeasured_domain_ids.contains(h.id.as_str()))
            .map(|(i, h)| {
                json!({
                    "id": h.id,
                    "name": h.name,
                    "rank": i + 1,
                    "score": h.score,
                })
            })
            .collect();

        let mut out = json!({ "results": results, "total": count });
        if ann_unavailable {
            // issue #91: escalate degradation to a top-level, self-explaining
            // signal instead of a bare total:0 or an unflagged partial list.
            // `ann_unavailable` is kept unchanged for existing callers; `degraded`
            // states the consequence so a caller does not have to infer it.
            out["ann_unavailable"] = json!(true);
            let (mode, note): (&str, &str) = match (count, fresh_rerank_applied) {
                (0, _) => (
                    "no_match",
                    "ANN index unavailable and lexical/FTS matching also found no \
                     domain for this query. This does NOT confirm the corpus has \
                     nothing relevant — only that this call could not find one. \
                     Do not cache as an absence; retry once the index is healthy.",
                ),
                (_, true) => (
                    "ann_candidates_degraded",
                    "ANN index unavailable: candidate retrieval used lexical/FTS \
                     matching, but fresh embedding cosine reranking was applied to \
                     those candidates. Final ranking includes a dense signal, while \
                     topically relevant domains outside the lexical candidate set may \
                     still be missing. Do not cache; retry once the index is healthy.",
                ),
                (_, false) => (
                    "lexical_only",
                    "ANN index unavailable and fresh embedding reranking did not run: \
                     these results were ranked by lexical/FTS matching only. Ranking \
                     may be less precise than a healthy call, and topically relevant \
                     domains outside the lexical match may be missing. Do not cache; \
                     retry once the index is healthy.",
                ),
            };
            out["degraded"] = json!({
                "reason": "ann_unavailable",
                "mode": mode,
                "cache_safe": false,
                "note": note,
            });
        }
        attach_lexical_timeout_degradation(&mut out, &lexical_timeouts);
        attach_hydration_degradation(&mut out, hydration_failures);
        attach_member_sizing_timeout_degradation(&mut out, &excluded);
        // The lexical stage's own budget no longer implies the request
        // deadline is spent (issue #1930 Amendment 2), so this last check
        // re-reads the live ambient deadline rather than the stage-local
        // flag: skip only when the request has actually stopped, never a
        // verb-level error for a degradation already reported above.
        if !khive_storage::request_read_is_cancelled() {
            khive_storage::ensure_request_read_active("knowledge.suggest")?;
        }
        Ok((out, query_embedding))
    }

    pub(super) const COMPOSE_ATOM_CHUNK_SIZE: usize = 64;

    pub(super) fn compose_domain_member_ids(atoms: &[Atom], members: &[String]) -> HashSet<String> {
        let mut first_by_slug = HashMap::with_capacity(atoms.len());
        for atom in atoms {
            first_by_slug.entry(atom.slug.as_str()).or_insert(atom.id);
        }
        members
            .iter()
            .filter_map(|slug| first_by_slug.get(slug.as_str()))
            .map(|id| id.to_string())
            .collect()
    }

    pub(super) async fn load_compose_atom_window(
        runtime: &KhiveRuntime,
        ns: &str,
        references: &[String],
    ) -> Result<Vec<Result<Atom, RuntimeError>>, RuntimeError> {
        if references.is_empty() {
            return Ok(Vec::new());
        }
        assert!(references.len() <= Self::COMPOSE_ATOM_CHUNK_SIZE);
        khive_storage::ensure_request_read_active("knowledge.compose")?;
        let raw: Vec<_> = references
            .iter()
            .map(|reference| reference.trim())
            .collect();
        let mut inputs = Vec::with_capacity(raw.len());
        let mut params = vec![SqlValue::Text(ns.to_owned())];
        for (ordinal, reference) in raw.iter().enumerate() {
            let is_uuid = Uuid::parse_str(reference).is_ok();
            let is_prefix = !is_uuid
                && reference.len() >= 8
                && reference.len() <= 36
                && reference
                    .chars()
                    .all(|character| character.is_ascii_hexdigit() || character == '-');
            let first = params.len() + 1;
            inputs.push(format!(
                "({ordinal},?{first},{},?{})",
                usize::from(is_uuid),
                first + 1
            ));
            params.push(SqlValue::Text((*reference).to_owned()));
            params.push(if is_prefix {
                SqlValue::Text(format!("{}%", hex_prefix_to_uuid_pattern(reference)))
            } else {
                SqlValue::Null
            });
        }
        let statement = SqlStatement {
            sql: format!(
                "WITH input(ordinal,raw_ref,is_uuid,prefix) AS (VALUES {}) \
                 SELECT input.ordinal AS compose_ordinal, \
                    CASE WHEN input.is_uuid=0 AND NOT EXISTS( \
                        SELECT 1 FROM knowledge_atoms slug WHERE slug.slug=input.raw_ref \
                        AND slug.namespace=?1 AND slug.deleted_at IS NULL) THEN 1 ELSE 0 END AS compose_prefix, atom.* \
                 FROM input JOIN knowledge_atoms atom ON \
                    atom.rowid IN(SELECT by_id.rowid FROM knowledge_atoms by_id \
                        WHERE input.is_uuid=1 AND by_id.id=input.raw_ref \
                        AND by_id.namespace=?1 AND by_id.deleted_at IS NULL \
                        UNION ALL SELECT by_slug.rowid FROM knowledge_atoms by_slug \
                        WHERE input.is_uuid=0 AND by_slug.slug=input.raw_ref \
                        AND by_slug.namespace=?1 AND by_slug.deleted_at IS NULL LIMIT 1) \
                    OR atom.rowid IN(SELECT prefixed.rowid FROM knowledge_atoms prefixed \
                        WHERE input.is_uuid=0 AND input.prefix IS NOT NULL \
                        AND NOT EXISTS(SELECT 1 FROM knowledge_atoms slug \
                            WHERE slug.slug=input.raw_ref AND slug.namespace=?1 AND slug.deleted_at IS NULL) \
                        AND prefixed.namespace=?1 AND prefixed.deleted_at IS NULL \
                        AND prefixed.id LIKE input.prefix LIMIT 2) \
                 ORDER BY input.ordinal",
                inputs.join(",")
            ),
            params,
            label: Some("compose_atom_window".into()),
        };
        let context = if Uuid::parse_str(raw[0]).is_ok() {
            "compose atom by id"
        } else {
            "compose atom by slug"
        };
        let access = runtime.sql();
        let mut reader = access
            .reader()
            .await
            .map_err(|error| sql_err("compose atom reader", error))?;
        #[cfg(test)]
        compose_read_tests::observe_reader();
        #[cfg(test)]
        let before = runtime
            .backend()
            .pool()
            .reader_acquisition_snapshot()
            .pooled_checkouts;
        let rows = reader
            .query_all(statement)
            .await
            .map_err(|error| sql_err(context, error))?;
        #[cfg(test)]
        compose_read_tests::observe_query(runtime, before);
        let mut aligned: Vec<Vec<khive_storage::types::SqlRow>> =
            (0..raw.len()).map(|_| Vec::new()).collect();
        for row in rows {
            let ordinal = row_i64(&row, "compose_ordinal")
                .and_then(|ordinal| usize::try_from(ordinal).ok())
                .filter(|ordinal| *ordinal < aligned.len())
                .ok_or_else(|| {
                    RuntimeError::Internal("invalid compose atom window ordinal".into())
                })?;
            aligned[ordinal].push(row);
        }
        let outcomes = raw
            .iter()
            .zip(aligned)
            .map(|(reference, rows)| {
                if rows.len() > 1
                    && rows.first().and_then(|row| row_i64(row, "compose_prefix")) == Some(1)
                {
                    return Err(RuntimeError::InvalidInput(format!(
                        "ambiguous atom prefix {reference:?} matches multiple atoms"
                    )));
                }
                rows.into_iter()
                    .next()
                    .and_then(|row| atom_from_row(&row))
                    .ok_or_else(|| RuntimeError::NotFound(format!("atom not found: {reference:?}")))
            })
            .collect();
        #[cfg(test)]
        compose_read_tests::pause_after_window().await;
        Ok(outcomes)
    }

    pub(crate) async fn compose(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
        ann: &vamana::SharedAnn,
        type_weights: HashMap<String, f32>,
    ) -> Result<Value, RuntimeError> {
        let p: ComposeParams = deser(params)?;

        // Registry dispatch already mints an exact token for an explicit
        // namespace. Direct handler callers must provide that same authorized
        // token; never turn an untrusted business parameter into a stronger
        // namespace capability here.
        let effective_token = match p.namespace.as_deref() {
            Some(ns_str) => {
                let ns = Namespace::parse(ns_str).map_err(|e| {
                    RuntimeError::InvalidInput(format!("invalid namespace {ns_str:?}: {e}"))
                })?;
                if &ns != token.namespace() {
                    return Err(RuntimeError::InvalidInput(
                        "knowledge.compose namespace does not match authorized token namespace"
                            .to_string(),
                    ));
                }
                // Equality above makes this a safe exact-scope narrowing of
                // any broader direct-call token.
                token.with_namespace(ns)
            }
            None => token.clone(),
        };
        let token = &effective_token;

        let raw_query = p.query.trim().to_string();
        if raw_query.is_empty() {
            return Err(RuntimeError::InvalidInput("query must not be empty".into()));
        }
        let explain = p.explain.unwrap_or(false);

        let mut domain_ids: Vec<String> = p
            .domain_ids
            .unwrap_or_default()
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .collect();
        let atom_ids: Vec<String> = p
            .atom_ids
            .unwrap_or_default()
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .collect();

        let is_auto = domain_ids.is_empty() && atom_ids.is_empty();
        // `atom_ids`-only calls (caller pinned exact atoms, no domain_ids) never
        // blend KG entities — the caller opted into exactly those atoms. Auto and
        // explicit-domain_ids calls both blend (ADR-051 Amendment 1).
        let atom_ids_only = domain_ids.is_empty() && !atom_ids.is_empty();
        let blend_kg = p.blend_kg.unwrap_or(true) && !atom_ids_only;
        let mut suggest_ann_unavailable = false;
        let mut suggest_hydration_failures = 0usize;
        let mut query_embedding = QueryEmbeddingCache::default();
        if is_auto {
            let word_count = raw_query.split_whitespace().count();
            if word_count < 10 {
                return Err(RuntimeError::InvalidInput(format!(
                    "auto-compose query must be at least 10 words for effective domain selection \
                     (got {word_count}). Provide explicit domain_ids/atom_ids for shorter queries."
                )));
            }
        }

        // #887: unconditional per-stage timing, WARN-on-slow and
        // WARN-on-abandoned. See `super::compose::ComposeTiming` for the
        // full rationale and the completion-contract every early return
        // below must honor (`finish()` before returning, or route the error
        // through `try_or_finish!`). Each `begin(Phase::X)` fires *before*
        // the phase's (possibly fallible, possibly long-running) work — not
        // after — so an in-flight phase is never lost from the breakdown if
        // the request errors, is cancelled, or is abandoned mid-phase.
        use super::compose::Phase;
        let mut timing = super::compose::ComposeTiming::start(&raw_query, is_auto);
        macro_rules! try_or_finish {
            ($e:expr) => {
                match $e {
                    Ok(v) => v,
                    Err(e) => {
                        timing.finish(0);
                        return Err(e.into());
                    }
                }
            };
        }
        try_or_finish!(timing.begin(Phase::Suggest));

        if is_auto {
            let auto_limit = p.auto_limit.unwrap_or(5).clamp(1, 20);
            let suggest_attempt = Self::suggest_with_query_embedding(
                runtime,
                token,
                json!({ "query": &raw_query, "limit": auto_limit }),
                ann,
                std::mem::take(&mut query_embedding),
            )
            .await;
            try_or_finish!(khive_storage::ensure_request_read_active(
                "knowledge.compose"
            ));
            let suggest_result = match suggest_attempt {
                Ok((v, reused_query_embedding)) => {
                    query_embedding = reused_query_embedding;
                    suggest_ann_unavailable = v
                        .get("ann_unavailable")
                        .and_then(|f| f.as_bool())
                        .unwrap_or(false);
                    suggest_hydration_failures = v
                        .pointer("/degraded/hydration_failures")
                        .and_then(Value::as_u64)
                        .and_then(|count| usize::try_from(count).ok())
                        .unwrap_or(0);
                    v
                }
                Err(e) => {
                    try_or_finish!(khive_storage::ensure_request_read_active(
                        "knowledge.compose"
                    ));
                    tracing::warn!(target: "khive_pack_knowledge::knowledge::search", error = %e, "auto-compose: internal suggest failed, returning empty");
                    let response = json!({
                        "status": "ok",
                        "data": {
                            "query": raw_query,
                            "markdown": "# Knowledge Briefing\n\nDomain suggestion unavailable.",
                            "domains": [],
                            "atoms": [],
                            "count": 0,
                            "suggest_error": e.to_string(),
                        },
                    });
                    try_or_finish!(khive_storage::ensure_request_read_active(
                        "knowledge.compose"
                    ));
                    timing.finish(0);
                    return Ok(response);
                }
            };
            if let Some(results) = suggest_result.get("results").and_then(|v| v.as_array()) {
                for r in results.iter().filter(|r| r["members"] != 0) {
                    if let Some(id) = r.get("id").and_then(|v| v.as_str()) {
                        domain_ids.push(id.to_string());
                    }
                }
            }
            if domain_ids.is_empty() {
                let mut data = json!({
                    "query": raw_query,
                    "markdown": "# Knowledge Briefing\n\nNo matching domains found for auto-suggest.",
                    "domains": [],
                    "atoms": [],
                    "count": 0,
                });
                if suggest_ann_unavailable {
                    data["ann_unavailable"] = json!(true);
                }
                attach_hydration_degradation(&mut data, suggest_hydration_failures);
                let response = json!({ "status": "ok", "data": data });
                try_or_finish!(khive_storage::ensure_request_read_active(
                    "knowledge.compose"
                ));
                timing.finish(0);
                return Ok(response);
            }
        }
        try_or_finish!(timing.begin(Phase::Fetch));

        let ns = token.namespace().as_str().to_owned();

        let mut resolved_domains: Vec<Domain> = Vec::new();
        let mut member_slugs: Vec<String> = Vec::new();

        for id in &domain_ids {
            try_or_finish!(khive_storage::ensure_request_read_active(
                "knowledge.compose"
            ));
            let domain = try_or_finish!(load_domain_by_id_or_slug(runtime, &ns, id).await);
            let members = try_or_finish!(parse_domain_members(&domain));
            member_slugs.extend(members);
            resolved_domains.push(domain);
        }

        let mut seen_ids: HashSet<String> = HashSet::new();
        let mut ordered_atoms: Vec<Atom> = Vec::new();
        let mut omitted_members: Vec<String> = Vec::new();

        for window in member_slugs.chunks(Self::COMPOSE_ATOM_CHUNK_SIZE) {
            let outcomes =
                try_or_finish!(Self::load_compose_atom_window(runtime, &ns, window).await);
            for (slug, outcome) in window.iter().zip(outcomes) {
                try_or_finish!(khive_storage::ensure_request_read_active(
                    "knowledge.compose"
                ));
                let atom = match outcome {
                    Ok(atom) => atom,
                    Err(RuntimeError::NotFound(_)) => {
                        omitted_members.push(slug.clone());
                        continue;
                    }
                    Err(error) => {
                        timing.finish(0);
                        return Err(error);
                    }
                };
                if seen_ids.insert(atom.id.to_string()) {
                    ordered_atoms.push(atom);
                }
            }
        }
        for window in atom_ids.chunks(Self::COMPOSE_ATOM_CHUNK_SIZE) {
            let outcomes =
                try_or_finish!(Self::load_compose_atom_window(runtime, &ns, window).await);
            for outcome in outcomes {
                try_or_finish!(khive_storage::ensure_request_read_active(
                    "knowledge.compose"
                ));
                let atom = try_or_finish!(outcome);
                if seen_ids.insert(atom.id.to_string()) {
                    ordered_atoms.push(atom);
                }
            }
        }

        // Auto-compose inherits the same quality default as knowledge.search and
        // knowledge.suggest: draft and deprecated atoms are excluded unless the caller
        // explicitly provided atom_ids (which is an opt-in to whatever those IDs hold).
        if is_auto {
            const COMPOSE_EXCLUDE: &[&str] = &["draft", "deprecated"];
            ordered_atoms.retain(|a| {
                let status = a.status.as_deref().unwrap_or("");
                !COMPOSE_EXCLUDE.contains(&status)
            });
        }

        if ordered_atoms.is_empty() {
            try_or_finish!(khive_storage::ensure_request_read_active(
                "knowledge.compose"
            ));
            let mut data = json!({
                "query": raw_query,
                "markdown": "# Knowledge Briefing\n\nNo atoms found.",
                "domains": [],
                "atoms": [],
                "count": 0,
            });
            if suggest_ann_unavailable {
                data["ann_unavailable"] = json!(true);
            }
            if !omitted_members.is_empty() {
                data["omissions"] = json!(omitted_members);
            }
            attach_hydration_degradation(&mut data, suggest_hydration_failures);
            let response = json!({ "status": "ok", "data": data });
            try_or_finish!(khive_storage::ensure_request_read_active(
                "knowledge.compose"
            ));
            timing.finish(0);
            return Ok(response);
        }

        let mut items: Vec<ScoredTextItem> = ordered_atoms
            .iter()
            .map(|a| ScoredTextItem {
                id: a.id.to_string(),
                slug: a.slug.clone(),
                name: a.name.clone(),
                text: atom_embed_text(a),
                score: 1.0,
            })
            .collect();

        try_or_finish!(timing.begin(Phase::Rerank));
        // The KG blend below requires a role-specific vector (search_kg_entities
        // gates on it, never a generic one — #2307). When this compose call can
        // reach the blend, attempt embed_query directly so a real failure keeps
        // the blend on its degradation path; rerank_text_items's own
        // combined-batch fallback still covers the failure case below. When the
        // blend cannot run anyway (blend_kg is off, or atom_ids_only), skip the
        // solo call and let rerank_text_items embed query + candidates in one
        // combined batch, matching the pre-cache single-job cost.
        if blend_kg
            && query_embedding.role_specific.is_not_attempted()
            && !runtime.default_embedder_name().is_empty()
        {
            let embedded = try_or_finish!(
                khive_storage::await_request_read_phase(
                    "knowledge.compose",
                    runtime.embed_query(&raw_query),
                )
                .await
            );
            query_embedding.role_specific = match embedded {
                Ok(v) => RoleSpecificEmbedding::Vector(v),
                Err(_) => RoleSpecificEmbedding::Failed,
            };
        }
        try_or_finish!(
            rerank_text_items(runtime, &raw_query, &mut query_embedding, &mut items,).await
        );

        let atom_ids: Vec<String> = ordered_atoms.iter().map(|a| a.id.to_string()).collect();
        let atom_cosine_scores: HashMap<String, f32> = items
            .iter()
            .map(|item| (item.id.clone(), item.score))
            .collect();

        try_or_finish!(timing.begin(Phase::Fetch));
        let section_map =
            try_or_finish!(super::compose::load_sections(runtime, &ns, &atom_ids).await);

        let has_sections = !section_map.is_empty();
        try_or_finish!(timing.begin(Phase::Rerank));

        let section_results = if has_sections {
            let domain_member_ids = Self::compose_domain_member_ids(&ordered_atoms, &member_slugs);

            let domain_scores: HashMap<String, f32> = ordered_atoms
                .iter()
                .map(|a| {
                    let id = a.id.to_string();
                    let score = if domain_member_ids.contains(&id) {
                        1.0
                    } else {
                        0.0
                    };
                    (id, score)
                })
                .collect();

            try_or_finish!(khive_storage::ensure_request_read_active(
                "knowledge.compose"
            ));

            if let Some(qe) = query_embedding.any() {
                try_or_finish!(super::compose::score_sections(
                    &raw_query,
                    qe,
                    &atom_cosine_scores,
                    &section_map,
                    &domain_scores,
                    &type_weights,
                    &super::compose::ComposeScoreWeights::default(),
                ))
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
        try_or_finish!(timing.begin(Phase::Trim));

        let max_tokens = p.max_tokens.unwrap_or(8000).clamp(500, 100_000);
        let char_budget = max_tokens * CHARS_PER_TOKEN;

        let packed = pack_compose_markdown(
            &raw_query,
            &resolved_domains,
            &ordered_atoms,
            &items,
            &section_results,
            explain,
            char_budget,
        );
        let section_json: Vec<Value> = if explain {
            packed
                .sections
                .iter()
                .map(|s| {
                        json!({
                            "section_id": s.section_id,
                            "atom_id": s.atom_id,
                            "section_type": s.section_type,
                            "heading": s.heading,
                            "score": (s.score * 10000.0).round() / 10000.0,
                            "breakdown": {
                                "section_cosine": (s.score_breakdown.section_cosine * 10000.0).round() / 10000.0,
                                "section_bm25": (s.score_breakdown.section_bm25 * 10000.0).round() / 10000.0,
                                "atom_cosine": (s.score_breakdown.atom_cosine * 10000.0).round() / 10000.0,
                                "domain_score": (s.score_breakdown.domain_score * 10000.0).round() / 10000.0,
                                "type_weight": (s.score_breakdown.type_weight * 10000.0).round() / 10000.0,
                            },
                        })
                })
                .collect()
        } else {
            Vec::new()
        };

        // KG entity blend (ADR-051 Amendment 1): additive "Knowledge graph"
        // section, trimmed against whatever budget the atom/section body left
        // over. Runs after the body is finalized so entities never displace an
        // atom or section — see `trim_kg_entities_to_budget`.
        //
        // Self-calibrating inclusion floor: an entity only blends in if its
        // rerank score clears the minimum rerank score among the atoms that
        // actually made the final body. A compose whose final body has zero
        // atoms (everything trimmed by `max_tokens`) has no floor to
        // calibrate against, so it blends no entities at all (ADR-051
        // Amendment 1, zero-atom edge case).
        let entity_score_floor: Option<f32> = packed
            .included_atom_ids
            .iter()
            .filter_map(|id| atom_cosine_scores.get(id).copied())
            .fold(None, |acc, s| Some(acc.map_or(s, |a: f32| a.min(s))));
        let mut markdown = packed.markdown;
        let mut kg_entities_json: Vec<Value> = Vec::new();
        if blend_kg {
            if let Some(floor) = entity_score_floor {
                // Discovery/hydration failures degrade to an atom-only
                // response instead of aborting the whole compose — the
                // finalized atom/section body above is still a valid,
                // useful briefing even without the supplementary KG section.
                // KG entities live on the core (main) backend; on a
                // secondary-assigned pack runtime this search would silently
                // blend against an empty graph (ADR-073).
                match search_kg_entities(
                    &runtime.core(),
                    token,
                    &ns,
                    &raw_query,
                    &mut query_embedding,
                    KG_BLEND_CAP,
                    floor,
                )
                .await
                {
                    Ok(kg_hits) => {
                        let remaining_budget = char_budget.saturating_sub(markdown.len());
                        let kg_hits = trim_kg_entities_to_budget(kg_hits, remaining_budget);
                        if !kg_hits.is_empty() {
                            let kg_markdown = format_kg_entities_markdown(&kg_hits);
                            if kg_markdown.len() <= remaining_budget {
                                markdown.push_str(&kg_markdown);
                                kg_entities_json = kg_hits
                                    .iter()
                                    .map(|e| {
                                        json!({
                                            "id": e.id,
                                            "kind": e.kind,
                                            "name": e.name,
                                            "score": (e.score * 10000.0).round() / 10000.0,
                                        })
                                    })
                                    .collect();
                            }
                        }
                    }
                    Err(e) => {
                        try_or_finish!(khive_storage::ensure_request_read_active(
                            "knowledge.compose"
                        ));
                        tracing::warn!(
                            target: "khive_pack_knowledge::knowledge::search",
                            error = %e,
                            "knowledge.compose: KG entity blend failed, continuing with atom-only response"
                        );
                    }
                }
            }
        }

        try_or_finish!(khive_storage::ensure_request_read_active(
            "knowledge.compose"
        ));

        let atom_json: Vec<Value> = items
            .iter()
            .filter(|item| packed.included_atom_ids.contains(&item.id))
            .map(|item| {
                json!({
                    "id": item.id,
                    "slug": item.slug,
                    "name": item.name,
                    "score": (item.score * 10000.0).round() / 10000.0,
                })
            })
            .collect();

        let domain_json: Vec<Value> = resolved_domains
            .iter()
            .map(|d| json!({ "id": d.id.to_string(), "slug": d.slug, "name": d.name }))
            .collect();

        let count = atom_json.len();

        let mut data = json!({
            "query": raw_query,
            "markdown": markdown,
            "domains": domain_json,
            "atoms": atom_json,
            "count": count,
        });
        if explain && !section_json.is_empty() {
            data["sections"] = json!(section_json);
            data["section_count"] = json!(section_json.len());
        }
        if !kg_entities_json.is_empty() {
            data["entities"] = json!(kg_entities_json);
        }
        if suggest_ann_unavailable {
            data["ann_unavailable"] = json!(true);
        }
        if !omitted_members.is_empty() {
            data["omissions"] = json!(omitted_members);
        }
        attach_hydration_degradation(&mut data, suggest_hydration_failures);

        let response = json!({
            "status": "ok",
            "data": data,
        });
        try_or_finish!(khive_storage::ensure_request_read_active(
            "knowledge.compose"
        ));
        timing.finish(count);
        Ok(response)
    }
}
