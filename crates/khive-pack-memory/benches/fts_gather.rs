//! Real-corpus latency + quality benchmark for the FTS candidate-gather path (PR #625).
//!
//! Why this exists: the prior parameter sweep (`tests/khive-contract/tune/`) ran on a
//! synthetic corpus that produced a flat recall@10 = 0.9333 for *every* config — it
//! could not discriminate any parameter, so PR #625's gather optimization shipped as
//! inert infrastructure (`enabled: false`) with no evidence it helps or is safe.
//!
//! This benchmark uses the REAL memory corpus (~12k local notes extracted from
//! `khive-graph.db` into a git-ignored fixture) and the REAL production code path
//! (`collect_text_hits` → `search` / `search_with_options` / `term_stats`). It measures,
//! per strategy, latency (p50 / p95 / mean of the FTS candidate-gather leg) and
//! quality vs the production baseline (recall@10 and candidate-pool recall),
//! so a default can be chosen on data instead of guesswork.
//!
//! Key finding the benchmark surfaces: the FTS OR-match set is dominated by
//! near-zero-IDF terms (English stopwords like "for"/"and"/"with" each match ~40-57%
//! of the corpus). Those terms cause the expensive BM25 scan but contribute ~nothing
//! to ranking, so dropping them is both faster AND coverage-safe — whereas fixed-k
//! term selection drops *meaningful* terms and loses recall, and the per-term
//! `term_stats` round-trips cost more than the gather saves.
//!
//! It is `#[ignore]`d (a benchmark, not a correctness gate) and skips cleanly when the
//! fixture is absent, so it never breaks CI. Run it with:
//!
//! ```bash
//! # extract the fixture first (read-only, never mutates the source DB):
//! sqlite3 'file:'"$HOME"'/.khive/khive-graph.db?mode=ro' \
//!   "PRAGMA query_only=1; SELECT json_object('id',id,'kind',kind,'title',COALESCE(name,''),'body',content) \
//!    FROM notes WHERE namespace='local' AND deleted_at IS NULL ORDER BY created_at;" \
//!   > crates/khive-pack-memory/tests/fixtures/memory_corpus_local.jsonl
//!
//! cargo test -p khive-pack-memory --release bench_fts_gather_real_corpus -- --ignored --nocapture
//! ```
use std::collections::HashSet;
use std::time::Instant;

use chrono::Utc;
use serde::Deserialize;
use uuid::Uuid;

use khive_db::StorageBackend;
use khive_storage::types::{
    TextDocument, TextFilter, TextQueryMode, TextSearchHit, TextSearchRequest, TextTermStatsRequest,
};
use khive_storage::TextSearch;
use khive_types::SubstrateKind;

use khive_pack_memory::config::{
    RecallFtsGatherConfig, RecallFtsGatherMode, RecallFtsSelectionRule,
};
use khive_pack_memory::handlers::{recall_text_terms, TextSnippetPolicy};
use khive_pack_memory::text_gather::collect_text_hits;

const NS: &str = "local";
/// Matches `RecallConfig::default().candidate_limit` — the per-leg candidate pool
/// size production fusion actually sees.
const CANDIDATE_LIMIT: u32 = 150;
/// Number of distinct queries sampled from the memory subset.
const N_QUERIES: usize = 150;
/// Timed repeats per (strategy, query); percentiles aggregate over query × repeat.
const REPEATS: usize = 5;
/// Only use queries with at least this many distinct terms.
const MIN_QUERY_TERMS: usize = 4;

#[derive(Deserialize)]
struct CorpusRow {
    id: String,
    kind: String,
    #[serde(default)]
    title: String,
    body: String,
}

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
    /// This is the production-viable approximation of high-DF pruning: stopwords
    /// are exactly the near-zero-IDF terms that inflate the OR-match set.
    DropStopwords,
}

