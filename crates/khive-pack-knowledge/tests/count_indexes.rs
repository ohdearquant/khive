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
    for sql in [EVENT_COUNT, ATOM_COUNT] {
        let statement = SqlStatement {
            sql: sql.into(),
            params: vec![SqlValue::Text("local".into())],
            label: None,
        };
        let started = Instant::now();
        let value = reader.query_scalar(statement.clone()).await.unwrap();
        let elapsed = started.elapsed();
        let plan = reader.explain(statement).await.unwrap();
        counts.push(json!({
            "sql": sql, "count": format!("{value:?}"),
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
        "list_total": listed["total"], "list_rows": listed["results"].as_array().unwrap().len()})
}

#[tokio::test]
#[ignore = "constructs two million events and 154600 atoms for paired count measurements"]
async fn benchmark_count_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(dir.path().join("counts.db")),
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
    let registry = builder.build().unwrap();
    execute(&runtime,
        "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<2000000) \
         INSERT INTO events (id, namespace, verb, substrate, actor, outcome, payload, created_at) \
         SELECT printf('fixture-event-%d', x), CASE x%4 WHEN 0 THEN 'local' ELSE printf('ns-%d', x%4) END, \
         CASE x%3 WHEN 0 THEN 'knowledge.learn' WHEN 1 THEN 'Knowledge.list' ELSE 'comm.inbox' END, \
         'entity', 'fixture', 'ok', json_object('padding', printf('%0256d', x)), x FROM n"
    ).await;
    execute(&runtime,
        "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<154600) \
         INSERT INTO knowledge_atoms (id, namespace, slug, name, content, tags, finalized, status, deleted_at, created_at, updated_at) \
         SELECT printf('fixture-atom-%d', x), CASE x%4 WHEN 0 THEN 'other' ELSE 'local' END, \
         printf('atom-%d',x), printf('Atom %d',x), printf('%0512d',x), \
         CASE WHEN x%11=0 THEN '[\"type:domain\"]' ELSE '[]' END, x%2, \
         CASE x%3 WHEN 0 THEN 'draft' WHEN 1 THEN 'reviewed' ELSE 'finalized' END, \
         CASE WHEN x%13=0 THEN 1 ELSE NULL END, x, x FROM n"
    ).await;
    let mut pairs = Vec::new();
    for round in 0..4 {
        let mut pair = serde_json::Map::new();
        // Adjacent paired arms reverse order to expose cache/order sensitivity.
        for indexed in if round % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
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
                execute(&runtime, "DROP INDEX IF EXISTS idx_events_ns_verb").await;
                execute(
                    &runtime,
                    "DROP INDEX IF EXISTS idx_knowledge_atoms_ns_live_counts",
                )
                .await;
            }
            pair.insert(
                if indexed { "after" } else { "before" }.into(),
                measure(&runtime, &registry).await,
            );
        }
        assert_eq!(pair["before"]["list_total"], pair["after"]["list_total"]);
        assert_eq!(pair["before"]["list_rows"], json!(1));
        for index in 0..2 {
            assert_eq!(
                pair["before"]["counts"][index]["count"],
                pair["after"]["counts"][index]["count"]
            );
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
