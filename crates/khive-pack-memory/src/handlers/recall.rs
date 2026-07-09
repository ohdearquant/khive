//! Handler for `memory.recall` — the main retrieval pipeline.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::recall_feedback::{on_recall_hit, on_recall_miss};

use serde_json::{json, Value};
use uuid::Uuid;

use khive_brain_core::PackTunable;
use khive_fusion::FusionStrategy;
use khive_runtime::{micros_to_iso, NamespaceToken, RuntimeError, SearchSource, VerbRegistry};
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
        // semantics: `Some(_)` — including `Some([])` — is explicit caller
        // intent and is always honored verbatim (an empty explicit list means
        // "no entity boost", not "auto-derive one for me"). Auto-extraction
        // via `extract_entity_candidates` only runs on `None`, i.e. when the
        // caller didn't send the field at all. See `extract_entity_candidates`
        // for the extraction rule and why it's grounded in how `EntityMatch`
        // actually matches.
        let entity_names: Vec<String> = match &p.entity_names {
            Some(names) => names.iter().map(|s| s.to_lowercase()).collect(),
            None => extract_entity_candidates(query_trimmed),
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
            // request (component 1 never ran). `entity_posterior_mean` is
            // read-only: it must never feed `scoring_cfg` (that's component
            // 2, Stage B).
            let (profile_component, entity_posterior_mean) = if is_verbose {
                let component = match &profile_state {
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
                };
                let ent_mean = profile_state
                    .as_ref()
                    .and_then(|s| s.entity_posteriors.get(&id))
                    .map(khive_brain_core::BetaPosterior::mean);
                (component, ent_mean)
            } else {
                (1.0, None)
            };

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
            let final_score = if !cfg.reranker_weights.is_empty() {
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
            let per_model: Vec<Value> = candidates
                .vector_hits_per_model
                .iter()
                .map(|(model, hits)| {
                    let hits_json: Vec<Value> = hits
                        .iter()
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
    use std::collections::HashMap;
    use std::sync::Arc;

    use async_trait::async_trait;
    use khive_pack_kg::KgPack;
    use khive_runtime::{EmbedderProvider, KhiveRuntime, Namespace, VerbRegistryBuilder};
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
    /// feedback and present+correct for one with a seeded posterior — and
    /// Stage A ranking is driven by component 1 alone: the seeded entity
    /// posterior on `H` must not move it back above `L`.
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
            "Stage A ranking must still be driven by component 1 alone (matching \
             the profile-differentiated ranking test) — the entity posterior term \
             (component 2, Stage B) must not move it even though H has a seeded \
             posterior: {hits:?}"
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
}
