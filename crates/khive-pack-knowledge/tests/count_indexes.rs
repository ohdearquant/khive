//! File-backed COUNT timings and retained-reader plans. Run with:
//! cargo test -p khive-pack-knowledge --test count_indexes -- --ignored --nocapture

use std::time::Instant;

use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{KhiveRuntime, RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use khive_storage::{SqlStatement, SqlValue};
use serde_json::{json, Value};

const EVENT_COUNT: &str =
    "SELECT COUNT(*) FROM events WHERE namespace = ?1 AND verb LIKE 'knowledge.%'";
const ATOM_COUNT: &str = "SELECT COUNT(*) FROM knowledge_atoms WHERE namespace = ?1 AND deleted_at IS NULL AND tags NOT LIKE '%type:domain%'";
const LIST_COUNT: &str = "SELECT COUNT(*) FROM knowledge_atoms WHERE namespace = ?1 AND deleted_at IS NULL AND tags NOT LIKE '%type:domain%' AND (status IS NULL OR status != 'deprecated')";

const SEED_EVENTS: &str =
    "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<?1) \
     INSERT INTO events (id, namespace, verb, substrate, actor, outcome, payload, created_at) \
     SELECT printf('10000000-0000-4000-8000-%012x', x), CASE x%4 WHEN 0 THEN 'local' ELSE printf('ns-%d', x%4) END, \
     CASE x%3 WHEN 0 THEN 'knowledge.learn' WHEN 1 THEN 'Knowledge.list' ELSE 'comm.inbox' END, \
     'entity', 'fixture', 'ok', json_object('padding', printf('%0256d', x)), x FROM n";
const SEED_ATOMS: &str =
    "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<?1) \
     INSERT INTO knowledge_atoms (id, namespace, slug, name, content, tags, finalized, status, deleted_at, created_at, updated_at) \
     SELECT printf('00000000-0000-4000-8000-%012x', x), CASE x%4 WHEN 0 THEN 'other' ELSE 'local' END, \
     printf('atom-%d',x), printf('Atom %d',x), printf('%0512d',x), \
     CASE WHEN x%11=0 THEN '[\"type:domain\"]' ELSE '[]' END, x%2, \
     CASE x%3 WHEN 0 THEN 'draft' WHEN 1 THEN 'reviewed' ELSE 'deprecated' END, \
     CASE WHEN x%13=0 THEN 1 ELSE NULL END, x, x FROM n";

fn fixture(path: &std::path::Path) -> (KhiveRuntime, VerbRegistry) {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(path.into()),
        embedding_model: None,
        additional_embedding_models: vec![],
        events_split: None,
        actor_id: None,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(KnowledgePack::new(runtime.clone()));
    (runtime, builder.build().unwrap())
}

async fn seed(runtime: &KhiveRuntime, events: i64, atoms: i64) {
    for (sql, size) in [(SEED_EVENTS, events), (SEED_ATOMS, atoms)] {
        runtime
            .sql()
            .writer()
            .await
            .unwrap()
            .execute(SqlStatement {
                sql: sql.into(),
                params: vec![SqlValue::Integer(size)],
                label: None,
            })
            .await
            .unwrap();
    }
}

fn set_indexes(runtime: &KhiveRuntime, indexed: bool) {
    let writer = runtime.backend().pool().writer().unwrap();
    writer
        .conn()
        .execute_batch(if indexed {
            include_str!("../../khive-db/sql/032-knowledge-count-indexes.sql")
        } else {
            "DROP INDEX IF EXISTS idx_events_ns_verb; \
             DROP INDEX IF EXISTS idx_knowledge_atoms_ns_live_counts;"
        })
        .unwrap();
}

fn measure_counts(reader: &khive_db::ReaderGuard<'_>) -> Vec<Value> {
    [EVENT_COUNT, ATOM_COUNT, LIST_COUNT]
        .into_iter()
        .map(|sql| {
            // A real query checks SQLite's schema cookie. EXPLAIN alone can
            // retain a stale schema after DDL on another connection, and the
            // SqlReader abstraction acquires a different lease per operation.
            // Keep COUNT and its plan on this one physical reader.
            let started = Instant::now();
            let count: i64 = reader.query_row(sql, ["local"], |row| row.get(0)).unwrap();
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            let plan: String = reader
                .query_row(&format!("EXPLAIN QUERY PLAN {sql}"), ["local"], |row| {
                    row.get(3)
                })
                .unwrap();
            json!({"sql": sql, "count": count, "ms": ms, "plan": plan})
        })
        .collect()
}

async fn measure(runtime: &KhiveRuntime, registry: &VerbRegistry) -> Value {
    let (counts, sqlite, installed_indexes) = {
        let reader = runtime.backend().pool().reader().unwrap();
        let counts = measure_counts(&reader);
        let sqlite: String = reader
            .query_row("SELECT sqlite_version()", [], |row| row.get(0))
            .unwrap();
        let installed_indexes: i64 = reader
            .query_row(
                "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'index' AND name IN \
                 ('idx_events_ns_verb', 'idx_knowledge_atoms_ns_live_counts')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        (counts, sqlite, installed_indexes)
    };
    // Public dispatch remains on its normal pooled SQL path; no raw reader
    // lease survives across this await. Public list dispatch is not timed.
    let listed = registry
        .dispatch("knowledge.list", json!({"limit": 1}))
        .await
        .unwrap();
    json!({"counts": counts, "sqlite": sqlite, "installed_count_indexes": installed_indexes,
        "list_total": listed["total"], "list_rows": listed["results"].as_array().unwrap().len(),
        "first_list_id": listed["results"][0]["id"]})
}

fn assert_plans(counts: &[Value], indexed: bool) {
    assert_eq!(counts.len(), 3);
    for (sample, expected) in counts.iter().zip([
        "idx_events_ns_verb",
        "idx_knowledge_atoms_ns_live_counts",
        "idx_knowledge_atoms_ns_live_counts",
    ]) {
        let plan = sample["plan"].as_str().unwrap();
        if indexed {
            assert!(
                plan.contains(&format!("COVERING INDEX {expected}")),
                "{sample}"
            );
            if expected == "idx_events_ns_verb" {
                assert!(
                    plan.contains("verb>?") && plan.contains("verb<?"),
                    "{sample}"
                );
            }
        } else {
            assert!(!plan.contains(expected), "stale indexed plan: {sample}");
        }
    }
}

fn assert_sample(sample: &Value, expected: [i64; 3], first_id: &str) {
    for (index, count) in expected.into_iter().enumerate() {
        assert_eq!(sample["counts"][index]["count"], json!(count), "{sample}");
    }
    assert_eq!(sample["list_total"], json!(expected[2]), "{sample}");
    assert_eq!(sample["list_rows"], json!(1), "{sample}");
    assert_eq!(sample["first_list_id"], first_id, "{sample}");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn count_fixture_reaches_public_list_before_and_after_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let (runtime, registry) = fixture(&dir.path().join("counts.db"));
    seed(&runtime, 120, 156).await;
    for indexed in [false, true] {
        set_indexes(&runtime, indexed);
        let sample = measure(&runtime, &registry).await;
        assert_sample(
            &sample,
            [20, 98, 65],
            "00000000-0000-4000-8000-000000000099",
        );
        assert_plans(sample["counts"].as_array().unwrap(), indexed);
        assert_eq!(
            sample["installed_count_indexes"],
            json!(if indexed { 2 } else { 0 })
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn retained_reader_count_plans_follow_index_changes() {
    let dir = tempfile::tempdir().unwrap();
    let (runtime, _registry) = fixture(&dir.path().join("counts.db"));
    seed(&runtime, 120, 156).await;
    // Hold the same physical connection while a separate writer changes DDL.
    // Repeated adds and drops exercise stale schemas in both directions.
    let reader = runtime.backend().pool().reader().unwrap();
    for indexed in [false, true, false, true] {
        set_indexes(&runtime, indexed);
        let counts = measure_counts(&reader);
        for (sample, expected) in counts.iter().zip([20, 98, 65]) {
            assert_eq!(sample["count"], json!(expected));
        }
        assert_plans(&counts, indexed);
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
#[ignore = "constructs two million events and 154600 atoms for paired count measurements"]
async fn benchmark_count_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let (runtime, registry) = fixture(&dir.path().join("counts.db"));
    seed(&runtime, 2_000_000, 154_600).await;
    let mut pairs = Vec::new();
    for round in 0..4 {
        let mut pair = serde_json::Map::new();
        // Alternate arm order across the four rounds; caches are not reset.
        for indexed in if round % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
            set_indexes(&runtime, indexed);
            pair.insert(
                if indexed { "after" } else { "before" }.into(),
                measure(&runtime, &registry).await,
            );
        }
        eprintln!("{}", json!({"round": round, "pair": pair}));
        for arm in ["before", "after"] {
            assert_sample(
                &pair[arm],
                [333_333, 97_301, 64_867],
                "00000000-0000-4000-8000-000000025be7",
            );
        }
        assert_plans(pair["before"]["counts"].as_array().unwrap(), false);
        assert_plans(pair["after"]["counts"].as_array().unwrap(), true);
        assert_eq!(pair["before"]["installed_count_indexes"], json!(0));
        assert_eq!(pair["after"]["installed_count_indexes"], json!(2));
        pairs.push(pair);
    }
    let count_medians_ms: Vec<_> = [EVENT_COUNT, ATOM_COUNT, LIST_COUNT]
        .into_iter()
        .enumerate()
        .map(|(index, sql)| {
            let median = |arm: &str| {
                let mut times: Vec<f64> = pairs
                    .iter()
                    .map(|pair| pair[arm]["counts"][index]["ms"].as_f64().unwrap())
                    .collect();
                times.sort_by(f64::total_cmp);
                // Each arm has four rounds: average the two middle samples.
                (times[1] + times[2]) / 2.0
            };
            json!({"sql": sql, "before": median("before"), "after": median("after")})
        })
        .collect();
    eprintln!(
        "{}",
        json!({"events": 2000000, "atoms":154600,
        "storage":"file-backed", "measurement":"COUNT-only wall-clock ms on retained reader; EXPLAIN and public list dispatch excluded",
        "timing_conditions":"four rounds per arm; alternating order; caches not reset",
        "count_medians_ms":count_medians_ms, "pairs":pairs})
    );
}
