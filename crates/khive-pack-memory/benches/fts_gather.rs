//! Criterion FTS gather over a deterministic 2,048-document frequency mixture.
//! Untimed recall@10 compares each actual result with the full-term ranked top10.
//! This quality cohort does not establish production quality or a tuning default.
#[allow(dead_code)]
#[path = "reducible_support/fixture.rs"]
mod fixture;
#[allow(dead_code)]
#[path = "reducible_support/report.rs"]
mod report;
use criterion::Criterion;
use khive_pack_memory::config::{
    RecallFtsGatherConfig, RecallFtsGatherMode, RecallFtsSelectionRule,
};
use khive_pack_memory::handlers::{recall_text_terms, TextSnippetPolicy};
use khive_pack_memory::text_gather::collect_text_hits;
use khive_storage::types::{
    TextDocument, TextFilter, TextQueryMode, TextSearchHit, TextSearchRequest, TextTermStatsRequest,
};
use khive_storage::TextSearch;
use khive_types::SubstrateKind;
use std::collections::HashSet;
use std::time::{Duration, Instant};
use uuid::Uuid;
const NS: &str = "local";
const CANDIDATE_LIMIT: u32 = 150;
const DOCUMENTS: usize = 2_048;
const QUERIES: usize = 32;
/// One query: its terms plus the per-term document frequency (computed once,
/// untimed, so term-pruning strategies don't pay the `term_stats` cost in the
/// measured path — that cost is measured separately by the PROD strategies).
struct Query {
    terms: Vec<String>,
    dfs: Vec<u64>,
}

/// How a strategy turns a query into FTS results.
enum Strategy {
    /// Run the production baseline ranked search over a *pre-pruned* term subset.
    /// Selection is computed in setup (untimed). `None` selector = all terms.
    RankedPruned(Selector),
    /// Run the production `collect_text_hits` path with this gather config
    /// (includes any `term_stats` round-trips in the timed path).
    Production(RecallFtsGatherConfig),
}

#[derive(Clone, Copy)]
enum Selector {
    /// Keep every term (= production baseline).
    All,
    /// Drop terms whose document frequency exceeds `frac` of the corpus
    /// (near-zero-IDF stopwords). Always keeps ≥1 term (the rarest).
    PruneDfFrac(f64),
    /// Keep the `k` highest-IDF (lowest-DF) terms, original order.
    TopIdf(usize),
    /// Drop terms in the static stopword set — FREE (no DB round-trip / no DF).
    /// Compare this heuristic with the full-term baseline on the generated corpus.
    DropStopwords,
}

/// Fixed function-word list; quality is measured against this generated corpus,
/// not inferred from any production frequency or safety claim.
const STOPWORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "but", "if", "then", "else", "for", "of", "to", "in", "on",
    "at", "by", "with", "from", "as", "is", "are", "was", "were", "be", "been", "being", "it",
    "its", "this", "that", "these", "those", "there", "here", "we", "you", "they", "he", "she",
    "i", "me", "my", "our", "your", "their", "his", "her", "them", "us", "do", "does", "did",
    "has", "have", "had", "not", "no", "so", "up", "out", "can", "will", "would", "should",
    "could", "may", "might", "must", "than", "too", "very", "just", "into", "over", "via", "per",
    "about", "after", "before", "when", "where", "which", "who", "what", "how", "all", "any",
    "each", "more", "most", "some", "such", "only", "own", "same", "also", "now", "new", "use",
    "using", "used", "get", "got",
];

struct BenchStrategy {
    name: &'static str,
    strategy: Strategy,
}

fn gather(
    selection: RecallFtsSelectionRule,
    term_k: usize,
    gather_mode: RecallFtsGatherMode,
) -> RecallFtsGatherConfig {
    RecallFtsGatherConfig {
        enabled: true,
        term_k,
        selection_rule: selection,
        gather_mode,
        gather_cap_multiplier: 4,
        ..RecallFtsGatherConfig::default()
    }
}