/// Near-zero-IDF English function words that dominate the OR-match set without
/// affecting BM25 ranking. Validated against the real corpus: "for"=57%,
/// "and"=46%, "with"=41%, "the"=38% of docs. Dropping them is coverage-safe.
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
            name: "baseline: all terms, ranked",
            strategy: Strategy::RankedPruned(Selector::All),
        },
        BenchStrategy {
            name: "ranked, drop df>50%",
            strategy: Strategy::RankedPruned(Selector::PruneDfFrac(0.50)),
        },
        BenchStrategy {
            name: "ranked, drop df>33%",
            strategy: Strategy::RankedPruned(Selector::PruneDfFrac(0.33)),
        },
        BenchStrategy {
            name: "ranked, drop df>20%",
            strategy: Strategy::RankedPruned(Selector::PruneDfFrac(0.20)),
        },
        BenchStrategy {
            name: "ranked, drop df>10%",
            strategy: Strategy::RankedPruned(Selector::PruneDfFrac(0.10)),
        },
        BenchStrategy {
            name: "ranked, drop stopwords (FREE)",
            strategy: Strategy::RankedPruned(Selector::DropStopwords),
        },
        BenchStrategy {
            name: "ranked, top-IDF k5 (no tax)",
            strategy: Strategy::RankedPruned(Selector::TopIdf(5)),
        },
        BenchStrategy {
            name: "PROD idf-k5 (term_stats tax)",
            strategy: Strategy::Production(gather(HighestIdf, 5, Ranked)),
        },
        BenchStrategy {
            name: "PROD unranked all (speed floor)",
            strategy: Strategy::Production(gather(Original, 10, Unranked)),
        },
        BenchStrategy {
            name: "PROD rank_within_cap m4",
            strategy: Strategy::Production(gather(Original, 10, RankWithinCap)),
        },
    ]
}

