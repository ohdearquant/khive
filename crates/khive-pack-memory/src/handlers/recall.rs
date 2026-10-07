//! Handler for `memory.recall` — the main retrieval pipeline.
//! See `crates/khive-pack-memory/docs/api/recall-pipeline.md`.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::recall_feedback::{on_recall_hit, on_recall_miss};

use serde_json::{json, Value};
use uuid::Uuid;

use khive_brain_core::{compute_query_class, PackTunable, ServeAttribution};
use khive_fusion::FusionStrategy;
use khive_runtime::{
    micros_to_iso, KhiveRuntime, Namespace, NamespaceToken, RequestIdentity, RuntimeError,
    SearchSource, VerbRegistry,
};
use khive_storage::types::{Direction, NeighborQuery};
use khive_storage::EdgeRelation;
use khive_types::{Details, KhiveError};

use crate::config::{RecallConfig, ScoreBreakdown};
use crate::rerank::{weighted_rerank, RerankFeatures};
use crate::scoring::{
    calculate_score, contains_cjk, extract_entity_candidates, normalize_min_score,
    normalize_rank_fusion_scores, normalize_rrf_scores, ScoreInput, ScoringConfig,
};
use crate::MemoryPack;

use super::common::{
    compute_score, deser, fuse_candidates, make_pipeline, note_has_any_tag, note_matches_tags,
    plog, plog_n, recall_candidate_count, to_json, validate_memory_type, RecallCandidateParams,
    RecallParams, RecallStageTimings, TextSnippetPolicy, DEFAULT_DECAY_EPISODIC,
    DEFAULT_DECAY_SEMANTIC, DEFAULT_SALIENCE_EPISODIC, DEFAULT_SALIENCE_SEMANTIC, PROF_CID,
    RECALL_CALL_ID, RECALL_SLOW_THRESHOLD_MS,
};

fn compare_rank_scores_desc(left: f32, right: f32) -> std::cmp::Ordering {
    match (left.is_nan(), right.is_nan()) {
        (true, false) => std::cmp::Ordering::Greater,
        (false, true) => std::cmp::Ordering::Less,
        (true, true) => std::cmp::Ordering::Equal,
        (false, false) => right.total_cmp(&left),
    }
}

fn checked_token_budget_chars(scoring_cfg: &ScoringConfig) -> Result<usize, RuntimeError> {
    if scoring_cfg.default_token_budget == 0 {
        return Err(RuntimeError::InvalidInput(
            "memory.recall config.scoring.default_token_budget must be greater than zero"
                .to_string(),
        ));
    }
    if scoring_cfg.chars_per_token == 0 {
        return Err(RuntimeError::InvalidInput(
            "memory.recall config.scoring.chars_per_token must be greater than zero".to_string(),
        ));
    }
    scoring_cfg
        .default_token_budget
        .checked_mul(scoring_cfg.chars_per_token)
        .ok_or_else(|| {
            RuntimeError::InvalidInput(
                "memory.recall effective character budget overflows platform size".to_string(),
            )
        })
}

fn emit_slow_recall_warning(
    total_ms: u64,
    timings: &RecallStageTimings,
    result_count: usize,
    query_bytes: usize,
    ann_degraded: bool,
    budget_capped: bool,
    is_verbose: bool,
) {
    if total_ms < RECALL_SLOW_THRESHOLD_MS {
        return;
    }
    tracing::warn!(
        total_ms,
        threshold_ms = RECALL_SLOW_THRESHOLD_MS,
        embed_ms = timings.embed_ms(),
        embed_attempted = timings.embed_attempted(),
        fts_ms = timings.fts_ms(),
        fts_attempted = timings.fts_attempted(),
        ann_ms = timings.ann_ms(),
        ann_attempted = timings.ann_attempted(),
        fresh_tail_ms = timings.fresh_tail_ms(),
        fresh_tail_attempted = timings.fresh_tail_attempted(),
        hydrate_ms = timings.hydrate_ms(),
        hydrate_attempted = timings.hydrate_attempted(),
        result_count,
        query_bytes,
        ann_degraded,
        budget_capped,
        is_verbose,
        "memory.recall exceeded slow-request threshold"
    );
}

async fn load_brain_profile(
    registry: &VerbRegistry,
    token: &NamespaceToken,
    profile_id: &str,
) -> Result<Value, RuntimeError> {
    registry
        .dispatch_with_identity(
            "brain.profile",
            json!({
                "namespace": token.namespace().as_str(),
                "profile_id": profile_id,
            }),
            Some(RequestIdentity::from_token(token)),
        )
        .await
}

fn reject_archived_brain_profile(response: &Value, profile_id: &str) -> Result<(), RuntimeError> {
    if response.get("lifecycle").and_then(Value::as_str) == Some("archived") {
        return Err(RuntimeError::InvalidInput(format!(
            "profile_id {profile_id:?} is archived and cannot serve memory.recall"
        )));
    }
    Ok(())
}

fn freshness_unmet(models: &[String]) -> RuntimeError {
    let mut failed = models.to_vec();
    failed.sort();
    failed.dedup();
    KhiveError::unavailable(format!(
        "freshness_unmet: memory.recall could not prove visibility for models {}",
        failed.join(", ")
    ))
    .with_details(Details::new_owned([
        ("reason", "freshness_unmet".to_string()),
        ("failed_models", failed.join(",")),
    ]))
    .into()
}

fn session_attempt_deadline(
    first_attempt: bool,
    wait_end: Instant,
    attempt_deadline: Instant,
) -> Instant {
    if first_attempt {
        attempt_deadline
    } else {
        attempt_deadline.min(wait_end)
    }
}

