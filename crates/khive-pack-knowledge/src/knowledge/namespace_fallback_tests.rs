use super::*;
use crate::KnowledgePack;
use khive_pack_kg::KgPack;
use khive_runtime::{Namespace, RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use rusqlite::{
    params, params_from_iter, types::Value as NativeValue, Connection, StatementStatus,
};
use std::collections::BTreeSet;

const QUERY: &str = "zznamespaceguard zzsecondguard";
const DRAFTS: usize = 20;
const LEGACY_FALLBACK: &str = "SELECT a.* FROM fts_knowledge \
    CROSS JOIN knowledge_atoms AS a ON a.rowid = fts_knowledge.rowid \
    WHERE fts_knowledge MATCH ?1 AND +a.namespace = ?2 AND a.deleted_at IS NULL \
    AND (status IS NULL OR (status != ?4 AND status != ?5)) \
    AND NOT khive_tag_contains(a.tags, 'type:domain') \
    ORDER BY fts_knowledge.rowid LIMIT ?3";
const LEGACY_EXISTENCE: &str = "SELECT 1 AS present FROM fts_knowledge \
    CROSS JOIN knowledge_atoms AS a ON a.rowid = fts_knowledge.rowid \
    WHERE fts_knowledge MATCH ?1 AND +a.namespace = ?2 LIMIT 1";

struct Fixture {
    runtime: KhiveRuntime,
    registry: VerbRegistry,
    _directory: tempfile::TempDir,
}

fn local_id(index: usize) -> String {
    format!("35140000-0000-4000-9000-{index:012}")
}

fn fixture(foreign: usize, reviewed: usize) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("namespace-fallback.db");
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(path.clone()),
        default_namespace: Namespace::parse("a").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        events_split: None,
        actor_id: None,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.with_default_namespace("a");
    builder.register(KgPack::new(runtime.clone()));
    builder.register(KnowledgePack::new(runtime.clone()));
    let registry = builder.build().unwrap();
    registry.apply_schema_plans(runtime.backend());
    runtime.install_edge_rules(registry.all_edge_rules());
    let mut connection = Connection::open(&path).unwrap();
    let transaction = connection.transaction().unwrap();
    // Foreign matches precede every local row in the ordinary, unchanged FTS index.
    for index in 0..foreign {
        transaction
            .execute(
                "INSERT INTO knowledge_atoms \
             (id,namespace,slug,name,content,tags,finalized,status,created_at,updated_at) \
             VALUES (?1,'b',?2,'Foreign Match',?3,'[]',1,'reviewed',1,1)",
                params![
                    format!("35140000-0000-4000-8000-{index:012}"),
                    format!("foreign-{index}"),
                    QUERY
                ],
            )
            .unwrap();
    }
    for index in 0..DRAFTS + reviewed + 3 {
        let (status, tags, deleted) = if index < DRAFTS {
            ("draft", "[]", None)
        } else if index < DRAFTS + reviewed {
            ("reviewed", "[]", None)
        } else {
            match index - DRAFTS - reviewed {
                0 => ("reviewed", "[\"type:domain\"]", None),
                1 => ("reviewed", "[]", Some(1_i64)),
                _ => ("deprecated", "[]", None),
            }
        };
        transaction.execute(
            "INSERT INTO knowledge_atoms \
             (id,namespace,slug,name,content,tags,finalized,status,created_at,updated_at,deleted_at) \
             VALUES (?1,'a',?2,?3,?4,?5,1,?6,1,1,?7)",
            params![local_id(index), format!("local-{index}"), format!("Local Match {index}"), QUERY, tags, status, deleted],
        ).unwrap();
    }
    transaction.commit().unwrap();
    drop(connection);
    Fixture {
        runtime,
        registry,
        _directory: directory,
    }
}

impl Fixture {
    async fn search(&self) -> (Value, Vec<SqlStatement>) {
        let probes = Arc::new(Mutex::new(Vec::new()));
        let response = TERM_PROBES.scope(
            probes.clone(),
            super::super::lexical::with_lexical_stage_budget_override_ms(
                120_000,
                super::super::lexical::with_phase_a_widen_ceiling_override(
                    DRAFTS,
                    self.registry.dispatch("knowledge.search", json!({
                        "namespace":"a", "query":QUERY, "kind":"atom", "rerank":false, "limit":10
                    })),
                ),
            ),
        ).await.unwrap();
        assert!(response.get("degraded").is_none(), "{response}");
        let statements = probes.lock().unwrap().clone();
        (response, statements)
    }

