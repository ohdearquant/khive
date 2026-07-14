//! Handler for `memory.recall` — the main retrieval pipeline.
//! See `crates/khive-pack-memory/docs/api/recall-pipeline.md`.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::recall_feedback::{on_recall_hit, on_recall_miss};

use serde_json::{json, Value};
use uuid::Uuid;

use khive_brain_core::{compute_query_class, PackTunable};
use khive_fusion::FusionStrategy;
use khive_runtime::{
    micros_to_iso, KhiveRuntime, Namespace, NamespaceToken, RuntimeError, SearchSource,
    VerbRegistry,
};
use khive_storage::types::{EdgeFilter, PageRequest};
use khive_storage::EdgeRelation;

use crate::config::{RecallConfig, ScoreBreakdown};
use crate::rerank::{weighted_rerank, RerankFeatures};
use crate::scoring::{
    calculate_score, contains_cjk, extract_entity_candidates, needs_multilingual,
    normalize_min_score, normalize_rank_fusion_scores, normalize_rrf_scores, ScoreInput,
};
use crate::MemoryPack;

use super::common::{
    compute_score, deser, fuse_candidates, make_pipeline, note_matches_tags, plog, plog_n,
    recall_candidate_count, to_json, validate_memory_type, RecallCandidateParams, RecallParams,
    TextSnippetPolicy, DEFAULT_DECAY_EPISODIC, DEFAULT_DECAY_SEMANTIC, DEFAULT_SALIENCE_EPISODIC,
    DEFAULT_SALIENCE_SEMANTIC, PROF_CID, RECALL_CALL_ID,
};

impl MemoryPack {
    pub(crate) async fn handle_recall(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        use std::sync::atomic::Ordering;

        let recall_start = Instant::now();
        let p: RecallParams = deser(params)?;

        // Re-derive dispatch's exact namespace as direct-call defense in depth; all later
        // candidate, graph, and ledger operations use this shadowed token.
        let effective_token: NamespaceToken = match p.namespace.as_deref() {
            Some(ns_str) => {
                let ns = Namespace::parse(ns_str).map_err(|e| {
                    RuntimeError::InvalidInput(format!("invalid namespace {ns_str:?}: {e}"))
                })?;
                token.with_namespace(ns)
            }
            None => token.clone(),
        };
        let token = &effective_token;

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
        if let Some(ref fs) = p.fusion_strategy {
            let mut new_strategy = super::common::parse_fusion_strategy_str(fs)?;
            if let (
                FusionStrategy::Weighted {
                    weights: ref mut new_w,
                },
                FusionStrategy::Weighted {
                    weights: ref existing_w,
                },
            ) = (&mut new_strategy, &cfg.fuse_strategy)
            {
                *new_w = existing_w.clone();
            }
            cfg.fuse_strategy = new_strategy;
        }
        cfg.validate()?;

        let effective_min_score: f32 = {
            let raw = if let Some(floor) = p.score_floor {
                floor as f64
            } else {
                cfg.min_score
            };
            normalize_min_score(raw).map_err(RuntimeError::from)?
        };

        let limit = if let Some(k) = p.top_k {
            k.min(crate::scoring::MAX_RECALL_LIMIT)
        } else {
            p.limit
                .map(|v| v as usize)
                .unwrap_or(10)
                .clamp(1, crate::scoring::MAX_RECALL_LIMIT)
        };
        let limit_u32 = u32::try_from(limit).unwrap_or(u32::MAX);

        let mut scoring_cfg = cfg.scoring.clone().unwrap_or_default();
        scoring_cfg.apply_dos_caps();

        let cjk_fts_bypass = scoring_cfg.enable_multilingual_routing && contains_cjk(query_trimmed);
        let use_multilingual =
            scoring_cfg.enable_multilingual_routing && needs_multilingual(query_trimmed);

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
        let served_by_profile_id: Option<String> = if let Some(ref pid) = p.profile_id {
            let resp = registry
                .dispatch("brain.profile", json!({ "profile_id": pid }))
                .await
                .map_err(|e| {
                    RuntimeError::InvalidInput(format!(
                        "profile_id {pid:?} is not a known profile: {e}"
                    ))
                })?;
            profile_state = super::common::balanced_recall_state_from_profile_response(&resp);
            Some(pid.clone())
        } else {
            let resolved =
                super::common::resolve_serving_profile(&self.brain_profile, token, registry).await;
            if let Some(ref profile_id) = resolved {
                match registry
                    .dispatch("brain.profile", json!({ "profile_id": profile_id }))
                    .await
                {
                    Ok(resp) => {
                        profile_state =
                            super::common::balanced_recall_state_from_profile_response(&resp);
                    }
                    Err(e) => {
                        tracing::warn!(
                            profile_id = %profile_id,
                            error = %e,
                            "ADR-104 §1: profile state read failed; recall scores with configured defaults"
                        );
                    }
                }
            }
            resolved
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
        let mut current_candidate_limit = candidate_limit;
        let mut candidates = self
            .collect_recall_candidates(
                query_trimmed,
                token,
                RecallCandidateParams {
                    candidate_limit: current_candidate_limit,
                    embedding_model: p.embedding_model.as_deref(),
                    cjk_fts_bypass,
                    use_multilingual,
                    scoring_cfg: &scoring_cfg,
                    snippet_policy: TextSnippetPolicy::Omit,
                    fts_gather: &effective_fts_gather,
                    ann_overfetch_max_rounds,
                    ann_ready_timeout_ms,
                },
            )
            .await?;
        let (mut memory_ids, mut notes_by_id) =
            self.load_memory_candidate_notes(token, &candidates).await?;

        for _round in 1..ann_overfetch_max_rounds {
            if memory_ids.len() >= limit {
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
            candidates = self
                .collect_recall_candidates(
                    query_trimmed,
                    token,
                    RecallCandidateParams {
                        candidate_limit: current_candidate_limit,
                        embedding_model: p.embedding_model.as_deref(),
                        cjk_fts_bypass,
                        use_multilingual,
                        scoring_cfg: &scoring_cfg,
                        snippet_policy: TextSnippetPolicy::Omit,
                        fts_gather: &effective_fts_gather,
                        ann_overfetch_max_rounds,
                        ann_ready_timeout_ms,
                    },
                )
                .await?;
            (memory_ids, notes_by_id) =
                self.load_memory_candidate_notes(token, &candidates).await?;
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

        let actual_multilingual_routed = candidates.multilingual_routed;
        // #836: at least one embedding model's vector leg hit the bounded
        // ANN readiness wait and was served FTS-only for this recall.
        let ann_degraded = candidates.ann_degraded;

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

        if prof {
            if let Some(ref t) = t_stage {
                plog_n(call_id, "fusion", t.elapsed().as_micros(), fused.len());
            }
            t_stage = Some(Instant::now());
        }

        if fused.is_empty() {
            self.track_recall_serve(
                token,
                registry,
                query_trimmed,
                served_by_profile_id.as_deref(),
                Vec::new(),
                recall_start.elapsed().as_micros() as i64,
            );
            if let Ok(mut state) = self.recall_state.lock() {
                on_recall_miss(&mut state);
            }
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

        // Only verbose responses pay for the second default-weight score.
        let is_verbose = cfg.include_breakdown || p.include_breakdown.unwrap_or(false);

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
            let prefix_len = scoring_cfg.mmr_prefix_len;
            let prefixes: Vec<String> = ranked
                .iter()
                .map(|sn| sn.note.content.chars().take(prefix_len).collect::<String>())
                .collect();

            for i in 1..ranked.len() {
                for j in 0..i {
                    if prefixes[i] == prefixes[j] {
                        ranked[i].rank_score =
                            (ranked[i].rank_score - scoring_cfg.mmr_penalty).max(0.0);
                        break;
                    }
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

            let graph = self.runtime.graph(token)?;
            let candidate_ids: Vec<Uuid> = ranked.iter().map(|sn| sn.id).collect();
            let mut superseded_by_edge: HashSet<Uuid> = HashSet::new();
            {
                let limit = candidate_ids.len().max(1) as u32;
                let edges = graph
                    .query_edges(
                        EdgeFilter {
                            target_ids: candidate_ids.clone(),
                            relations: vec![EdgeRelation::Supersedes],
                            ..EdgeFilter::default()
                        },
                        vec![],
                        PageRequest { limit, offset: 0 },
                    )
                    .await?;
                for edge in &edges.items {
                    superseded_by_edge.insert(edge.target_id);
                }
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

        ranked.sort_by(|a, b| {
            b.rank_score
                .partial_cmp(&a.rank_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.id.cmp(&b.id))
        });
        ranked.truncate(limit);

        let token_budget_chars = scoring_cfg.default_token_budget * scoring_cfg.chars_per_token;
        let pre_budget_count = ranked.len();
        let mut total_chars = 0usize;
        let mut budget_cutoff: Option<usize> = None;
        for (i, sn) in ranked.iter().enumerate() {
            let entry_chars = sn.note.content.len();
            if total_chars + entry_chars > token_budget_chars {
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
                    "score": sn.score,
                    "rank_score": sn.rank_score,
                    "raw_score": sn.raw_score,
                    "content": content_out,
                    "salience": sn.effective_salience,
                    "decay_factor": sn.effective_decay_factor,
                    "memory_type": sn.resolved_memory_type,
                    "created_at": micros_to_iso(sn.note.created_at),
                });
                if is_verbose {
                    result["breakdown"] = json!(sn.breakdown);
                }
                if actual_multilingual_routed {
                    result["multilingual_routed"] = json!(true);
                }
                if ann_degraded {
                    // Per-result stamp keeps degradation visible without verbose output.
                    result["degraded"] = json!("ann_unavailable");
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

        let target_ids = results
            .iter()
            .filter_map(|r| r.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();
        self.track_recall_serve(
            token,
            registry,
            query_trimmed,
            served_by_profile_id.as_deref(),
            target_ids,
            recall_start.elapsed().as_micros() as i64,
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

        to_json(&results)
    }

    fn track_recall_serve(
        &self,
        token: &NamespaceToken,
        registry: &VerbRegistry,
        query_raw: &str,
        served_by_profile_id: Option<&str>,
        target_ids: Vec<String>,
        latency_us: i64,
    ) {
        let result_count = target_ids.len();
        let registry = registry.clone();
        let namespace = token.namespace().as_str().to_string();
        let query_raw = query_raw.to_string();
        let query_class = compute_query_class(&query_raw);
        let served_by_profile_id = served_by_profile_id.map(str::to_string);
        let served_at_us = chrono::Utc::now().timestamp_micros();
        let actor = format!("{}:{}", token.actor().kind, token.actor().id);
        let runtime = self.runtime.clone();
        let token = token.clone();

        khive_runtime::track_background_task(async move {
            let mut ledger_params = json!({
                "namespace": namespace,
                "consumer_kind": "recall",
                "target_ids": target_ids,
                "query_raw": query_raw,
                "served_at": served_at_us,
            });
            if let Some(ref profile_id) = served_by_profile_id {
                ledger_params["served_by_profile_id"] = json!(profile_id);
            }
            if let Err(error) = registry.dispatch("brain.record_serve", ledger_params).await {
                tracing::warn!(
                    error = %error,
                    "serve ledger dispatch failed; recall result is unaffected"
                );
            }

            emit_recall_executed_event(
                &runtime,
                &token,
                actor,
                served_by_profile_id,
                query_class,
                result_count,
                latency_us,
            )
            .await;
        });
    }
}

/// Append best-effort recall telemetry without affecting the recall response.
async fn emit_recall_executed_event(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    actor: String,
    served_by_profile_id: Option<String>,
    query_class: String,
    result_count: usize,
    latency_us: i64,
) {
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
    let payload = json!({
        "actor": actor,
        "served_by_profile_id": served_by_profile_id,
        "query_class": query_class,
        "result_count": result_count,
        "latency_us": latency_us,
    });
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

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex as StdMutex};

    use async_trait::async_trait;
    use khive_pack_kg::KgPack;
    use khive_runtime::{
        EmbedderProvider, KhiveRuntime, Namespace, RuntimeError, VerbRegistryBuilder,
    };
    use khive_storage::Entity;
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
    use serde_json::Value;
    use serial_test::serial;
    use tokio::sync::Notify;
    use tracing::field::{Field, Visit};
    use uuid::Uuid;

    use crate::MemoryPack;

    #[derive(Clone, Debug, Default)]
    struct CapturedWarning {
        fields: HashMap<String, String>,
    }

    #[derive(Default)]
    struct WarningVisitor(CapturedWarning);

    impl Visit for WarningVisitor {
        fn record_str(&mut self, field: &Field, value: &str) {
            self.0
                .fields
                .insert(field.name().to_string(), value.to_string());
        }

        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0.fields.insert(
                field.name().to_string(),
                format!("{value:?}").trim_matches('"').to_string(),
            );
        }
    }

    struct WarningCapture {
        warnings: Arc<StdMutex<Vec<CapturedWarning>>>,
    }

    impl tracing::Subscriber for WarningCapture {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            if *event.metadata().level() == tracing::Level::WARN {
                let mut visitor = WarningVisitor::default();
                event.record(&mut visitor);
                self.warnings.lock().unwrap().push(visitor.0);
            }
        }

        fn enter(&self, _: &tracing::span::Id) {}

        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// Exercises `$` sanitization; serialized because non-empty recall tracks background work.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn recall_with_dollar_sign_query_does_not_error() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let ns = Namespace::parse("local").expect("local namespace");
        let token = rt.authorize(ns).expect("authorize local");

        rt.create_note(
            &token,
            "memory",
            None,
            "use $prev.id to chain calls",
            Some(0.7),
            None,
            vec![],
        )
        .await
        .expect("create note");

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        let registry = builder.build().expect("registry");

        let result = registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "query": "$prev.id",
                    "limit": 10
                }),
            )
            .await;

        assert!(
            result.is_ok(),
            "#388 memory.recall must not hard-fail on a '$'-bearing query, got: {:?}",
            result.err()
        );
    }

    // Deterministic embedding service: distinct vector per unique text via FNV
    // hash (copied from `pack.rs`'s `ann_route_tests` — not semantically
    // meaningful, but reproducible: identical input text always yields an
    // identical vector, which is all a cosine-similarity vector leg needs to
    // prove it found the right note).
    struct HashVecService {
        dims: usize,
    }

    fn fnv_to_vec(text: &str, dims: usize) -> Vec<f32> {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in text.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0001_0000_01b3);
        }
        let mut v = Vec::with_capacity(dims);
        let mut s = h;
        for _ in 0..dims {
            s = s
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            v.push(((s >> 33) as f32) / (0x7fff_ffff_u32 as f32) - 1.0);
        }
        v
    }

    #[async_trait]
    impl EmbeddingService for HashVecService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts.iter().map(|t| fnv_to_vec(t, self.dims)).collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "hash-vec"
        }
    }

    struct HashVecProvider {
        model_name: String,
        dims: usize,
    }

    #[async_trait]
    impl EmbedderProvider for HashVecProvider {
        fn name(&self) -> &str {
            &self.model_name
        }

        fn dimensions(&self) -> usize {
            self.dims
        }