impl MemoryPack {
    async fn collect_recall_candidates_with_session(
        &self,
        query: &str,
        token: &NamespaceToken,
        opts: RecallCandidateParams<'_>,
        wait_end: Instant,
        attempt_deadline: Instant,
    ) -> Result<super::common::RecallCandidateSet, RuntimeError> {
        if opts
            .session_fence
            .is_none_or(|fence| fence.fences.is_empty())
        {
            return self.collect_recall_candidates(query, token, opts).await;
        }
        let required_models: Vec<String> = opts
            .session_fence
            .expect("nonempty session fence checked")
            .fences
            .iter()
            .map(|fence| fence.model.clone())
            .collect();
        let fence = opts.session_fence.expect("nonempty session fence checked");
        let mut first_attempt = true;
        loop {
            // A zero wait still gets one immediate attempt. Only retries are
            // bounded by the caller's wait window as well as the request cap.
            let deadline = session_attempt_deadline(first_attempt, wait_end, attempt_deadline);
            if Instant::now() >= deadline {
                return Err(freshness_unmet(&required_models));
            }
            first_attempt = false;

            // Poll the inexpensive fence without embedding, FTS or KNN. The
            // candidate-producing read proves it again in its own snapshot.
            let probe = Box::pin(crate::ann::session_unmet_fences(&self.runtime, fence));
            let unmet = tokio::time::timeout_at(deadline.into(), probe)
                .await
                .map_err(|_| freshness_unmet(&required_models))?;
            let failed_models = if unmet.is_empty() {
                let candidate_future = Box::pin(self.collect_recall_candidates(query, token, opts));
                let candidates = tokio::time::timeout_at(deadline.into(), candidate_future)
                    .await
                    .map_err(|_| freshness_unmet(&required_models))??;
                if candidates.session_unmet_models.is_empty() {
                    return Ok(candidates);
                }
                candidates.session_unmet_models
            } else {
                unmet
            };
            khive_storage::ensure_request_read_active("memory.recall")?;
            let now = Instant::now();
            if now >= wait_end {
                return Err(freshness_unmet(&failed_models));
            }
            let pause = wait_end
                .saturating_duration_since(now)
                .min(Duration::from_millis(40));
            tokio::time::sleep(pause).await;
            khive_storage::ensure_request_read_active("memory.recall")?;
        }
    }

