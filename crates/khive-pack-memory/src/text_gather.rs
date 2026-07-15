//! FTS candidate-gather helpers for memory.recall.
//!
//! Separates term-selection and gather-mode logic from the recall handler so
//! each concern has a direct test seam. The handler calls into this module;
//! the module calls into the khive-storage TextSearch trait.
//! See `crates/khive-pack-memory/docs/api/text-retrieval.md`.
// FILE SIZE JUSTIFICATION: text_gather.rs includes both select_terms_by_stats and
// collect_text_hits plus their inline tests; the inline tests use private test fixtures
// (MockTextSearch) that require access to module-private types and would be duplicated
// or require pub(crate) promotion if moved to the integration test directory.

use khive_runtime::RuntimeError;
use khive_storage::types::{TextFilter, TextSearchHit, TextSearchRequest, TextTermStatsRequest};
use khive_storage::TextSearch;
use khive_types::SubstrateKind;

use crate::config::{RecallFtsGatherConfig, RecallFtsGatherMode, RecallFtsSelectionRule};
use crate::handlers::TextSnippetPolicy;

/// Select at most `k` terms by original order, lowest DF, or highest IDF.
///
/// Ties retain input order; incomplete statistics fall back gracefully.
/// See `crates/khive-pack-memory/docs/api/text-retrieval.md`.
pub fn select_terms_by_stats(
    terms: &[String],
    stats: &[khive_storage::types::TextTermStats],
    rule: RecallFtsSelectionRule,
    k: usize,
) -> Vec<String> {
    if terms.is_empty() || k == 0 {
        return Vec::new();
    }
    let k = k.min(terms.len());

    match rule {
        RecallFtsSelectionRule::Original => terms[..k].to_vec(),
        RecallFtsSelectionRule::LowestDf | RecallFtsSelectionRule::HighestIdf => {
            // Build a (term, idf, original_index) vec for sorting.
            // Terms without a matching stat entry get idf=0 (treated as maximally
            // common, sorted to the back).
            let mut ranked: Vec<(usize, f64)> = terms
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let idf = stats
                        .iter()
                        .find(|s| &s.term == t || &s.sanitized_term == t)
                        .map(|s| s.inverse_document_frequency)
                        .unwrap_or(0.0);
                    (i, idf)
                })
                .collect();

            // Sort descending by IDF (highest IDF = most selective = lowest DF).
            // Stable sort preserves original index order for ties.
            ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

            // Take top-k by selectivity, then restore original query order for determinism.
            let mut selected_indices: Vec<usize> = ranked[..k].iter().map(|(i, _)| *i).collect();
            selected_indices.sort_unstable();

            selected_indices
                .into_iter()
                .filter_map(|i| terms.get(i).cloned())
                .collect()
        }
    }
}