    fn measure(&self, statement: &SqlStatement) -> (Vec<Vec<NativeValue>>, u64, Vec<String>) {
        self.runtime
            .backend()
            .pool_arc()
            .writer()
            .unwrap()
            .autocommit(|connection| {
                let bindings: Vec<NativeValue> = statement
                    .params
                    .iter()
                    .map(|value| match value {
                        SqlValue::Text(value) => NativeValue::Text(value.clone()),
                        SqlValue::Integer(value) => NativeValue::Integer(*value),
                        _ => panic!("unexpected lexical bind: {value:?}"),
                    })
                    .collect();
                let mut query = connection.prepare(&statement.sql).unwrap();
                let columns = query.column_count();
                let rows = query
                    .query_map(params_from_iter(bindings.iter()), |row| {
                        (0..columns)
                            .map(|column| row.get::<_, NativeValue>(column))
                            .collect::<rusqlite::Result<Vec<_>>>()
                    })
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap();
                let steps = query.get_status(StatementStatus::VmStep) as u64;
                let mut explain = connection
                    .prepare(&format!("EXPLAIN QUERY PLAN {}", statement.sql))
                    .unwrap();
                let plan = explain
                    .query_map(params_from_iter(bindings.iter()), |row| {
                        row.get::<_, String>(3)
                    })
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<String>>>()
                    .unwrap();
                Ok((rows, steps, plan))
            })
            .unwrap()
    }
}

fn assert_namespace_outer(plan: &[String]) {
    let namespace = plan
        .iter()
        .position(|detail| detail.contains("idx_knowledge_atoms_ns"))
        .expect("namespace index must drive the outer loop");
    let fts = plan
        .iter()
        .position(|detail| detail.contains("fts_knowledge VIRTUAL TABLE"))
        .expect("FTS membership probe must remain present");
    assert!(namespace < fts, "namespace must precede FTS: {plan:?}");
}

fn assert_local_work_growth(small: u64, large: u64, local_rows: usize) {
    // This bounds VDBE/join work, not posting-list decoding inside an FTS step.
    let allowance = 32 * local_rows as u64 + 128;
    assert!(
        large <= small + allowance,
        "foreign matches expanded VDBE work: small={small}, large={large}, local_rows={local_rows}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eligibility_fallback_scopes_matching_row_work() {
    let mut baseline = None;
    let mut work = Vec::new();
    for foreign in [0, 4000] {
        let fixture = fixture(foreign, 2);
        let (response, statements) = fixture.search().await;
        assert_eq!(response["total"], 2);
        assert_eq!(response["candidate_provenance"]["lexical"], "matched");
        let expected: BTreeSet<_> = (DRAFTS..DRAFTS + 2).map(local_id).collect();
        let actual: BTreeSet<_> = response["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(actual, expected);
        if let Some(before) = baseline.replace(response.clone()) {
            assert_eq!(before, response);
        }
        let statement = statements
            .iter()
            .find(|statement| {
                statement.sql.starts_with("SELECT a.* FROM")
                    && statement.sql.contains("a.deleted_at IS NULL")
            })
            .expect("real dispatch must execute eligibility fallback");
        let (rows, steps, plan) = fixture.measure(statement);
        let mut legacy = statement.clone();
        legacy.sql = LEGACY_FALLBACK.into();
        assert_eq!(rows, fixture.measure(&legacy).0, "legacy row/order parity");
        assert_eq!(
            rows.iter().map(|row| row[0].clone()).collect::<Vec<_>>(),
            (DRAFTS..DRAFTS + 2)
                .map(|index| NativeValue::Text(local_id(index)))
                .collect::<Vec<_>>()
        );
        eprintln!("NAMESPACE_FALLBACK_WORK phase=eligibility foreign={foreign} vm_steps={steps} plan={plan:?}");
        work.push((steps, plan));
    }
    assert_local_work_growth(work[0].0, work[1].0, DRAFTS + 5);
    for (_, plan) in &work {
        assert_namespace_outer(plan);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn namespace_existence_scopes_matching_row_work() {
    let mut baseline = None;
    let mut work = Vec::new();
    // Both nonempty foreign prefixes hide the fixed local drafts from phase A.
    for foreign in [1000, 4000] {
        let fixture = fixture(foreign, 0);
        let (response, statements) = fixture.search().await;
        assert_eq!(response["total"], 0);
        assert_eq!(response["candidate_provenance"]["lexical"], "filtered");
        if let Some(before) = baseline.replace(response.clone()) {
            assert_eq!(before, response);
        }
        let statement = statements
            .iter()
            .find(|statement| {
                statement.sql.starts_with("SELECT 1 AS present FROM")
                    && statement.sql.contains("fts_knowledge MATCH")
            })
            .expect("real dispatch must execute namespace existence");
        let (rows, steps, plan) = fixture.measure(statement);
        let mut legacy = statement.clone();
        legacy.sql = LEGACY_EXISTENCE.into();
        assert_eq!(rows, fixture.measure(&legacy).0, "legacy existence parity");
        assert_eq!(rows, vec![vec![NativeValue::Integer(1)]]);
        eprintln!("NAMESPACE_FALLBACK_WORK phase=existence foreign={foreign} vm_steps={steps} plan={plan:?}");
        work.push((steps, plan));
    }
    assert_local_work_growth(work[0].0, work[1].0, DRAFTS + 3);
    for (_, plan) in &work {
        assert_namespace_outer(plan);
    }
}