fn strategies() -> Vec<BenchStrategy> {
    use RecallFtsGatherMode::*;
    use RecallFtsSelectionRule::*;
    vec![
        BenchStrategy {
            name: "baseline",
            strategy: Strategy::RankedPruned(Selector::All),
        },
        BenchStrategy {
            name: "df-50",
            strategy: Strategy::RankedPruned(Selector::PruneDfFrac(0.50)),
        },
        BenchStrategy {
            name: "df-33",
            strategy: Strategy::RankedPruned(Selector::PruneDfFrac(0.33)),
        },
        BenchStrategy {
            name: "df-20",
            strategy: Strategy::RankedPruned(Selector::PruneDfFrac(0.20)),
        },
        BenchStrategy {
            name: "df-10",
            strategy: Strategy::RankedPruned(Selector::PruneDfFrac(0.10)),
        },
        BenchStrategy {
            name: "stopwords",
            strategy: Strategy::RankedPruned(Selector::DropStopwords),
        },
        BenchStrategy {
            name: "idf-k5-precomputed",
            strategy: Strategy::RankedPruned(Selector::TopIdf(5)),
        },
        BenchStrategy {
            name: "idf-k5-production",
            strategy: Strategy::Production(gather(HighestIdf, 5, Ranked)),
        },
        BenchStrategy {
            name: "unranked-production",
            strategy: Strategy::Production(gather(Original, 10, Unranked)),
        },
        BenchStrategy {
            name: "rank-within-cap-production",
            strategy: Strategy::Production(gather(Original, 10, RankWithinCap)),
        },
    ]
}

fn note_filter() -> TextFilter {
    TextFilter {
        namespaces: vec![NS.to_string()],
        kinds: vec![SubstrateKind::Note],
        record_kinds: vec!["memory".to_string()],
        ..TextFilter::default()
    }
}

/// Select a term subset for a `RankedPruned` strategy. Pure Rust over the
/// pre-computed DFs — no DB access — so it is computed in setup, not timed.
fn select(sel: Selector, q: &Query, doc_count: u64) -> Vec<String> {
    match sel {
        Selector::All => q.terms.clone(),
        Selector::PruneDfFrac(frac) => {
            let threshold = (frac * doc_count as f64) as u64;
            let kept: Vec<String> = q
                .terms
                .iter()
                .zip(q.dfs.iter())
                .filter(|(_, &df)| df <= threshold)
                .map(|(t, _)| t.clone())
                .collect();
            if kept.is_empty() {
                // All terms are common — keep the single rarest so the query is non-empty.
                let min_idx = q
                    .dfs
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, &df)| df)
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                vec![q.terms[min_idx].clone()]
            } else {
                kept
            }
        }
        Selector::TopIdf(k) => {
            let mut idx: Vec<usize> = (0..q.terms.len()).collect();
            // Lowest DF = highest IDF = most selective; stable to preserve order on ties.
            idx.sort_by_key(|&i| q.dfs[i]);
            idx.truncate(k.min(q.terms.len()));
            idx.sort_unstable();
            idx.into_iter().map(|i| q.terms[i].clone()).collect()
        }
        Selector::DropStopwords => {
            let kept: Vec<String> = q
                .terms
                .iter()
                .filter(|t| !STOPWORDS.contains(&t.as_str()))
                .cloned()
                .collect();
            if kept.is_empty() {
                q.terms.clone()
            } else {
                kept
            }
        }
    }
}

/// Production baseline ranked search over an explicit term list (reproduces the
/// `else` branch of `handle_recall`'s text leg).
async fn ranked_search(searcher: &dyn TextSearch, terms: &[String]) -> Vec<TextSearchHit> {
    if terms.is_empty() {
        return Vec::new();
    }
    let mut h = searcher
        .search(TextSearchRequest {
            query: terms.join(" "),
            mode: TextQueryMode::AnyTerm,
            filter: Some(note_filter()),
            top_k: CANDIDATE_LIMIT,
            snippet_chars: 0,
        })
        .await
        .expect("ranked search");
    h.sort_by_key(|h| h.rank);
    h.truncate(CANDIDATE_LIMIT as usize);
    h
}

