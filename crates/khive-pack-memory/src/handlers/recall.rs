//! Handler for `memory.recall` — the main retrieval pipeline.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::recall_feedback::{on_recall_hit, on_recall_miss};

use serde_json::{json, Value};
use uuid::Uuid;

use khive_fusion::FusionStrategy;
use khive_runtime::{micros_to_iso, NamespaceToken, RuntimeError, SearchSource, VerbRegistry};
use khive_storage::types::{EdgeFilter, PageRequest};
use khive_storage::EdgeRelation;

use crate::config::ScoreBreakdown;
use crate::rerank::{weighted_rerank, RerankFeatures};
use crate::scoring::{
    calculate_score, contains_cjk, needs_multilingual, normalize_min_score,
    normalize_rank_fusion_scores, normalize_rrf_scores, ScoreInput,
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

        let entity_names: Vec<String> = p
            .entity_names
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|s| s.to_lowercase())
            .collect();

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

            let rank_score = calculate_score(
                &ScoreInput {
                    salience: salience as f32,
                    memory_type_str: &note_memory_type,
                    content: &note.content,
                    created_at_millis: note.created_at / 1_000,
                    decay_factor: decay_factor as f32,
                    now_millis,
                    relevance_score: norm_relevance,
                    entity_names: &entity_names,
                },
                &scoring_cfg,
            );

            let age_days_f64 =
                ((now_micros - note.created_at).max(0) as f64) / (1_000_000.0 * 86_400.0);
            let (_, breakdown) = compute_score(
                &cfg,
                &recall_pipeline,
                norm_relevance as f64,
                salience,
                decay_factor,
                age_days_f64,
            );

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

        let is_verbose = cfg.include_breakdown || p.include_breakdown.unwrap_or(false);
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

        // ADR-081 §5 (#394): resolve the serving profile via the same tiers
        // 1-2 memory.feedback uses (resolve_serving_profile), stamp it into
        // each result, then fire the cross-session serve-ledger append
        // (ADR-081 §4) asynchronously off the response path — the recall
        // caller must not wait on a brain-pack dispatch. An unresolved
        // profile omits the stamp rather than guessing one; the ledger row
        // is still written with a null served_by_profile_id in that case.
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
        let served_by_profile_id =
            super::common::resolve_serving_profile(&self.brain_profile, token, registry).await;
        if prof {
            if let Some(ref t) = t_stage {
                plog(call_id, "profile_resolve", t.elapsed().as_micros());
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
                tokio::spawn(async move {
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
    use std::sync::Arc;

    use async_trait::async_trait;
    use khive_pack_kg::KgPack;
    use khive_runtime::{EmbedderProvider, KhiveRuntime, Namespace, VerbRegistryBuilder};
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};

    use crate::MemoryPack;

    /// #388 regression (sanitizer path): `sanitize_fts5_query` (khive-db) strips
    /// `$`, so this query no longer reaches the runtime-level fail-open `Err` arm
    /// added in PR #389 — it exercises the *sanitizer*, not the fail-open net.
    /// See `recall_with_residual_fts5_char_degrades_and_vector_leg_survives` below
    /// for a test that forces the `Err` arm itself (PR #389 codex round-1 Medium).
    #[tokio::test]
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

    /// PR #389 codex round-1 Medium regression: unlike `$`, `@` is NOT stripped
    /// by `sanitize_fts5_query` (by design — the sanitizer stays minimal per
    /// #388 scope; the fail-open net is the systematic answer for residual
    /// punctuation). SQLite FTS5's bareword parser still rejects `@`
    /// unconditionally, so this query reaches the `Err` arm added to
    /// `collect_recall_text_hits` (khive-pack-memory/handlers/common.rs) and must
    /// degrade to vector-only results rather than aborting the recall.
    ///
    /// Ties Medium to the codex round-1 High-2 finding: with a real (non-null)
    /// embedder registered, this proves the vector leg still returns the
    /// correct note while the FTS leg is degraded — i.e. degradation loses only
    /// the FTS signal, not the overall recall. (This test does NOT assert an
    /// `fts_degraded`/`partial` advisory field: `memory.recall`'s common-path
    /// wire shape is a bare JSON array today, so adding such a field here would
    /// be a breaking array-to-object shape change — reported separately as
    /// blocked-on-shape, not fixed in this PR.)
    #[tokio::test]
    async fn recall_with_residual_fts5_char_degrades_and_vector_leg_survives() {
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
        // (which has no semantic notion of similarity) produces an identical
        // vector for query and note, guaranteeing a vector-leg hit.
        let result = registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "query": NOTE_TEXT,
                    "limit": 10
                }),
            )
            .await
            .expect("#389 memory.recall must not hard-fail on a residual FTS5 char ('@')");

        let hits = result
            .as_array()
            .expect("memory.recall common-path result is a bare JSON array");
        assert!(
            !hits.is_empty(),
            "vector leg must still return the seeded note while the FTS leg is \
             degraded by the residual FTS5 char ('@'), got empty results"
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

    #[tokio::test]
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

        // The ledger append is fired via tokio::spawn off the response path —
        // poll briefly rather than assume it has landed by the time recall returns.
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

    #[tokio::test]
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

    #[tokio::test]
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
}