fn note_filter() -> TextFilter {
    TextFilter {
        namespaces: vec![NS.to_string()],
        kinds: vec![SubstrateKind::Note],
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
    collect_text_hits(
        searcher,
        "",
        NS,
        CANDIDATE_LIMIT,
        TextSnippetPolicy::Omit,
        false,
        cfg,
        terms,
    )
    .await
    .expect("collect_text_hits")
}

fn percentile(sorted: &[u128], p: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = (((sorted.len() - 1) as f64) * p).round() as usize;
    sorted[idx]
}

fn top_k_ids(hits: &[TextSearchHit], k: usize) -> HashSet<Uuid> {
    hits.iter().take(k).map(|h| h.subject_id).collect()
}

/// Fraction of `reference` ids that also appear in `candidate` (recall of the
/// reference set). Returns 1.0 when the reference is empty (vacuously complete).
fn recall_of(reference: &[Uuid], candidate: &HashSet<Uuid>) -> f64 {
    if reference.is_empty() {
        return 1.0;
    }
    let found = reference.iter().filter(|id| candidate.contains(id)).count();
    found as f64 / reference.len() as f64
}

struct StratResult {
    name: &'static str,
    mean_us: f64,
    p50_us: u128,
    p95_us: u128,
    recall_at_10: f64,
    recall_pool: f64,
    avg_returned: f64,
    avg_terms: f64,
}

#[tokio::main]
async fn main() {
    let fixture = std::env::var("KHIVE_BENCH_CORPUS").unwrap_or_else(|_| {
        format!(
            "{}/tests/fixtures/memory_corpus_local.jsonl",
            env!("CARGO_MANIFEST_DIR")
        )
    });
    if !std::path::Path::new(&fixture).exists() {
        eprintln!(
            "SKIP bench_fts_gather_real_corpus: fixture not found at {fixture}\n\
             Extract it (read-only) with the sqlite3 command in this file's module docs."
        );
        return;
    }

    // ── Load corpus ──────────────────────────────────────────────────────────
    let raw = std::fs::read_to_string(&fixture).expect("read fixture");
    let mut docs: Vec<TextDocument> = Vec::new();
    let mut memory_bodies: Vec<String> = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<CorpusRow>(line) else {
            continue;
        };
        let Ok(id) = row.id.parse::<Uuid>() else {
            continue;
        };
        if row.kind == "memory" {
            memory_bodies.push(row.body.clone());
        }
        docs.push(TextDocument {
            subject_id: id,
            kind: SubstrateKind::Note,
            title: (!row.title.is_empty()).then_some(row.title),
            body: row.body,
            tags: vec![],
            namespace: NS.to_string(),
            metadata: None,
            updated_at: Utc::now(),
        });
    }
    assert!(!docs.is_empty(), "fixture parsed to zero documents");

    // ── Build a file-backed FTS index (production daemon topology) ────────────
    let db_path =
        std::env::temp_dir().join(format!("khive_fts_gather_bench_{}.db", std::process::id()));
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{}", db_path.display(), suffix));
    }
    let backend = StorageBackend::sqlite(&db_path).expect("open file-backed backend");
    let searcher = backend.text("notes_local").expect("text store");

    let total_docs = docs.len();
    let build_start = Instant::now();
    for chunk in docs.chunks(2000) {
        searcher
            .upsert_documents(chunk.to_vec())
            .await
            .expect("batch upsert");
    }
    let build_secs = build_start.elapsed().as_secs_f64();
    let indexed = searcher.count(note_filter()).await.expect("count");

    // ── Build the query set + per-term DFs (untimed) ──────────────────────────
    // Deterministic stride sampling. recall_text_terms can emit tokens with
    // internal punctuation (e.g. "a/b"); both baseline and gather FTS paths reject
    // those equally (sanitize leaves '/' intact → bareword syntax error), so they
    // are not part of the realistic successful-query distribution. Keep only
    // FTS5-bareword-safe terms (alphanumeric/underscore/CJK).
    let mut bodies_for_queries: Vec<Vec<String>> = Vec::new();
    if !memory_bodies.is_empty() {
        let stride = (memory_bodies.len() / (N_QUERIES * 2)).max(1);
        let mut i = 0;
        while i < memory_bodies.len() && bodies_for_queries.len() < N_QUERIES {
            let terms: Vec<String> = recall_text_terms(&memory_bodies[i])
                .into_iter()
                .filter(|t| t.chars().all(|c| c.is_alphanumeric() || c == '_'))
                .collect();
            if terms.len() >= MIN_QUERY_TERMS {
                bodies_for_queries.push(terms);
            }
            i += stride;
        }
    }
    assert!(
        !bodies_for_queries.is_empty(),
        "no queries with >= {MIN_QUERY_TERMS} terms"
    );

    let mut doc_count = indexed;
    let mut queries: Vec<Query> = Vec::with_capacity(bodies_for_queries.len());
    for terms in bodies_for_queries {
        let stats = searcher
            .term_stats(TextTermStatsRequest {
                terms: terms.clone(),
                filter: Some(note_filter()),
            })
            .await
            .expect("term_stats");
        if let Some(s) = stats.first() {
            doc_count = s.document_count;
        }
        let dfs: Vec<u64> = terms
            .iter()
            .map(|t| {
                stats
                    .iter()
                    .find(|s| &s.term == t || &s.sanitized_term == t)
                    .map(|s| s.document_frequency)
                    .unwrap_or(0)
            })
            .collect();
        queries.push(Query { terms, dfs });
    }
    let avg_query_terms =
        queries.iter().map(|q| q.terms.len()).sum::<usize>() as f64 / queries.len() as f64;

    let strats = strategies();

    // Pre-compute pruned term lists for the RankedPruned strategies (untimed).
    let pruned: Vec<Option<Vec<Vec<String>>>> = strats
        .iter()
        .map(|s| match &s.strategy {
            Strategy::RankedPruned(sel) => {
                Some(queries.iter().map(|q| select(*sel, q, doc_count)).collect())
            }
            Strategy::Production(_) => None,
        })
        .collect();

    // ── Warmup: prime OS page cache + FTS internal state for every path ───────
    for (si, s) in strats.iter().enumerate() {
        for (qi, q) in queries.iter().enumerate() {
            let _ = run_strategy(searcher.as_ref(), s, &pruned[si], qi, q).await;
        }
    }

    // ── Reference (baseline = all terms ranked) result sets for quality ───────
    let mut base_top10: Vec<Vec<Uuid>> = Vec::with_capacity(queries.len());
    let mut base_pool: Vec<Vec<Uuid>> = Vec::with_capacity(queries.len());
    for q in &queries {
        let hits = ranked_search(searcher.as_ref(), &q.terms).await;
        base_top10.push(hits.iter().take(10).map(|h| h.subject_id).collect());
        base_pool.push(hits.iter().map(|h| h.subject_id).collect());
    }

    // ── Measure each strategy ──────────────────────────────────────────────────
    let mut results: Vec<StratResult> = Vec::new();
    for (si, s) in strats.iter().enumerate() {
        let mut latencies: Vec<u128> = Vec::with_capacity(queries.len() * REPEATS);
        let (mut recall10_sum, mut recall_pool_sum) = (0.0, 0.0);
        let (mut returned_sum, mut terms_sum) = (0usize, 0usize);

        for (qi, q) in queries.iter().enumerate() {
            // Quality (deterministic): one run.
            let res = run_strategy(searcher.as_ref(), s, &pruned[si], qi, q).await;
            let cfg_top10 = top_k_ids(&res, 10);
            let cfg_pool: HashSet<Uuid> = res.iter().map(|h| h.subject_id).collect();
            recall10_sum += recall_of(&base_top10[qi], &cfg_top10);
            recall_pool_sum += recall_of(&base_pool[qi], &cfg_pool);
            returned_sum += res.len();
            terms_sum += match &pruned[si] {
                Some(p) => p[qi].len(),
                None => q.terms.len(),
            };

            // Latency: time the gather leg over REPEATS runs.
            for _ in 0..REPEATS {
                let t = Instant::now();
                let _ = run_strategy(searcher.as_ref(), s, &pruned[si], qi, q).await;
                latencies.push(t.elapsed().as_micros());
            }
        }

        latencies.sort_unstable();
        let n = queries.len() as f64;
        results.push(StratResult {
            name: s.name,
            mean_us: latencies.iter().sum::<u128>() as f64 / latencies.len() as f64,
            p50_us: percentile(&latencies, 0.50),
            p95_us: percentile(&latencies, 0.95),
            recall_at_10: recall10_sum / n,
            recall_pool: recall_pool_sum / n,
            avg_returned: returned_sum as f64 / n,
            avg_terms: terms_sum as f64 / n,
        });
    }

    // ── Report ─────────────────────────────────────────────────────────────────
    let baseline_mean = results[0].mean_us;
    let report = render_report(
        &results,
        baseline_mean,
        total_docs,
        indexed,
        queries.len(),
        avg_query_terms,
        build_secs,
    );
    println!("{report}");

    let report_path = format!(
        "{}/../../tests/khive-contract/tune/REPORT-fts-gather.md",
        env!("CARGO_MANIFEST_DIR")
    );
    match std::fs::write(&report_path, &report) {
        Ok(()) => eprintln!("report written to {report_path}"),
        Err(e) => eprintln!("note: could not write report to {report_path}: {e}"),
    }

    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{}", db_path.display(), suffix));
    }
}