/// Collect bounded FTS hits using configured term selection, CJK bypass, and gather mode.
///
/// Returns storage/runtime errors from statistics or search. See
/// `crates/khive-pack-memory/docs/api/text-retrieval.md`.
// REASON: all parameters are independent scalar or slice inputs with distinct types;
// extracting a struct would require callers to construct a temporary just to call this fn.
#[allow(clippy::too_many_arguments)]
pub async fn collect_text_hits(
    searcher: &dyn TextSearch,
    _query: &str,
    namespaces: &[String],
    candidate_limit: u32,
    snippet_policy: TextSnippetPolicy,
    cjk_fts_bypass: bool,
    cfg: &RecallFtsGatherConfig,
    all_terms: &[String],
) -> Result<Vec<TextSearchHit>, RuntimeError> {
    use khive_storage::types::TextQueryMode;

    let filter = Some(TextFilter {
        namespaces: namespaces.to_vec(),
        kinds: vec![SubstrateKind::Note],
        ..TextFilter::default()
    });

    // CJK bypass: skip term selection and use existing ranked path.
    if cjk_fts_bypass && cfg.cjk_bypass_ranked {
        let selected_terms: Vec<String> = all_terms.iter().take(cfg.term_k).cloned().collect();
        let join_query = if selected_terms.is_empty() {
            return Ok(Vec::new());
        } else {
            selected_terms.join(" ")
        };

        let mut hits = searcher
            .search(TextSearchRequest {
                query: join_query,
                mode: TextQueryMode::AnyTerm,
                filter,
                top_k: candidate_limit,
                snippet_chars: snippet_policy.snippet_chars(),
            })
            .await
            .map_err(|e| RuntimeError::Internal(e.to_string()))?;
        hits.sort_by_key(|h| h.rank);
        hits.truncate(candidate_limit as usize);
        return Ok(hits);
    }

    // Non-CJK path: optionally select terms by DF/IDF.
    let selected_terms: Vec<String> = if cfg.enabled
        && !matches!(cfg.selection_rule, RecallFtsSelectionRule::Original)
        && !all_terms.is_empty()
    {
        // Fetch per-term document frequency from the DB.
        let stats_result = searcher
            .term_stats(TextTermStatsRequest {
                terms: all_terms.to_vec(),
                filter: Some(TextFilter {
                    namespaces: namespaces.to_vec(),
                    kinds: vec![SubstrateKind::Note],
                    ..TextFilter::default()
                }),
            })
            .await;

        match stats_result {
            Ok(stats) => select_terms_by_stats(all_terms, &stats, cfg.selection_rule, cfg.term_k),
            Err(_) => {
                // term_stats unsupported or failed — fall back to original-order selection.
                all_terms[..cfg.term_k.min(all_terms.len())].to_vec()
            }
        }
    } else {
        all_terms[..cfg.term_k.min(all_terms.len())].to_vec()
    };

    if selected_terms.is_empty() {
        return Ok(Vec::new());
    }

    let join_query = selected_terms.join(" ");

    let request = TextSearchRequest {
        query: join_query,
        mode: TextQueryMode::AnyTerm,
        filter,
        top_k: candidate_limit,
        snippet_chars: snippet_policy.snippet_chars(),
    };

    let mut hits = if cfg.enabled && !matches!(cfg.gather_mode, RecallFtsGatherMode::Ranked) {
        let options = cfg
            .to_search_options(candidate_limit)
            .map_err(|e| RuntimeError::InvalidInput(format!("fts_gather config error: {e}")))?;
        searcher
            .search_with_options(request, options)
            .await
            .map_err(|e| RuntimeError::Internal(e.to_string()))?
    } else {
        searcher
            .search(request)
            .await
            .map_err(|e| RuntimeError::Internal(e.to_string()))?
    };

    hits.sort_by_key(|h| h.rank);
    hits.truncate(candidate_limit as usize);
    Ok(hits)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use khive_storage::types::TextTermStats;

    fn term_stats(terms: &[(&str, u64, u64)]) -> Vec<TextTermStats> {
        terms
            .iter()
            .map(|(t, df, n)| {
                let idf = ((*n as f64 - *df as f64 + 0.5) / (*df as f64 + 0.5) + 1.0).ln();
                TextTermStats {
                    term: t.to_string(),
                    sanitized_term: t.to_string(),
                    document_frequency: *df,
                    document_count: *n,
                    inverse_document_frequency: idf,
                }
            })
            .collect()
    }

    #[test]
    fn original_rule_takes_first_k() {
        let terms: Vec<String> = vec!["a", "b", "c", "d", "e"]
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        let stats = term_stats(&[("a", 100, 1000), ("b", 10, 1000), ("c", 50, 1000)]);
        let selected = select_terms_by_stats(&terms, &stats, RecallFtsSelectionRule::Original, 3);
        assert_eq!(selected, vec!["a", "b", "c"]);
    }

    #[test]
    fn lowest_df_selects_most_selective_terms() {
        // b has df=10 (rarest), c has df=50, a has df=100 (most common)
        let terms: Vec<String> = vec!["a", "b", "c"]
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        let stats = term_stats(&[("a", 100, 1000), ("b", 10, 1000), ("c", 50, 1000)]);
        let selected = select_terms_by_stats(&terms, &stats, RecallFtsSelectionRule::LowestDf, 2);
        // b and c are most selective; result must be in original query order
        assert_eq!(selected, vec!["b", "c"]);
    }

    #[test]
    fn highest_idf_equivalent_to_lowest_df() {
        let terms: Vec<String> = vec!["a", "b", "c"]
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        let stats = term_stats(&[("a", 100, 1000), ("b", 10, 1000), ("c", 50, 1000)]);
        let by_df = select_terms_by_stats(&terms, &stats, RecallFtsSelectionRule::LowestDf, 2);
        let by_idf = select_terms_by_stats(&terms, &stats, RecallFtsSelectionRule::HighestIdf, 2);
        assert_eq!(by_df, by_idf);
    }

    #[test]
    fn tie_breaks_preserve_original_order() {
        // All terms have identical DF — tie must resolve to original query order.
        let terms: Vec<String> = vec!["x", "y", "z"]
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        let stats = term_stats(&[("x", 250, 1000), ("y", 250, 1000), ("z", 250, 1000)]);
        let selected = select_terms_by_stats(&terms, &stats, RecallFtsSelectionRule::HighestIdf, 2);
        // Must be x and y (first two in original order), not y and z or x and z.
        assert_eq!(selected, vec!["x", "y"]);
    }

    #[test]
    fn empty_terms_returns_empty() {
        let selected = select_terms_by_stats(&[], &[], RecallFtsSelectionRule::HighestIdf, 3);
        assert!(selected.is_empty());
    }

    #[test]
    fn k_larger_than_terms_returns_all() {
        let terms: Vec<String> = vec!["a", "b"].into_iter().map(|s| s.to_string()).collect();
        let stats = term_stats(&[("a", 10, 100), ("b", 20, 100)]);
        let selected = select_terms_by_stats(&terms, &stats, RecallFtsSelectionRule::Original, 10);
        assert_eq!(selected, vec!["a", "b"]);
    }
}