    pub(crate) async fn handle_recall(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
        hard_deadline: Option<Instant>,
    ) -> Result<Value, RuntimeError> {
        use std::sync::atomic::Ordering;

        let recall_start = Instant::now();
        let p: RecallParams = deser(params)?;

        // Registry dispatch already supplies an exact token for an explicit
        // namespace. Direct callers must present a token authorized for the
        // same primary namespace before its visibility is narrowed to the
        // requested arm; caller parameters may never elevate a token.
        let effective_token: NamespaceToken = match p.namespace.as_deref() {
            Some(ns_str) => {
                let ns = Namespace::parse(ns_str).map_err(|e| {
                    RuntimeError::InvalidInput(format!("invalid namespace {ns_str:?}: {e}"))
                })?;
                if &ns != token.namespace() {
                    return Err(RuntimeError::InvalidInput(
                        "memory.recall namespace does not match authorized token namespace"
                            .to_string(),
                    ));
                }
                token.with_namespace(ns)
            }
            None => token.clone(),
        };
        let token = &effective_token;
        let requested_models = p
            .embedding_model
            .as_ref()
            .map(|model| vec![model.clone()])
            .unwrap_or_else(|| self.runtime.registered_embedding_model_names());
        let visible_namespaces = token.visible_namespace_strs();
        let session_fence = crate::visibility::parse_recall_visibility(
            &self.runtime,
            p.consistency.as_ref(),
            p.visibility_token.as_ref(),
            &visible_namespaces,
            &requested_models,
        )?;
        let timeout_ms = crate::visibility::parse_timeout_ms(p.timeout_ms.as_ref())?;
        let wait_started = Instant::now();
        let wait_by_caller = wait_started + Duration::from_millis(timeout_ms);
        let request_deadline = hard_deadline.or_else(|| {
            wait_started.checked_add(Duration::from_millis(crate::pack::recall_deadline_ms()))
        });
        let wait_by_request = request_deadline
            .and_then(|deadline| deadline.checked_sub(Duration::from_secs(2)))
            .unwrap_or(wait_started);
        let session_wait_end = wait_by_caller.min(wait_by_request);

        let created_after_us = p
            .created_after
            .as_deref()
            .map(|raw| super::common::parse_recall_bound("created_after", raw))
            .transpose()?;
        let created_before_us = p
            .created_before
            .as_deref()
            .map(|raw| super::common::parse_recall_bound("created_before", raw))
            .transpose()?;
        if let (Some(after), Some(before)) = (created_after_us, created_before_us) {
            if after >= before {
                return Err(RuntimeError::InvalidInput(format!(
                    "memory.recall: created_after {:?} is not earlier than created_before {:?}; \
                     the window is empty",
                    p.created_after.as_deref().unwrap_or_default(),
                    p.created_before.as_deref().unwrap_or_default(),
                )));
            }
        }

        let prof = super::common::recall_profile_enabled();
        let call_id = if prof {
            let id = RECALL_CALL_ID.fetch_add(1, Ordering::Relaxed);
            PROF_CID.with(|c| c.set(id));
            id
        } else {
            0
        };
        let t_total = if prof { Some(Instant::now()) } else { None };
        let mut t_stage = if prof { Some(Instant::now()) } else { None };

        let query_trimmed = p.query.trim();
        if query_trimmed.is_empty() {
            return Err(RuntimeError::InvalidInput("query must not be empty".into()));
        }
        if !crate::scoring::is_meaningful_query(query_trimmed) {
            return Err(RuntimeError::InvalidInput(format!(
                "query {query_trimmed:?} does not contain enough meaningful content \
                 (must have at least 2 alphabetic or CJK characters and not consist \
                 of repeated characters)"
            )));
        }

        if let Some(mt) = &p.memory_type {
            validate_memory_type(mt)?;
        }

        if let Some(ref fs) = p.fusion_strategy {
            super::common::parse_fusion_strategy_str(fs)?;
        }

        let mut cfg = p.effective_config(self.active_config());
        super::common::apply_requested_fusion_strategy(&mut cfg, p.fusion_strategy.as_deref())?;
        cfg.validate()?;

        let effective_min_score: f32 = {
            let raw = if let Some(floor) = p.score_floor {
                floor as f64
            } else {
                cfg.min_score
            };
            normalize_min_score(raw).map_err(RuntimeError::from)?
        };

        // `limit` and `top_k` agree on zero: both mean no hits. A caller that
        // computes a limit which reaches zero gets an empty page, never a
        // single result smuggled in by a lower clamp.
        let limit = if let Some(k) = p.top_k {
            k.min(crate::scoring::MAX_RECALL_LIMIT)
        } else {
            p.limit
                .map(|v| v as usize)
                .unwrap_or(10)
                .min(crate::scoring::MAX_RECALL_LIMIT)
        };
        let limit_u32 = u32::try_from(limit).unwrap_or(u32::MAX);

        let mut scoring_cfg = cfg.scoring.clone().unwrap_or_default();
        scoring_cfg.apply_dos_caps();
        // Validate before retrieval so a degenerate caller-supplied budget
        // cannot masquerade as a genuine recall miss, and an oversized
        // chars-per-token value cannot wrap or panic after scoring.
        let token_budget_chars = checked_token_budget_chars(&scoring_cfg)?;

        let cjk_fts_bypass = scoring_cfg.enable_cjk_fts_bypass && contains_cjk(query_trimmed);

        let candidate_limit =
            recall_candidate_count(&cfg, limit_u32).min(scoring_cfg.max_recall_candidates as u32);

        if prof {
            if let Some(ref t) = t_stage {
                plog(call_id, "setup", t.elapsed().as_micros());
            }
            t_stage = Some(Instant::now());
        }

        // Resolve once BEFORE scoring so projection, response stamp, and ledger cannot drift.
        // Explicit unknown IDs error; unreadable bound state degrades to configured defaults.
        let mut profile_state: Option<khive_brain_core::BalancedRecallState> = None;
        let (served_by_profile_id, serve_attribution): (Option<String>, ServeAttribution) =
            if let Some(ref pid) = p.profile_id {
                let resp = load_brain_profile(registry, token, pid)
                    .await
                    .map_err(|e| {
                        RuntimeError::InvalidInput(format!(
                            "profile_id {pid:?} is not a known profile: {e}"
                        ))
                    })?;
                reject_archived_brain_profile(&resp, pid)?;
                profile_state = super::common::balanced_recall_state_from_profile_response(&resp);
                (Some(pid.clone()), ServeAttribution::Profile)
            } else {
                let resolved =
                    super::common::resolve_serving_profile(&self.brain_profile, token, registry)
                        .await;
                if let Some(profile_id) = resolved {
                    match load_brain_profile(registry, token, &profile_id).await {
                        Ok(resp) => {
                            reject_archived_brain_profile(&resp, &profile_id)?;
                            profile_state =
                                super::common::balanced_recall_state_from_profile_response(&resp);
                            (Some(profile_id), ServeAttribution::Profile)
                        }
                        Err(e) => {
                            tracing::warn!(
                                profile_id = %profile_id,
                                error = %e,
                                "ADR-104 §1: profile record unreadable; recall scores with configured defaults and is not attributed to the profile"
                            );
                            // A profile whose record cannot be read never served this recall;
                            // stamping it would credit downstream feedback to a profile that
                            // had no effect on ranking. A readable record with a null snapshot
                            // (new profile) still stamps — that is the posterior bootstrap path.
                            (None, ServeAttribution::Unattributed)
                        }
                    }
                } else {
                    (None, ServeAttribution::Unspecified)
                }
            };

        // Project request-local weights without mutating pack config; retain defaults for ratios.
        let default_weights = scoring_cfg.weights.clone();
        if let Some(ref state) = profile_state {
            if let Ok(projected) =
                serde_json::from_value::<RecallConfig>(self.project_config(state))
            {
                scoring_cfg.weights.relevance = projected.relevance_weight as f32;
                scoring_cfg.weights.salience = projected.salience_weight as f32;
                scoring_cfg.weights.temporal = projected.temporal_weight as f32;
            }
        }

        if prof {
            if let Some(ref t) = t_stage {
                plog(call_id, "profile_resolve", t.elapsed().as_micros());
            }
            t_stage = Some(Instant::now());
        }
        let effective_fts_gather = crate::config::RecallFtsGatherConfig::from_env()
            .map_err(|e| RuntimeError::InvalidInput(format!("fts_gather env parse error: {e}")))?
            .unwrap_or_else(|| cfg.fts_gather.clone());

        // Request widening policy precedes the process-wide fallback.
        let ann_overfetch_max_rounds = cfg
            .ann_overfetch_max_rounds
            .unwrap_or_else(super::common::ann_overfetch_max_rounds);

        // Bound cold ANN readiness before degrading this vector leg to FTS-only.
        let ann_ready_timeout_ms = cfg
            .ann_ready_timeout_ms
            .unwrap_or_else(super::common::ann_ready_timeout_ms);

        // Retrieval caps all note kinds, so re-gather after hydration when non-memory rows
        // starve eligible memories; widening remains round- and server-cap bounded.
        // The candidate future contains the complete text/vector fan-out,
        // including the 1/2/N embedding and ANN branches. Keep it behind a
        // pointer at both await sites so its state is not inlined into this
        // already-large pipeline and then into the MCP dispatch poll stack.
        let mut current_candidate_limit = candidate_limit;
        let mut recall_stage_timings = RecallStageTimings::default();
        let mut candidates = Box::pin(self.collect_recall_candidates_with_session(
            query_trimmed,
            token,
            RecallCandidateParams {
                candidate_limit: current_candidate_limit,
                embedding_model: p.embedding_model.as_deref(),
                session_fence: session_fence.as_ref(),
                cjk_fts_bypass,
                snippet_policy: TextSnippetPolicy::Omit,
                fts_gather: &effective_fts_gather,
                ann_overfetch_max_rounds,
                ann_ready_timeout_ms,
            },
            session_wait_end,
            wait_by_request,
        ))
        .await?;
        recall_stage_timings.add_retrieval_round(candidates.timings);
        let hydrate_started = Instant::now();
        let (mut memory_ids, mut notes_by_id) =
            self.load_memory_candidate_notes(token, &candidates).await?;
        recall_stage_timings.add_hydration(hydrate_started.elapsed());

        // Widening must count only candidates the created_at window can keep:
        // the window predicate runs post-fusion, so counting raw candidates
        // would let strong out-of-window rows satisfy the break condition and
        // starve eligible ones deeper in the corpus. With no window set this
        // is exactly the candidate count.
        let in_window = |note: &khive_storage::note::Note| {
            created_after_us.is_none_or(|after| note.created_at >= after)
                && created_before_us.is_none_or(|before| note.created_at < before)
        };
        // ... and only candidates the selected fusion strategy can keep:
        // KeywordOnly discards vector-leg-only hits at fusion and VectorOnly
        // discards text-leg-only hits, so counting the hydrated union would
        // let discarded-leg rows satisfy the break condition and stop
        // widening with zero survivors even when an in-window match exists
        // one round deeper.
        let keyword_only = matches!(&cfg.fuse_strategy, FusionStrategy::KeywordOnly);
        let vector_only = matches!(&cfg.fuse_strategy, FusionStrategy::VectorOnly);
        let count_eligible = |cands: &super::common::RecallCandidateSet,
                              notes: &HashMap<Uuid, khive_storage::note::Note>|
         -> usize {
            let kept: Option<HashSet<Uuid>> = if keyword_only {
                Some(cands.text_hits.iter().map(|h| h.subject_id).collect())
            } else if vector_only {
                Some(
                    cands
                        .vector_hits_per_model
                        .iter()
                        .flat_map(|(_, hits)| hits.iter().map(|h| h.subject_id))
                        .collect(),
                )
            } else {
                None
            };
            notes
                .iter()
                .filter(|(id, n)| in_window(n) && kept.as_ref().is_none_or(|k| k.contains(id)))
                .count()
        };
        let mut eligible_count = count_eligible(&candidates, &notes_by_id);

        for _round in 1..ann_overfetch_max_rounds {
            if eligible_count >= limit {
                break;
            }
            let corpus_exhausted = candidates.text_hits.len() < current_candidate_limit as usize
                && candidates
                    .vector_hits_per_model
                    .iter()
                    .all(|(_, h)| h.len() < current_candidate_limit as usize);
            if corpus_exhausted {
                break;
            }
            let widened = current_candidate_limit
                .saturating_mul(4)
                .min(scoring_cfg.max_recall_candidates as u32);
            if widened <= current_candidate_limit {
                break;
            }
            current_candidate_limit = widened;
            candidates = Box::pin(self.collect_recall_candidates_with_session(
                query_trimmed,
                token,
                RecallCandidateParams {
                    candidate_limit: current_candidate_limit,
                    embedding_model: p.embedding_model.as_deref(),
                    session_fence: session_fence.as_ref(),
                    cjk_fts_bypass,
                    snippet_policy: TextSnippetPolicy::Omit,
                    fts_gather: &effective_fts_gather,
                    ann_overfetch_max_rounds,
                    ann_ready_timeout_ms,
                },
                session_wait_end,
                wait_by_request,
            ))
            .await?;
            recall_stage_timings.add_retrieval_round(candidates.timings);
            let hydrate_started = Instant::now();
            (memory_ids, notes_by_id) =
                self.load_memory_candidate_notes(token, &candidates).await?;
            recall_stage_timings.add_hydration(hydrate_started.elapsed());
            eligible_count = count_eligible(&candidates, &notes_by_id);
        }
        let candidate_limit = current_candidate_limit;

        if prof {
            if let Some(ref t) = t_stage {
                plog_n(
                    call_id,
                    "candidates",
                    t.elapsed().as_micros(),
                    candidates.text_hits.len()
                        + candidates
                            .vector_hits_per_model
                            .iter()
                            .map(|(_, h)| h.len())
                            .sum::<usize>(),
                );
            }
            t_stage = Some(Instant::now());
        }

        // #836: at least one embedding model's vector leg hit the bounded
        // ANN readiness wait and was served FTS-only for this recall.
        let ann_degraded = candidates.ann_degraded;
        // #1657: the third state needs a machine-readable marker even when the
        // degraded result set is empty; keep the failure-site reason so an
        // empty degraded response can cite it verbatim (a genuine no-match
        // carries neither).
        let ann_degraded_reason: Option<String> = candidates.ann_degraded_reason.clone();

        if prof {
            if let Some(ref t) = t_stage {
                plog_n(
                    call_id,
                    "hydration",
                    t.elapsed().as_micros(),
                    notes_by_id.len(),
                );
            }
            t_stage = Some(Instant::now());
        }

        let raw_vec_scores: HashMap<Uuid, f32> = {
            let mut map = HashMap::new();
            for (_, hits) in &candidates.vector_hits_per_model {
                for h in hits {
                    let score = h.score.to_f64() as f32;
                    map.entry(h.subject_id)
                        .and_modify(|s| {
                            if score > *s {
                                *s = score;
                            }
                        })
                        .or_insert(score);
                }
            }
            map
        };

        let fused = fuse_candidates(&candidates, &memory_ids, &cfg, candidate_limit as usize);
        // Needed on both the empty and non-empty completion paths.
        let is_verbose = cfg.include_breakdown || p.include_breakdown.unwrap_or(false);

        if prof {
            if let Some(ref t) = t_stage {
                plog_n(call_id, "fusion", t.elapsed().as_micros(), fused.len());
            }
            t_stage = Some(Instant::now());
        }

        if fused.is_empty() {
            khive_storage::ensure_request_read_active("memory.recall")?;
            self.track_recall_serve(
                token,
                registry,
                RecallServeFields {
                    query_raw: query_trimmed,
                    served_by_profile_id: served_by_profile_id.as_deref(),
                    serve_attribution,
                    target_ids: Vec::new(),
                    latency_us: recall_start.elapsed().as_micros() as i64,
                    ann_degraded,
                    ann_degraded_reason: ann_degraded_reason.clone(),
                },
            );
            if let Ok(mut state) = self.recall_state.lock() {
                on_recall_miss(&mut state);
            }
            emit_slow_recall_warning(
                recall_start.elapsed().as_millis() as u64,
                &recall_stage_timings,
                0,
                query_trimmed.len(),
                ann_degraded,
                false,
                is_verbose,
            );
            // #1657: an empty degraded response is a third state — surface the
            // marker here too, otherwise a bare [] is indistinguishable from a
            // genuine no-match.
            if ann_degraded {
                let reason = ann_degraded_reason
                    .unwrap_or_else(|| super::common::ANN_DEGRADED_REASON.to_string());
                khive_storage::ensure_request_read_active("memory.recall")?;
                return to_json(&json!({
                    "results": Vec::<Value>::new(),
                    "degraded": true,
                    // Captured at the failure site (see
                    // collect_model_ann_hits_inner / collect_model_ann_hits).
                    "degraded_reason": reason,
                }));
            }
            khive_storage::ensure_request_read_active("memory.recall")?;
            return to_json(&Vec::<Value>::new());
        }

        let fused_pairs: Vec<(Uuid, f32)> = fused
            .iter()
            .map(|h| (h.entity_id, h.score.to_f64() as f32))
            .collect();
        let is_rrf = matches!(&cfg.fuse_strategy, FusionStrategy::Rrf { .. });
        let normalized_relevance: HashMap<Uuid, f32> = if is_rrf {
            normalize_rrf_scores(fused_pairs, &scoring_cfg)
        } else {
            normalize_rank_fusion_scores(fused_pairs, &scoring_cfg)
        };

        let source_by_id: HashMap<Uuid, SearchSource> =
            fused.iter().map(|h| (h.entity_id, h.source)).collect();

        let now_micros = chrono::Utc::now().timestamp_micros();
        let now_millis = now_micros / 1_000;

        // Any explicit list, including empty, bypasses both automatic entity sources.
        // Otherwise combine capitalized heuristics with one bounded real-entity lookup;
        // lookup failure preserves the heuristic result and never fails recall.
        let entity_names: Vec<String> = match &p.entity_names {
            Some(names) => names.iter().map(|s| s.to_lowercase()).collect(),
            None => {
                let mut candidates = extract_entity_candidates(query_trimmed);
                match self.entity_anchored_candidates(token, query_trimmed).await {
                    Ok(anchored) => {
                        for name in anchored {
                            if !candidates.contains(&name) {
                                candidates.push(name);
                            }
                        }
                    }
                    Err(e) => {
                        khive_storage::ensure_request_read_active("memory.recall")?;
                        tracing::warn!(
                            error = %e,
                            "ADR-104 §5: entity-anchored candidate lookup failed; \
                             falling back to capitalized-token extraction only"
                        );
                    }
                }
                candidates
            }
        };

        struct ScoredNote {
            id: Uuid,
            rank_score: f32,
            score: f32,
            raw_score: Option<f32>,
            breakdown: ScoreBreakdown,
            note: khive_storage::note::Note,
            resolved_memory_type: String,
            effective_salience: f64,
            effective_decay_factor: f64,
        }

        let recall_pipeline = make_pipeline(&cfg);

        let mut ranked: Vec<ScoredNote> = Vec::new();
        for hit in &fused {
            let id = hit.entity_id;
            let norm_relevance = match normalized_relevance.get(&id) {
                Some(&v) => v,
                None => continue,
            };

            if let Some(&raw) = raw_vec_scores.get(&id) {
                if raw < scoring_cfg.min_raw_relevance {
                    continue;
                }
            }

            let note = match notes_by_id.remove(&id) {
                Some(note) => note,
                None => continue,
            };
            let note_memory_type: String = note
                .properties
                .as_ref()
                .and_then(|pr| pr.get("memory_type"))
                .and_then(|v| v.as_str())
                .unwrap_or("episodic")
                .to_owned();
            if let Some(mt) = &p.memory_type {
                if note_memory_type != mt.as_str() {
                    continue;
                }
            }
            if let Some(filter_tags) = p.tags.as_ref().filter(|tags| !tags.is_empty()) {
                if !note_matches_tags(note.properties.as_ref(), filter_tags, p.tag_mode) {
                    continue;
                }
            }
            if let Some(excluded) = p.exclude_tags.as_ref().filter(|tags| !tags.is_empty()) {
                if note_has_any_tag(note.properties.as_ref(), excluded) {
                    continue;
                }
            }
            // Same predicate the widening loop counts with; one definition so
            // a boundary change cannot drift between the two paths.
            if !in_window(&note) {
                continue;
            }
            let salience = note.salience.unwrap_or(if note_memory_type == "semantic" {
                DEFAULT_SALIENCE_SEMANTIC
            } else {
                DEFAULT_SALIENCE_EPISODIC
            });
            let decay_factor = note
                .decay_factor
                .unwrap_or(if note_memory_type == "semantic" {
                    DEFAULT_DECAY_SEMANTIC
                } else {
                    DEFAULT_DECAY_EPISODIC
                });
            if salience < cfg.min_salience {
                continue;
            }

            let score_input = ScoreInput {
                salience: salience as f32,
                memory_type_str: &note_memory_type,
                content: &note.content,
                created_at_millis: note.created_at / 1_000,
                decay_factor: decay_factor as f32,
                now_millis,
                relevance_score: norm_relevance,
                entity_names: &entity_names,
            };
            let rank_score = calculate_score(&score_input, &scoring_cfg);

            // Profile component is projected/default before the orthogonal entity term.
            let profile_component = if is_verbose {
                match &profile_state {
                    Some(_) => {
                        let mut default_cfg = scoring_cfg.clone();
                        default_cfg.weights = default_weights.clone();
                        let default_score = calculate_score(&score_input, &default_cfg);
                        if default_score.abs() > f32::EPSILON {
                            (rank_score / default_score) as f64
                        } else {
                            1.0
                        }
                    }
                    None => 1.0,
                }
            } else {
                1.0
            };

            // Read profile state once per recall. Apply the entity term LAST and exactly once
            // to whichever composite (default or weighted rerank) actually reaches ranking.
            let entity_posterior_mean: Option<f64> = profile_state
                .as_ref()
                .and_then(|s| s.entity_posteriors.get(&id))
                .map(khive_brain_core::BetaPosterior::mean);
            let entity_term = crate::scoring::entity_posterior_term(
                entity_posterior_mean,
                crate::scoring::ENTITY_POSTERIOR_WEIGHT,
            );

            let age_days_f64 =
                ((now_micros - note.created_at).max(0) as f64) / (1_000_000.0 * 86_400.0);
            let (_, mut breakdown) = compute_score(
                &cfg,
                &recall_pipeline,
                norm_relevance as f64,
                salience,
                decay_factor,
                age_days_f64,
            );
            breakdown.profile_component = profile_component;
            breakdown.entity_posterior_mean = entity_posterior_mean;

            let source = source_by_id.get(&id).copied().unwrap_or(SearchSource::Text);
            let pre_entity_term_score = if !cfg.reranker_weights.is_empty() {
                let features = RerankFeatures {
                    relevance: norm_relevance as f64,
                    salience: breakdown.salience_decayed,
                    temporal: breakdown.temporal,
                    text_match: matches!(source, SearchSource::Text | SearchSource::Both),
                    vector_match: matches!(source, SearchSource::Vector | SearchSource::Both),
                };
                weighted_rerank(&features, &cfg.reranker_weights) as f32
            } else {
                rank_score
            };
            let final_score = pre_entity_term_score * entity_term;
            let final_score = if final_score.is_finite() {
                final_score
            } else {
                0.0
            };

            let raw_score_opt = raw_vec_scores.get(&id).copied();
            let absolute_relevance = raw_score_opt.unwrap_or(final_score).clamp(0.0, 1.0);
            debug_assert!(
                absolute_relevance <= 1.0,
                "score violates [0,1] contract: {absolute_relevance}"
            );

            if final_score < effective_min_score {
                continue;
            }

            ranked.push(ScoredNote {
                id,
                rank_score: final_score,
                score: absolute_relevance,
                raw_score: raw_score_opt,
                breakdown,
                note,
                resolved_memory_type: note_memory_type,
                effective_salience: salience,
                effective_decay_factor: decay_factor,
            });
        }

        if prof {
            if let Some(ref t) = t_stage {
                plog_n(call_id, "scoring", t.elapsed().as_micros(), ranked.len());
            }
            t_stage = Some(Instant::now());
        }

        if scoring_cfg.mmr_penalty > 0.0 && scoring_cfg.mmr_prefix_len > 0 {
            // Choose the duplicate keeper from the full composite score, not
            // the fused retrieval order that populated `ranked`.
            ranked.sort_by(|a, b| {
                compare_rank_scores_desc(a.rank_score, b.rank_score).then(a.id.cmp(&b.id))
            });
            let prefix_len = scoring_cfg.mmr_prefix_len;
            let prefixes: Vec<String> = ranked
                .iter()
                .map(|sn| sn.note.content.chars().take(prefix_len).collect::<String>())
                .collect();

            for (candidate, duplicate) in ranked.iter_mut().zip(duplicate_prefix_flags(&prefixes)) {
                if duplicate {
                    candidate.rank_score =
                        (candidate.rank_score - scoring_cfg.mmr_penalty).max(0.0);
                }
            }
        }

        if prof {
            if let Some(ref t) = t_stage {
                plog_n(call_id, "mmr", t.elapsed().as_micros(), ranked.len());
            }
            t_stage = Some(Instant::now());
        }

        if scoring_cfg.enable_supersedes_suppression {
            let mut superseded_by_prop: HashSet<Uuid> = HashSet::new();
            for sn in &ranked {
                if let Some(target_str) = sn
                    .note
                    .properties
                    .as_ref()
                    .and_then(|pr| pr.get("supersedes"))
                    .and_then(|v| v.as_str())
                {
                    if let Ok(uid) = target_str.parse::<Uuid>() {
                        superseded_by_prop.insert(uid);
                    } else {
                        let prefix = target_str.to_lowercase();
                        for sn2 in &ranked {
                            if sn2.id.as_hyphenated().to_string().starts_with(&prefix) {
                                superseded_by_prop.insert(sn2.id);
                                break;
                            }
                        }
                    }
                }
            }

            let candidate_ids: Vec<Uuid> = ranked.iter().map(|sn| sn.id).collect();
            let mut superseded_by_edge: HashSet<Uuid> = HashSet::new();
            if !candidate_ids.is_empty() {
                let graph = self.runtime.graph(token)?;
                // One batched read for every candidate; the first element of each
                // returned pair is the requested candidate that has an incoming
                // `supersedes` edge.
                superseded_by_edge = graph
                    .batch_neighbors(
                        &candidate_ids,
                        NeighborQuery {
                            direction: Direction::In,
                            relations: Some(vec![EdgeRelation::Supersedes]),
                            limit: Some(1),
                            min_weight: None,
                        },
                    )
                    .await?
                    .into_iter()
                    .map(|(candidate_id, _)| candidate_id)
                    .collect();
                khive_storage::ensure_request_read_active("memory.recall")?;
            }

            let superseded_ids: HashSet<Uuid> = superseded_by_prop
                .union(&superseded_by_edge)
                .copied()
                .collect();
            if !superseded_ids.is_empty() {
                ranked.retain(|sn| !superseded_ids.contains(&sn.id));
            }
        }

        if prof {
            if let Some(ref t) = t_stage {
                plog_n(call_id, "supersedes", t.elapsed().as_micros(), ranked.len());
            }
            t_stage = Some(Instant::now());
        }

        // MMR can lower a composite after the admission gate above.
        ranked.retain(|sn| sn.rank_score >= effective_min_score);
        ranked.sort_by(|a, b| {
            compare_rank_scores_desc(a.rank_score, b.rank_score).then(a.id.cmp(&b.id))
        });
        ranked.truncate(limit);

        let pre_budget_count = ranked.len();
        let mut total_chars = 0usize;
        let mut budget_cutoff: Option<usize> = None;
        for (i, sn) in ranked.iter().enumerate() {
            let entry_chars = sn.note.content.len();
            if entry_chars > token_budget_chars.saturating_sub(total_chars) {
                budget_cutoff = Some(i);
                break;
            }
            total_chars += entry_chars;
        }
        if let Some(cut) = budget_cutoff {
            ranked.truncate(cut);
        }
        let budget_capped = ranked.len() < pre_budget_count;

        let full_content = p.full_content.unwrap_or(true);
        const PREVIEW_CHARS: usize = 200;

        // Source provenance is the memory's `annotates` edge (never a property);
        // read it only when asked, one edge query per returned hit.
        let mut source_ids: HashMap<Uuid, Option<String>> = HashMap::new();
        if p.include_source_id.unwrap_or(false) {
            for id in ranked.iter().map(|sn| sn.id) {
                let source = self
                    .runtime
                    .neighbors_with_query(
                        &effective_token,
                        id,
                        NeighborQuery {
                            direction: Direction::Out,
                            relations: Some(vec![EdgeRelation::Annotates]),
                            limit: Some(1),
                            min_weight: None,
                        },
                    )
                    .await?
                    .into_iter()
                    .next()
                    .map(|hit| hit.node_id.to_string());
                source_ids.insert(id, source);
            }
        }

        let mut results: Vec<Value> = ranked
            .into_iter()
            .map(|sn| {
                let content_out =
                    if !full_content && sn.note.content.chars().count() > PREVIEW_CHARS {
                        let preview: String = sn.note.content.chars().take(PREVIEW_CHARS).collect();
                        format!("{preview}…")
                    } else {
                        sn.note.content.clone()
                    };
                let mut result = json!({
                    "id": sn.id.to_string(),
                    "full_id": sn.id.to_string(),
                    "score": sn.score,
                    "rank_score": sn.rank_score,
                    "raw_score": sn.raw_score,
                    "content": content_out,
                    "salience": sn.effective_salience,
                    "decay_factor": sn.effective_decay_factor,
                    "memory_type": sn.resolved_memory_type,
                    "created_at": micros_to_iso(sn.note.created_at),
                });
                if let Some(source) = source_ids.get(&sn.id) {
                    result["source_id"] = json!(source);
                }
                if is_verbose {
                    result["breakdown"] = json!(sn.breakdown);
                }
                if ann_degraded {
                    // Per-result stamp keeps degradation visible without verbose output.
                    result["degraded"] = json!("ann_unavailable");
                    // #1477: additive, non-empty failure-site reason so a
                    // caller can distinguish why serving degraded (e.g. an
                    // exceptional fresh-tail skip) without breaking the
                    // load-bearing bare-array shape non-empty callers rely on.
                    if let Some(ref reason) = ann_degraded_reason {
                        result["degraded_reason"] = json!(reason);
                    }
                }
                if budget_capped {
                    // Surviving partial results retain the per-item signal so callers
                    // that inspect hits individually can still detect truncation.
                    result["truncated"] = json!(true);
                }
                result
            })
            .collect();

        // Stamp the same profile used for scoring; ledger append stays off the response path.
        if prof {
            if let Some(ref t) = t_stage {
                plog_n(
                    call_id,
                    "results_build",
                    t.elapsed().as_micros(),
                    results.len(),
                );
            }
            t_stage = Some(Instant::now());
        }

        if let Some(ref profile_id) = served_by_profile_id {
            for r in results.iter_mut() {
                r["served_by_profile_id"] = json!(profile_id);
            }
        }
        for r in results.iter_mut() {
            r["serve_attribution"] = json!(serve_attribution);
        }

        let target_ids = results
            .iter()
            .filter_map(|r| r.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();
        self.track_recall_serve(
            token,
            registry,
            RecallServeFields {
                query_raw: query_trimmed,
                served_by_profile_id: served_by_profile_id.as_deref(),
                serve_attribution,
                target_ids,
                latency_us: recall_start.elapsed().as_micros() as i64,
                ann_degraded,
                ann_degraded_reason: ann_degraded_reason.clone(),
            },
        );

        // Update recall-domain posteriors before returning.
        {
            let latency_us = recall_start.elapsed().as_micros() as i64;
            let top_id = results.first().and_then(|r| {
                r.get("id")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<Uuid>().ok())
            });
            if let Ok(mut state) = self.recall_state.lock() {
                if let Some(tid) = top_id {
                    on_recall_hit(&mut state, tid, latency_us);
                } else {
                    on_recall_miss(&mut state);
                }
            }
        }

        // #30/#889: unconditional slow-request observability, mirroring the
        // knowledge.compose WARN added for #887. Fires on every completed recall
        // whose total handler time crosses the threshold, regardless of whether
        // KHIVE_RECALL_PROFILE is set, so a slow-but-completing recall leaves
        // daemon-side evidence even when nobody opted into per-stage profiling.
        emit_slow_recall_warning(
            recall_start.elapsed().as_millis() as u64,
            &recall_stage_timings,
            results.len(),
            query_trimmed.len(),
            ann_degraded,
            budget_capped,
            is_verbose,
        );

        if is_verbose && candidates.vector_hits_per_model.len() > 1 {
            // Raw global ANN diagnostics MUST use the same hydrated namespace filter as results.
            let per_model: Vec<Value> = candidates
                .vector_hits_per_model
                .iter()
                .map(|(model, hits)| {
                    let hits_json: Vec<Value> = hits
                        .iter()
                        .filter(|h| memory_ids.contains(&h.subject_id))
                        .map(|h| {
                            json!({
                                "id": h.subject_id.to_string(),
                                "score": h.score.to_f64(),
                                "rank": h.rank,
                            })
                        })
                        .collect();
                    json!({ "model": model, "hits": hits_json })
                })
                .collect();
            let truncated_for_budget = if budget_capped {
                pre_budget_count - results.len()
            } else {
                0
            };
            khive_storage::ensure_request_read_active("memory.recall")?;
            return to_json(&json!({
                "results": results,
                "candidates": {
                    "vector_candidates_per_model": per_model,
                },
                "budget_capped": budget_capped,
                "truncated_for_budget": truncated_for_budget,
            }));
        }

        if prof {
            if let Some(ref t) = t_stage {
                plog_n(call_id, "serialize", t.elapsed().as_micros(), results.len());
            }
            if let Some(ref t) = t_total {
                plog(call_id, "total", t.elapsed().as_micros());
            }
        }

        if budget_capped && results.is_empty() {
            // Only the ambiguous case changes shape: a budget cutoff at the first
            // ranked candidate leaves no item to carry the per-result stamp, so a
            // bare [] would be indistinguishable from a genuine no-match. Non-empty
            // capped responses keep the bare-array shape (load-bearing for callers
            // that index the top-level array) with per-item truncated stamps.
            let mut envelope = json!({
                "results": results,
                "truncated": true,
            });
            // A budget cutoff must not silence a concurrent ANN degradation:
            // this branch returns before the degradation envelope below, and
            // without these fields a capped-empty degraded response would
            // read as clean-empty-but-truncated.
            if ann_degraded {
                let reason = ann_degraded_reason
                    .clone()
                    .unwrap_or_else(|| super::common::ANN_DEGRADED_REASON.to_string());
                envelope["degraded"] = json!(true);
                // Captured at the failure site (see
                // collect_model_ann_hits_inner / collect_model_ann_hits).
                envelope["degraded_reason"] = json!(reason);
            }
            khive_storage::ensure_request_read_active("memory.recall")?;
            return to_json(&envelope);
        }

        if results.is_empty() && ann_degraded {
            // #1657: mirror the budget-cap precedent — an empty degraded
            // response changes shape to {results: [], degraded: true,
            // degraded_reason} because a bare [] would be indistinguishable
            // from a genuine no-match. Non-empty degraded responses keep the
            // bare-array shape with per-item "degraded": "ann_unavailable"
            // stamps (load-bearing for callers that index the top-level
            // array).
            let reason = ann_degraded_reason
                .unwrap_or_else(|| super::common::ANN_DEGRADED_REASON.to_string());
            khive_storage::ensure_request_read_active("memory.recall")?;
            return to_json(&json!({
                "results": results,
                "degraded": true,
                // Captured at the failure site (see
                // collect_model_ann_hits_inner / collect_model_ann_hits).
                "degraded_reason": reason,
            }));
        }

        khive_storage::ensure_request_read_active("memory.recall")?;
        to_json(&results)
    }

