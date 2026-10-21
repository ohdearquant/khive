//! Criterion recall over 256 generated notes and fixed synthetic-v1 embeddings.
//! Setup, parameter cloning, response checks, and writer teardown are untimed.
//! These are synthetic retrieval costs, not real-model or historical baselines.
#[allow(dead_code)]
#[path = "reducible_support/fixture.rs"]
mod fixture;
#[allow(dead_code)]
#[path = "reducible_support/report.rs"]
mod report;

use criterion::Criterion;
use khive_pack_memory::config::{
    RecallConfig, RecallFtsGatherConfig, RecallFtsGatherMode, RecallFtsSelectionRule,
};
use lattice_embed::EmbeddingModel;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const IDS: [&str; 10] = [
    "e2e_recall/synthetic-v1/fts-baseline",
    "e2e_recall/synthetic-v1/fts-ranked",
    "e2e_recall/synthetic-v1/fts-lowest-df-k5",
    "e2e_recall/synthetic-v1/fts-highest-idf-k5",
    "e2e_recall/synthetic-v1/fts-rank-subset-m4",
    "e2e_recall/synthetic-v1/fts-unranked",
    "e2e_recall/synthetic-v1/fusion-weighted",
    "e2e_recall/synthetic-v1/fusion-vector-only",
    "e2e_recall/synthetic-v1/fusion-keyword-only",
    "e2e_recall/synthetic-v1/fusion-rrf",
];

fn params(case: usize, query: usize) -> Value {
    use RecallFtsGatherMode::{RankWithinCap, Ranked, Unranked};
    use RecallFtsSelectionRule::{HighestIdf, LowestDf, Original};
    let (enabled, selection_rule, term_k, gather_mode) = match case {
        1 => (true, Original, 10, Ranked),
        2 => (true, LowestDf, 5, Ranked),
        3 => (true, HighestIdf, 5, Ranked),
        4 => (true, Original, 10, RankWithinCap),
        5 => (true, Original, 10, Unranked),
        _ => (false, Original, 10, Ranked),
    };
    let config = RecallConfig {
        fts_gather: RecallFtsGatherConfig {
            enabled,
            selection_rule,
            term_k,
            gather_mode,
            gather_cap_multiplier: 4,
            ..RecallFtsGatherConfig::default()
        },
        ..RecallConfig::default()
    };
    let mut params = json!({"query": fixture::query(query), "limit":10, "full_content":false,
        "embedding_model":EmbeddingModel::AllMiniLmL6V2.to_string(), "config":config});
    if case >= 6 {
        params["fusion_strategy"] =
            json!(["weighted", "vector_only", "keyword_only", "rrf"][case - 6]);
    }
    params
}

fn main() {
    let synthetic = fixture::synthetic_cohort();
    let ids: Vec<_> = IDS
        .iter()
        .map(|id| {
            if synthetic {
                (*id).to_owned()
            } else {
                id.replace("synthetic-v1", "real-model")
            }
        })
        .collect();
    if report::list_requested(&ids.iter().map(String::as_str).collect::<Vec<_>>()) {
        return;
    }
    report::assert_not_list_probe();
    fixture::reject_fts_overrides();
    if synthetic {
        // SAFETY: main has not created Criterion, an executor, or any threads.
        unsafe { fixture::configure_synthetic_ann_debounce() };
    }
    let mut criterion = Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_millis(100))
        .measurement_time(Duration::from_millis(300))
        .configure_from_args();
    let executor = fixture::executor();
    let mut database: Option<fixture::Fixture> = None;
    for (case, id) in ids.iter().enumerate() {
        criterion.bench_function(id, |bencher| {
            let database = database.get_or_insert_with(|| {
                executor.block_on(async {
                    let database =
                        fixture::Fixture::new(&[EmbeddingModel::AllMiniLmL6V2], synthetic);
                    database.seed(EmbeddingModel::AllMiniLmL6V2, 256).await;
                    database
                })
            });
            executor.block_on(async {
                let mut warm = false;
                for _ in 0..200 {
                    let response = database
                        .registry
                        .dispatch("memory.recall", params(case, 0))
                        .await
                        .expect("warm recall");
                    if response.as_array().is_some_and(|rows| {
                        !rows.is_empty() && rows.iter().all(|r| r.get("degraded").is_none())
                    }) {
                        fixture::check_recall(&response);
                        warm = true;
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                assert!(warm, "refuse a degraded synthetic recall benchmark");
            });
            let queries: Vec<_> = (0..32).map(|q| params(case, q)).collect();
            let mut next = 0;
            bencher.iter_custom(|iterations| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..iterations {
                    let params = queries[next % queries.len()].clone();
                    next += 1;
                    let start = Instant::now();
                    let response = executor
                        .block_on(database.registry.dispatch("memory.recall", params))
                        .expect("recall");
                    elapsed += start.elapsed();
                    fixture::check_recall(&response);
                }
                elapsed
            });
        });
    }
    if let Some(database) = database {
        executor.block_on(database.close());
    }
    criterion.final_summary();
}