        async fn build(&self) -> Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
            Ok(Arc::new(HashVecService { dims: self.dims }))
        }
    }

    /// Verifies `@` is quoted as an unsafe FTS5 bareword and dispatch succeeds.
    // `#[serial(background_tasks)]`: kept to match the fixture setup used by
    // the sibling dollar-sign test above.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn recall_with_residual_fts5_char_now_sanitized() {
        const MODEL: &str = "recall-residual-char-test-model";
        const DIMS: usize = 32;
        const NOTE_TEXT: &str = "foo@bar chain call helper note";

        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        rt.register_embedder(HashVecProvider {
            model_name: MODEL.to_owned(),
            dims: DIMS,
        });

        let ns = Namespace::parse("local").expect("local namespace");
        let token = rt.authorize(ns).expect("authorize local");

        // embedding_model: None — create_note auto-detects the registered
        // custom provider (resolve_embedding_model only handles lattice
        // aliases; custom provider names go through the auto-detect path).
        rt.create_note(&token, "memory", None, NOTE_TEXT, Some(0.7), None, vec![])
            .await
            .expect("create note");

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        let registry = builder.build().expect("registry");

        // Query text matches the note content exactly so the hash-vec embedder
        // (which has no semantic notion of similarity) would produce an
        // identical vector for query and note if the FTS leg degraded instead
        // of failing loud.
        let result = registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "query": NOTE_TEXT,
                    "limit": 10
                }),
            )
            .await;

        let value = result.unwrap_or_else(|e| {
            panic!("#916 memory.recall must not fail on an '@'-bearing query, got: {e:?}")
        });
        let hits = value.as_array().expect("recall result must be an array");
        assert!(
            !hits.is_empty(),
            "#916 '@'-bearing query must still find the seeded note; got {value:?}"
        );
    }

    // ── #836: bounded ANN readiness wait + FTS-only degraded fallback ─────────

    /// A held ANN warm lock must time out to a marked FTS-only result.
    /// See `crates/khive-pack-memory/docs/recall-reliability.md`.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn recall_836_degrades_to_fts_only_when_ann_lock_is_held() {
        const MODEL: &str = "recall-836-ann-timeout-model";
        const DIMS: usize = 16;
        const NOTE_TEXT: &str = "issue 836 bounded ann acquire recall fts fallback note";

        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        rt.register_embedder(HashVecProvider {
            model_name: MODEL.to_owned(),
            dims: DIMS,
        });

        let ns = Namespace::parse("local").expect("local namespace");
        let token = rt.authorize(ns).expect("authorize local");

        rt.create_note(&token, "memory", None, NOTE_TEXT, Some(0.7), None, vec![])
            .await
            .expect("create note");

        let pack = MemoryPack::new(rt.clone());
        let ann_handle = pack.ann.clone();

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(pack);
        let registry = builder.build().expect("registry");

        // Simulate the daemon's boot-time background warm holding the
        // per-model single-flight lock mid-build (ann.rs `model_warm_lock`),
        // exactly the contention #836 diagnosed.
        let key = crate::ann::AnnKey::new("local", MODEL);
        let _held = crate::ann::hold_model_warm_lock_for_test(&ann_handle, &key).await;

        let start = std::time::Instant::now();
        let result = registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "query": "836 bounded ann acquire",
                    "limit": 10,
                    "config": { "ann_ready_timeout_ms": 100 }
                }),
            )
            .await
            .expect("recall must not error when the ANN leg times out");
        let elapsed = start.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "#836 recall must return within the bounded ANN wait, took {elapsed:?}"
        );

        let results = result.as_array().expect("recall result must be an array");
        assert!(
            !results.is_empty(),
            "FTS leg must still surface the seeded note when the ANN leg degrades"
        );
        for r in results {
            assert_eq!(
                r.get("degraded").and_then(Value::as_str),
                Some("ann_unavailable"),
                "#836 degraded result must carry the ann_unavailable marker, got: {r:?}"
            );
        }
    }

    /// Uncontended ANN readiness must not add a degradation marker.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn recall_836_normal_path_has_no_degraded_marker() {
        const MODEL: &str = "recall-836-ann-normal-model";
        const DIMS: usize = 16;
        const NOTE_TEXT: &str = "issue 836 normal path recall without any ann contention";

        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        rt.register_embedder(HashVecProvider {
            model_name: MODEL.to_owned(),
            dims: DIMS,
        });

        let ns = Namespace::parse("local").expect("local namespace");
        let token = rt.authorize(ns).expect("authorize local");

        rt.create_note(&token, "memory", None, NOTE_TEXT, Some(0.7), None, vec![])
            .await
            .expect("create note");

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        let registry = builder.build().expect("registry");

        let result = registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "query": "836 normal path recall",
                    "limit": 10
                }),
            )
            .await
            .expect("recall must succeed on the normal, uncontended path");

        let results = result.as_array().expect("recall result must be an array");
        assert!(
            !results.is_empty(),
            "normal recall must surface the seeded note"
        );
        for r in results {
            assert!(
                r.get("degraded").is_none(),
                "normal recall must not carry a degraded marker, got: {r:?}"
            );
        }
    }

    /// ANN timeout plus no FTS match returns an empty result, never an error.
    #[tokio::test]
    async fn recall_836_degraded_with_zero_fts_hits_returns_empty_not_error() {
        const MODEL: &str = "recall-836-ann-timeout-empty-model";
        const DIMS: usize = 16;

        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        rt.register_embedder(HashVecProvider {
            model_name: MODEL.to_owned(),
            dims: DIMS,
        });

        // Deliberately no notes seeded: the FTS leg has nothing to match, so
        // an ANN-degraded recall must still resolve to an empty array rather
        // than propagating an error.
        let pack = MemoryPack::new(rt.clone());
        let ann_handle = pack.ann.clone();

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(pack);
        let registry = builder.build().expect("registry");

        let key = crate::ann::AnnKey::new("local", MODEL);
        let _held = crate::ann::hold_model_warm_lock_for_test(&ann_handle, &key).await;

        let result = registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "query": "no such content exists anywhere",
                    "limit": 10,
                    "config": { "ann_ready_timeout_ms": 100 }
                }),
            )
            .await
            .expect("recall must not error when both legs come up empty under ANN degradation");

        assert_eq!(
            result,
            serde_json::json!([]),
            "#836 degraded recall with no FTS hits must return an empty array, not an error"
        );
    }

    /// A self-build timeout must leave a tracked build that warms later recalls.
    /// See `crates/khive-pack-memory/docs/recall-reliability.md`.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn recall_836_self_build_timeout_detaches_build_instead_of_dropping_it() {
        const MODEL: &str = "recall-836-self-build-detach-model";
        const DIMS: usize = 16;
        const NOTE_TEXT: &str = "issue 836 self build detach recall regression note";

        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        rt.register_embedder(HashVecProvider {
            model_name: MODEL.to_owned(),
            dims: DIMS,
        });

        let ns = Namespace::parse("local").expect("local namespace");
        let token = rt.authorize(ns).expect("authorize local");

        rt.create_note(&token, "memory", None, NOTE_TEXT, Some(0.7), None, vec![])
            .await
            .expect("create note");

        let pack = MemoryPack::new(rt.clone());
        let ann_handle = pack.ann.clone();

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(pack);
        let registry = builder.build().expect("registry");

        // Deliberately no lock held here — this is the self-build case: this
        // recall's own detached `ensure_ann_for_model` call is the only
        // build in flight for this model.
        let result = registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "query": "836 self build detach",
                    "limit": 10,
                    "config": { "ann_ready_timeout_ms": 0 }
                }),
            )
            .await
            .expect("recall must not error when the self-build ANN leg times out");

        let results = result.as_array().expect("recall result must be an array");
        assert!(
            !results.is_empty(),
            "FTS leg must still surface the seeded note while the ANN leg degrades"
        );
        for r in results {
            assert_eq!(
                r.get("degraded").and_then(Value::as_str),
                Some("ann_unavailable"),
                "first recall must still degrade to FTS-only within the \
                 near-zero timeout, got: {r:?}"
            );
        }

        // The detached build must keep running after the timed-out recall
        // returns — poll the ANN cache directly (mirrors ann.rs's own
        // #812/#844 convergence tests) rather than sleeping a fixed amount.
        let key = crate::ann::AnnKey::new("local", MODEL);
        let mut warmed = false;
        for _ in 0..300 {
            if crate::ann::is_current(&ann_handle, &key).await {
                warmed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            warmed,
            "the detached build must eventually install a fresh ANN index for \
             {MODEL} instead of being dropped on timeout (#836)"
        );

        // A later recall must now take the vector path — no degraded marker.
        let result2 = registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "query": "836 self build detach",
                    "limit": 10
                }),
            )
            .await
            .expect("recall must succeed once the detached build has warmed the index");
        let results2 = result2.as_array().expect("recall result must be an array");
        assert!(
            !results2.is_empty(),
            "warmed recall must still surface the seeded note"
        );
        for r in results2 {
            assert!(
                r.get("degraded").is_none(),
                "a recall issued after the detached build completes must take \
                 the vector path, not degrade, got: {r:?}"
            );
        }
    }

    // ── ADR-081 §5 (#394): recall serve-time attribution + ledger append ──────

    fn build_full_rt_with_brain() -> khive_runtime::KhiveRuntime {
        let tmp = tempfile::Builder::new()
            .prefix("khive-mem-recall-adr081-")
            .tempdir_in(std::env::temp_dir())
            .expect("temp dir");
        let db_path = tmp.path().join("khive.db");
        std::mem::forget(tmp);

        khive_runtime::KhiveRuntime::new(khive_runtime::RuntimeConfig {
            db_path: Some(db_path),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string(), "memory".to_string(), "brain".to_string()],
            ..khive_runtime::RuntimeConfig::default()
        })
        .expect("runtime")
    }

    // `#[serial(background_tasks)]`: see the note on
    // `recall_with_dollar_sign_query_does_not_error` above — this test
    // directly exercises the same `track_background_task`-driven ledger
    // append it names, so it shares the process-wide counter.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn recall_stamps_served_by_profile_id_and_appends_serve_ledger_row() {
        use khive_pack_brain::BrainPack;

        let rt = build_full_rt_with_brain();
        let ns = Namespace::parse("local").expect("local namespace");
        let token = rt.authorize(ns.clone()).expect("authorize local");

        let note_id = rt
            .create_note(
                &token,
                "memory",
                None,
                "adr081 recall stamp note",
                Some(0.7),
                None,
                vec![],
            )
            .await
            .expect("create note");

        let brain = BrainPack::new(rt.clone());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        builder.register(brain);
        let registry = builder.build().expect("registry");

        registry
            .dispatch(
                "brain.create_profile",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "name": "adr081-recall-v1",
                    "consumer_kind": "recall",
                }),
            )
            .await
            .expect("create profile");
        registry
            .dispatch(
                "brain.activate",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "profile_id": "adr081-recall-v1",
                }),
            )
            .await
            .expect("activate profile");
        registry
            .dispatch(
                "brain.bind",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "profile_id": "adr081-recall-v1",
                    "consumer_kind": "recall",
                }),
            )
            .await
            .expect("bind profile");

        let result = registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "query": "adr081 recall stamp note",
                    "limit": 10
                }),
            )
            .await
            .expect("memory.recall");

        let hits = result.as_array().expect("bare array result");
        assert!(!hits.is_empty(), "must find the seeded note");
        assert_eq!(
            hits[0]["served_by_profile_id"],
            serde_json::json!("adr081-recall-v1"),
            "recall response must stamp the resolved serving profile"
        );

        // The ledger append is fired via track_background_task off the response
        // path, so poll briefly rather than assume it has landed by the time recall returns.
        let target_id = note_id.id.to_string();
        let mut found = false;
        for _ in 0..100 {
            let mut reader = rt.sql().reader().await.expect("reader");
            let row = reader
                .query_row(khive_storage::types::SqlStatement {
                    sql: "SELECT served_by_profile_id FROM brain_serve_ledger \
                          WHERE target_id = ?1"
                        .into(),
                    params: vec![khive_storage::types::SqlValue::Text(target_id.clone())],
                    label: None,
                })
                .await
                .expect("query row");
            if let Some(row) = row {
                assert!(
                    matches!(
                        row.get("served_by_profile_id"),
                        Some(khive_storage::types::SqlValue::Text(s)) if s == "adr081-recall-v1"
                    ),
                    "ledger row must carry the same served_by_profile_id as the response stamp"
                );
                found = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            found,
            "serve ledger row for the recalled target must appear within 2s"
        );
    }

    // ── #866: `recall_executed` event-plane emission ────────────────────────

    // `#[serial(background_tasks)]`: shares `recall_stamps_served_by_profile_id_
    // and_appends_serve_ledger_row`'s rationale above: this test drives the
    // same `track_background_task`-fired path (serve-ledger append +
    // `recall_executed` emission now live in the same tracked task).
    #[tokio::test]
    #[serial(background_tasks)]
    async fn recall_emits_exactly_one_recall_executed_event() {
        let rt = build_full_rt_with_brain();
        let ns = Namespace::parse("local").expect("local namespace");
        let token = rt.authorize(ns.clone()).expect("authorize local");

        rt.create_note(
            &token,
            "memory",
            None,
            "866 recall executed event note",
            Some(0.7),
            None,
            vec![],
        )
        .await
        .expect("create note");

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        builder.register(khive_pack_brain::BrainPack::new(rt.clone()));
        let registry = builder.build().expect("registry");

        let result = registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "query": "866 recall executed event note",
                    "limit": 10
                }),
            )
            .await
            .expect("memory.recall");
        let hits = result.as_array().expect("bare array result");
        assert!(!hits.is_empty(), "must find the seeded note");

        // The emission is fired via track_background_task off the response
        // path (same seam as the serve-ledger append), so poll briefly rather
        // than assume it has landed by the time recall returns.
        let store = rt.events(&token).expect("event store for local namespace");
        let mut recall_events = Vec::new();
        for _ in 0..100 {
            let page = store
                .query_events(
                    khive_storage::EventFilter {
                        kinds: vec![khive_types::EventKind::RecallExecuted],
                        ..Default::default()
                    },
                    khive_storage::types::PageRequest {
                        limit: 50,
                        offset: 0,
                    },
                )
                .await
                .expect("query_events");
            if !page.items.is_empty() {
                recall_events = page.items;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        assert_eq!(
            recall_events.len(),
            1,
            "exactly one recall_executed event per recall serve, got: {recall_events:?}"
        );
        let event = &recall_events[0];
        assert_eq!(event.kind, khive_types::EventKind::RecallExecuted);
        assert_eq!(event.verb, "memory.recall");
        assert_eq!(
            event.payload["result_count"],
            serde_json::json!(hits.len()),
            "result_count must match the number of returned results"
        );
        assert_eq!(
            event.payload["served_by_profile_id"],
            serde_json::Value::Null,
            "no profile was bound/resolved for this call"
        );
        assert!(
            event.payload["query_class"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "query_class must be a non-empty deterministic key, got: {:?}",
            event.payload["query_class"]
        );
        assert!(
            event.payload["latency_us"]
                .as_i64()
                .is_some_and(|us| us >= 0),
            "latency_us must be a non-negative measured duration, got: {:?}",
            event.payload["latency_us"]
        );
        assert!(
            event.payload["actor"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "actor must be stamped, got: {:?}",
            event.payload["actor"]
        );
        assert_eq!(event.payload["actor"], serde_json::json!(event.actor));

        let counts = registry
            .dispatch(
                "brain.event_counts",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "since": "1970-01-01T00:00:00Z",
                    "kind": "recall_executed",
                }),
            )
            .await
            .expect("brain.event_counts");
        assert_eq!(counts["total"], serde_json::json!(1));
        assert_eq!(
            counts["counts_by_kind"]["recall_executed"],
            serde_json::json!(1)
        );
    }

    #[tokio::test]
    #[serial(background_tasks)]
    async fn successful_empty_recall_emits_recall_executed_event() {
        let rt = build_full_rt_with_brain();
        let ns = Namespace::parse("local").expect("local namespace");
        let token = rt.authorize(ns.clone()).expect("authorize local");

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        builder.register(khive_pack_brain::BrainPack::new(rt.clone()));
        let registry = builder.build().expect("registry");

        let result = registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "query": "no matching memory exists",
                    "limit": 10
                }),
            )
            .await
            .expect("memory.recall");
        assert_eq!(result, serde_json::json!([]));

        let store = rt.events(&token).expect("event store for local namespace");
        let mut recall_events = Vec::new();
        for _ in 0..100 {
            let page = store
                .query_events(
                    khive_storage::EventFilter {
                        kinds: vec![khive_types::EventKind::RecallExecuted],
                        ..Default::default()
                    },
                    khive_storage::types::PageRequest {
                        limit: 50,
                        offset: 0,
                    },
                )
                .await
                .expect("query_events");
            if !page.items.is_empty() {
                recall_events = page.items;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        assert_eq!(recall_events.len(), 1);
        assert_eq!(
            recall_events[0].payload["result_count"],
            serde_json::json!(0)
        );
    }

    #[tokio::test]
    #[serial(background_tasks)]
    async fn recall_failure_emits_no_recall_executed_event() {
        let rt = build_full_rt_with_brain();
        let ns = Namespace::parse("local").expect("local namespace");
        let token = rt.authorize(ns.clone()).expect("authorize local");

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        builder.register(khive_pack_brain::BrainPack::new(rt.clone()));
        let registry = builder.build().expect("registry");

        // `RecallParams::query` has no `#[serde(default)]`, so omitting it fails
        // deserialization before the handler can schedule telemetry.
        let result = registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "limit": 10
                }),
            )
            .await;
        assert!(
            result.is_err(),
            "a recall missing the required `query` field must fail, not silently succeed"
        );

        // Give any (incorrectly fired) background emission a moment to land,
        // then assert the event plane stayed empty for this kind.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let store = rt.events(&token).expect("event store for local namespace");
        let page = store
            .query_events(
                khive_storage::EventFilter {
                    kinds: vec![khive_types::EventKind::RecallExecuted],
                    ..Default::default()
                },
                khive_storage::types::PageRequest {
                    limit: 50,
                    offset: 0,
                },
            )
            .await
            .expect("query_events");
        assert!(
            page.items.is_empty(),
            "a failed recall must not emit any recall_executed event, got: {:?}",
            page.items
        );
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial(background_tasks)]
    async fn recall_event_store_acquisition_failure_warns_without_failing_response() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let db_path = tmp.path().join("khive.db");
        let backend = Arc::new(khive_db::StorageBackend::sqlite(&db_path).expect("backend"));
        {
            let mut writer = backend.pool().writer().expect("migration writer");
            khive_db::run_migrations(writer.conn_mut()).expect("migrations");
        }
        let config = khive_runtime::RuntimeConfig {
            db_path: Some(db_path),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string(), "memory".to_string(), "brain".to_string()],
            ..khive_runtime::RuntimeConfig::default()
        };
        let rt = KhiveRuntime::from_backend(Arc::clone(&backend), config);
        let ns = Namespace::parse("local").expect("local namespace");
        let token = rt.authorize(ns.clone()).expect("authorize local");

        rt.create_note(
            &token,
            "memory",
            None,
            "event acquisition failure recall note",
            Some(0.7),
            None,
            vec![],
        )
        .await
        .expect("create note");

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        builder.register(khive_pack_brain::BrainPack::new(rt.clone()));
        let registry = builder.build().expect("registry");

        {
            let writer = backend.pool().writer().expect("fault injection writer");
            writer
                .conn()
                .execute_batch("DROP TABLE events; PRAGMA query_only = ON;")
                .expect("remove only the event schema and reject its recreation");
        }

        let warnings = Arc::new(StdMutex::new(Vec::new()));
        let _capture = tracing::subscriber::set_default(WarningCapture {
            warnings: Arc::clone(&warnings),
        });

        let result = registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "query": "event acquisition failure recall note",
                    "limit": 10
                }),
            )
            .await
            .expect("event-store acquisition failure must not fail memory.recall");
        assert!(!result.as_array().expect("bare array result").is_empty());

        let warning = loop {
            if let Some(warning) = warnings.lock().unwrap().iter().find(|warning| {
                warning.fields.get("message").map(String::as_str)
                    == Some("recall_executed event store acquisition failed; recall result is unaffected")
            }).cloned() {
                break warning;
            }
            assert!(
                khive_runtime::background_task_count() > 0,
                "background task completed without an observable acquisition warning"
            );
            tokio::task::yield_now().await;
        };

        assert_eq!(
            warning.fields.get("namespace").map(String::as_str),
            Some("local")
        );
        assert_eq!(
            warning.fields.get("event_kind").map(String::as_str),
            Some("recall_executed")
        );
        assert!(warning
            .fields
            .get("error")
            .is_some_and(|error| !error.is_empty()));
    }

    // `#[serial(background_tasks)]`: see the note on
    // `recall_with_dollar_sign_query_does_not_error` above — this test
    // directly exercises the same `track_background_task`-driven ledger
    // append it names, so it shares the process-wide counter.
    //
    // #697 (c): the serve-time stamp resolves through an actor-scoped binding,
    // not just a namespace-scoped one. Before #697, `resolve_serving_profile`
    // called `resolve_consumer_profile` with no actor, so a binding keyed on
    // actor (namespace left "*") could never match here.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn recall_stamps_served_by_profile_id_via_actor_binding() {
        use khive_pack_brain::BrainPack;

        let tmp = tempfile::Builder::new()
            .prefix("khive-mem-recall-actor-binding-")
            .tempdir_in(std::env::temp_dir())
            .expect("temp dir");
        let db_path = tmp.path().join("khive.db");
        std::mem::forget(tmp);

        let rt = khive_runtime::KhiveRuntime::new(khive_runtime::RuntimeConfig {
            db_path: Some(db_path),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string(), "memory".to_string(), "brain".to_string()],
            actor_id: Some("leo".to_string()),
            ..khive_runtime::RuntimeConfig::default()
        })
        .expect("runtime");
        let ns = Namespace::parse("local").expect("local namespace");
        let token = rt.authorize(ns.clone()).expect("authorize local");
        assert_eq!(
            token.actor().id,
            "leo",
            "test setup: token must carry the configured actor"
        );

        let note_id = rt
            .create_note(
                &token,
                "memory",
                None,
                "actor binding recall stamp note",
                Some(0.7),
                None,
                vec![],
            )
            .await
            .expect("create note");

        let brain = BrainPack::new(rt.clone());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        builder.register(brain);
        // `VerbRegistry` mints its own per-dispatch tokens from its own
        // construction-baked actor id (independent of `RuntimeConfig::actor_id`,
        // which only affects tokens minted directly via `rt.authorize`) — bake
        // the same actor here so `registry.dispatch` calls carry it too.
        builder.with_actor_id(Some("leo".to_string()));
        let registry = builder.build().expect("registry");

        registry
            .dispatch(
                "brain.create_profile",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "name": "leo-actor-recall-v1",
                    "consumer_kind": "recall",
                }),
            )
            .await
            .expect("create profile");
        registry
            .dispatch(
                "brain.activate",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "profile_id": "leo-actor-recall-v1",
                }),
            )
            .await
            .expect("activate profile");
        // Bind by actor only — namespace defaults to the "*" wildcard — so a
        // namespace-only resolution can never reach this binding.
        registry
            .dispatch(
                "brain.bind",
                serde_json::json!({
                    "actor": "leo",
                    "profile_id": "leo-actor-recall-v1",
                    "consumer_kind": "recall",
                }),
            )
            .await
            .expect("bind profile to actor");

        let result = registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "query": "actor binding recall stamp note",
                    "limit": 10
                }),
            )
            .await
            .expect("memory.recall");

        let hits = result.as_array().expect("bare array result");
        assert!(!hits.is_empty(), "must find the seeded note");
        assert_eq!(
            hits[0]["served_by_profile_id"],
            serde_json::json!("leo-actor-recall-v1"),
            "recall response must stamp the actor-bound profile, not the default"
        );

        // The ledger append is fired via track_background_task off the response
        // path — poll briefly rather than assume it has landed by the time recall returns.
        let target_id = note_id.id.to_string();
        let mut found = false;
        for _ in 0..100 {
            let mut reader = rt.sql().reader().await.expect("reader");
            let row = reader
                .query_row(khive_storage::types::SqlStatement {
                    sql: "SELECT served_by_profile_id FROM brain_serve_ledger \
                          WHERE target_id = ?1"
                        .into(),
                    params: vec![khive_storage::types::SqlValue::Text(target_id.clone())],
                    label: None,
                })
                .await
                .expect("query row");
            if let Some(row) = row {
                assert!(
                    matches!(
                        row.get("served_by_profile_id"),
                        Some(khive_storage::types::SqlValue::Text(s)) if s == "leo-actor-recall-v1"
                    ),
                    "serve ledger row must carry the actor-bound profile id"
                );
                found = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            found,
            "serve ledger row for the recalled target must appear within 2s"
        );
    }

    // The actor-resolved profile must both project weights and stamp the response.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn recall_serve_time_projection_uses_the_actor_resolved_profile() {
        use khive_pack_brain::BrainPack;

        let tmp = tempfile::Builder::new()
            .prefix("khive-mem-recall-adr104-resolution-")
            .tempdir_in(std::env::temp_dir())
            .expect("temp dir");
        let db_path = tmp.path().join("khive.db");
        std::mem::forget(tmp);

        let rt = khive_runtime::KhiveRuntime::new(khive_runtime::RuntimeConfig {
            db_path: Some(db_path),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string(), "memory".to_string(), "brain".to_string()],
            actor_id: Some("leo".to_string()),
            ..khive_runtime::RuntimeConfig::default()
        })
        .expect("runtime");
        rt.register_embedder(FixedVecProvider {
            model_name: ADR104_MODEL.to_string(),
            map: adr104_fixed_vectors(),
        });
        let ns = Namespace::parse("local").expect("local namespace");
        let token = rt.authorize(ns.clone()).expect("authorize local");
        assert_eq!(
            token.actor().id,
            "leo",
            "test setup: token must carry the configured actor"
        );

        let note_id = rt
            .create_note(
                &token,
                "memory",
                None,
                ADR104_H_CONTENT,
                Some(0.1),
                None,
                vec![],
            )
            .await
            .expect("create note")
            .id;

        let brain = BrainPack::new(rt.clone());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        builder.register(brain);
        builder.with_actor_id(Some("leo".to_string()));
        let registry = builder.build().expect("registry");

        registry
            .dispatch(
                "brain.create_profile",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "name": "adr104-resolution-v1",
                    "consumer_kind": "recall",
                }),
            )
            .await
            .expect("create profile");
        registry
            .dispatch(
                "brain.bind",
                serde_json::json!({
                    "actor": "leo",
                    "profile_id": "adr104-resolution-v1",
                    "consumer_kind": "recall",
                }),
            )
            .await
            .expect("bind profile to actor");

        // Skew the posterior so the assertion verifies projection, not only stamping.
        adr104_skew_salience(&registry, "adr104-resolution-v1", note_id, 30).await;

        let result = registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "query": ADR104_QUERY,
                    "fusion_strategy": "vector_only",
                    "embedding_model": ADR104_MODEL,
                    "include_breakdown": true,
                    "limit": 10
                }),
            )
            .await
            .expect("memory.recall");

        let hits = result.as_array().expect("bare array result");
        assert!(!hits.is_empty(), "must find the seeded note");
        assert_eq!(
            hits[0]["served_by_profile_id"],
            serde_json::json!("adr104-resolution-v1"),
            "recall response must stamp the actor-resolved profile, not the \
             profile_id override path (which was not used in this test)"
        );

        let profile_component = hits[0]["breakdown"]["profile_component"]
            .as_f64()
            .expect("profile_component present under include_breakdown");
        assert!(
            (profile_component - 1.0).abs() > 1e-6,
            "serve-time projection must have used the actor-resolved profile's \
             skewed posterior state, not defaults: profile_component={profile_component}"
        );
    }

    // Anonymous callers must not match an explicit `actor="local"` binding.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn recall_anonymous_caller_does_not_match_explicit_actor_local_binding() {
        use khive_pack_brain::BrainPack;

        // No `actor_id` configured — `rt.authorize` mints the anonymous actor
        // (id="local"), matching an unauthenticated caller.
        let rt = build_full_rt_with_brain();
        let ns = Namespace::parse("local").expect("local namespace");
        let token = rt.authorize(ns.clone()).expect("authorize local");
        assert!(
            token.actor().is_anonymous(),
            "test setup: token must carry the anonymous actor"
        );

        let note_id = rt
            .create_note(
                &token,
                "memory",
                None,
                "anonymous actor binding fall-through note",
                Some(0.7),
                None,
                vec![],
            )
            .await
            .expect("create note");

        let brain = BrainPack::new(rt.clone());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        builder.register(brain);
        // No `with_actor_id` call — registry-minted tokens stay anonymous too.
        let registry = builder.build().expect("registry");

        registry
            .dispatch(
                "brain.create_profile",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "name": "anon-local-recall-v1",
                    "consumer_kind": "recall",
                }),
            )
            .await
            .expect("create profile");
        registry
            .dispatch(
                "brain.activate",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "profile_id": "anon-local-recall-v1",
                }),
            )
            .await
            .expect("activate profile");
        // Bind explicitly to actor="local" — the exact id anonymous tokens carry.
        registry
            .dispatch(
                "brain.bind",
                serde_json::json!({
                    "actor": "local",
                    "profile_id": "anon-local-recall-v1",
                    "consumer_kind": "recall",
                }),
            )
            .await
            .expect("bind profile to actor=local");

        let result = registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "query": "anonymous actor binding fall-through note",
                    "limit": 10
                }),
            )
            .await
            .expect("memory.recall");

        let hits = result.as_array().expect("bare array result");
        assert!(!hits.is_empty(), "must find the seeded note");
        assert!(
            hits[0].get("served_by_profile_id").is_none(),
            "anonymous caller must NOT match the actor=\"local\" binding: the \
             serve stamp must be omitted (unresolved profile), not carry \
             anon-local-recall-v1: {:?}",
            hits[0]
        );

        // The target note's id must never appear in the ledger with the
        // bound profile — poll briefly to catch a delayed async append.
        let target_id = note_id.id.to_string();
        for _ in 0..20 {
            let mut reader = rt.sql().reader().await.expect("reader");
            let row = reader
                .query_row(khive_storage::types::SqlStatement {
                    sql: "SELECT served_by_profile_id FROM brain_serve_ledger \
                          WHERE target_id = ?1"
                        .into(),
                    params: vec![khive_storage::types::SqlValue::Text(target_id.clone())],
                    label: None,
                })
                .await
                .expect("query row");
            if let Some(row) = row {
                assert!(
                    !matches!(
                        row.get("served_by_profile_id"),
                        Some(khive_storage::types::SqlValue::Text(s)) if s == "anon-local-recall-v1"
                    ),
                    "serve ledger row must not credit the actor=\"local\" binding \
                     to an anonymous caller"
                );
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    // `#[serial(background_tasks)]`: non-empty recall — see the note on
    // `recall_with_dollar_sign_query_does_not_error` above.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn recall_without_brain_pack_omits_stamp_and_does_not_error() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let ns = Namespace::parse("local").expect("local namespace");
        let token = rt.authorize(ns.clone()).expect("authorize local");

        rt.create_note(
            &token,
            "memory",
            None,
            "no brain pack loaded note",
            Some(0.7),
            None,
            vec![],
        )
        .await
        .expect("create note");

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        let registry = builder.build().expect("registry");

        let result = registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "query": "no brain pack loaded note",
                    "limit": 10
                }),
            )
            .await
            .expect("recall must succeed even without a brain pack loaded");

        let hits = result.as_array().expect("bare array result");
        assert!(!hits.is_empty());
        assert!(
            hits[0].get("served_by_profile_id").is_none(),
            "no brain pack registered => no profile resolvable => no stamp"
        );
    }

    // ── Auto entity-name extraction (dead `entity_names` parameter fix) ────────

    /// Return one hit's score; a single-note RRF corpus isolates entity adjustment effects.
    async fn dispatch_single_note_recall(
        content: &str,
        query: &str,
        entity_names: Option<&[&str]>,
    ) -> f64 {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let ns = Namespace::parse("local").expect("local namespace");
        let token = rt.authorize(ns).expect("authorize local");
        rt.create_note(&token, "memory", None, content, Some(0.5), None, vec![])
            .await
            .expect("create note");

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        let registry = builder.build().expect("registry");

        let mut params = serde_json::json!({
            "query": query,
            "fusion_strategy": "rrf",
            "limit": 10
        });
        if let Some(names) = entity_names {
            params["entity_names"] = serde_json::json!(names);
        }

        let result = registry
            .dispatch("memory.recall", params)
            .await
            .expect("memory.recall");
        let hits = result.as_array().expect("bare array result");
        assert_eq!(hits.len(), 1, "single-note corpus must yield one hit");
        hits[0]["rank_score"].as_f64().expect("rank_score")
    }

    /// Omitted entity names auto-extract a 1.3x boost; explicit empty opts out.
    /// See `crates/khive-pack-memory/docs/recall-reliability.md`.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn recall_auto_extraction_from_capitalized_query_fires_entity_match() {
        const CONTENT: &str = "the committee reviewed the proposal from Zenlake last week";
        const QUERY: &str = "committee proposal Zenlake";

        let auto_score = dispatch_single_note_recall(CONTENT, QUERY, None).await;
        let opted_out_score = dispatch_single_note_recall(CONTENT, QUERY, Some(&[])).await;

        assert!(
            auto_score > opted_out_score,
            "auto-extraction (entity_names omitted) must boost the score \
             above the explicit-opt-out baseline: auto={auto_score} \
             opted_out={opted_out_score}"
        );
        let ratio = auto_score / opted_out_score;
        assert!(
            (ratio - 1.3).abs() < 0.01,
            "expected ~1.3x lift from EntityMatch firing on the \
             auto-extracted candidate, got ratio {ratio}"
        );
    }

    /// An explicit empty entity list disables automatic extraction.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn recall_explicit_empty_entity_names_disables_boost_where_auto_extraction_would_fire() {
        const CONTENT: &str = "the committee reviewed the proposal from Zenlake last week";
        const QUERY: &str = "committee proposal Zenlake";

        let opted_out_score = dispatch_single_note_recall(CONTENT, QUERY, Some(&[])).await;
        // An all-stopword query provides a baseline with no entity boost opportunity.
        let never_boosted_baseline =
            dispatch_single_note_recall("is it for me too", "is it for me", None).await;

        assert!(
            (opted_out_score - never_boosted_baseline).abs() < 1e-4,
            "explicit entity_names: [] must land on the same unboosted score \
             as a query that never had an entity candidate to begin with: \
             opted_out={opted_out_score} baseline={never_boosted_baseline}"
        );
    }

    /// A non-empty explicit entity list passes through even when the query yields none.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn recall_explicit_nonempty_entity_names_suppresses_auto_extraction() {
        const QUERY: &str = "is it for me"; // every token is an ENTITY_STOPWORDS entry
        const CONTENT: &str = "is it for me glorptastic";

        let auto_extracted_score = dispatch_single_note_recall(CONTENT, QUERY, None).await;
        let explicit_score =
            dispatch_single_note_recall(CONTENT, QUERY, Some(&["glorptastic"])).await;

        assert!(
            explicit_score > auto_extracted_score,
            "explicit entity_names must be honored (not overridden by \
             query-derived auto-extraction, which yields empty candidates \
             for this all-stopword query): auto={auto_extracted_score} \
             explicit={explicit_score}"
        );
        let ratio = explicit_score / auto_extracted_score;
        assert!(
            (ratio - 1.3).abs() < 0.01,
            "expected ~1.3x lift from the EntityMatch adjustment when the \
             explicit entity_names path is honored, got ratio {ratio}"
        );
    }

    // `#[serial(background_tasks)]`: both `timed_recall` calls below return
    // non-empty results — see the note on
    // `recall_with_dollar_sign_query_does_not_error` above.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn recall_profile_resolution_latency_is_bounded() {
        use khive_pack_brain::BrainPack;
        use std::time::Duration;

        async fn timed_recall(with_brain: bool) -> Duration {
            let rt = if with_brain {
                build_full_rt_with_brain()
            } else {
                KhiveRuntime::memory().expect("in-memory runtime")
            };
            let ns = Namespace::parse("local").expect("ns");
            let token = rt.authorize(ns.clone()).expect("token");
            rt.create_note(
                &token,
                "memory",
                None,
                "latency probe note",
                Some(0.7),
                None,
                vec![],
            )
            .await
            .expect("create note");

            let mut builder = VerbRegistryBuilder::new();
            builder.register(KgPack::new(rt.clone()));
            builder.register(MemoryPack::new(rt.clone()));
            if with_brain {
                builder.register(BrainPack::new(rt.clone()));
            }
            let registry = builder.build().expect("registry");

            if with_brain {
                registry
                    .dispatch(
                        "brain.create_profile",
                        serde_json::json!({
                            "namespace": ns.as_str(),
                            "name": "latency-recall-v1",
                            "consumer_kind": "recall",
                        }),
                    )
                    .await
                    .expect("create profile");
                registry
                    .dispatch(
                        "brain.activate",
                        serde_json::json!({
                            "namespace": ns.as_str(),
                            "profile_id": "latency-recall-v1",
                        }),
                    )
                    .await
                    .expect("activate profile");
                registry
                    .dispatch(
                        "brain.bind",
                        serde_json::json!({
                            "namespace": ns.as_str(),
                            "profile_id": "latency-recall-v1",
                            "consumer_kind": "recall",
                        }),
                    )
                    .await
                    .expect("bind profile");
            }

            let start = std::time::Instant::now();
            registry
                .dispatch(
                    "memory.recall",
                    serde_json::json!({
                        "namespace": ns.as_str(),
                        "query": "latency probe note",
                        "limit": 10
                    }),
                )
                .await
                .expect("recall");
            start.elapsed()
        }

        let without_brain = timed_recall(false).await;
        let with_brain = timed_recall(true).await;
        eprintln!(
            "[ADR-081 §5 latency] recall without brain pack: {without_brain:?}; \
             recall with brain pack (profile resolution + async ledger dispatch): {with_brain:?}"
        );
        assert!(
            with_brain < Duration::from_secs(2),
            "profile resolution must not introduce unbounded latency, got {with_brain:?}"
        );
    }

    // ── ADR-104 Stage A: serve-time profile projection ─────────────────────

    /// Exact-text vectors make ADR-104 cosine gaps analytically controllable.
    struct FixedVecService {
        map: HashMap<String, Vec<f32>>,
    }

    #[async_trait]
    impl EmbeddingService for FixedVecService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts
                .iter()
                .map(|t| self.map.get(t).cloned().unwrap_or_else(|| vec![0.0; 8]))
                .collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "fixed-vec"
        }
    }

    struct FixedVecProvider {
        model_name: String,
        map: HashMap<String, Vec<f32>>,
    }

    #[async_trait]
    impl EmbedderProvider for FixedVecProvider {
        fn name(&self) -> &str {
            &self.model_name
        }

        fn dimensions(&self) -> usize {
            8
        }

        async fn build(&self) -> Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
            Ok(Arc::new(FixedVecService {
                map: self.map.clone(),
            }))
        }
    }

    const ADR104_MODEL: &str = "adr104-fixed-vec-model";
    const ADR104_QUERY: &str = "profile ranking probe query";
    const ADR104_FILLER_LOW: &str = "filler low relevance content";
    const ADR104_FILLER_HIGH: &str = "filler high relevance content";
    const ADR104_H_CONTENT: &str = "candidate h content marker";
    const ADR104_L_CONTENT: &str = "candidate l content marker";

    /// Unit vectors anchor H/L inside a known percentile band rather than at its extremes.
    fn adr104_fixed_vectors() -> HashMap<String, Vec<f32>> {
        let mut m = HashMap::new();
        m.insert(
            ADR104_QUERY.to_string(),
            vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        );
        m.insert(
            ADR104_FILLER_LOW.to_string(),
            vec![0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        );
        m.insert(
            ADR104_FILLER_HIGH.to_string(),
            vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        );
        m.insert(
            ADR104_H_CONTENT.to_string(),
            vec![0.717, 0.6971, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        );
        m.insert(
            ADR104_L_CONTENT.to_string(),
            vec![0.5, 0.8660254, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        );
        m
    }

    /// Build a controlled four-note corpus whose profile salience projection flips H/L order.
    async fn adr104_build_ranking_corpus() -> (
        khive_runtime::KhiveRuntime,
        khive_runtime::VerbRegistry,
        Namespace,
        Uuid,
        Uuid,
    ) {
        use khive_pack_brain::BrainPack;

        let rt = build_full_rt_with_brain();
        rt.register_embedder(FixedVecProvider {
            model_name: ADR104_MODEL.to_string(),
            map: adr104_fixed_vectors(),
        });
        let ns = Namespace::parse("local").expect("local namespace");
        let token = rt.authorize(ns.clone()).expect("authorize local");

        rt.create_note(
            &token,
            "memory",
            None,
            ADR104_FILLER_LOW,
            Some(0.5),
            None,
            vec![],
        )
        .await
        .expect("filler low note");
        rt.create_note(
            &token,
            "memory",
            None,
            ADR104_FILLER_HIGH,
            Some(0.5),
            None,
            vec![],
        )
        .await
        .expect("filler high note");
        let h_id = rt
            .create_note(
                &token,
                "memory",
                None,
                ADR104_H_CONTENT,
                Some(0.1),
                None,
                vec![],
            )
            .await
            .expect("candidate h note")
            .id;
        let l_id = rt
            .create_note(
                &token,
                "memory",
                None,
                ADR104_L_CONTENT,
                Some(0.9),
                None,
                vec![],
            )
            .await
            .expect("candidate l note")
            .id;

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        builder.register(BrainPack::new(rt.clone()));
        let registry = builder.build().expect("registry");

        (rt, registry, ns, h_id, l_id)
    }

    /// Send useful feedback that updates both global salience and target posterior.
    async fn adr104_skew_salience(
        registry: &khive_runtime::VerbRegistry,
        profile_id: &str,
        target_id: Uuid,
        n: usize,
    ) {
        for _ in 0..n {
            registry
                .dispatch(
                    "brain.feedback",
                    serde_json::json!({
                        "target_id": target_id.to_string(),
                        "signal": "useful",
                        "served_by_profile_id": profile_id,
                    }),
                )
                .await
                .expect("skew salience posterior");
        }
    }

    /// Index of the hit whose `id` matches `id` in a `memory.recall` hits array.
    fn adr104_position(hits: &[Value], id: Uuid) -> usize {
        let target = id.to_string();
        hits.iter()
            .position(|h| h["id"].as_str() == Some(target.as_str()))
            .unwrap_or_else(|| panic!("id {target} not present in recall hits: {hits:?}"))
    }

    /// Different profile state must flip ordering for the same corpus and query.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn adr104_profile_differentiated_ranking_flips_order() {
        let (_rt, registry, ns, h_id, l_id) = adr104_build_ranking_corpus().await;

        registry
            .dispatch(
                "brain.create_profile",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "name": "adr104-skew-recall-v1",
                    "consumer_kind": "recall",
                }),
            )
            .await
            .expect("create profile");
        adr104_skew_salience(&registry, "adr104-skew-recall-v1", h_id, 30).await;

        let base_params = serde_json::json!({
            "namespace": ns.as_str(),
            "query": ADR104_QUERY,
            "fusion_strategy": "vector_only",
            "embedding_model": ADR104_MODEL,
            "limit": 10,
        });

        let default_result = registry
            .dispatch("memory.recall", base_params.clone())
            .await
            .expect("default recall");
        let default_hits = default_result.as_array().expect("bare array result");

        let mut skewed_params = base_params;
        skewed_params["profile_id"] = serde_json::json!("adr104-skew-recall-v1");
        let skewed_result = registry
            .dispatch("memory.recall", skewed_params)
            .await
            .expect("skewed-profile recall");
        let skewed_hits = skewed_result.as_array().expect("bare array result");

        assert!(
            adr104_position(default_hits, h_id) < adr104_position(default_hits, l_id),
            "at configured-default weights H's relevance edge must win: {default_hits:?}"
        );
        assert!(
            adr104_position(skewed_hits, l_id) < adr104_position(skewed_hits, h_id),
            "under the salience-skewed profile L must overtake H — if this still \
             matches the default ordering, the serve-time projection call is not \
             wired into scoring: {skewed_hits:?}"
        );
    }

    /// Without a resolved profile, loading the brain pack must not change scores.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn recall_no_profile_scores_identically_with_or_without_brain_pack() {
        use khive_pack_brain::BrainPack;

        async fn score_for(with_brain: bool) -> f64 {
            let rt = if with_brain {
                build_full_rt_with_brain()
            } else {
                KhiveRuntime::memory().expect("in-memory runtime")
            };
            let ns = Namespace::parse("local").expect("ns");
            let token = rt.authorize(ns.clone()).expect("token");
            rt.create_note(
                &token,
                "memory",
                None,
                "adr104 no-profile baseline note",
                Some(0.6),
                None,
                vec![],
            )
            .await
            .expect("create note");

            let mut builder = VerbRegistryBuilder::new();
            builder.register(KgPack::new(rt.clone()));
            builder.register(MemoryPack::new(rt.clone()));
            if with_brain {
                builder.register(BrainPack::new(rt.clone()));
            }
            let registry = builder.build().expect("registry");

            let result = registry
                .dispatch(
                    "memory.recall",
                    serde_json::json!({
                        "namespace": ns.as_str(),
                        "query": "adr104 no-profile baseline note",
                        "limit": 10
                    }),
                )
                .await
                .expect("recall");
            let hits = result.as_array().expect("bare array result");
            assert_eq!(hits.len(), 1);
            assert!(
                hits[0].get("served_by_profile_id").is_none(),
                "no bound profile => no stamp, whether or not brain is loaded"
            );
            hits[0]["rank_score"].as_f64().expect("rank_score")
        }

        let without_brain = score_for(false).await;
        let with_brain = score_for(true).await;
        assert!(
            (without_brain - with_brain).abs() < 1e-9,
            "ADR-104 §1: with no resolvable profile, scoring must be byte-identical \
             to the pre-change baseline regardless of whether the brain pack is \
             loaded: without_brain={without_brain} with_brain={with_brain}"
        );
    }

    /// Explicit profile override stamps results/ledger; an unknown ID is an error.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn recall_profile_id_override_stamps_ledger_and_rejects_unknown_profile() {
        use khive_pack_brain::BrainPack;

        let rt = build_full_rt_with_brain();
        let ns = Namespace::parse("local").expect("ns");
        let token = rt.authorize(ns.clone()).expect("token");

        let note = rt
            .create_note(
                &token,
                "memory",
                None,
                "adr104 profile_id override note",
                Some(0.7),
                None,
                vec![],
            )
            .await
            .expect("create note");

        let brain = BrainPack::new(rt.clone());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        builder.register(brain);
        let registry = builder.build().expect("registry");

        // No binding created — the override must serve purely from the
        // explicit `profile_id` param, bypassing binding resolution.
        registry
            .dispatch(
                "brain.create_profile",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "name": "adr104-override-v1",
                    "consumer_kind": "recall",
                }),
            )
            .await
            .expect("create profile");

        let result = registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "query": "adr104 profile_id override note",
                    "profile_id": "adr104-override-v1",
                    "limit": 10
                }),
            )
            .await
            .expect("memory.recall with profile_id override");

        let hits = result.as_array().expect("bare array result");
        assert!(!hits.is_empty());
        assert_eq!(
            hits[0]["served_by_profile_id"],
            serde_json::json!("adr104-override-v1"),
            "profile_id override must stamp the named profile with no binding required"
        );

        let target_id = note.id.to_string();
        let mut found = false;
        for _ in 0..100 {
            let mut reader = rt.sql().reader().await.expect("reader");
            let row = reader
                .query_row(khive_storage::types::SqlStatement {
                    sql: "SELECT served_by_profile_id FROM brain_serve_ledger \
                          WHERE target_id = ?1"
                        .into(),
                    params: vec![khive_storage::types::SqlValue::Text(target_id.clone())],
                    label: None,
                })
                .await
                .expect("query row");
            if let Some(row) = row {
                assert!(
                    matches!(
                        row.get("served_by_profile_id"),
                        Some(khive_storage::types::SqlValue::Text(s)) if s == "adr104-override-v1"
                    ),
                    "serve ledger row must carry the profile_id override"
                );
                found = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            found,
            "serve ledger row for the override must appear within 2s"
        );

        let bad_result = registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "query": "adr104 profile_id override note",
                    "profile_id": "adr104-does-not-exist",
                    "limit": 10
                }),
            )
            .await;
        assert!(
            bad_result.is_err(),
            "unknown profile_id must be a per-op error, not a silent fallback to defaults"
        );
    }

    /// Breakdown reports neutral/default profile state and learned entity posterior state.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn recall_breakdown_reports_profile_component_and_entity_posterior_mean() {
        {
            let rt = KhiveRuntime::memory().expect("in-memory runtime");
            let ns = Namespace::parse("local").expect("ns");
            let token = rt.authorize(ns.clone()).expect("token");
            rt.create_note(
                &token,
                "memory",
                None,
                "adr104 breakdown no-profile note",
                Some(0.5),
                None,
                vec![],
            )
            .await
            .expect("create note");

            let mut builder = VerbRegistryBuilder::new();
            builder.register(KgPack::new(rt.clone()));
            builder.register(MemoryPack::new(rt.clone()));
            let registry = builder.build().expect("registry");

            let result = registry
                .dispatch(
                    "memory.recall",
                    serde_json::json!({
                        "namespace": ns.as_str(),
                        "query": "adr104 breakdown no-profile note",
                        "include_breakdown": true,
                        "limit": 10
                    }),
                )
                .await
                .expect("recall");
            let hits = result.as_array().expect("bare array result");
            assert_eq!(hits.len(), 1);
            let breakdown = &hits[0]["breakdown"];
            assert_eq!(
                breakdown["profile_component"].as_f64(),
                Some(1.0),
                "no profile served the request => profile_component must be neutral 1.0"
            );
            assert!(
                breakdown["entity_posterior_mean"].is_null(),
                "no profile served the request => entity_posterior_mean must be absent"
            );
        }

        let (_rt, registry, ns, h_id, l_id) = adr104_build_ranking_corpus().await;

        registry
            .dispatch(
                "brain.create_profile",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "name": "adr104-breakdown-skew-v1",
                    "consumer_kind": "recall",
                }),
            )
            .await
            .expect("create profile");
        // Seeds both the global salience posterior (drives component 1) and
        // H's per-entity posterior (reported by component 3) in one loop.
        adr104_skew_salience(&registry, "adr104-breakdown-skew-v1", h_id, 30).await;

        let result = registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "query": ADR104_QUERY,
                    "fusion_strategy": "vector_only",
                    "embedding_model": ADR104_MODEL,
                    "profile_id": "adr104-breakdown-skew-v1",
                    "include_breakdown": true,
                    "limit": 10
                }),
            )
            .await
            .expect("recall");
        let hits = result.as_array().expect("bare array result");

        let h_pos = adr104_position(hits, h_id);
        let l_pos = adr104_position(hits, l_id);
        assert!(
            l_pos < h_pos,
            "the component-1 salience-projection margin (matching the \
             profile-differentiated ranking test) is wide enough that H's \
             bounded (<=+15%) component-2 entity term does not overturn it: \
             {hits:?}"
        );

        let h_component = hits[h_pos]["breakdown"]["profile_component"]
            .as_f64()
            .expect("profile_component present");
        assert!(
            (h_component - 1.0).abs() > 1e-6,
            "H's score moved under projected weights (differentiating profile) => \
             profile_component must be != 1.0, got {h_component}"
        );
        let h_ent_mean = hits[h_pos]["breakdown"]["entity_posterior_mean"]
            .as_f64()
            .expect("H has a seeded entity posterior");
        assert!(
            h_ent_mean > 0.9,
            "H received 30 'useful' signals against an uninformative Beta(1,1) \
             prior; its entity posterior mean must be high, got {h_ent_mean}"
        );

        assert!(
            hits[l_pos]["breakdown"]["entity_posterior_mean"].is_null(),
            "L never received feedback => entity_posterior_mean must be absent, \
             not a guessed prior mean"
        );
    }

    /// Identical store, query, and profile state must yield identical ranks and scores.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn recall_profile_projection_is_deterministic_across_repeated_calls() {
        let (_rt, registry, ns, h_id, _l_id) = adr104_build_ranking_corpus().await;

        registry
            .dispatch(
                "brain.create_profile",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "name": "adr104-determinism-v1",
                    "consumer_kind": "recall",
                }),
            )
            .await
            .expect("create profile");
        adr104_skew_salience(&registry, "adr104-determinism-v1", h_id, 30).await;

        let params = serde_json::json!({
            "namespace": ns.as_str(),
            "query": ADR104_QUERY,
            "fusion_strategy": "vector_only",
            "embedding_model": ADR104_MODEL,
            "profile_id": "adr104-determinism-v1",
            "limit": 10
        });

        let first = registry
            .dispatch("memory.recall", params.clone())
            .await
            .expect("first recall");
        let second = registry
            .dispatch("memory.recall", params)
            .await
            .expect("second recall");

        let first_hits = first.as_array().expect("bare array result");
        let second_hits = second.as_array().expect("bare array result");

        let first_order: Vec<&str> = first_hits
            .iter()
            .map(|h| h["id"].as_str().expect("id"))
            .collect();
        let second_order: Vec<&str> = second_hits
            .iter()
            .map(|h| h["id"].as_str().expect("id"))
            .collect();
        assert_eq!(
            first_order, second_order,
            "identical store/query/profile-state must produce identical ranking \
             across repeated calls"
        );

        let first_scores: Vec<f64> = first_hits
            .iter()
            .map(|h| h["rank_score"].as_f64().expect("rank_score"))
            .collect();
        let second_scores: Vec<f64> = second_hits
            .iter()
            .map(|h| h["rank_score"].as_f64().expect("rank_score"))
            .collect();
        assert_eq!(
            first_scores, second_scores,
            "identical store/query/profile-state must produce byte-identical \
             rank_score values"
        );
    }

    // ── ADR-104 Stage B: bounded per-entity posterior term (row-B gate) ────

    /// A missing entity posterior is an exact identity under a fresh default profile.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn adr104_stage_b_no_posterior_candidate_scores_identically_with_fresh_profile() {
        use khive_pack_brain::BrainPack;

        let rt = build_full_rt_with_brain();
        let ns = Namespace::parse("local").expect("ns");
        let token = rt.authorize(ns.clone()).expect("token");
        rt.create_note(
            &token,
            "memory",
            None,
            "adr104b neutrality probe note",
            Some(0.6),
            None,
            vec![],
        )
        .await
        .expect("create note");

        let brain = BrainPack::new(rt.clone());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        builder.register(brain);
        let registry = builder.build().expect("registry");

        registry
            .dispatch(
                "brain.create_profile",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "name": "adr104b-neutral-v1",
                    "consumer_kind": "recall",
                }),
            )
            .await
            .expect("create profile");

        let with_profile = registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "query": "adr104b neutrality probe note",
                    "profile_id": "adr104b-neutral-v1",
                    "include_breakdown": true,
                    "limit": 10
                }),
            )
            .await
            .expect("recall with fresh profile");
        let without_profile = registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "query": "adr104b neutrality probe note",
                    "limit": 10
                }),
            )
            .await
            .expect("recall with defaults");

        let with_hits = with_profile.as_array().expect("bare array result");
        let without_hits = without_profile.as_array().expect("bare array result");
        assert_eq!(with_hits.len(), 1);
        assert_eq!(without_hits.len(), 1);

        assert!(
            with_hits[0]["breakdown"]["entity_posterior_mean"].is_null(),
            "fresh profile holds no posterior for this UUID => entity_posterior_mean absent"
        );

        let score_with = with_hits[0]["rank_score"].as_f64().expect("rank_score");
        let score_without = without_hits[0]["rank_score"].as_f64().expect("rank_score");
        assert!(
            (score_with - score_without).abs() < 1e-9,
            "no-posterior candidate must score identically served vs unserved: \
             with_profile={score_with} without_profile={score_without}"
        );
    }

    /// One useful signal lifts only the targeted profile's next equivalent recall.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn adr104_stage_b_one_signal_lifts_rank_only_under_the_served_profile() {
        use khive_pack_brain::BrainPack;

        let rt = build_full_rt_with_brain();
        let ns = Namespace::parse("local").expect("ns");
        let token = rt.authorize(ns.clone()).expect("token");
        let note_id = rt
            .create_note(
                &token,
                "memory",
                None,
                "adr104b feedback lift probe note",
                Some(0.6),
                None,
                vec![],
            )
            .await
            .expect("create note")
            .id;

        let brain = BrainPack::new(rt.clone());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        builder.register(brain);
        let registry = builder.build().expect("registry");

        for name in ["adr104b-lift-a-v1", "adr104b-lift-b-v1"] {
            registry
                .dispatch(
                    "brain.create_profile",
                    serde_json::json!({
                        "namespace": ns.as_str(),
                        "name": name,
                        "consumer_kind": "recall",
                    }),
                )
                .await
                .expect("create profile");
        }

        async fn recall_score(
            registry: &khive_runtime::VerbRegistry,
            ns: &Namespace,
            profile_id: Option<&str>,
        ) -> f64 {
            let mut params = serde_json::json!({
                "namespace": ns.as_str(),
                "query": "adr104b feedback lift probe note",
                "limit": 10
            });
            if let Some(pid) = profile_id {
                params["profile_id"] = serde_json::json!(pid);
            }
            let result = registry
                .dispatch("memory.recall", params)
                .await
                .expect("recall");
            let hits = result.as_array().expect("bare array result");
            assert_eq!(hits.len(), 1);
            hits[0]["rank_score"].as_f64().expect("rank_score")
        }

        let a_before = recall_score(&registry, &ns, Some("adr104b-lift-a-v1")).await;
        let b_before = recall_score(&registry, &ns, Some("adr104b-lift-b-v1")).await;
        let default_before = recall_score(&registry, &ns, None).await;
        assert!(
            (a_before - default_before).abs() < 1e-9 && (b_before - default_before).abs() < 1e-9,
            "both fresh profiles must start identical to defaults: a={a_before} \
             b={b_before} default={default_before}"
        );

        registry
            .dispatch(
                "brain.feedback",
                serde_json::json!({
                    "target_id": note_id.to_string(),
                    "signal": "useful",
                    "served_by_profile_id": "adr104b-lift-a-v1",
                }),
            )
            .await
            .expect("one explicit useful signal under profile A");

        let a_after = recall_score(&registry, &ns, Some("adr104b-lift-a-v1")).await;
        let b_after = recall_score(&registry, &ns, Some("adr104b-lift-b-v1")).await;
        let default_after = recall_score(&registry, &ns, None).await;

        assert!(
            a_after > a_before,
            "one useful signal under profile A must lift the score under \
             profile A: before={a_before} after={a_after}"
        );
        assert!(
            (b_after - b_before).abs() < 1e-9,
            "profile B never received the signal => its score must be \
             unchanged: before={b_before} after={b_after}"
        );
        assert!(
            (default_after - default_before).abs() < 1e-9,
            "defaults (no profile) must be unchanged by feedback given under \
             an explicit profile: before={default_before} after={default_after}"
        );
    }

    /// Near-saturated entity feedback must remain inside the ±15% pipeline clamp.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn adr104_stage_b_saturated_posterior_never_exceeds_clamp_bound_end_to_end() {
        use khive_pack_brain::BrainPack;

        let rt = build_full_rt_with_brain();
        let ns = Namespace::parse("local").expect("ns");
        let token = rt.authorize(ns.clone()).expect("token");
        let note_id = rt
            .create_note(
                &token,
                "memory",
                None,
                "adr104b clamp probe note",
                Some(0.6),
                None,
                vec![],
            )
            .await
            .expect("create note")
            .id;

        let brain = BrainPack::new(rt.clone());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        builder.register(brain);
        let registry = builder.build().expect("registry");

        registry
            .dispatch(
                "brain.create_profile",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "name": "adr104b-clamp-v1",
                    "consumer_kind": "recall",
                }),
            )
            .await
            .expect("create profile");

        let baseline = registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "query": "adr104b clamp probe note",
                    "limit": 10
                }),
            )
            .await
            .expect("baseline recall");
        let baseline_score = baseline.as_array().expect("array")[0]["rank_score"]
            .as_f64()
            .expect("rank_score");

        adr104_skew_salience(&registry, "adr104b-clamp-v1", note_id, 200).await;

        let saturated = registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "query": "adr104b clamp probe note",
                    "profile_id": "adr104b-clamp-v1",
                    "include_breakdown": true,
                    "limit": 10
                }),
            )
            .await
            .expect("saturated-profile recall");
        let hits = saturated.as_array().expect("array");
        let saturated_score = hits[0]["rank_score"].as_f64().expect("rank_score");
        let ent_mean = hits[0]["breakdown"]["entity_posterior_mean"]
            .as_f64()
            .expect("entity_posterior_mean present after 200 signals");
        assert!(
            ent_mean > 0.95,
            "expected a near-saturated mean, got {ent_mean}"
        );

        // Divide out the global component to assert the entity clamp in isolation.
        let profile_component = hits[0]["breakdown"]["profile_component"]
            .as_f64()
            .expect("profile_component present");
        let implied_entity_term = (saturated_score / baseline_score) / profile_component;
        assert!(
            implied_entity_term <= crate::scoring::ENTITY_POSTERIOR_CLAMP_MAX as f64 + 1e-6,
            "entity term must never exceed the +15% clamp bound: implied={implied_entity_term}"
        );
        assert!(
            implied_entity_term >= crate::scoring::ENTITY_POSTERIOR_CLAMP_MIN as f64 - 1e-6,
            "entity term must never fall below the -15% clamp bound: implied={implied_entity_term}"
        );
    }

    /// Matched global feedback counts isolate the target-specific entity multiplier.
    /// See `crates/khive-pack-memory/docs/recall-reliability.md`.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn adr104_stage_b_entity_term_isolated_via_matched_global_feedback_count() {
        use khive_pack_brain::BrainPack;

        let rt = build_full_rt_with_brain();
        let ns = Namespace::parse("local").expect("ns");
        let token = rt.authorize(ns.clone()).expect("token");

        let target_x_id = rt
            .create_note(
                &token,
                "memory",
                None,
                "adr104b isolation target x note",
                Some(0.6),
                None,
                vec![],
            )
            .await
            .expect("create note x")
            .id;
        let target_y_id = rt
            .create_note(
                &token,
                "memory",
                None,
                "adr104b isolation target y note",
                Some(0.6),
                None,
                vec![],
            )
            .await
            .expect("create note y")
            .id;

        let brain = BrainPack::new(rt.clone());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        builder.register(brain);
        let registry = builder.build().expect("registry");

        for name in ["adr104b-iso-x-v1", "adr104b-iso-y-v1"] {
            registry
                .dispatch(
                    "brain.create_profile",
                    serde_json::json!({
                        "namespace": ns.as_str(),
                        "name": name,
                        "consumer_kind": "recall",
                    }),
                )
                .await
                .expect("create profile");
        }

        // Equal signal counts keep global state equal while entity state differs.
        adr104_skew_salience(&registry, "adr104b-iso-x-v1", target_x_id, 1).await;
        adr104_skew_salience(&registry, "adr104b-iso-y-v1", target_y_id, 1).await;

        // Shared vocabulary can return both notes, so locate the target by ID.
        async fn recall_target_x(
            registry: &khive_runtime::VerbRegistry,
            ns: &Namespace,
            profile_id: &str,
            target_x_id: Uuid,
        ) -> (f64, f64, Option<f64>) {
            let result = registry
                .dispatch(
                    "memory.recall",
                    serde_json::json!({
                        "namespace": ns.as_str(),
                        "query": "adr104b isolation target x note",
                        "profile_id": profile_id,
                        "include_breakdown": true,
                        "limit": 10
                    }),
                )
                .await
                .expect("recall");
            let hits = result.as_array().expect("bare array result");
            let pos = adr104_position(hits, target_x_id);
            let rank_score = hits[pos]["rank_score"].as_f64().expect("rank_score");
            let profile_component = hits[pos]["breakdown"]["profile_component"]
                .as_f64()
                .expect("profile_component present");
            let entity_posterior_mean = hits[pos]["breakdown"]["entity_posterior_mean"].as_f64();
            (rank_score, profile_component, entity_posterior_mean)
        }

        let (score_under_x, component_under_x, ent_mean_under_x) =
            recall_target_x(&registry, &ns, "adr104b-iso-x-v1", target_x_id).await;
        let (score_under_y, component_under_y, ent_mean_under_y) =
            recall_target_x(&registry, &ns, "adr104b-iso-y-v1", target_x_id).await;

        assert!(
            (component_under_x - component_under_y).abs() < 1e-9,
            "component 1 (profile_component) must be identical under both \
             profiles — they received the same global feedback count, just \
             on different targets: under_x={component_under_x} under_y={component_under_y}"
        );
        assert!(
            ent_mean_under_x.is_some(),
            "profile X received feedback directly on target_x => entity_posterior_mean must be present"
        );
        assert!(
            ent_mean_under_y.is_none(),
            "profile Y's signal targeted target_y, not target_x => target_x must have no \
             posterior under profile Y: got {ent_mean_under_y:?}"
        );

        let expected_term = crate::scoring::entity_posterior_term(
            ent_mean_under_x,
            crate::scoring::ENTITY_POSTERIOR_WEIGHT,
        ) as f64;
        let observed_ratio = score_under_x / score_under_y;
        assert!(
            (observed_ratio - expected_term).abs() < 1e-4,
            "with component 1 held constant, the rank_score ratio between the \
             two profiles must equal the entity term exactly: observed={observed_ratio} \
             expected={expected_term} (ent_mean_under_x={ent_mean_under_x:?})"
        );
    }

    /// The entity term must apply after weighted reranking, not only default scoring.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn adr104_stage_b_entity_term_applies_under_weighted_reranker() {
        use khive_pack_brain::BrainPack;

        let rt = build_full_rt_with_brain();
        let ns = Namespace::parse("local").expect("ns");
        let token = rt.authorize(ns.clone()).expect("token");
        let note_id = rt
            .create_note(
                &token,
                "memory",
                None,
                "adr104b reranker path probe note",
                Some(0.6),
                None,
                vec![],
            )
            .await
            .expect("create note")
            .id;

        let brain = BrainPack::new(rt.clone());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        builder.register(brain);
        let registry = builder.build().expect("registry");

        registry
            .dispatch(
                "brain.create_profile",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "name": "adr104b-rerank-v1",
                    "consumer_kind": "recall",
                }),
            )
            .await
            .expect("create profile");

        let params = serde_json::json!({
            "namespace": ns.as_str(),
            "query": "adr104b reranker path probe note",
            "profile_id": "adr104b-rerank-v1",
            "include_breakdown": true,
            "config": {
                "reranker_weights": {
                    "relevance": 0.6,
                    "salience": 0.3,
                    "temporal": 0.1
                }
            },
            "limit": 10
        });

        let before = registry
            .dispatch("memory.recall", params.clone())
            .await
            .expect("recall before feedback");
        let before_hits = before.as_array().expect("array");
        let score_before = before_hits[0]["rank_score"].as_f64().expect("rank_score");
        assert!(
            before_hits[0]["breakdown"]["entity_posterior_mean"].is_null(),
            "no feedback yet => entity_posterior_mean must be absent"
        );

        registry
            .dispatch(
                "brain.feedback",
                serde_json::json!({
                    "target_id": note_id.to_string(),
                    "signal": "useful",
                    "served_by_profile_id": "adr104b-rerank-v1",
                }),
            )
            .await
            .expect("one explicit useful signal");

        let after = registry
            .dispatch("memory.recall", params)
            .await
            .expect("recall after feedback");
        let after_hits = after.as_array().expect("array");
        let score_after = after_hits[0]["rank_score"].as_f64().expect("rank_score");
        let ent_mean_after = after_hits[0]["breakdown"]["entity_posterior_mean"]
            .as_f64()
            .expect("entity_posterior_mean present after feedback");

        assert!(
            score_after > score_before,
            "the entity term must lift rank_score on the weighted-rerank path too: \
             before={score_before} after={score_after}"
        );

        let expected_ratio = crate::scoring::entity_posterior_term(
            Some(ent_mean_after),
            crate::scoring::ENTITY_POSTERIOR_WEIGHT,
        ) as f64;
        let observed_ratio = score_after / score_before;
        assert!(
            (observed_ratio - expected_ratio).abs() < 1e-4,
            "reranker-path score ratio must equal the entity term exactly \
             (weighted_rerank's inputs are unaffected by the one signal, so \
             the entire delta must be the Stage B multiplier): \
             observed={observed_ratio} expected={expected_ratio}"
        );
    }

    // ── ADR-104 R2: measured per-recall overhead of the profile-state read ─

    /// Ignored benchmark comparing median/p95 recall with and without profile-state reads.
    #[tokio::test]
    #[ignore]
    async fn adr104_r2_measure_profile_state_read_overhead() {
        use khive_pack_brain::BrainPack;

        const ITERATIONS: usize = 150;

        async fn recall_once(registry: &khive_runtime::VerbRegistry, params: &Value) {
            registry
                .dispatch("memory.recall", params.clone())
                .await
                .expect("recall");
        }

        fn percentile(mut samples: Vec<f64>, p: f64) -> f64 {
            samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let idx = ((samples.len() - 1) as f64 * p).round() as usize;
            samples[idx]
        }

        let rt = build_full_rt_with_brain();
        let ns = Namespace::parse("local").expect("ns");
        let token = rt.authorize(ns.clone()).expect("token");
        rt.create_note(
            &token,
            "memory",
            None,
            "adr104 r2 overhead probe note",
            Some(0.6),
            None,
            vec![],
        )
        .await
        .expect("create note");

        let brain = BrainPack::new(rt.clone());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        builder.register(brain);
        let registry = builder.build().expect("registry");

        registry
            .dispatch(
                "brain.create_profile",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "name": "adr104-r2-v1",
                    "consumer_kind": "recall",
                }),
            )
            .await
            .expect("create profile");
        registry
            .dispatch(
                "brain.bind",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "profile_id": "adr104-r2-v1",
                    "consumer_kind": "recall",
                }),
            )
            .await
            .expect("bind profile");

        // A fresh unbound namespace exercises the genuine no-profile path.
        let unbound_ns = Namespace::parse("adr104-r2-unbound").expect("ns");
        let unbound_token = rt.authorize(unbound_ns.clone()).expect("token");
        rt.create_note(
            &unbound_token,
            "memory",
            None,
            "adr104 r2 overhead probe note",
            Some(0.6),
            None,
            vec![],
        )
        .await
        .expect("create note in unbound namespace");
        let params_without_profile = serde_json::json!({
            "namespace": unbound_ns.as_str(),
            "query": "adr104 r2 overhead probe note",
            "limit": 10,
        });

        let params_with_profile = serde_json::json!({
            "namespace": ns.as_str(),
            "query": "adr104 r2 overhead probe note",
            "limit": 10,
        });

        // Warm up (first-call effects: ANN index build, query embedding cache).
        recall_once(&registry, &params_without_profile).await;
        recall_once(&registry, &params_with_profile).await;

        let mut without_profile_us: Vec<f64> = Vec::with_capacity(ITERATIONS);
        let mut with_profile_us: Vec<f64> = Vec::with_capacity(ITERATIONS);
        for _ in 0..ITERATIONS {
            let start = std::time::Instant::now();
            recall_once(&registry, &params_without_profile).await;
            without_profile_us.push(start.elapsed().as_micros() as f64);

            let start = std::time::Instant::now();
            recall_once(&registry, &params_with_profile).await;
            with_profile_us.push(start.elapsed().as_micros() as f64);
        }

        let median_without = percentile(without_profile_us.clone(), 0.50);
        let p95_without = percentile(without_profile_us, 0.95);
        let median_with = percentile(with_profile_us.clone(), 0.50);
        let p95_with = percentile(with_profile_us, 0.95);

        eprintln!(
            "[ADR-104 R2] N={ITERATIONS} iterations\n\
             without profile-state read: median={median_without:.1}us p95={p95_without:.1}us\n\
             with profile-state read:    median={median_with:.1}us p95={p95_with:.1}us\n\
             delta:                      median={:.1}us p95={:.1}us",
            median_with - median_without,
            p95_with - p95_without,
        );
    }

    // ── #733 slice 1: optional `namespace` param on memory.recall ──────────

    /// Poll eventually consistent ANN results, returning the last response on exhaustion.
    async fn recall_until(
        registry: &khive_runtime::VerbRegistry,
        verb: &str,
        args: Value,
        mut ready: impl FnMut(&Value) -> bool,
    ) -> Value {
        let mut result = registry
            .dispatch(verb, args.clone())
            .await
            .unwrap_or_else(|e| panic!("{verb}: {e}"));
        // 7.5s tolerates blocking-pool contention; common cases settle in milliseconds.
        for _ in 0..300 {
            if ready(&result) {
                return result;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            result = registry
                .dispatch(verb, args.clone())
                .await
                .unwrap_or_else(|e| panic!("{verb}: {e}"));
        }
        result
    }

    /// Seed two local memories and one bench-a memory sharing a query term.
    async fn ns733_seed_three_memories() -> (khive_runtime::VerbRegistry, Uuid, Uuid, Uuid) {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        let registry = builder.build().expect("registry");

        async fn remember(
            registry: &khive_runtime::VerbRegistry,
            content: &str,
            namespace: &str,
        ) -> Uuid {
            let result = registry
                .dispatch(
                    "memory.remember",
                    serde_json::json!({
                        "content": content,
                        "memory_type": "semantic",
                        "namespace": namespace,
                    }),
                )
                .await
                .expect("memory.remember");
            result["id"]
                .as_str()
                .expect("id")
                .parse::<Uuid>()
                .expect("valid uuid")
        }

        let local_id_1 = remember(&registry, "ns733 probe term local arm one", "local").await;
        let local_id_2 = remember(&registry, "ns733 probe term local arm two", "local").await;
        let bench_id = remember(&registry, "ns733 probe term bench arm alpha", "bench-a").await;

        (registry, local_id_1, local_id_2, bench_id)
    }

    /// With no override, recall uses exactly the caller token's visible namespaces.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn ns733_recall_namespace_absent_regresses_to_local_only() {
        let (registry, local_id_1, local_id_2, _bench_id) = ns733_seed_three_memories().await;

        let result = registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "query": "ns733 probe term",
                    "limit": 10
                }),
            )
            .await
            .expect("memory.recall with no namespace param");
        let hits = result.as_array().expect("bare array result");
        let ids: HashSet<Uuid> = hits
            .iter()
            .map(|h| h["id"].as_str().expect("id").parse::<Uuid>().expect("uuid"))
            .collect();

        assert_eq!(
            ids,
            HashSet::from([local_id_1, local_id_2]),
            "no namespace param => must resolve to exactly the caller's default \
             visible namespace set (local), never bench-a: {hits:?}"
        );
    }

    /// An explicit namespace narrows recall to that exact namespace.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn ns733_recall_namespace_explicit_returns_only_that_namespace() {
        let (registry, _local_id_1, _local_id_2, bench_id) = ns733_seed_three_memories().await;

        let result = registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "query": "ns733 probe term",
                    "namespace": "bench-a",
                    "limit": 10
                }),
            )
            .await
            .expect("memory.recall with namespace=bench-a");
        let hits = result.as_array().expect("bare array result");
        let ids: HashSet<Uuid> = hits
            .iter()
            .map(|h| h["id"].as_str().expect("id").parse::<Uuid>().expect("uuid"))
            .collect();

        assert_eq!(
            ids,
            HashSet::from([bench_id]),
            "namespace=\"bench-a\" must return exactly the bench-a memory and \
             neither local memory: {hits:?}"
        );
    }

    /// An absent namespace returns an empty successful result.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn ns733_recall_namespace_no_match_returns_empty_ok() {
        let (registry, ..) = ns733_seed_three_memories().await;

        let result = registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "query": "ns733 probe term",
                    "namespace": "bench-nonexistent",
                    "limit": 10
                }),
            )
            .await
            .expect("memory.recall with a namespace matching no memories must still be Ok");
        let hits = result.as_array().expect("bare array result");
        assert!(
            hits.is_empty(),
            "namespace matching no memories must yield an empty result set, got: {hits:?}"
        );
    }

    /// An invalid namespace is a per-operation error naming the supplied value.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn ns733_recall_invalid_namespace_is_a_per_op_error() {
        let (registry, ..) = ns733_seed_three_memories().await;

        let result = registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "query": "ns733 probe term",
                    "namespace": "bad namespace",
                    "limit": 10
                }),
            )
            .await;

        let err = result.expect_err(
            "an invalid namespace string must be a per-op error, not a silent fallback",
        );
        let msg = err.to_string();
        // Checking the supplied value distinguishes this from a generic namespace error.
        assert!(
            msg.contains("bad namespace"),
            "error message must name the supplied invalid value \"bad namespace\", got: {msg}"
        );
    }

    const NS733_ANN_MODEL: &str = "ns733-ann-namespace-model";
    const NS733_QUERY: &str = "ns733 ann overfetch query";
    const NS733_TARGET_CONTENT: &str = "ns733 ann overfetch bench target";
    const NS733_FILLER_COUNT: usize = 35;

    /// Fixed vectors place the bench-a target deterministically behind all local fillers.
    fn ns733_ann_fixed_vectors() -> HashMap<String, Vec<f32>> {
        let mut m = HashMap::new();
        m.insert(
            NS733_QUERY.to_string(),
            vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        );
        for i in 0..NS733_FILLER_COUNT {
            m.insert(
                format!("ns733 ann overfetch local filler {i}"),
                vec![0.9, 0.4358899, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            );
        }
        m.insert(
            NS733_TARGET_CONTENT.to_string(),
            vec![0.5, 0.8660254, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        );
        m
    }

    /// Widening finds only the bench-a target; disabling widening leaves it unreachable.
    /// See `crates/khive-pack-memory/docs/recall-reliability.md`.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn ns733_recall_ann_overfetch_retry_loop_respects_effective_namespace() {
        // Poll only through the bounded stale-serve window; never retry corpus setup.
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        rt.register_embedder(FixedVecProvider {
            model_name: NS733_ANN_MODEL.to_string(),
            map: ns733_ann_fixed_vectors(),
        });

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        let registry = builder.build().expect("registry");

        // Omitting the model fans out to the sole registered custom provider.
        for i in 0..NS733_FILLER_COUNT {
            registry
                .dispatch(
                    "memory.remember",
                    serde_json::json!({
                        "content": format!("ns733 ann overfetch local filler {i}"),
                        "memory_type": "semantic",
                        "namespace": "local",
                    }),
                )
                .await
                .expect("remember filler");
        }
        let target_id = registry
            .dispatch(
                "memory.remember",
                serde_json::json!({
                    "content": NS733_TARGET_CONTENT,
                    "memory_type": "semantic",
                    "namespace": "bench-a",
                }),
            )
            .await
            .expect("remember target")["id"]
            .as_str()
            .expect("id")
            .parse::<Uuid>()
            .expect("valid uuid");

        let base_params = serde_json::json!({
            "query": NS733_QUERY,
            "namespace": "bench-a",
            "fusion_strategy": "vector_only",
            "embedding_model": NS733_ANN_MODEL,
            "config": { "candidate_limit": 1 },
            "limit": 1,
        });

        // Case 1: default widening — the target must be found, and only the
        // target. Poll (#791) until the background warm settles the target
        // into the ANN cache.
        let widened_result = recall_until(&registry, "memory.recall", base_params.clone(), |r| {
            r.as_array().is_some_and(|hits| {
                hits.len() == 1
                    && hits[0]["id"].as_str().and_then(|s| s.parse::<Uuid>().ok())
                        == Some(target_id)
            })
        })
        .await;
        let widened_hits = widened_result.as_array().expect("bare array result");
        assert_eq!(
            widened_hits.len(),
            1,
            "default widening must surface exactly the bench-a target, got: {widened_hits:?}"
        );
        assert_eq!(
            widened_hits[0]["id"]
                .as_str()
                .and_then(|s| s.parse::<Uuid>().ok()),
            Some(target_id),
            "the single hit must be the bench-a target, not a local filler"
        );

        // Case 2: widening disabled (`ann_overfetch_max_rounds: 1`) — round 1's
        // narrow window is exhausted entirely by `local` fillers ranked ahead
        // of the target, so the namespace-scoped post-filter finds nothing.
        let mut disabled_params = base_params;
        disabled_params["config"]["ann_overfetch_max_rounds"] = serde_json::json!(1);
        let disabled_result = registry
            .dispatch("memory.recall", disabled_params)
            .await
            .expect("memory.recall with widening disabled");
        let disabled_hits = disabled_result.as_array().expect("bare array result");
        assert!(
            disabled_hits.is_empty(),
            "with widening disabled, round 1's over-fetch window is saturated by \
             closer local fillers and must not reach the bench-a target: {disabled_hits:?}"
        );
    }

    // ── #733: verbose multi-model breakdown must not
    // leak off-namespace ANN candidate IDs ──────────────────────────────────

    const NS733B_MODEL_A: &str = "ns733b-breakdown-model-a";
    const NS733B_MODEL_B: &str = "ns733b-breakdown-model-b";
    const NS733B_QUERY: &str = "ns733b breakdown query";
    const NS733B_TARGET_CONTENT: &str = "ns733b breakdown bench target";
    const NS733B_FILLER_COUNT: usize = 5;

    /// Reuse the controlled namespace vector ordering for both registered models.
    fn ns733b_fixed_vectors() -> HashMap<String, Vec<f32>> {
        let mut m = HashMap::new();
        m.insert(
            NS733B_QUERY.to_string(),
            vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        );
        for i in 0..NS733B_FILLER_COUNT {
            m.insert(
                format!("ns733b breakdown local filler {i}"),
                vec![0.9, 0.4358899, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            );
        }
        m.insert(
            NS733B_TARGET_CONTENT.to_string(),
            vec![0.5, 0.8660254, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        );
        m
    }

    /// Seed local fillers and one bench-a target across every registered model.
    async fn ns733b_seed_two_model_corpus(
        registry: &khive_runtime::VerbRegistry,
    ) -> (HashSet<Uuid>, Uuid) {
        let mut local_filler_ids: HashSet<Uuid> = HashSet::new();
        for i in 0..NS733B_FILLER_COUNT {
            let r = registry
                .dispatch(
                    "memory.remember",
                    serde_json::json!({
                        "content": format!("ns733b breakdown local filler {i}"),
                        "memory_type": "semantic",
                        "namespace": "local",
                    }),
                )
                .await
                .expect("remember filler");
            local_filler_ids.insert(
                r["id"]
                    .as_str()
                    .expect("id")
                    .parse::<Uuid>()
                    .expect("valid uuid"),
            );
        }
        let target_id = registry
            .dispatch(
                "memory.remember",
                serde_json::json!({
                    "content": NS733B_TARGET_CONTENT,
                    "memory_type": "semantic",
                    "namespace": "bench-a",
                }),
            )
            .await
            .expect("remember target")["id"]
            .as_str()
            .expect("id")
            .parse::<Uuid>()
            .expect("valid uuid");
        (local_filler_ids, target_id)
    }

    /// Multi-model verbose breakdown must exclude every off-namespace ANN candidate.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn ns733b_recall_verbose_multi_model_breakdown_excludes_off_namespace_candidates() {
        // Poll only through the bounded stale-serve window; never retry corpus setup.
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        rt.register_embedder(FixedVecProvider {
            model_name: NS733B_MODEL_A.to_string(),
            map: ns733b_fixed_vectors(),
        });
        rt.register_embedder(FixedVecProvider {
            model_name: NS733B_MODEL_B.to_string(),
            map: ns733b_fixed_vectors(),
        });

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        let registry = builder.build().expect("registry");

        let (local_filler_ids, target_id) = ns733b_seed_two_model_corpus(&registry).await;

        let recall_args = serde_json::json!({
            "query": NS733B_QUERY,
            "namespace": "bench-a",
            "fusion_strategy": "vector_only",
            "include_breakdown": true,
            "limit": 10,
        });
        let result = recall_until(&registry, "memory.recall", recall_args, |r| {
            r["candidates"]["vector_candidates_per_model"]
                .as_array()
                .is_some_and(|per_model| {
                    per_model.iter().any(|entry| {
                        entry["hits"].as_array().is_some_and(|hits| {
                            hits.iter().any(|hit| {
                                hit["id"].as_str().and_then(|s| s.parse::<Uuid>().ok())
                                    == Some(target_id)
                            })
                        })
                    })
                })
        })
        .await;

        ns733b_assert_breakdown(&result, &local_filler_ids, target_id);
    }

    /// Assert both model breakdowns exclude local fillers and retain the bench target.
    fn ns733b_assert_breakdown(result: &Value, local_filler_ids: &HashSet<Uuid>, target_id: Uuid) {
        let per_model = result["candidates"]["vector_candidates_per_model"]
            .as_array()
            .expect("multi-model breakdown present (two models registered)");
        assert_eq!(
            per_model.len(),
            2,
            "both registered models must appear in the breakdown: {per_model:?}"
        );

        for model_entry in per_model {
            let hits = model_entry["hits"].as_array().expect("hits array");
            for hit in hits {
                let id = hit["id"]
                    .as_str()
                    .expect("id")
                    .parse::<Uuid>()
                    .expect("valid uuid");
                assert!(
                    !local_filler_ids.contains(&id),
                    "namespace=\"bench-a\" breakdown must not leak a local filler \
                     UUID ({id}) for model {:?}: {model_entry:?}",
                    model_entry["model"]
                );
            }
        }

        // Sanity: the fix must not have filtered away everything — the
        // bench-a target itself is entitled to appear (proves this is a
        // real filter, not a filter-everything regression).
        let any_model_has_target = per_model.iter().any(|entry| {
            entry["hits"].as_array().unwrap().iter().any(|hit| {
                hit["id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) == Some(target_id)
            })
        });
        assert!(
            any_model_has_target,
            "the bench-a target must still appear in at least one model's \
             breakdown after the namespace filter: {per_model:?}"
        );
    }

    // ── #733: `memory.recall_candidates` must be
    // covered by its own regression, independent of `memory.recall`'s ──────

    /// The candidates subhandler's independent model map must exclude off-namespace IDs.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn ns733b_recall_candidates_multi_model_excludes_off_namespace_candidates() {
        // Candidate diagnostics share the same bounded stale-serve window as recall.
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        rt.register_embedder(FixedVecProvider {
            model_name: NS733B_MODEL_A.to_string(),
            map: ns733b_fixed_vectors(),
        });
        rt.register_embedder(FixedVecProvider {
            model_name: NS733B_MODEL_B.to_string(),
            map: ns733b_fixed_vectors(),
        });

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        let registry = builder.build().expect("registry");

        let (local_filler_ids, target_id) = ns733b_seed_two_model_corpus(&registry).await;

        // Registry dispatch scopes the token before stripping this sub-handler's namespace.
        let recall_candidates_args = serde_json::json!({
            "query": NS733B_QUERY,
            "namespace": "bench-a",
            "limit": 10,
        });
        let result = recall_until(
            &registry,
            "memory.recall_candidates",
            recall_candidates_args,
            |r| {
                r["vector_candidates_per_model"]
                    .as_object()
                    .is_some_and(|per_model| {
                        per_model.values().any(|hits| {
                            hits.as_array().is_some_and(|hits| {
                                hits.iter().any(|hit| {
                                    hit["id"].as_str().and_then(|s| s.parse::<Uuid>().ok())
                                        == Some(target_id)
                                })
                            })
                        })
                    })
            },
        )
        .await;

        let per_model = result["vector_candidates_per_model"]
            .as_object()
            .expect("multi-model breakdown present (two models registered)");
        assert_eq!(
            per_model.len(),
            2,
            "both registered models must appear in the breakdown: {per_model:?}"
        );

        for (model_name, hits) in per_model {
            let hits = hits.as_array().expect("hits array");
            for hit in hits {
                let id = hit["id"]
                    .as_str()
                    .expect("id")
                    .parse::<Uuid>()
                    .expect("valid uuid");
                assert!(
                    !local_filler_ids.contains(&id),
                    "namespace=\"bench-a\" recall_candidates breakdown must not leak a \
                     local filler UUID ({id}) for model {model_name:?}: {hits:?}"
                );
            }
        }

        // Sanity: the fix must not have filtered away everything — the
        // bench-a target itself is entitled to appear (proves this is a
        // real filter, not a filter-everything regression).
        let any_model_has_target = per_model.values().any(|hits| {
            hits.as_array().unwrap().iter().any(|hit| {
                hit["id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) == Some(target_id)
            })
        });
        assert!(
            any_model_has_target,
            "the bench-a target must still appear in at least one model's \
             recall_candidates breakdown after the namespace filter: {per_model:?}"
        );
    }

    // ── ADR-104 §5 (Stage C): entity-anchored candidate extraction ─────────────

    /// Recalls one note after optionally seeding the entity needed for anchored lookup.
    async fn dispatch_single_note_recall_with_entity(
        entity_name: Option<&str>,
        content: &str,
        query: &str,
        entity_names: Option<&[&str]>,
    ) -> f64 {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let ns = Namespace::parse("local").expect("local namespace");
        let token = rt.authorize(ns.clone()).expect("authorize local");
        rt.create_note(&token, "memory", None, content, Some(0.5), None, vec![])
            .await
            .expect("create note");

        if let Some(name) = entity_name {
            rt.entities(&token)
                .expect("entity store")
                .upsert_entity(Entity::new(ns.as_str(), "concept", name))
                .await
                .expect("seed entity");
        }

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        let registry = builder.build().expect("registry");

        let mut params = serde_json::json!({
            "query": query,
            "fusion_strategy": "rrf",
            "limit": 10
        });
        if let Some(names) = entity_names {
            params["entity_names"] = serde_json::json!(names);
        }

        let result = registry
            .dispatch("memory.recall", params)
            .await
            .expect("memory.recall");
        let hits = result.as_array().expect("bare array result");
        assert_eq!(hits.len(), 1, "single-note corpus must yield one hit");
        hits[0]["rank_score"].as_f64().expect("rank_score")
    }

    /// Anchored lookup gives a lowercase real-entity query the EntityMatch boost.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn adr104_stage_c_lowercase_query_naming_real_entity_gets_boost() {
        const CONTENT: &str = "the committee reviewed the proposal from zenlake last week";
        const QUERY: &str = "committee proposal zenlake";

        let anchored_score =
            dispatch_single_note_recall_with_entity(Some("zenlake"), CONTENT, QUERY, None).await;
        let opted_out_score =
            dispatch_single_note_recall_with_entity(Some("zenlake"), CONTENT, QUERY, Some(&[]))
                .await;

        assert!(
            anchored_score > opted_out_score,
            "a lowercase query naming a real entity must be boosted above the \
             explicit opt-out baseline: anchored={anchored_score} opted_out={opted_out_score}"
        );
        let ratio = anchored_score / opted_out_score;
        assert!(
            (ratio - 1.3).abs() < 0.01,
            "expected ~1.3x lift from EntityMatch firing on the entity-anchored \
             candidate, got ratio {ratio}"
        );
    }

    #[tokio::test]
    #[serial(background_tasks)]
    async fn adr104_stage_c_duplicate_name_crowding_preserves_each_candidate_boost() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let ns = Namespace::parse("local").expect("local namespace");
        let token = rt.authorize(ns.clone()).expect("authorize local");
        let store = rt.entities(&token).expect("entity store");

        let mut older_b = Entity::new(ns.as_str(), "concept", "crowdbeta");
        older_b.created_at = 1;
        older_b.updated_at = 1;
        store
            .upsert_entity(older_b)
            .await
            .expect("seed candidate B");

        for created_at in 2..=258 {
            let mut newer_a = Entity::new(ns.as_str(), "concept", "CrowdAlpha");
            newer_a.created_at = created_at;
            newer_a.updated_at = created_at;
            store
                .upsert_entity(newer_a)
                .await
                .expect("seed duplicate candidate A");
        }

        let anchored = MemoryPack::new(rt)
            .entity_anchored_candidates(&token, "crowdalpha crowdbeta")
            .await
            .expect("Stage C lookup");
        assert!(anchored.contains(&"crowdalpha".to_string()));
        assert!(anchored.contains(&"crowdbeta".to_string()));

        let baseline_names = vec!["crowdalpha".to_string()];
        let now_millis = chrono::Utc::now().timestamp_millis();
        let score = |entity_names: &[String]| {
            crate::scoring::calculate_score(
                &crate::scoring::ScoreInput {
                    salience: 0.5,
                    memory_type_str: "semantic",
                    content: "the archive concerns crowdbeta",
                    created_at_millis: now_millis,
                    decay_factor: 0.005,
                    now_millis,
                    relevance_score: 0.2,
                    entity_names,
                },
                &crate::scoring::ScoringConfig::default(),
            )
        };
        let boosted = score(&anchored);
        let baseline = score(&baseline_names);
        assert!(boosted > baseline);
        assert!(
            (boosted / baseline - 1.3).abs() < 0.01,
            "candidate B must retain its EntityMatch boost after candidate A duplicate crowding: \
             boosted={boosted} baseline={baseline}"
        );
    }

    #[tokio::test]
    #[serial(background_tasks)]
    async fn adr104_stage_c_non_ascii_case_lookup_end_to_end_is_bounded() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let ns = Namespace::parse("local").expect("local namespace");
        let token = rt.authorize(ns.clone()).expect("authorize local");
        rt.entities(&token)
            .expect("entity store")
            .upsert_entity(Entity::new(ns.as_str(), "concept", "École"))
            .await
            .expect("seed entity");
        let pack = MemoryPack::new(rt);

        let same_spelling = pack
            .entity_anchored_candidates(&token, "École research archive")
            .await
            .expect("same-spelling extraction");
        let different_case = pack
            .entity_anchored_candidates(&token, "école research archive")
            .await
            .expect("differently-cased extraction");

        // Bounded contract: ASCII is case-insensitive, but cased non-ASCII
        // characters require the exact form used by the stored entity name.
        assert_eq!(same_spelling, vec!["école"]);
        assert!(different_case.is_empty());
    }

    /// Bounded substring enumeration finds entities in unsegmented CJK queries.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn adr104_stage_c_unsegmented_cjk_query_gets_entity_via_substring() {
        const ENTITY_NAME: &str = "北京大学";
        // Identical query/content isolates entity matching from retrieval relevance.
        const CONTENT: &str = "我在北京大学学习";
        const QUERY: &str = "我在北京大学学习";

        let anchored_score =
            dispatch_single_note_recall_with_entity(Some(ENTITY_NAME), CONTENT, QUERY, None).await;
        let opted_out_score =
            dispatch_single_note_recall_with_entity(Some(ENTITY_NAME), CONTENT, QUERY, Some(&[]))
                .await;

        assert!(
            anchored_score > opted_out_score,
            "an unsegmented CJK query containing a real entity name must be \
             boosted above the explicit opt-out baseline: anchored={anchored_score} \
             opted_out={opted_out_score}"
        );
        let ratio = anchored_score / opted_out_score;
        assert!(
            (ratio - 1.3).abs() < 0.01,
            "expected ~1.3x lift from EntityMatch firing on the CJK \
             substring-anchored candidate, got ratio {ratio}"
        );
    }

    #[tokio::test]
    #[serial(background_tasks)]
    async fn adr104_stage_c_late_cjk_entity_survives_candidate_cap() {
        const ENTITY_NAME: &str = "龍鳳凰";
        let mut query: String = (0..62)
            .map(|offset| char::from_u32(0x4e00 + offset).expect("valid CJK character"))
            .collect();
        query.push_str(ENTITY_NAME);
        assert_eq!(query.chars().count(), 65);

        let anchored_score =
            dispatch_single_note_recall_with_entity(Some(ENTITY_NAME), &query, &query, None).await;
        let opted_out_score =
            dispatch_single_note_recall_with_entity(Some(ENTITY_NAME), &query, &query, Some(&[]))
                .await;

        assert!(
            anchored_score > opted_out_score,
            "a CJK entity in the final 10 characters of a 65-character unsegmented query must \
             survive the candidate cap: \
             anchored={anchored_score} opted_out={opted_out_score}"
        );
    }

    #[tokio::test]
    #[serial(background_tasks)]
    async fn adr104_stage_c_first_cjk_entity_survives_candidate_cap() {
        let query: String = (0..20)
            .map(|offset| char::from_u32(0x4e00 + offset).expect("valid CJK character"))
            .collect();
        let entity_name: String = query.chars().take(2).collect();
        assert_eq!(query.chars().count(), 20);

        let anchored_score =
            dispatch_single_note_recall_with_entity(Some(&entity_name), &query, &query, None).await;
        let opted_out_score =
            dispatch_single_note_recall_with_entity(Some(&entity_name), &query, &query, Some(&[]))
                .await;

        assert!(
            anchored_score > opted_out_score,
            "a CJK entity in the first two characters of a 20-character unsegmented query must \
             survive the candidate cap: \
             anchored={anchored_score} opted_out={opted_out_score}"
        );
    }

    #[tokio::test]
    #[serial(background_tasks)]
    async fn adr104_stage_c_eight_character_cjk_entity_matches() {
        const ENTITY_NAME: &str = "甲乙丙丁戊己庚辛";
        const QUERY: &str = "甲乙丙丁戊己庚辛";

        let anchored_score =
            dispatch_single_note_recall_with_entity(Some(ENTITY_NAME), QUERY, QUERY, None).await;
        let opted_out_score =
            dispatch_single_note_recall_with_entity(Some(ENTITY_NAME), QUERY, QUERY, Some(&[]))
                .await;

        assert!(
            anchored_score > opted_out_score,
            "an eight-character CJK entity must match at the documented maximum: \
             anchored={anchored_score} opted_out={opted_out_score}"
        );
    }

    /// A token with no matching entity receives no lexical-overlap reward.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn adr104_stage_c_lowercase_token_naming_no_entity_gets_no_boost() {
        const CONTENT: &str = "the committee reviewed the proposal from zenlake last week";
        const QUERY: &str = "committee proposal zenlake";

        let auto_score = dispatch_single_note_recall_with_entity(None, CONTENT, QUERY, None).await;
        let opted_out_score =
            dispatch_single_note_recall_with_entity(None, CONTENT, QUERY, Some(&[])).await;

        assert!(
            (auto_score - opted_out_score).abs() < 1e-4,
            "no real entity named \"zenlake\" exists, so a lowercase query \
             naming it must not be boosted: auto={auto_score} opted_out={opted_out_score}"
        );
    }

    /// Explicit entity names override extraction; an empty list remains a full opt-out.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn adr104_stage_c_explicit_entity_names_still_win_over_anchored_extraction() {
        const CONTENT: &str = "the committee reviewed the proposal from zenlake last week";
        const QUERY: &str = "committee proposal zenlake";

        let anchored_score =
            dispatch_single_note_recall_with_entity(Some("zenlake"), CONTENT, QUERY, None).await;
        let opted_out_score =
            dispatch_single_note_recall_with_entity(Some("zenlake"), CONTENT, QUERY, Some(&[]))
                .await;
        let explicit_score = dispatch_single_note_recall_with_entity(
            Some("zenlake"),
            CONTENT,
            QUERY,
            Some(&["zenlake"]),
        )
        .await;

        assert!(
            (explicit_score - anchored_score).abs() < 1e-6,
            "explicit entity_names=[\"zenlake\"] must reach the same boosted \
             score as Stage C anchored extraction (both resolve to the same \
             single candidate here): explicit={explicit_score} anchored={anchored_score}"
        );
        assert!(
            (explicit_score - opted_out_score).abs() > 1e-6,
            "explicit non-empty entity_names must still be honored (boosted \
             above the opt-out baseline): explicit={explicit_score} opted_out={opted_out_score}"
        );
    }

    /// Candidate extraction covers ASCII case, non-ASCII forms, stopwords, and caps.
    #[test]
    fn entity_lookup_candidates_extracts_unigrams_and_bigrams_lowercased() {
        let out = crate::scoring::entity_lookup_candidates("New York City guide");
        assert!(out.contains(&"new".to_string()));
        assert!(out.contains(&"york".to_string()));
        assert!(out.contains(&"city".to_string()));
        assert!(out.contains(&"guide".to_string()));
        assert!(out.contains(&"new york".to_string()));
        assert!(out.contains(&"york city".to_string()));
        assert!(out.contains(&"city guide".to_string()));
    }

    #[test]
    fn entity_lookup_candidates_preserves_raw_non_ascii_case() {
        let out = crate::scoring::entity_lookup_candidates("ÉCOLE Research");
        assert!(out.contains(&"ÉCOLE".to_string()));
        assert!(out.contains(&"École".to_string()));
        assert!(!out.contains(&"école".to_string()));
    }

    #[test]
    fn entity_lookup_candidates_enumerates_bounded_cjk_substrings() {
        let out = crate::scoring::entity_lookup_candidates("我在北京大学学习");
        assert!(out.contains(&"北京大学".to_string()));
        assert!(!out.iter().any(|candidate| candidate.chars().count() == 1));
        assert!(out.iter().all(|candidate| candidate.chars().count() <= 8));
    }

    #[test]
    fn entity_lookup_candidates_samples_both_cjk_endpoints() {
        let query: String = (0..20)
            .map(|offset| char::from_u32(0x4e00 + offset).expect("valid CJK character"))
            .collect();
        let first_bigram: String = query.chars().take(2).collect();
        let final_bigram: String = query.chars().skip(18).collect();

        let out = crate::scoring::entity_lookup_candidates(&query);

        assert!(out.contains(&first_bigram));
        assert!(out.contains(&final_bigram));
    }

    #[tokio::test]
    #[serial(background_tasks)]
    async fn adr104_stage_c_long_query_preserves_adjacent_bigram_entity_for_ascii_case() {
        const ENTITY_NAME: &str = "silver comet";
        const LOWERCASE_QUERY: &str = "alpha bravo charlie delta echo foxtrot golf hotel india \
                                      juliet kilo lima mike november oscar papa silver comet";
        const TITLE_CASE_QUERY: &str = "Alpha Bravo Charlie Delta Echo Foxtrot Golf Hotel India \
                                       Juliet Kilo Lima Mike November Oscar Papa Silver Comet";

        for query in [LOWERCASE_QUERY, TITLE_CASE_QUERY] {
            let anchored_score =
                dispatch_single_note_recall_with_entity(Some(ENTITY_NAME), query, query, None)
                    .await;
            let opted_out_score =
                dispatch_single_note_recall_with_entity(Some(ENTITY_NAME), query, query, Some(&[]))
                    .await;

            assert!(
                anchored_score > opted_out_score,
                "an 18-token query must retain and match its final adjacent bigram entity \
                 regardless of ASCII case: query={query:?} anchored={anchored_score} \
                 opted_out={opted_out_score}"
            );
        }
    }

    #[test]
    fn entity_lookup_candidates_empty_query_returns_empty() {
        assert!(crate::scoring::entity_lookup_candidates("").is_empty());
        assert!(crate::scoring::entity_lookup_candidates("   ").is_empty());
    }

    // A controlled `Notify` makes deadline contention deterministic across hosts.

    struct SlowEmbedService {
        hold: Arc<Notify>,
    }

    #[async_trait]
    impl EmbeddingService for SlowEmbedService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> Result<Vec<Vec<f32>>, EmbedError> {
            self.hold.notified().await;
            Ok(texts.iter().map(|_| vec![0.0_f32; 8]).collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "slow-notify"
        }
    }

    struct SlowEmbedProvider {
        model_name: String,
        hold: Arc<Notify>,
    }

    #[async_trait]
    impl EmbedderProvider for SlowEmbedProvider {
        fn name(&self) -> &str {
            &self.model_name
        }

        fn dimensions(&self) -> usize {
            8
        }

        async fn build(&self) -> Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
            Ok(Arc::new(SlowEmbedService {
                hold: self.hold.clone(),
            }))
        }
    }

    /// Deadline overrides accept positive values, fall through on null, and reject invalid input.
    #[test]
    fn recall_889_parse_deadline_override_precedence_and_validation() {
        use crate::pack::parse_recall_deadline_override as parse_override;

        assert_eq!(
            parse_override(&serde_json::json!({ "query": "x", "limit": 5 })).unwrap(),
            None,
            "absent config.recall_deadline_ms must fall through to the process default"
        );
        assert_eq!(
            parse_override(&serde_json::json!({
                "config": { "recall_deadline_ms": Value::Null }
            }))
            .unwrap(),
            None,
            "an explicit JSON null override must fall through like an absent one"
        );
        assert_eq!(
            parse_override(&serde_json::json!({ "config": { "recall_deadline_ms": 5000 } }))
                .unwrap(),
            Some(5000),
            "a valid positive override must win over the process default"
        );

        for bad in [
            serde_json::json!({ "config": { "recall_deadline_ms": 0 } }),
            serde_json::json!({ "config": { "recall_deadline_ms": -5 } }),
            serde_json::json!({ "config": { "recall_deadline_ms": "not-a-number" } }),
            serde_json::json!({ "config": { "recall_deadline_ms": [1, 2] } }),
        ] {
            match parse_override(&bad) {
                Err(RuntimeError::InvalidInput(_)) => {}
                other => {
                    panic!("expected InvalidInput for a malformed override {bad:?}, got: {other:?}")
                }
            }
        }
    }

    /// Invalid or absent operator deadline values fall back to 30 seconds.
    #[test]
    fn recall_889_env_deadline_ms_validates_and_falls_back_to_default() {
        use crate::pack::parse_recall_deadline_env as parse_env;

        const DEFAULT_MS: u64 = 30_000;
        assert_eq!(parse_env(None), DEFAULT_MS);
        assert_eq!(parse_env(Some("5000")), 5000);

        for bad in ["0", "-5", "not-a-number", ""] {
            assert_eq!(
                parse_env(Some(bad)),
                DEFAULT_MS,
                "invalid env value {bad:?} must fall back to the default, not brick the daemon"
            );
        }
    }

    /// A zero request deadline is a per-operation `InvalidInput` error.
    #[tokio::test]
    async fn recall_889_zero_deadline_override_returns_invalid_input_via_dispatch() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let ns = Namespace::parse("local").expect("local namespace");
        let token = rt.authorize(ns).expect("authorize local");

        rt.create_note(
            &token,
            "memory",
            None,
            "issue 889 zero deadline override validation note",
            Some(0.7),
            None,
            vec![],
        )
        .await
        .expect("create note");

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        let registry = builder.build().expect("registry");

        let result = registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "query": "889 zero deadline override validation",
                    "limit": 10,
                    "config": { "recall_deadline_ms": 0 }
                }),
            )
            .await;

        match result {
            Err(RuntimeError::InvalidInput(msg)) => {
                assert!(
                    msg.contains("recall_deadline_ms"),
                    "InvalidInput message should name the offending field, got: {msg:?}"
                );
            }
            other => panic!("expected InvalidInput for a zero deadline override, got: {other:?}"),
        }
    }

    /// A genuinely held embed stage returns typed `DeadlineExceeded` promptly.
    #[tokio::test]
    async fn recall_889_deadline_exceeded_with_held_embed_stage_returns_typed_error_promptly() {
        const MODEL: &str = "recall-889-slow-model";
        let hold = Arc::new(Notify::new());

        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        rt.register_embedder(SlowEmbedProvider {
            model_name: MODEL.to_owned(),
            hold: hold.clone(),
        });

        let ns = Namespace::parse("local").expect("local namespace");
        let token = rt.authorize(ns).expect("authorize local");

        // The setup embed consumes the sole permit, so the recall embed genuinely blocks.
        hold.notify_one();
        rt.create_note(
            &token,
            "memory",
            None,
            "issue 889 held embed stage test note",
            Some(0.7),
            None,
            vec![],
        )
        .await
        .expect("create note");

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        let registry = builder.build().expect("registry");

        let start = std::time::Instant::now();
        let result = registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "query": "889 held embed stage test",
                    "limit": 10,
                    "config": { "recall_deadline_ms": 50 }
                }),
            )
            .await;
        let elapsed = start.elapsed();

        // Release the timed-out worker so it does not occupy a blocking-pool slot.
        hold.notify_one();

        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "#889 a timed-out recall must return promptly even while its embed \
             stage is genuinely held, not wait for the held stage to release; \
             took {elapsed:?}"
        );

        match result {
            Err(RuntimeError::DeadlineExceeded {
                operation,
                budget_ms,
                ..
            }) => {
                assert_eq!(operation, "memory.recall");
                assert_eq!(budget_ms, 50);
            }
            other => {
                panic!("#889 expected DeadlineExceeded with the embed stage held, got: {other:?}")
            }
        }
    }

    /// A deadline-exceeded dispatch does not affect a concurrent sibling dispatch.
    /// See `crates/khive-pack-memory/docs/recall-reliability.md`.
    #[tokio::test]
    async fn recall_889_deadline_exceeded_does_not_affect_concurrent_sibling_op() {
        const MODEL: &str = "recall-889-slow-sibling-model";
        let hold = Arc::new(Notify::new());

        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        rt.register_embedder(SlowEmbedProvider {
            model_name: MODEL.to_owned(),
            hold: hold.clone(),
        });

        let ns = Namespace::parse("local").expect("local namespace");
        let token = rt.authorize(ns).expect("authorize local");

        hold.notify_one();
        rt.create_note(
            &token,
            "memory",
            None,
            "issue 889 sibling isolation held note",
            Some(0.7),
            None,
            vec![],
        )
        .await
        .expect("create note");

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        let registry = builder.build().expect("registry");

        let slow_recall = registry.dispatch(
            "memory.recall",
            serde_json::json!({
                "query": "889 sibling isolation held note",
                "limit": 10,
                "config": { "recall_deadline_ms": 50 }
            }),
        );
        // `stats` touches no embedder and no held model, so it must
        // complete quickly regardless of the sibling recall's fate.
        let sibling_stats = registry.dispatch("stats", serde_json::json!({}));

        let (slow_result, sibling_result) = tokio::join!(slow_recall, sibling_stats);
        hold.notify_one();

        let slow_err = slow_result.expect_err("expected the held-stage recall to time out");
        match &slow_err {
            RuntimeError::DeadlineExceeded {
                operation,
                budget_ms,
                ..
            } => {
                assert_eq!(operation, "memory.recall");
                assert_eq!(*budget_ms, 50);
            }
            other => panic!("expected DeadlineExceeded, got: {other:?}"),
        }
        let slow_err_text = slow_err.to_string();
        assert!(
            slow_err_text.to_lowercase().contains("deadline"),
            "DeadlineExceeded display text must name the deadline for operator/CLI \
             visibility, got: {slow_err_text:?}"
        );

        assert!(
            sibling_result.is_ok(),
            "a concurrently-dispatched sibling op must succeed independently of a \
             sibling deadline timeout — isolation must hold at the VerbRegistry \
             dispatch boundary the MCP parallel-batch executor sits on top of; got: {:?}",
            sibling_result.err()
        );
    }

    /// The 30-second default leaves normal uncontended recall unchanged.
    #[tokio::test]
    async fn recall_889_normal_path_succeeds_within_default_deadline() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let ns = Namespace::parse("local").expect("local namespace");
        let token = rt.authorize(ns).expect("authorize local");

        rt.create_note(
            &token,
            "memory",
            None,
            "issue 889 normal path recall note",
            Some(0.7),
            None,
            vec![],
        )
        .await
        .expect("create note");

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        let registry = builder.build().expect("registry");

        let result = registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "query": "889 normal path recall",
                    "limit": 10
                }),
            )
            .await
            .expect("recall must succeed within the default deadline");

        let results = result.as_array().expect("recall result must be an array");
        assert!(
            !results.is_empty(),
            "normal recall must surface the seeded note"
        );
    }

    /// A generous request override leaves normal recall unchanged.
    #[tokio::test]
    async fn recall_889_generous_override_succeeds() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let ns = Namespace::parse("local").expect("local namespace");
        let token = rt.authorize(ns).expect("authorize local");

        rt.create_note(
            &token,
            "memory",
            None,
            "issue 889 generous override recall note",
            Some(0.7),
            None,
            vec![],
        )
        .await
        .expect("create note");

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        let registry = builder.build().expect("registry");

        let result = registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "query": "889 generous override recall",
                    "limit": 10,
                    "config": { "recall_deadline_ms": 120_000 }
                }),
            )
            .await
            .expect("recall must succeed under a generous override");

        let results = result.as_array().expect("recall result must be an array");
        assert!(
            !results.is_empty(),
            "normal recall must surface the seeded note under a generous override"
        );
    }
}