    fn track_recall_serve(
        &self,
        token: &NamespaceToken,
        registry: &VerbRegistry,
        fields: RecallServeFields<'_>,
    ) {
        let RecallServeFields {
            query_raw,
            served_by_profile_id,
            serve_attribution,
            target_ids,
            latency_us,
            ann_degraded,
            ann_degraded_reason,
        } = fields;
        let registry = registry.clone();
        let namespace = token.namespace().as_str().to_string();
        let query_raw = query_raw.to_string();
        let query_class = compute_query_class(&query_raw);
        let served_by_profile_id = served_by_profile_id.map(str::to_string);
        let served_at_us = chrono::Utc::now().timestamp_micros();
        let actor = format!("{}:{}", token.actor().kind, token.actor().id);
        let runtime = self.runtime.clone();
        let token = token.clone();

        khive_runtime::track_recall_ledger_task(async move {
            // The serve ledger lives in the brain pack; without it loaded
            // there is nothing to record, so skip the guaranteed-failed
            // dispatch (and its per-recall warn) entirely.
            if registry.has_verb("brain.record_serve") {
                let mut ledger_params = json!({
                    "namespace": namespace,
                    "consumer_kind": "recall",
                    "target_ids": target_ids.clone(),
                    "query_raw": query_raw.clone(),
                    "served_at": served_at_us,
                    "serve_attribution": serve_attribution,
                });
                if let Some(ref profile_id) = served_by_profile_id {
                    ledger_params["served_by_profile_id"] = json!(profile_id);
                }
                if let Err(error) = registry
                    .dispatch_with_identity(
                        "brain.record_serve",
                        ledger_params,
                        Some(RequestIdentity::from_token(&token)),
                    )
                    .await
                {
                    tracing::warn!(
                        error = %error,
                        "serve ledger dispatch failed; recall result is unaffected"
                    );
                }
            }

            emit_recall_executed_event(
                &runtime,
                &token,
                RecallExecutedFields {
                    actor,
                    served_by_profile_id,
                    serve_attribution,
                    query_raw,
                    query_class,
                    target_ids,
                    latency_us,
                    ann_degraded,
                    ann_degraded_reason,
                },
            )
            .await;
        });
    }
}