/// Production gather path (`collect_text_hits`) over the full term list.
async fn production_search(
    searcher: &dyn TextSearch,
    cfg: &RecallFtsGatherConfig,
    terms: &[String],
) -> Vec<TextSearchHit> {
    let ns_vec = vec![NS.to_string()];
    collect_text_hits(
        searcher,
        "",
        &ns_vec,
        CANDIDATE_LIMIT,
        TextSnippetPolicy::Omit,
        false,
        cfg,
        terms,
    )
    .await
    .expect("collect_text_hits")
}

fn recall_of(reference: &[Uuid], candidate: &HashSet<Uuid>) -> f64 {
    assert_eq!(reference.len(), 10, "nonvacuous full top10 reference");
    reference.iter().filter(|id| candidate.contains(id)).count() as f64 / reference.len() as f64
}

struct Inputs {
    queries: Vec<Query>,
    reference: Vec<Vec<Uuid>>,
}

async fn prepare(searcher: &dyn TextSearch) -> Inputs {
    let timestamp = chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("fixed timestamp");
    let documents = (0..DOCUMENTS)
        .map(|i| TextDocument {
            subject_id: Uuid::from_u128((u128::from(fixture::SEED) << 64) | (i as u128 + 1)),
            kind: SubstrateKind::Note,
            record_kind: Some("memory".into()),
            title: None,
            body: fixture::body(i),
            tags: vec![],
            namespace: NS.into(),
            metadata: None,
            updated_at: timestamp,
        })
        .collect();
    let summary = searcher
        .upsert_documents(documents)
        .await
        .expect("generated FTS corpus");
    assert_eq!(summary.attempted, DOCUMENTS as u64);
    assert_eq!(summary.affected, DOCUMENTS as u64);
    assert_eq!(summary.failed, 0);
    assert_eq!(
        searcher.count(note_filter()).await.expect("FTS count"),
        DOCUMENTS as u64
    );
    let mut queries = Vec::new();
    let mut reference = Vec::new();
    for q in 0..QUERIES {
        let terms = recall_text_terms(&fixture::query(q));
        assert!(terms.len() >= 6);
        let stats = searcher
            .term_stats(TextTermStatsRequest {
                terms: terms.clone(),
                filter: Some(note_filter()),
            })
            .await
            .expect("frequencies");
        let dfs = terms
            .iter()
            .map(|term| {
                stats
                    .iter()
                    .find(|s| &s.term == term || &s.sanitized_term == term)
                    .expect("every requested term has stats")
                    .document_frequency
            })
            .collect();
        let baseline = ranked_search(searcher, &terms).await;
        let ids: Vec<_> = baseline.iter().take(10).map(|h| h.subject_id).collect();
        assert_eq!(ids.len(), 10);
        assert_eq!(ids.iter().copied().collect::<HashSet<_>>().len(), 10);
        queries.push(Query { terms, dfs });
        reference.push(ids);
    }
    Inputs { queries, reference }
}

async fn run(
    searcher: &dyn TextSearch,
    strategy: &Strategy,
    terms: &[String],
) -> Vec<TextSearchHit> {
    match strategy {
        Strategy::RankedPruned(_) => ranked_search(searcher, terms).await,
        Strategy::Production(config) => production_search(searcher, config, terms).await,
    }
}

fn selected_terms(strategy: &Strategy, inputs: &Inputs) -> Vec<Vec<String>> {
    inputs
        .queries
        .iter()
        .map(|q| match strategy {
            Strategy::RankedPruned(selector) => select(*selector, q, DOCUMENTS as u64),
            Strategy::Production(_) => q.terms.clone(),
        })
        .collect()
}

