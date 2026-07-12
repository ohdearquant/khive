//! Handler for `memory.recall` — the main retrieval pipeline.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::recall_feedback::{on_recall_hit, on_recall_miss};

use serde_json::{json, Value};
use uuid::Uuid;

use khive_brain_core::PackTunable;
use khive_fusion::FusionStrategy;
use khive_runtime::{
    micros_to_iso, Namespace, NamespaceToken, RuntimeError, SearchSource, VerbRegistry,
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

        // #733: exact-match read-namespace escape. `VerbRegistry::dispatch`'s
        // Rule-3 explicit-namespace escape already mints `token` with
        // `visible=[namespace]` when the caller passed `namespace=` at the
        // dispatch boundary, so this is normally a no-op re-derivation of the
        // token we were already handed. It is defense-in-depth for direct
        // (non-dispatch) callers, mirroring `handle_remember`'s identical
        // pattern for the write-namespace override. Every subsequent use of
        // `token` in this function (FTS/vector candidate fetch, the ANN
        // over-fetch retry loop's visible-namespace gate, the supersedes
        // graph read, and the serve-ledger namespace stamp) reads this
        // shadowed binding, so the effective namespace flows uniformly
        // through the whole pipeline.
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

        // ADR-104 §1 (Stage A) + §4: resolve the serving profile *before*
        // scoring, either via an explicit `profile_id` override (§4,
        // short-circuits binding resolution; unknown/invalid id is a hard
        // per-op error, not a silent fallback) or the ADR-081 binding
        // resolution already used for the serve-time stamp. This is the
        // same `served_by_profile_id` stamped on the response and appended
        // to the serve ledger further down — resolved once here, reused
        // throughout, so the two paths cannot drift apart.
        //
        // A resolved-but-unreadable profile state degrades to configured
        // defaults with a WARN log (never fails the recall) — only the
        // explicit override treats a lookup failure as caller error.
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

        // Serve-time projection (ADR-104 §1): derive this request's scoring
        // weights from the served profile's posterior means via the existing
        // `PackTunable::project_config` path, used as a pure function — never
        // `apply_config`, never a mutation of `self.config`. `default_weights`
        // is kept for the breakdown's `profile_component` ratio (§3).
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

        // Prefer the per-request config param; fall back to the process-wide OnceLock
        // env so production callers without an explicit config field get the default (3).
        let ann_overfetch_max_rounds = cfg
            .ann_overfetch_max_rounds
            .unwrap_or_else(super::common::ann_overfetch_max_rounds);

        // #836: bounded wait for a cold-miss `ensure_ann_for_model` on this
        // recall's own vector leg before it degrades to FTS-only.
        let ann_ready_timeout_ms = cfg
            .ann_ready_timeout_ms
            .unwrap_or_else(super::common::ann_ready_timeout_ms);

        // #430: the FTS/vector candidate cap (`candidate_limit`) is applied over all
        // note kinds, not just `memory` rows, so a query pool dominated by higher-ranking
        // non-memory notes can starve out eligible memories before hydration ever sees
        // them. `load_memory_candidate_notes` is the first point where kind=="memory"
        // eligibility is known, so widen the cap and re-gather here (bounded by
        // `ann_overfetch_max_rounds` and `max_recall_candidates`) until enough eligible
        // memories are hydrated or the corpus is exhausted, instead of applying the
        // memory-kind scope only after the cap has already discarded candidates.
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

        // `entity_names` feeds the `EntityMatch` ×1.3 boost in `default_adjustments`
        // (scoring.rs). It used to be purely caller-supplied and no caller ever
        // populated it, leaving the boost dead code in practice. Opt-out
        // semantics: `Some(_)`, including `Some([])`, is explicit caller
        // intent and is always honored verbatim (an empty explicit list means
        // "no entity boost", not "auto-derive one for me"). Auto-extraction
        // via `extract_entity_candidates` only runs on `None`, i.e. when the
        // caller didn't send the field at all. See `extract_entity_candidates`
        // for the extraction rule and why it's grounded in how `EntityMatch`
        // actually matches.
        // ADR-104 §5 (Stage C): when the caller didn't supply `entity_names`
        // at all, extend the #738 capitalized-token heuristic with a second,
        // precision-safe source: query tokens/bigrams that match a real KG
        // entity name under the bounded Stage C case contract, resolved via
        // one batched lookup
        // (`entity_anchored_candidates`, R1). A lookup failure degrades to
        // the capitalized-token list alone (never fails the recall); an
        // explicit `entity_names` (including `Some([])`) still bypasses both
        // sources entirely. #738 opt-out semantics are unchanged.
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

        // ADR-104 §3: breakdown fields are only reported when the caller asked
        // for them — gate the extra default-weight score computation on that
        // rather than paying it on every candidate.
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

            // ADR-104 §3: `profile_component` reports the projected-weight
            // score's ratio against what the same candidate would have
            // scored under configured-default weights — computed only for
            // verbose responses, since it costs a second `calculate_score`
            // call per candidate. Neutral (1.0) when no profile served the
            // request (component 1 never ran). It is computed against the
            // pre-entity-term `rank_score` so it stays a pure read on
            // component 1 (weight projection) — component 2 (the entity
            // term, applied below) is orthogonal to the weight ratio.
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

            // ADR-104 §2 (Stage B): bounded per-entity posterior term. This
            // is a single `HashMap::get` against the profile state Stage A
            // already fetched once per recall (see the profile-resolution
            // block above) — never a second profile-state read, and never
            // gated behind `is_verbose` like `profile_component`, because
            // (unlike that diagnostic ratio) it actually feeds the score
            // below and so must run for every request, breakdown or not.
            //
            // Applied to `final_score` (below), not to the local
            // `rank_score` here — `final_score` is whichever composite score
            // actually reaches ranking (either `rank_score` on the default
            // path, or `weighted_rerank(...)`'s output when a caller sets
            // `reranker_weights`). Applying the multiplier to `rank_score`
            // alone would leave it dead code on the weighted-rerank path:
            // `weighted_rerank` recomposes its own score from raw features
            // and never reads `rank_score`, so the entity term must be the
            // *last* step, applied exactly once regardless of which path
            // produced the pre-entity-term composite.
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
                    // #836: this recall's ANN leg degraded to FTS-only after
                    // hitting the bounded readiness wait — stamped per result
                    // (same convention as `multilingual_routed`) so a plain,
                    // non-verbose response array still carries the signal.
                    result["degraded"] = json!("ann_unavailable");
                }
                result
            })
            .collect();

        // ADR-081 §5 (#394): stamp the serving profile resolved earlier
        // (ADR-104 §1, above — same value, so the score projection and the
        // response stamp can never drift apart) into each result, then fire
        // the cross-session serve-ledger append (ADR-081 §4) asynchronously
        // off the response path — the recall caller must not wait on a
        // brain-pack dispatch. An unresolved profile omits the stamp rather
        // than guessing one; the ledger row is still written with a null
        // served_by_profile_id in that case.
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

        if !results.is_empty() {
            let target_ids: Vec<String> = results
                .iter()
                .filter_map(|r| r.get("id").and_then(Value::as_str).map(str::to_string))
                .collect();
            if !target_ids.is_empty() {
                let registry_owned = registry.clone();
                let namespace = token.namespace().as_str().to_string();
                let query_raw = query_trimmed.to_string();
                let served_by = served_by_profile_id.clone();
                let served_at_us = chrono::Utc::now().timestamp_micros();
                // Tracked, not a bare tokio::spawn, so daemon shutdown's drain()
                // waits for this append instead of a SIGTERM aborting it
                // mid-flight with no ledger row and no log (internal review PR #583
                // round-1 Medium). The response path still only pays for the
                // enqueue (an atomic increment) — never the SQL write itself.
                khive_runtime::track_background_task(async move {
                    let mut ledger_params = json!({
                        "namespace": namespace,
                        "consumer_kind": "recall",
                        "target_ids": target_ids,
                        "query_raw": query_raw,
                        "served_at": served_at_us,
                    });
                    if let Some(profile_id) = served_by {
                        ledger_params["served_by_profile_id"] = json!(profile_id);
                    }
                    if let Err(e) = registry_owned
                        .dispatch("brain.record_serve", ledger_params)
                        .await
                    {
                        eprintln!(
                            "[memory] serve ledger dispatch failed (non-fatal, ADR-081 §4): {e}"
                        );
                    }
                });
            }
        }

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
            // Review finding (#733 fix-round 1, High): the ANN index is global
            // across namespaces (`ann.rs`: "one index per model covers all
            // namespaces"), so `candidates.vector_hits_per_model` still
            // carries raw, pre-hydration over-fetch candidates from outside
            // the effective namespace at this point — unlike `results` above,
            // which is scoped via `memory_ids` (populated by
            // `load_memory_candidate_notes`'s visible-namespace post-filter).
            // Filter each per-model list through the same `memory_ids` set
            // before serializing it into this diagnostic breakdown, or a
            // verbose multi-model recall with an explicit `namespace=` can
            // leak off-namespace candidate UUIDs even though `results` itself
            // stays correctly scoped.
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
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;

    use async_trait::async_trait;
    use khive_pack_kg::KgPack;
    use khive_runtime::{EmbedderProvider, KhiveRuntime, Namespace, VerbRegistryBuilder};
    use khive_storage::Entity;
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
    use serde_json::Value;
    use serial_test::serial;
    use uuid::Uuid;

    use crate::MemoryPack;

    /// #388 regression (sanitizer path): `sanitize_fts5_query` (khive-db) strips
    /// `$`, so this query no longer reaches the runtime-level fail-open `Err` arm
    /// added in PR #389 — it exercises the *sanitizer*, not the fail-open net.
    /// See `recall_with_residual_fts5_char_degrades_and_vector_leg_survives` below
    /// for a test that forces the `Err` arm itself (PR #389 internal review round 1 Medium).
    ///
    /// `#[serial(background_tasks)]`: a non-empty `memory.recall` fires the
    /// serve-ledger append via `khive_runtime::track_background_task`
    /// (see below), which drives the same process-wide counter that
    /// `ann.rs`'s `ensure_ann_background_registers_a_tracked_task_not_a_bare_spawn`
    /// asserts on — untagged, cargo's default parallelism can race them.
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

    /// #569 regression: unlike `$`, `@` is NOT stripped by `sanitize_fts5_query`
    /// (by design — the sanitizer stays minimal per #388 scope). SQLite FTS5's
    /// bareword parser still rejects `@` unconditionally, so this query reaches
    /// the `Err` arm in `collect_recall_text_hits`
    /// (khive-pack-memory/handlers/common.rs), which must now fail loud
    /// instead of degrading to vector-only results as it did before #569.
    /// This assertion fails against the pre-#569 fail-open behavior (which
    /// returned `Ok` with a non-empty result here) and passes once the FTS
    /// leg fails closed.
    // `#[serial(background_tasks)]`: kept to match the fixture setup used by
    // the sibling dollar-sign test above.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn recall_with_residual_fts5_char_fails_loud() {
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

        assert!(
            result.is_err(),
            "#569 memory.recall must fail loud when the FTS leg errors on a residual \
             FTS5 char ('@'), not silently degrade to vector-only results, got: {:?}",
            result.ok()
        );
    }

    // ── #836: bounded ANN readiness wait + FTS-only degraded fallback ─────────

    /// #836 regression: `memory.recall`'s vector leg must not block on
    /// `ensure_ann_for_model`'s per-model single-flight lock for longer than
    /// the configured `ann_ready_timeout_ms` bound. A concurrent holder of
    /// that lock (mirroring the daemon's boot-time
    /// `warm_existing_memory_indexes` mid-build) previously meant the recall
    /// waited out the full build duration (300s+ observed in production);
    /// this asserts the bounded wait fires instead and the recall serves
    /// FTS-only results, marked `"degraded": "ann_unavailable"`.
    ///
    /// Fail-on-revert proof: reverting the `tokio::time::timeout` wrap in
    /// `collect_recall_vector_hits` (handlers/common.rs) back to a bare
    /// `.await` on `ensure_ann_for_model` makes this test hang until the
    /// held lock guard is dropped (it never is, within the test), so it
    /// would fail on the `elapsed < ...` bound (or time out entirely under
    /// `cargo test`'s own test-thread deadline).
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

    /// #836: the normal, uncontended recall path must be byte-identical to
    /// pre-fix behavior — no `degraded` marker when the ANN leg warms (or
    /// serves from cache) within the bounded wait.
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

    /// #836: an ANN-degraded recall whose FTS leg also has nothing to match
    /// must resolve to an empty result, not an error — the timeout path
    /// must never surface as a hard failure even with zero candidates from
    /// either leg.
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

    /// #836 review fix: on a genuine SELF-BUILD timeout — no other holder of
    /// the per-model `model_warm_lock` (unlike
    /// `recall_836_degrades_to_fts_only_when_ann_lock_is_held` above, which
    /// simulates a concurrent holder), this recall's own
    /// `ensure_ann_for_model` call is the ONLY build in flight — the timed-out
    /// build must not be dropped. This asserts the detached background build
    /// eventually installs a fresh ANN index and a later recall takes the
    /// vector path (no `ann_unavailable` marker), instead of every recall
    /// restarting and re-timing-out on the same doomed build forever.
    ///
    /// A near-zero `ann_ready_timeout_ms` deterministically forces the bounded
    /// wait to expire on its very first poll (the freshly spawned detached
    /// task cannot have sent its result yet), without needing an artificially
    /// large corpus to slow the real build down.
    ///
    /// Fail-on-revert proof: reverting `collect_recall_vector_hits`'s detach
    /// (handlers/common.rs) back to a bare `tokio::time::timeout` wrapping
    /// `ensure_ann_for_model(...)` directly drops that future on timeout —
    /// the model never warms, so the `is_current` poll loop below exhausts
    /// its budget and `warmed` stays `false`, and the second recall keeps
    /// carrying the `ann_unavailable` marker forever.
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
             {MODEL} instead of being dropped on timeout (#836 review)"
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

    // ADR-104 §1 resolution-path regression: the profile that SERVES the
    // request (projects weights, gets stamped) must be the one `brain.bind`
    // resolves through the actor-scoped dispatch path (#699/#708) — not the
    // `profile_id` override (component 4), which is a separate, deliberate
    // short-circuit covered by its own test. A resolution regression here
    // (e.g. `resolve_serving_profile` losing actor-threading, or serve-time
    // projection reading the wrong profile's state) must fail this test
    // rather than being masked by the override path.
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

        // Skew the bound profile's salience posterior away from the default
        // prior BEFORE issuing the recall — this is what makes
        // `profile_component != 1.0` a genuine assertion about serve-time
        // projection reading the actor-resolved profile's state, not merely
        // about the stamp.
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

    // `#[serial(background_tasks)]`: non-empty recall — see the note on
    // `recall_with_dollar_sign_query_does_not_error` above.
    //
    // Systemic-fix regression: an ANONYMOUS caller must not match an explicit
    // `actor="local"` binding. `ActorRef::anonymous()` carries `id: "local"`
    // (`khive-gate/src/actor.rs`); before the `binding_id()` fix,
    // `resolve_serving_profile` threaded `token.actor().id` unconditionally,
    // so an anonymous token could match a binding a pre-actor-aware `None`
    // never could. This binds `anon-local-recall-v1` by `actor="local"` and
    // asserts an anonymous-token recall omits the serve stamp (falls through
    // exactly as before actor-threading), rather than crediting the bound profile.
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

    /// Dispatches `memory.recall` against a fresh single-note corpus and
    /// returns the sole hit's `rank_score`.
    ///
    /// `entity_names`: `None` omits the request field entirely (the JSON key
    /// is absent, so `RecallParams::entity_names` deserializes to `None` —
    /// auto-extraction runs). `Some(&[])` sends an explicit empty JSON array
    /// (`RecallParams::entity_names` deserializes to `Some(vec![])` —
    /// explicit opt-out, auto-extraction must NOT run). `Some(&[..])`
    /// non-empty sends explicit names verbatim.
    ///
    /// A single-note corpus + forced RRF fusion keeps retrieval-stage
    /// relevance identical across calls (fusion/RRF normalization does not
    /// consult `entity_names`; with one hit, `normalize_rrf_scores` collapses
    /// to the constant `baseline_relevance + range`), so any `rank_score`
    /// difference between calls is attributable to the EntityMatch scoring
    /// adjustment alone — not to a query term also moving retrieval-stage
    /// rank (the two-item-corpus percentile-normalization confound: see the
    /// design note below).
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

    /// `#[serial(background_tasks)]`: non-empty recall — see the note on
    /// `recall_with_dollar_sign_query_does_not_error` above.
    ///
    /// Proves the full wiring (`handle_recall` → `extract_entity_candidates`
    /// → `calculate_score`) by comparing `rank_score` for the *same*
    /// single-note corpus and query, varying only `entity_names`:
    /// - `entity_names` omitted (`None`) → `extract_entity_candidates`
    ///   derives `["zenlake"]` from the capitalized query token, which
    ///   matches the note's content → EntityMatch fires.
    /// - explicit empty list (`Some([])`) → opt-out, auto-extraction does
    ///   not run → EntityMatch does not fire.
    ///
    /// An earlier version of this test instead compared two *different*
    /// notes (one mentioning the entity, one not) and asserted ranking
    /// order. Review correctly flagged that as non-vacuous-looking but
    /// actually weak: the entity term is, by construction, also a query
    /// term, so it already changes retrieval-stage relevance independent of
    /// the EntityMatch adjustment — the test would still pass if
    /// auto-extraction were deleted entirely. Comparing the *same*
    /// single-hit corpus/query across two calls (only `entity_names`
    /// differing) isolates the scoring-stage effect and asserts the exact
    /// ×1.3 ratio directly, so it fails if auto-extraction stops firing.
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

    /// `#[serial(background_tasks)]`: non-empty recall — see the note on
    /// `recall_with_dollar_sign_query_does_not_error` above.
    ///
    /// [High-2 regression] `entity_names: []` is explicit caller intent
    /// ("no entity boost"), distinct from omitting the field. This uses the
    /// exact corpus/query from the sibling test above — where omitting
    /// `entity_names` auto-extracts `"zenlake"` and fires EntityMatch — and
    /// proves that sending `Some([])` disables the boost instead of being
    /// treated the same as `None` (which would silently re-enable
    /// auto-extraction and leave callers with no way to opt out).
    #[tokio::test]
    #[serial(background_tasks)]
    async fn recall_explicit_empty_entity_names_disables_boost_where_auto_extraction_would_fire() {
        const CONTENT: &str = "the committee reviewed the proposal from Zenlake last week";
        const QUERY: &str = "committee proposal Zenlake";

        let opted_out_score = dispatch_single_note_recall(CONTENT, QUERY, Some(&[])).await;
        // A query/content pair with no entity-boost opportunity at all
        // (all-stopword query → `extract_entity_candidates` yields nothing
        // even on `None`) establishes the true "no boost applied" baseline
        // score for comparison, independent of any entity-extraction path.
        let never_boosted_baseline =
            dispatch_single_note_recall("is it for me too", "is it for me", None).await;

        assert!(
            (opted_out_score - never_boosted_baseline).abs() < 1e-4,
            "explicit entity_names: [] must land on the same unboosted score \
             as a query that never had an entity candidate to begin with: \
             opted_out={opted_out_score} baseline={never_boosted_baseline}"
        );
    }

    /// `#[serial(background_tasks)]`: non-empty recall — see the note on
    /// `recall_with_dollar_sign_query_does_not_error` above.
    ///
    /// Both calls target the exact same single-note corpus and the exact same
    /// query — a query built entirely out of `ENTITY_STOPWORDS` tokens, so
    /// `extract_entity_candidates` deterministically yields an *empty* list
    /// (see `extract_entity_candidates_all_stopwords_returns_empty` in
    /// scoring.rs). The only way the note (whose content contains
    /// "glorptastic") can pick up the EntityMatch ×1.3 boost is if the
    /// handler passes the caller's explicit, non-empty, query-unrelated
    /// `entity_names` straight through instead of (re-)deriving candidates
    /// from the query.
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

    /// Deterministic embedding service returning a hand-picked fixed vector
    /// per exact input text. Unlike `HashVecService` above (deterministic
    /// but not analytically controllable), this lets a test pin the exact
    /// cosine similarity between a query and a given note's content, which
    /// the ADR-104 ranking tests need to build a small corpus with a known,
    /// controlled relevance gap between two candidates.
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

    /// 8-dim unit vectors, only the first two components carrying signal —
    /// cosine similarity between any two of these equals the dot product of
    /// those two components. Query = `(1, 0)`. `FILLER_LOW` is orthogonal
    /// (cos 0.0) and `FILLER_HIGH` is identical to the query (cos 1.0); they
    /// anchor the min/max of `normalize_rank_fusion_scores`'s percentile
    /// band so that `H` (cos 0.717) and `L` (cos 0.5) calibrate to a known,
    /// modest ~1.3x relevance ratio instead of the min/max extremes a
    /// 2-point corpus would otherwise force them to.
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

    /// Builds a runtime with kg+memory+brain registered, the deterministic
    /// fixed-vector embedder above, and a 4-note corpus: two filler notes
    /// anchoring the relevance percentile band, plus two candidates —
    /// `H` (higher relevance, cos 0.717; lower salience, 0.1) and `L`
    /// (lower relevance, cos 0.5; higher salience, 0.9). The ~1.3x
    /// calibrated relevance edge for `H` is deliberately small enough that
    /// pushing the salience posterior's projected weight high enough (via
    /// repeated `brain.feedback` "useful" signals against a profile) flips
    /// the ranking to `L` — proving the projection is load-bearing for
    /// ordering, not just magnitude. Returns `(runtime, registry, namespace,
    /// h_id, l_id)`.
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

    /// Skews `profile_id`'s salience posterior via `n` repeated explicit
    /// "useful" `brain.feedback` signals targeted at `target_id`
    /// (`served_by_profile_id` bypasses binding resolution). Each signal
    /// updates both the profile's global salience posterior (starts at
    /// `Beta(2,8)`, mean 0.2) and `target_id`'s per-entity posterior (starts
    /// at the uninformative `Beta(1,1)`, mean 0.5) — the same feedback event
    /// drives both ADR-104 component 1 (via the global posterior) and the
    /// component-3 `entity_posterior_mean` report (via the per-entity one).
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

    /// Profile-differentiated ranking (ADR-104 §1): two profiles with
    /// different posterior state must produce DIFFERENT orderings for the
    /// same store+query. `H` leads `L` at configured-default weights (its
    /// modest relevance edge dominates); pushing a profile's salience
    /// posterior far above the default weight overturns that edge and `L`
    /// leads instead. Deleting the serve-time projection call collapses
    /// both cases to the same (default) ordering, failing this test.
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

    /// No-profile path unchanged (ADR-104 §1): a request with no resolvable
    /// profile must score byte-identically whether or not the brain pack is
    /// even loaded — merely having a default profile registered must not
    /// perturb scoring absent an explicit config or a matching binding.
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

    /// `profile_id` override (ADR-104 §4): stamps the named profile with no
    /// binding required, participates in the serve ledger identically to a
    /// resolved profile, and an unknown profile_id is a hard per-op error
    /// rather than a silent fallback to defaults.
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

    /// Breakdown fields (ADR-104 §3): `profile_component` is neutral (1.0)
    /// with no profile and != 1.0 once a differentiating profile serves the
    /// request; `entity_posterior_mean` is absent for a target with no
    /// feedback and present+correct for one with a seeded posterior. With
    /// Stage B (§2) live, the entity term on `H` is also in effect here —
    /// bounded to at most +15% — but the salience-projection margin that
    /// puts `L` ahead (component 1, driven by 30 signals against the global
    /// salience posterior) is wide enough that the entity term alone does
    /// not overturn it. Component 2's isolated, order-flipping effect is
    /// covered by the feedback-lift and neutrality tests below (ADR-104
    /// Stage B gate), which hold the corpus fixed and vary only the entity
    /// posterior.
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

    /// Determinism (ADR-104 §1): identical store/query/profile-state must
    /// produce identical ranking and identical `rank_score` values across
    /// repeated calls — `project_config` is used as a pure function, never
    /// mutating shared state.
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

    /// Neutrality (row-B gate): a candidate with no per-entity posterior must
    /// score identically whether or not a profile serves the request, as
    /// long as that profile's *global* weights also equal defaults (a fresh
    /// profile with untouched Beta priors projects to exactly
    /// `RecallConfig::default()` — see `tunable.rs`'s
    /// `project_config_with_default_priors_matches_expected_defaults`). This
    /// isolates component 2 (the entity term) from component 1 (weight
    /// projection): with no posterior for this UUID, component 2 must be
    /// the identity multiplier, so profile-served and default-served scores
    /// coincide exactly.
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

    /// Feedback-lift (row-B gate, the ADR's headline test): one explicit
    /// `useful` signal on a recalled memory changes that memory's rank_score
    /// on the next equivalent query — under the profile the signal targeted
    /// — and does NOT change it under a different profile or under defaults.
    /// Both non-targeted arms start numerically identical to the pre-signal
    /// baseline (a fresh profile projects to default weights, per the
    /// neutrality test above), so any post-signal divergence is attributable
    /// to the signal.
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

    /// Clamp (row-B gate) at the pipeline level: driving a profile's
    /// per-entity posterior to its practical ceiling (repeated `useful`
    /// signals push the Beta posterior mean arbitrarily close to 1.0, never
    /// reaching it exactly) must never lift `rank_score` by more than the
    /// documented +15% bound relative to the same candidate's score under a
    /// fresh, untouched profile. The exact-boundary case
    /// (`entity_posterior_term`'s clamp at mean=0.0/1.0 precisely) is
    /// covered at the unit level in `scoring.rs`; this test proves the bound
    /// holds end-to-end through the handler, not just in the pure function.
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

        // 200 "useful" signals also drive the global salience posterior
        // (component 1) far from its prior, so `saturated_score` reflects
        // both components, not component 2 alone. Bound the *entity term's*
        // contribution directly instead of the composite ratio: divide out
        // the component-1 (profile_component) ratio the response already
        // reports, leaving only component 2's multiplier for the assertion.
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

    /// Isolation (row-B gate, internal review PR round-1 Medium): the earlier
    /// feedback-lift and clamp tests above give one profile strictly more
    /// feedback than another, which also perturbs that profile's *global*
    /// salience posterior (component 1, ADR-104 §1) — `on_explicit_feedback`
    /// updates `state.salience` on every signal regardless of `target_id`
    /// (see `recall_feedback.rs`). So a passing feedback-lift test alone does
    /// not prove component 2 (the entity term) did the lifting; it could be
    /// entirely a Stage A weight-projection effect.
    ///
    /// This test controls for that: two profiles each receive exactly ONE
    /// `useful` signal — identical global salience posterior state — but
    /// aimed at *different* targets (`target_x` for profile X, `target_y`
    /// for profile Y). Recalling `target_x`'s note under both profiles must
    /// therefore show identical `profile_component` (component 1 is
    /// target-independent), while `entity_posterior_mean` for `target_x` is
    /// present only under profile X. The measured `rank_score` ratio between
    /// the two must equal `entity_posterior_term(mean, ENTITY_POSTERIOR_WEIGHT)`
    /// exactly — the only degree of freedom left once component 1 is held
    /// constant — proving the multiplier is live end-to-end, not just
    /// present in the pure function.
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

        // Exactly one signal per profile — same weight, same count — but
        // profile X's signal targets target_x and profile Y's targets
        // target_y. Global salience posterior state ends up identical;
        // per-entity posterior state does not.
        adr104_skew_salience(&registry, "adr104b-iso-x-v1", target_x_id, 1).await;
        adr104_skew_salience(&registry, "adr104b-iso-y-v1", target_y_id, 1).await;

        // Both notes share most of their vocabulary ("adr104b isolation ...
        // note"), so an FTS query naming only `target_x_id`'s note can still
        // surface `target_y_id`'s note as a secondary hit — locate
        // `target_x_id` by id within the returned hits rather than assuming
        // a single-hit result.
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

    /// Regression (row-B gate, internal review PR round-1 High): the entity
    /// term must apply on the weighted-rerank path too, not only the
    /// default `rank_score` path. `weighted_rerank` recomposes its score
    /// from raw relevance/salience/temporal features and never reads
    /// `rank_score`, so a naive "multiply `rank_score`" fix is dead code
    /// whenever a caller sets non-empty `reranker_weights`. This drives a
    /// single-note corpus through the reranker path before and after one
    /// `useful` signal and asserts the score moves by exactly the entity
    /// term, proving the multiplier is applied to whichever composite score
    /// actually reaches ranking.
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

    /// Median + p95 wall-clock overhead of the ADR-104 §1 profile-state read
    /// (`brain.profile` dispatch + snapshot deserialize + `project_config`)
    /// against an otherwise identical recall with no resolvable profile.
    /// `#[ignore]`d — a timing measurement, not a correctness gate; run
    /// explicitly (`cargo test -p khive-pack-memory -- --ignored
    /// adr104_r2`) and the printed numbers are recorded verbatim in
    /// IMPL_REPORT.md per the Stage A binding sign-off rider (R2).
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

        // The "without profile" arm exercises the exact same handler path
        // with binding resolution finding nothing: a fresh namespace with no
        // binding, rather than an invalid profile_id (which would error,
        // not degrade).
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

    /// #791: `memory.recall`'s ANN cache-hit path now serves a
    /// present-but-stale entry immediately rather than blocking the request
    /// on a synchronous full-corpus rebuild (`ann::search_loaded`'s call
    /// site in `handlers/common.rs` no longer gates on `ann::is_current`).
    /// A `memory.remember` immediately followed by a `memory.recall` for the
    /// same model is therefore only eventually, not immediately, consistent:
    /// the background warm the write fires (`ann::ensure_ann_background`)
    /// has to install before the just-written note is reflected. Tests that
    /// seed notes and then assert on recall results poll a bounded number of
    /// times instead of asserting on a single dispatch, matching the
    /// documented indexing-latency contract ("a stale ANN serving window is
    /// acceptable"). `ready` decides when the response is settled enough to
    /// assert against; on exhaustion this returns the last response so the
    /// caller's own assertions produce the real diagnostic.
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
        // 300 * 25ms = ~7.5s ceiling — generous relative to the small
        // in-memory corpora these tests seed, to stay reliable under
        // parallel test-suite CPU contention (background warms share the
        // process-wide blocking pool with every other concurrently running
        // test), while still resolving in low milliseconds in the common case.
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

    /// Seeds a fresh in-memory runtime with `kg` + `memory` registered, three
    /// memories — two in `local`, one in `bench-a` — all sharing a query term
    /// so a namespace-agnostic FTS/RRF recall would surface all three absent
    /// any namespace filtering. Returns `(registry, local_id_1, local_id_2,
    /// bench_id)`.
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

    /// Regression (spec item 1): `namespace` absent must be byte-identical to
    /// pre-#733 behavior — recall reads the caller token's default visible
    /// namespace set (`local` here), so a no-arg recall over a corpus with
    /// two `local` memories and one `bench-a` memory surfaces only the two
    /// `local` hits.
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

    /// Spec item 2: `namespace="bench-a"` returns only the bench-a memory,
    /// not either `local` memory — the exact-match escape narrows the read
    /// scope instead of widening it.
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

    /// Spec item 3: a `namespace` that matches nothing in the corpus returns
    /// an empty result set with `ok:true` (dispatch succeeds), not an error.
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

    /// Spec item 4: an invalid `namespace` string is a per-op error naming
    /// the problem, never silent coercion to a fallback namespace. Validated
    /// via the same `Namespace::parse` machinery used elsewhere (a space is
    /// rejected — `NamespaceError::InvalidCharacter`).
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
        // Review finding (#733 fix-round 1, Medium): asserting only that the
        // message contains the word "namespace" passes vacuously (every
        // variant of this error, valid or not, contains that word). Assert
        // the *supplied* invalid value itself appears, proving the error
        // actually names the problem rather than a generic namespace
        // complaint (`resolve_explicit_namespace` in
        // `khive-runtime/src/pack.rs`, the path this dispatched call goes
        // through, now includes `{ns_str:?}` in its message).
        assert!(
            msg.contains("bad namespace"),
            "error message must name the supplied invalid value \"bad namespace\", got: {msg}"
        );
    }

    const NS733_ANN_MODEL: &str = "ns733-ann-namespace-model";
    const NS733_QUERY: &str = "ns733 ann overfetch query";
    const NS733_TARGET_CONTENT: &str = "ns733 ann overfetch bench target";
    const NS733_FILLER_COUNT: usize = 35;

    /// 8-dim vectors, first two components carry signal (ADR-104 test pattern
    /// reused here). Query = (1, 0) — cos 1.0 against itself. All 35 `local`
    /// filler notes share an identical vector at cos 0.9 against the query;
    /// the single `bench-a` target sits at cos 0.5, strictly below every
    /// filler. Cosine-ranked purely by similarity, the target is therefore
    /// guaranteed last (rank 36 of 36) — not a probabilistic near-miss.
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

    /// Spec item 5: the ANN over-fetch retry loop (`config.rs`'s
    /// "visible-namespace candidates" widening, `ann_overfetch_max_rounds`)
    /// must respect the effective (explicit-namespace-narrowed) visible set,
    /// not just eventually surface *a* result.
    ///
    /// Setup: a single global per-model ANN index (confirmed at the source —
    /// `AnnKey` carries no namespace field, `ann.rs`: "One index per model
    /// covers all namespaces") holds 35 `local` filler vectors all closer to
    /// the query than the one `bench-a` target vector, with `candidate_limit`
    /// pinned to 1 so the initial over-fetch window (`max(limit*4,
    /// limit+32)` = 33) is narrower than the filler count — round 1 excludes
    /// the target outright. This proves two things in one test:
    ///
    /// 1. With default widening (`ann_overfetch_max_rounds` unset, env
    ///    fallback 3): the retry loop widens past round 1, the target enters
    ///    the fetch window, and `namespace="bench-a"`'s post-filter still
    ///    returns *only* the target — none of the 35 `local` fillers ever
    ///    leak into the response despite sharing the same global ANN index.
    /// 2. With widening explicitly disabled (`ann_overfetch_max_rounds: 1`,
    ///    per `config.rs`'s "Pass `Some(1)` to disable widening entirely"):
    ///    the same query against the same corpus returns nothing — proving
    ///    the round-1 result in case 1 was not a coincidence of corpus size,
    ///    but genuinely produced by the widening loop.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn ns733_recall_ann_overfetch_retry_loop_respects_effective_namespace() {
        // #750: this test used to retry the whole seed-and-recall flow
        // against a fresh `KhiveRuntime` up to 5 times, and separately poll
        // `memory.recall` for up to 500ms per attempt, to paper over a
        // pre-existing ANN warm-cache race: `ensure_ann_for_model` installed
        // whichever queued build acquired the per-model lock first via
        // `entry(key).or_insert(bridge)`, permanently, even when it had
        // snapshotted the corpus before a still-in-flight `remember`
        // committed. The write-generation-checked install
        // (`install_if_fresher` in ann.rs) fixed that permanent-win bug.
        //
        // #791: `handlers/common.rs`'s recall path now serves a
        // stale-but-present cache entry immediately instead of treating it
        // as a miss, to stop a recall from paying for a synchronous
        // full-corpus rebuild on its own request path. That reintroduces a
        // *bounded, eventually-resolving* staleness window here: the very
        // next recall right after `remember`ing the target can observe a
        // cache that predates it. `recall_until` polls for the settled
        // (post-background-warm) response instead of asserting on a single
        // dispatch.
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        rt.register_embedder(FixedVecProvider {
            model_name: NS733_ANN_MODEL.to_string(),
            map: ns733_ann_fixed_vectors(),
        });

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        let registry = builder.build().expect("registry");

        // No `embedding_model` on remember: `create_note_inner`'s auto-detect
        // path fans out to every *registered* model when the field is
        // omitted (`resolve_embedding_model` only accepts lattice aliases —
        // an explicit custom provider name here would hit `UnknownModel`,
        // same gotcha documented on `recall_with_residual_fts5_char_fails_loud`
        // above). `NS733_ANN_MODEL` is the only model registered on this
        // runtime, so auto-detect resolves to exactly it.
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

    // ── #733 fix-round 1 (codex High): verbose multi-model breakdown must not
    // leak off-namespace ANN candidate IDs ──────────────────────────────────

    const NS733B_MODEL_A: &str = "ns733b-breakdown-model-a";
    const NS733B_MODEL_B: &str = "ns733b-breakdown-model-b";
    const NS733B_QUERY: &str = "ns733b breakdown query";
    const NS733B_TARGET_CONTENT: &str = "ns733b breakdown bench target";
    const NS733B_FILLER_COUNT: usize = 5;

    /// Same fixed-vector scheme as `ns733_ann_fixed_vectors` (query cos 1.0,
    /// `local` fillers cos 0.9, `bench-a` target cos 0.5) — reused for both
    /// registered models so the ANN over-fetch genuinely returns filler IDs
    /// under each model, not just the target.
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

    /// Seeds `registry`'s runtime with `NS733B_FILLER_COUNT` `local` filler
    /// memories plus one `bench-a` target memory, all sharing
    /// `ns733b_fixed_vectors`'s vector scheme (query cos 1.0, fillers cos
    /// 0.9, target cos 0.5). No `embedding_model` on remember: auto-detect
    /// fans out to every registered model, matching
    /// `ns733_seed_three_memories`'s documented gotcha above. Shared between
    /// the `memory.recall` verbose-breakdown regression (#733 fix-round 1,
    /// High) and the `memory.recall_candidates` regression (#733 fix-round
    /// 2, Medium) that protects `handle_recall_candidates`'s independent
    /// per-model serialization site (`sub_handlers.rs`). Returns
    /// `(local_filler_ids, target_id)`.
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

    /// Codex review finding (#733 fix-round 1, High): with `namespace="bench-a"`,
    /// more than one registered embedding model, and `include_breakdown=true`,
    /// `memory.recall`'s verbose response embeds
    /// `candidates.vector_candidates_per_model` — built directly from the
    /// (pre-hydration, namespace-agnostic) global ANN over-fetch results,
    /// bypassing the `memory_ids` visible-namespace filter that scopes
    /// `results` itself. This seeds two registered models, five `local`
    /// filler memories ranked closer to the query than the one `bench-a`
    /// target (guaranteeing the raw per-model over-fetch actually contains
    /// filler IDs under both models — a non-vacuous corpus), and asserts no
    /// `local` filler UUID appears anywhere in the breakdown for either
    /// model, while the target UUID is present.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn ns733b_recall_verbose_multi_model_breakdown_excludes_off_namespace_candidates() {
        // #750: this test used to retry the whole corpus-seed-and-recall flow
        // against a fresh `KhiveRuntime` up to 5 times, and separately poll
        // `memory.recall` for up to 500ms per attempt, to paper over the
        // same pre-existing ANN warm-cache race documented in detail on
        // `ns733_recall_ann_overfetch_retry_loop_respects_effective_namespace`
        // above — `ensure_ann_for_model`'s old `entry(key).or_insert(bridge)`
        // let whichever queued build acquired the per-model lock first win
        // permanently, even one that had snapshotted the corpus before a
        // still-in-flight sibling `remember` committed. The
        // write-generation-checked install (`install_if_fresher`, ann.rs)
        // fixed that permanent-win bug.
        //
        // #791: `handlers/common.rs`'s recall path now serves a
        // stale-but-present cache entry immediately (see the rationale on
        // `ns733_recall_ann_overfetch_retry_loop_respects_effective_namespace`
        // above), so `recall_until` polls for the settled response instead
        // of asserting on a single dispatch.
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

    /// Assertion body for
    /// `ns733b_recall_verbose_multi_model_breakdown_excludes_off_namespace_candidates`,
    /// split out so the retry loop above can call it once a settled response
    /// is in hand without duplicating the assertions per attempt.
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

    // ── #733 fix-round 2 (codex Medium): `memory.recall_candidates` must be
    // covered by its own regression, independent of `memory.recall`'s ──────

    /// Codex re-review finding (#733 fix-round 2, Medium): the fix-round-1
    /// regression above dispatches only `memory.recall` with
    /// `include_breakdown=true`, which is mutation-sensitive for
    /// `handle_recall`'s filter (`recall.rs`) but *not* for
    /// `handle_recall_candidates`'s independent filter (`sub_handlers.rs:182`,
    /// reached via the separate `memory.recall_candidates` verb —
    /// `pack.rs`'s dispatch table routes the two verbs to two different
    /// handler functions with two separate `vector_hits_per_model`
    /// serialization sites). Removing the `.filter(...)` at
    /// `sub_handlers.rs:182` would not fail any existing test. This
    /// dispatches `memory.recall_candidates` directly against the identical
    /// two-model/`local`-fillers/`bench-a`-target corpus and asserts the
    /// same no-leak + target-present properties against
    /// `handle_recall_candidates`'s response shape, which differs from
    /// `handle_recall`'s: `vector_candidates_per_model` here is a JSON
    /// *object* keyed by model name (`{"model-a": [...], "model-b": [...]}`,
    /// `sub_handlers.rs:194`), not an array of `{model, hits}` entries.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn ns733b_recall_candidates_multi_model_excludes_off_namespace_candidates() {
        // #750: same pre-existing ANN warm-cache race documented in detail on
        // `ns733b_recall_verbose_multi_model_breakdown_excludes_off_namespace_candidates`
        // above applied identically here — `handle_recall_candidates` reads
        // the same shared per-model ANN index via the same
        // `collect_recall_candidates` path `handle_recall` uses. Fixed the
        // same way (write-generation-checked install).
        //
        // #791: same stale-serve behavior change as the sibling test above
        // — polls for the settled response via `recall_until` rather than
        // asserting on a single dispatch.
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

        // `memory.recall_candidates`'s `HandlerDef` declares no
        // `namespace` `ParamDef` (`pack.rs`: `params: &[]`), so
        // `VerbRegistry::dispatch`'s generic Rule-3 explicit-namespace
        // escape (ADR-007 Rev 4/6) is the *only* thing scoping this call
        // — it mints the token with `visible=["bench-a"]` before
        // `handle_recall_candidates` ever runs, then strips the
        // `namespace` key from params (the handler never sees it). No
        // `fusion_strategy`/`include_breakdown` params exist on this
        // sub-verb — it always returns the per-model breakdown whenever
        // more than one model is registered (`sub_handlers.rs:168`).
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

    /// Dispatches `memory.recall` against a fresh single-note corpus, exactly
    /// like `dispatch_single_note_recall` above, but first seeds a real KG
    /// entity (`entity_name`, when `Some`) directly into the entity store.
    /// the record `entity_anchored_candidates` must find via its batched
    /// lookup for the boost to fire on a lowercase or CJK query. Returns the
    /// sole hit's `rank_score`.
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

    /// Gate (a): a lowercase query naming a real KG entity gets the
    /// candidate and the EntityMatch ×1.3 boost. `extract_entity_candidates`
    /// (#738) extracts nothing here (no capitalized token in either the
    /// query or the entity name). The lift can only come from the Stage C
    /// batched entity-anchored lookup finding the seeded "zenlake" entity.
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

    /// Gate (b): an unsegmented CJK query containing a real entity name as a
    /// substring gets it through bounded substring enumeration and the same
    /// indexed exact-name lookup used by alphabetic-script candidates.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn adr104_stage_c_unsegmented_cjk_query_gets_entity_via_substring() {
        const ENTITY_NAME: &str = "北京大学";
        // `content` and `query` are byte-identical (guarantees the FTS leg
        // retrieves the note; retrieval-stage relevance is not what this
        // test is about). The entity has Han characters on both sides, proving
        // CJK entity matching does not require whitespace or punctuation.
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

    /// Gate (c): a token that does not name any real entity gets nothing,
    /// no lexical-overlap reward. Same lowercase shape as gate (a), but no
    /// entity named "zenlake" (or anything else) exists in the store, so the
    /// batched lookup returns no candidates and the auto path must score
    /// identically to the explicit opt-out.
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

    /// Gate (e): explicit `entity_names` still wins over Stage C extraction
    /// even when a real, matching entity exists, and `entity_names: []`
    /// stays a full opt-out. Mirrors the #738 override tests above, but with
    /// a seeded entity in the store to prove Stage C's batched lookup is
    /// bypassed entirely (not merely outscored) when the caller is explicit.
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

    /// `entity_lookup_candidates` unit coverage (pure function, ADR-104 §5):
    /// raw and ASCII-lowercased unigrams and adjacent bigrams, stopwords
    /// excluded from unigrams, capped at `MAX_ENTITY_LOOKUP_CANDIDATES`.
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

    #[tokio::test]
    #[serial(background_tasks)]
    async fn adr104_stage_c_long_query_preserves_adjacent_bigram_entity() {
        const ENTITY_NAME: &str = "silver comet";
        const QUERY: &str = "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo \
                             lima mike november oscar silver comet";

        let anchored_score =
            dispatch_single_note_recall_with_entity(Some(ENTITY_NAME), QUERY, QUERY, None).await;
        let opted_out_score =
            dispatch_single_note_recall_with_entity(Some(ENTITY_NAME), QUERY, QUERY, Some(&[]))
                .await;

        assert!(
            anchored_score > opted_out_score,
            "a 17-token query must retain its final adjacent bigram entity: \
             anchored={anchored_score} opted_out={opted_out_score}"
        );
    }

    #[test]
    fn entity_lookup_candidates_empty_query_returns_empty() {
        assert!(crate::scoring::entity_lookup_candidates("").is_empty());
        assert!(crate::scoring::entity_lookup_candidates("   ").is_empty());
    }
}