/// Exercises gather modes and CJK routing against an in-memory FTS5 backend.
#[cfg(test)]
mod collect_text_hits_tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::{RecallFtsGatherConfig, RecallFtsGatherMode, RecallFtsSelectionRule};
    use crate::handlers::TextSnippetPolicy;
    use chrono::Utc;
    use khive_db::StorageBackend;
    use khive_storage::types::TextDocument;
    use khive_types::SubstrateKind;
    use uuid::Uuid;

    fn backend_text(key: &str) -> Arc<dyn khive_storage::TextSearch> {
        let backend = StorageBackend::memory().expect("in-memory backend");
        backend.text(key).expect("text store")
    }

    fn make_note(ns: &str, body: &str) -> (Uuid, TextDocument) {
        let id = Uuid::new_v4();
        let doc = TextDocument {
            subject_id: id,
            kind: SubstrateKind::Note,
            title: None,
            body: body.to_string(),
            tags: vec![],
            namespace: ns.to_string(),
            metadata: None,
            updated_at: Utc::now(),
        };
        (id, doc)
    }

    fn baseline() -> RecallFtsGatherConfig {
        RecallFtsGatherConfig::default() // enabled=false
    }

    // ── Regression: known fixture, baseline gather returns expected top-K ─────

    /// Insert a unique-term fixture doc plus noise. Baseline gather must include
    /// the fixture in results and respect candidate_limit.
    #[tokio::test]
    async fn gather_baseline_fixture_returns_expected_top_k() {
        let searcher = backend_text("ctf_baseline");
        let ns = "ctf";

        let fixture_id = Uuid::new_v4();
        searcher
            .upsert_document(TextDocument {
                subject_id: fixture_id,
                kind: SubstrateKind::Note,
                title: None,
                body: "quantum_xqzzy_unique signal_phrase_fixture relevant_term".to_string(),
                tags: vec![],
                namespace: ns.to_string(),
                metadata: None,
                updated_at: Utc::now(),
            })
            .await
            .expect("upsert fixture");

        for i in 0..9u32 {
            let (_, d) = make_note(ns, &format!("noise document irrelevant content {i}"));
            searcher.upsert_document(d).await.expect("upsert noise");
        }

        let terms = vec![
            "quantum_xqzzy_unique".to_string(),
            "signal_phrase_fixture".to_string(),
        ];
        let hits = collect_text_hits(
            &*searcher,
            "quantum_xqzzy_unique signal_phrase_fixture",
            &[ns.to_string()],
            10,
            TextSnippetPolicy::Omit,
            false,
            &baseline(),
            &terms,
        )
        .await
        .expect("baseline gather");

        let ids: Vec<Uuid> = hits.iter().map(|h| h.subject_id).collect();
        assert!(
            ids.contains(&fixture_id),
            "fixture doc must be in top-K; got {ids:?}"
        );
        assert!(hits.len() <= 10, "must not exceed candidate_limit=10");
    }

    // ── candidate_limit=150 boundary ──────────────────────────────────────────

    /// 200 docs all match; must cap at 150.
    #[tokio::test]
    async fn gather_candidate_limit_150_boundary() {
        let searcher = backend_text("ctf_limit150");
        let ns = "ctf";

        for i in 0..200u32 {
            let (_, d) = make_note(ns, &format!("boundary_token_zzq content {i}"));
            searcher.upsert_document(d).await.expect("upsert");
        }

        let terms = vec!["boundary_token_zzq".to_string()];
        let hits = collect_text_hits(
            &*searcher,
            "boundary_token_zzq",
            &[ns.to_string()],
            150,
            TextSnippetPolicy::Omit,
            false,
            &baseline(),
            &terms,
        )
        .await
        .expect("limit 150 gather");

        assert!(!hits.is_empty(), "must return hits");
        assert!(hits.len() <= 150, "must cap at 150, got {}", hits.len());
    }

    // ── Edge case: empty query terms ──────────────────────────────────────────

    #[tokio::test]
    async fn gather_empty_terms_returns_empty() {
        let searcher = backend_text("ctf_empty_terms");
        let ns = "ctf";

        let (_, d) = make_note(ns, "some content here");
        searcher.upsert_document(d).await.expect("upsert");

        let hits = collect_text_hits(
            &*searcher,
            "",
            &[ns.to_string()],
            10,
            TextSnippetPolicy::Omit,
            false,
            &baseline(),
            &[],
        )
        .await
        .expect("empty terms gather");

        assert!(
            hits.is_empty(),
            "empty terms must return empty, got {} hits",
            hits.len()
        );
    }

    // ── Edge case: single rare term with HighestIdf selection ─────────────────

    /// 1 doc has "rare_xqzzy_token", 10 docs have only "common_word_term".
    /// IDF selection with k=1 picks "rare_xqzzy_token" → only the rare doc returns.
    #[tokio::test]
    async fn gather_single_rare_term_idf_selection_returns_rare_doc() {
        let searcher = backend_text("ctf_rare_term");
        let ns = "ctf";

        let rare_id = Uuid::new_v4();
        searcher
            .upsert_document(TextDocument {
                subject_id: rare_id,
                kind: SubstrateKind::Note,
                title: None,
                body: "rare_xqzzy_token common_word_term context".to_string(),
                tags: vec![],
                namespace: ns.to_string(),
                metadata: None,
                updated_at: Utc::now(),
            })
            .await
            .expect("upsert rare doc");

        for i in 0..10u32 {
            let (_, d) = make_note(ns, &format!("common_word_term filler content {i}"));
            searcher
                .upsert_document(d)
                .await
                .expect("upsert common doc");
        }

        let terms = vec![
            "common_word_term".to_string(),
            "rare_xqzzy_token".to_string(),
        ];
        let cfg = RecallFtsGatherConfig {
            enabled: true,
            term_k: 1,
            selection_rule: RecallFtsSelectionRule::HighestIdf,
            gather_mode: RecallFtsGatherMode::Ranked,
            ..RecallFtsGatherConfig::default()
        };

        let hits = collect_text_hits(
            &*searcher,
            "common_word_term rare_xqzzy_token",
            &[ns.to_string()],
            10,
            TextSnippetPolicy::Omit,
            false,
            &cfg,
            &terms,
        )
        .await
        .expect("rare term gather");

        let ids: Vec<Uuid> = hits.iter().map(|h| h.subject_id).collect();
        assert!(
            ids.contains(&rare_id),
            "rare doc must be in IDF-selected results; got {ids:?}"
        );
        // k=1 selects only "rare_xqzzy_token"; only 1 doc has that term.
        assert_eq!(
            hits.len(),
            1,
            "exactly 1 doc matches rare term; got {}",
            hits.len()
        );
    }

    // ── Edge case: all high-DF terms still return hits ────────────────────────

    #[tokio::test]
    async fn gather_all_high_df_terms_still_returns_hits() {
        let searcher = backend_text("ctf_high_df");
        let ns = "ctf";

        for i in 0..8u32 {
            let (_, d) = make_note(
                ns,
                &format!("common_alpha common_beta common_gamma doc {i}"),
            );
            searcher.upsert_document(d).await.expect("upsert");
        }

        let terms = vec![
            "common_alpha".to_string(),
            "common_beta".to_string(),
            "common_gamma".to_string(),
        ];
        let hits = collect_text_hits(
            &*searcher,
            "common_alpha common_beta common_gamma",
            &[ns.to_string()],
            10,
            TextSnippetPolicy::Omit,
            false,
            &baseline(),
            &terms,
        )
        .await
        .expect("high df gather");

        assert!(!hits.is_empty(), "high-DF terms must still return hits");
    }

    // ── CJK case: trigram path stays covered ──────────────────────────────────

    /// Insert a doc with Chinese text, query via trigram bypass path.
    /// Verifies the CJK bypass (`cjk_fts_bypass=true, cjk_bypass_ranked=true`) finds it.
    #[tokio::test]
    async fn gather_cjk_bypass_finds_cjk_document() {
        let searcher = backend_text("ctf_cjk");
        let ns = "ctf";

        let cjk_id = Uuid::new_v4();
        searcher
            .upsert_document(TextDocument {
                subject_id: cjk_id,
                kind: SubstrateKind::Note,
                title: None,
                body: "这是一个中文注释关于机器学习的内容".to_string(),
                tags: vec![],
                namespace: ns.to_string(),
                metadata: None,
                updated_at: Utc::now(),
            })
            .await
            .expect("upsert CJK doc");

        for i in 0..3u32 {
            let (_, d) = make_note(ns, &format!("unrelated latin content noise {i}"));
            searcher.upsert_document(d).await.expect("upsert noise");
        }

        let terms = vec!["机器学习".to_string()];
        let cfg = RecallFtsGatherConfig {
            cjk_bypass_ranked: true,
            ..RecallFtsGatherConfig::default()
        };

        let hits = collect_text_hits(
            &*searcher,
            "机器学习",
            &[ns.to_string()],
            10,
            TextSnippetPolicy::Omit,
            true, // cjk_fts_bypass=true
            &cfg,
            &terms,
        )
        .await
        .expect("CJK gather");

        let ids: Vec<Uuid> = hits.iter().map(|h| h.subject_id).collect();
        assert!(
            ids.contains(&cjk_id),
            "CJK doc must be found by trigram bypass path; got {ids:?}"
        );
    }

    // ── Gather modes: enabled ranked matches baseline result set ──────────────

    #[tokio::test]
    async fn gather_enabled_ranked_matches_baseline_result_set() {
        let searcher = backend_text("ctf_ranked_parity");
        let ns = "ctf";

        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        searcher
            .upsert_document(TextDocument {
                subject_id: id1,
                kind: SubstrateKind::Note,
                title: None,
                body: "alpha_tok beta_tok primary content".to_string(),
                tags: vec![],
                namespace: ns.to_string(),
                metadata: None,
                updated_at: Utc::now(),
            })
            .await
            .expect("upsert id1");
        searcher
            .upsert_document(TextDocument {
                subject_id: id2,
                kind: SubstrateKind::Note,
                title: None,
                body: "alpha_tok secondary content".to_string(),
                tags: vec![],
                namespace: ns.to_string(),
                metadata: None,
                updated_at: Utc::now(),
            })
            .await
            .expect("upsert id2");

        let terms = vec!["alpha_tok".to_string(), "beta_tok".to_string()];

        let baseline_hits = collect_text_hits(
            &*searcher,
            "alpha_tok beta_tok",
            &[ns.to_string()],
            10,
            TextSnippetPolicy::Omit,
            false,
            &baseline(),
            &terms,
        )
        .await
        .expect("baseline");

        let ranked_cfg = RecallFtsGatherConfig {
            enabled: true,
            gather_mode: RecallFtsGatherMode::Ranked,
            ..RecallFtsGatherConfig::default()
        };
        let ranked_hits = collect_text_hits(
            &*searcher,
            "alpha_tok beta_tok",
            &[ns.to_string()],
            10,
            TextSnippetPolicy::Omit,
            false,
            &ranked_cfg,
            &terms,
        )
        .await
        .expect("ranked");

        let baseline_ids: std::collections::HashSet<Uuid> =
            baseline_hits.iter().map(|h| h.subject_id).collect();
        let ranked_ids: std::collections::HashSet<Uuid> =
            ranked_hits.iter().map(|h| h.subject_id).collect();
        assert_eq!(
            baseline_ids, ranked_ids,
            "enabled ranked must produce same result set as baseline"
        );
    }

    // ── RankWithinCap: caps at candidate_limit ────────────────────────────────

    #[tokio::test]
    async fn gather_rank_within_cap_caps_at_candidate_limit() {
        let searcher = backend_text("ctf_rank_within_cap");
        let ns = "ctf";

        for i in 0..20u32 {
            let (_, d) = make_note(ns, &format!("candidate_tok xqzzy_fixture content {i}"));
            searcher.upsert_document(d).await.expect("upsert");
        }

        let terms = vec!["candidate_tok".to_string()];
        let cfg = RecallFtsGatherConfig {
            enabled: true,
            gather_mode: RecallFtsGatherMode::RankWithinCap,
            gather_cap_multiplier: 4,
            ..RecallFtsGatherConfig::default()
        };

        let hits = collect_text_hits(
            &*searcher,
            "candidate_tok",
            &[ns.to_string()],
            5,
            TextSnippetPolicy::Omit,
            false,
            &cfg,
            &terms,
        )
        .await
        .expect("rank_within_cap");

        assert!(!hits.is_empty(), "must return hits");
        assert!(
            hits.len() <= 5,
            "rank_within_cap must cap at candidate_limit=5, got {}",
            hits.len()
        );
    }
}