/// Values captured at the recall response boundary for background serve
/// accounting and telemetry.
struct RecallServeFields<'a> {
    query_raw: &'a str,
    served_by_profile_id: Option<&'a str>,
    serve_attribution: ServeAttribution,
    target_ids: Vec<String>,
    latency_us: i64,
    /// #836: at least one vector leg was served FTS-only for this recall. The
    /// response envelope already distinguishes this from a genuine no-match;
    /// the event plane could not, so it is carried through here.
    ann_degraded: bool,
    /// Reason captured at the failure site, `None` when the recall was not
    /// degraded.
    ann_degraded_reason: Option<String>,
}

/// Fields for the best-effort `RecallExecuted` telemetry event. Grouped into a
/// struct rather than passed positionally to stay under clippy's
/// too-many-arguments threshold.
struct RecallExecutedFields {
    actor: String,
    served_by_profile_id: Option<String>,
    serve_attribution: ServeAttribution,
    query_raw: String,
    query_class: String,
    target_ids: Vec<String>,
    latency_us: i64,
    ann_degraded: bool,
    ann_degraded_reason: Option<String>,
}

/// Append best-effort recall telemetry without affecting the recall response.
///
/// khive#36: carries the full returned result-ID list (both `candidates` and
/// `selected` — the recall pipeline does not track a broader pre-selection
/// candidate pool at this emission boundary, so every served id is reported
/// as both), the typed result kind (`memory.recall` always serves `note`
/// substrate records), the full query text, `served_by_profile_id`, the
/// tri-state `serve_attribution`, the calling actor, and a timestamp
/// (`Event::new` stamps `created_at`).
async fn emit_recall_executed_event(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    fields: RecallExecutedFields,
) {
    let RecallExecutedFields {
        actor,
        served_by_profile_id,
        serve_attribution,
        query_raw,
        query_class,
        target_ids,
        latency_us,
        ann_degraded,
        ann_degraded_reason,
    } = fields;
    let store = match rt.events(token) {
        Ok(store) => store,
        Err(err) => {
            tracing::warn!(
                error = %err,
                namespace = token.namespace().as_str(),
                event_kind = "recall_executed",
                "recall_executed event store acquisition failed; recall result is unaffected"
            );
            return;
        }
    };
    let result_count = target_ids.len();
    // A degraded recall that returns nothing is a different state from a
    // genuine no-match, and both serve `result_count: 0`. The response
    // envelope has carried that distinction since #1657; without these two
    // fields the event plane collapses them into one row, so a count of
    // recalls cannot tell a configuration problem from an empty corpus.
    let mut payload = json!({
        "actor": actor,
        "served_by_profile_id": served_by_profile_id,
        "serve_attribution": serve_attribution,
        "query": query_raw,
        "query_class": query_class,
        "result_kind": "note",
        "result_count": result_count,
        "candidates": target_ids.clone(),
        "selected": target_ids,
        "latency_us": latency_us,
        "degraded": ann_degraded,
    });
    if ann_degraded {
        payload["degraded_reason"] =
            json!(ann_degraded_reason
                .unwrap_or_else(|| super::common::ANN_DEGRADED_REASON.to_string()));
    }
    let event = khive_storage::Event::new(
        token.namespace().as_str(),
        "memory.recall",
        khive_types::EventKind::RecallExecuted,
        khive_types::SubstrateKind::Event,
        actor,
    )
    .with_payload(payload)
    .with_duration_us(latency_us);
    if let Err(err) = store.append_event(event).await {
        tracing::warn!(
            error = %err,
            "recall_executed event append failed; recall result is unaffected"
        );
    }
}

/// One set insertion per prefix; the first occurrence keeps its original score.
fn duplicate_prefix_flags(prefixes: &[String]) -> impl Iterator<Item = bool> + '_ {
    let mut seen = HashSet::with_capacity(prefixes.len());
    prefixes.iter().map(move |prefix| {
        #[cfg(test)]
        loop_3711_tests::visit();
        !seen.insert(prefix.as_str())
    })
}

#[cfg(test)]
#[path = "recall_loop_3711_tests.rs"]
mod loop_3711_tests;

#[cfg(test)]
#[path = "recall_tests.rs"]
mod tests;
