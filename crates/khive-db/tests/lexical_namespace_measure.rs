//! Manual FTS5 namespace-growth measurements; rows visited are not exposed.

#[path = "../../test_support/retrieval_measure.rs"]
mod measure;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use khive_db::StorageBackend;
use khive_storage::usage::{self, UsageContext};
use khive_storage::{
    SqlStatement, SqlValue, SubstrateKind, TextFilter, TextQueryMode, TextSearch, TextSearchRequest,
};
use serde_json::{json, Value};
use uuid::Uuid;

fn substrate(table: &str) -> SubstrateKind {
    match table {
        "entities" => SubstrateKind::Entity,
        "notes" => SubstrateKind::Note,
        _ => panic!("measurement corpus must be entities or notes"),
    }
}

async fn seed(index: &Arc<dyn TextSearch>, table: &str, foreign: usize, local_matches: usize) {
    let kind = substrate(table);
    let granular = if table == "entities" {
        "concept"
    } else {
        "observation"
    };
    // Foreign postings must precede the identical local set in every cell.
    measure::seed_documents(
        index,
        (0..foreign).map(|ordinal| {
            measure::document(
                Uuid::from_u128(1_000_000 + ordinal as u128),
                "foreign",
                kind,
                granular,
                "Namespace measurement row".into(),
                "namespacechannel absentchannel ordinary padding".into(),
            )
        }),
    )
    .await;
    measure::seed_documents(
        index,
        (0..10).map(|ordinal| {
            measure::document(
                Uuid::from_u128(1 + ordinal as u128),
                "local",
                kind,
                granular,
                "Namespace measurement row".into(),
                if ordinal < local_matches {
                    "namespacechannel absentchannel ordinary padding"
                } else {
                    "unrelated ordinary padding"
                }
                .into(),
            )
        }),
    )
    .await;
}

fn request(query: &str) -> TextSearchRequest {
    TextSearchRequest {
        query: query.into(),
        mode: TextQueryMode::Plain,
        filter: Some(TextFilter {
            namespaces: vec!["local".into()],
            ..TextFilter::default()
        }),
        top_k: 10,
        snippet_chars: 0,
    }
}

fn reference_query(table_key: &str, query: &str, explain: bool) -> SqlStatement {
    assert!(matches!(table_key, "entities" | "notes"));
    let table = format!("fts_{table_key}");
    let namespace_predicate = format!(" AND {table}.namespace = ?3");
    let terms = query
        .split_whitespace()
        .map(|term| {
            assert!(term.chars().all(|character| character.is_ascii_lowercase()));
            format!("\"{term}\"")
        })
        .collect::<Vec<_>>()
        .join(" AND ");
    SqlStatement {
        sql: format!(
            "{}SELECT subject_id, rank, title, NULL AS snippet FROM {table} \
             WHERE {table} MATCH ?1 \
             AND rank MATCH 'bm25(0.0, 0.0, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0)'\
             {namespace_predicate} ORDER BY rank LIMIT ?2",
            if explain { "EXPLAIN QUERY PLAN " } else { "" },
        ),
        params: vec![
            SqlValue::Text(format!("{{title body}} : ({terms})")),
            SqlValue::Integer(10),
            SqlValue::Text("local".into()),
        ],
        label: Some("measurement-reference-namespace-query".into()),
    }
}

async fn sample(
    backend: &StorageBackend,
    table: &str,
    query: &str,
    foreign: usize,
    local_matches: usize,
) -> Value {
    let index = backend.text(table).unwrap();
    let usage = UsageContext::new();
    let start = Instant::now();
    let hits = usage::scope(usage.clone(), index.search(request(query)))
        .await
        .unwrap();
    let elapsed_ns = start.elapsed().as_nanos();
    assert_eq!(
        hits.len(),
        local_matches,
        "actual store must return exactly the matching local documents"
    );
    assert!(hits.iter().all(|hit| hit.subject_id.as_u128() <= 10));
    let issued = usage.snapshot();
    assert_eq!(
        issued["fts_passes"], 1,
        "the actual store must prepare a real FTS statement"
    );
    let access = backend.sql();
    let mut reader = access.reader().await.unwrap();
    let reference = reader
        .query_all(reference_query(table, query, false))
        .await
        .unwrap();
    assert_eq!(
        reference.len(),
        local_matches,
        "reference namespace query must not admit foreign postings"
    );
    let plan = reader
        .query_all(reference_query(table, query, true))
        .await
        .unwrap();
    assert!(!plan.is_empty(), "reference EXPLAIN must produce a plan");
    json!({"table":table, "query":query, "foreign_rows":foreign, "local_documents":10,
        "local_matches":local_matches, "returned_rows":hits.len(), "returned_ids":hits.iter().map(|hit| hit.subject_id).collect::<Vec<_>>(),
        "latency_ns":elapsed_ns.to_string(), "fts_passes":issued["fts_passes"],
        "rows_visited":null, "rows_visited_status":"UNMEASURED: actual store Statement is private; FtsPasses counts statements, not rows",
        "reference_rows":reference.len(), "reference_explain_query_plan":plan,
        "reference_plan_qualification":"separate fixture SQL mirrors known ASCII Plain query/rank/no-snippet/namespace shape; not the store-owned Statement; outside timed interval",
        "performance_verdict":"exploratory; growth requires same-state samples, no latency threshold"})
}