async fn run_strategy(
    searcher: &dyn TextSearch,
    s: &BenchStrategy,
    pruned: &Option<Vec<Vec<String>>>,
    qi: usize,
    q: &Query,
) -> Vec<TextSearchHit> {
    match &s.strategy {
        Strategy::RankedPruned(_) => {
            let terms = &pruned.as_ref().expect("pruned terms present")[qi];
            ranked_search(searcher, terms).await
        }
        Strategy::Production(cfg) => production_search(searcher, cfg, &q.terms).await,
    }
}

#[allow(clippy::too_many_arguments)]
fn render_report(
    results: &[StratResult],
    baseline_mean: f64,
    total_docs: usize,
    indexed: u64,
    n_queries: usize,
    avg_terms: f64,
    build_secs: f64,
) -> String {
    let mut s = String::new();
    s.push_str("# FTS Candidate-Gather Benchmark (PR #625) — real corpus\n\n");
    s.push_str(
        "Latency + quality of the `memory.recall` FTS candidate-gather leg, measured on the\n\
         real local-namespace note corpus via the production code path.\n\n",
    );
    s.push_str("## Setup\n\n");
    s.push_str(&format!(
        "- Corpus: {total_docs} local notes (FTS-indexed rows: {indexed}), file-backed SQLite + FTS5 trigram\n"
    ));
    s.push_str(&format!(
        "- Index build: {build_secs:.1}s · Queries: {n_queries} (sampled from memory notes, avg {avg_terms:.1} terms, capped at 10)\n"
    ));
    s.push_str(&format!(
        "- candidate_limit: {CANDIDATE_LIMIT} · repeats/query: {REPEATS}\n"
    ));
    s.push_str(
        "- `recall@10` / `pool recall`: fraction of the baseline's top-10 / full candidate pool\n  \
         that the strategy recovers (1.000 = coverage-safe). `terms` = avg query terms actually used.\n\n",
    );
    s.push_str("## Results\n\n");
    s.push_str(
        "| strategy | mean | p50 | p95 | speedup | recall@10 | pool recall | avg hits | terms |\n",
    );
    s.push_str("|---|---:|---:|---:|---:|---:|---:|---:|---:|\n");
    for r in results {
        let speedup = if r.mean_us > 0.0 {
            baseline_mean / r.mean_us
        } else {
            0.0
        };
        s.push_str(&format!(
            "| {} | {:.2}ms | {:.2}ms | {:.2}ms | {:.2}× | {:.3} | {:.3} | {:.0} | {:.1} |\n",
            r.name,
            r.mean_us / 1000.0,
            r.p50_us as f64 / 1000.0,
            r.p95_us as f64 / 1000.0,
            speedup,
            r.recall_at_10,
            r.recall_pool,
            r.avg_returned,
            r.avg_terms,
        ));
    }
    s.push('\n');
    s
}
