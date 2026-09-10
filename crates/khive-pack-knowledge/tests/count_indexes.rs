//! File-backed count benchmark. Run explicitly with:
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

async fn set_indexes(runtime: &KhiveRuntime, indexed: bool) {
    if indexed {
        runtime
            .backend()
            .pool()
            .writer()
            .unwrap()
            .conn()
            .execute_batch(include_str!(
                "../../khive-db/sql/032-knowledge-count-indexes.sql"
            ))
            .unwrap();
    } else {
        execute(runtime, "DROP INDEX IF EXISTS idx_events_ns_verb").await;
        execute(
            runtime,
            "DROP INDEX IF EXISTS idx_knowledge_atoms_ns_live_counts",
        )
        .await;
    }
}

async fn execute(runtime: &KhiveRuntime, sql: &str) {
    runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute(SqlStatement {
            sql: sql.into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap();
}

async fn measure(runtime: &KhiveRuntime, registry: &VerbRegistry) -> Value {
    let mut reader = runtime.sql().reader().await.unwrap();
    let mut counts = Vec::new();
    for sql in [EVENT_COUNT, ATOM_COUNT, LIST_COUNT] {
        let statement = SqlStatement {
            sql: sql.into(),
            params: vec![SqlValue::Text("local".into())],
            label: None,
        };
        let started = Instant::now();
        let value = reader.query_scalar(statement.clone()).await.unwrap();
        let elapsed = started.elapsed();
        let Some(SqlValue::Integer(count)) = value else {
            panic!("COUNT did not return an integer: {value:?}");
        };
        let plan = reader.explain(statement).await.unwrap();
        counts.push(json!({
            "sql": sql, "count": count,
            "ms": elapsed.as_secs_f64() * 1000.0, "plan": format!("{plan:?}")
        }));
    }
    drop(reader);
    let started = Instant::now();
    let listed = registry
        .dispatch("knowledge.list", json!({"limit": 1}))
        .await
        .unwrap();
    json!({"counts": counts, "list_ms": started.elapsed().as_secs_f64() * 1000.0,
        "list_total": listed["total"], "list_rows": listed["results"].as_array().unwrap().len(),
        "first_list_id": listed["results"][0]["id"]})
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
        set_indexes(&runtime, indexed).await;
        assert_sample(
            &measure(&runtime, &registry).await,
            [20, 98, 65],
            "00000000-0000-4000-8000-000000000099",
        );
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
        // Adjacent paired arms reverse order to expose cache/order sensitivity.
        for indexed in if round % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
            set_indexes(&runtime, indexed).await;
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
        for index in 0..3 {
            assert!(pair["after"]["counts"][index]["plan"]
                .as_str()
                .unwrap()
                .contains("COVERING INDEX"));
        }
        pairs.push(pair);
    }
    eprintln!(
        "{}",
        json!({"events": 2000000, "atoms":154600, "sqlite": "bundled",
        "storage":"file-backed", "cache_control":"warm/order-reversed; no OS cache eviction", "pairs":pairs})
    );
}