async fn shutdown(backend: StorageBackend) {
    let join = backend.pool().take_writer_task_join();
    drop(backend);
    if let Some(join) = join {
        join.await.unwrap();
    }
}

#[tokio::test]
async fn lexical_namespace_reference_and_actual_store_are_non_vacuous() {
    for table in ["entities", "notes"] {
        for (foreign, local_matches) in [(0, 3), (64, 0), (64, 3)] {
            let dir = tempfile::tempdir().unwrap();
            let backend = StorageBackend::sqlite_for_test(dir.path().join("namespace.db")).unwrap();
            {
                let index = backend.text(table).unwrap();
                seed(&index, table, foreign, local_matches).await;
            }
            for query in ["namespacechannel", "namespacechannel absentchannel"] {
                let row = sample(&backend, table, query, foreign, local_matches).await;
                if foreign == 0 {
                    assert!(row["returned_rows"].as_u64().unwrap() > 0);
                    assert!(row["reference_rows"].as_u64().unwrap() > 0);
                }
                println!("LEXICAL_NAMESPACE_SANITY {row}");
            }
            shutdown(backend).await;
        }
    }
}

#[tokio::test]
#[ignore = "manual measurement: entities and notes with up to 200,000 foreign postings; run on an otherwise idle host"]
async fn measure_entity_and_note_lexical_namespace_growth() {
    let mut output = measure::RawRows::new("lexical-namespace");
    let mut zero_foreign = HashMap::new();
    for table in ["entities", "notes"] {
        for foreign in [0, 1000, 10_000, 100_000, 200_000] {
            for local_matches in [0, 3] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("namespace.db");
                let backend = StorageBackend::sqlite_for_test(&path).unwrap();
                {
                    let index = backend.text(table).unwrap();
                    seed(&index, table, foreign, local_matches).await;
                }
                shutdown(backend).await;
                for query in ["namespacechannel", "namespacechannel absentchannel"] {
                    for first_after_reopen in [true, false] {
                        let mut latencies = Vec::new();
                        for index in 0..5 {
                            let backend = StorageBackend::sqlite_for_test(&path).unwrap();
                            if !first_after_reopen {
                                let store = backend.text(table).unwrap();
                                for _ in 0..5 {
                                    store.search(request(query)).await.unwrap();
                                }
                            }
                            let mut row =
                                sample(&backend, table, query, foreign, local_matches).await;
                            row["cache"] = measure::cache_state(first_after_reopen);
                            row["sample"] = json!(index);
                            latencies
                                .push(row["latency_ns"].as_str().unwrap().parse::<u128>().unwrap());
                            println!("LEXICAL_NAMESPACE {row}");
                            output.push(&row);
                            shutdown(backend).await;
                        }
                        latencies.sort_unstable();
                        let median = latencies[latencies.len() / 2];
                        let key = (table, query, local_matches, first_after_reopen);
                        if foreign == 0 {
                            zero_foreign.insert(key, median);
                        }
                        let baseline = zero_foreign[&key];
                        let summary = json!({"summary":"namespace_growth_cell", "table":table, "query":query,
                            "foreign_rows":foreign, "local_matches":local_matches,
                            "cache":measure::cache_state(first_after_reopen), "samples":latencies.len(),
                            "median_latency_ns":median.to_string(), "zero_foreign_median_ns":baseline.to_string(),
                            "median_ratio_to_zero_foreign":if baseline > 0 { Some(median as f64 / baseline as f64) } else { None },
                            "observed_median_increase":foreign > 0 && median > baseline,
                            "growth_verdict":if foreign == 0 { "zero-foreign baseline" } else if median > baseline { "higher observed cell median than zero-foreign" } else { "no higher observed cell median than zero-foreign" },
                            "growth_qualification":"descriptive only: five samples, uncontrolled OS cache, no calibrated or statistical conclusion",
                            "rows_visited_status":"UNMEASURED"});
                        println!("LEXICAL_NAMESPACE_GROWTH {summary}");
                        output.push(&summary);
                    }
                }
            }
        }
    }
    output.finish();
}