async fn quality(
    searcher: &dyn TextSearch,
    strategy: &Strategy,
    terms: &[Vec<String>],
    inputs: &Inputs,
) -> f64 {
    let mut total = 0.0;
    for (i, query) in terms.iter().enumerate() {
        let hits = run(searcher, strategy, query).await;
        assert!(!hits.is_empty() && hits.len() <= CANDIDATE_LIMIT as usize);
        let candidate = hits.iter().take(10).map(|h| h.subject_id).collect();
        total += recall_of(&inputs.reference[i], &candidate);
    }
    total / inputs.queries.len() as f64
}

fn quality_controls(inputs: &Inputs) {
    // A proper subset and unrelated UUIDs must not receive perfect quality.
    // These controls use an independently captured baseline, never a candidate
    // set as its own oracle.
    for reference in &inputs.reference {
        assert_eq!(
            recall_of(reference, &reference.iter().copied().collect()),
            1.0
        );
        assert_eq!(
            recall_of(reference, &reference[..5].iter().copied().collect()),
            0.5
        );
        assert_eq!(
            recall_of(reference, &[Uuid::nil()].into_iter().collect()),
            0.0
        );
    }
}

fn main() {
    let strategies = strategies();
    let ids: Vec<_> = strategies
        .iter()
        .map(|s| format!("fts_gather/synthetic-v1/{}", s.name))
        .collect();
    if report::list_requested(&ids.iter().map(String::as_str).collect::<Vec<_>>()) {
        return;
    }
    report::assert_not_list_probe();
    let regression = report::regression_case();
    if let Some(case) = regression.as_deref() {
        assert_eq!(case, "quality", "unknown regression case");
    }
    let mut criterion = regression.is_none().then(|| {
        Criterion::default()
            .sample_size(10)
            .warm_up_time(Duration::from_millis(100))
            .measurement_time(Duration::from_millis(300))
            .configure_from_args()
    });
    let executor = fixture::executor();
    let mut state = None;
    for (strategy, id) in strategies.iter().zip(&ids) {
        let mut reported = false;
        let mut measure = |bencher: Option<&mut criterion::Bencher<'_>>| {
            let (database, inputs) = state.get_or_insert_with(|| {
                executor.block_on(async {
                    let database = fixture::Fixture::new(&[], true);
                    let searcher = database.backend.text("notes_local").expect("FTS");
                    let inputs = prepare(searcher.as_ref()).await;
                    quality_controls(&inputs);
                    (database, inputs)
                })
            });
            let searcher = database.backend.text("notes_local").expect("FTS");
            let terms = selected_terms(&strategy.strategy, inputs);
            if !reported {
                let overlap = executor.block_on(quality(
                    searcher.as_ref(),
                    &strategy.strategy,
                    &terms,
                    inputs,
                ));
                assert!((0.0..=1.0).contains(&overlap));
                if strategy.name == "baseline" {
                    assert_eq!(overlap, 1.0);
                }
                // Exactly one quality observation per selected case. Criterion owns
                // latency estimates; recall is a dimensionless, untimed statistic.
                report::emit(id, "recall_at_10", overlap, QUERIES);
                reported = true;
            }
            if let Some(bencher) = bencher {
                let mut next = 0;
                bencher.iter_custom(|iterations| {
                    let mut elapsed = Duration::ZERO;
                    for _ in 0..iterations {
                        let query = &terms[next % terms.len()];
                        next += 1;
                        let start = Instant::now();
                        let hits =
                            executor.block_on(run(searcher.as_ref(), &strategy.strategy, query));
                        elapsed += start.elapsed();
                        assert!(!hits.is_empty() && hits.len() <= CANDIDATE_LIMIT as usize);
                    }
                    elapsed
                });
            }
        };
        if let Some(criterion) = &mut criterion {
            criterion.bench_function(id, |b| measure(Some(b)));
        } else {
            measure(None);
        }
    }
    if let Some((database, _)) = state {
        executor.block_on(database.close());
    }
    if let Some(criterion) = criterion {
        criterion.final_summary();
    }
}
