//! #3514: paired, feature-only slot-table/prefixed-tokenizer experiment over
//! the real knowledge.search dispatch. The runtime creates the ordinary schema
//! in a temporary file, applies a namespace-keyed slot-table baseline there,
//! and adds one prefixed-tokenizer shadow index. Neither is a shipped migration.

use super::*;

use crate::KnowledgePack;
use khive_db::namespace_trigram_proto::{envelope, key_for_slot, register, scoped_match};
use khive_pack_kg::KgPack;
use khive_runtime::{RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use rusqlite::{params, Connection};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const A_NAMESPACE: &str = "a";
const B_NAMESPACE: &str = "b";
const QUERY: &str = "zznamespaceguard zzsecondguard";
const TERM: &str = "zznamespaceguard";
// A design baseline for comparison, held as a test fixture rather than
// installed as a numbered migration.
const SLOT_TABLE_BASELINE_SQL: &str = include_str!("namespace_trigram_slot_table_baseline.sql.txt");

struct Fixture {
    _directory: tempfile::TempDir,
    runtime: KhiveRuntime,
    registry: VerbRegistry,
    a_key: String,
}

fn fixture(foreign_rows: i64) -> Fixture {
    let directory = tempfile::tempdir().expect("temporary prototype database directory");
    let path = directory.path().join("namespace-trigram-proto.db");
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(path.clone()),
        default_namespace: Namespace::parse(A_NAMESPACE).expect("A namespace"),
        embedding_model: None,
        additional_embedding_models: vec![],
        events_split: None,
        actor_id: None,
        ..RuntimeConfig::default()
    })
    .expect("file-backed runtime creates the existing schema");
    let mut builder = VerbRegistryBuilder::new();
    builder.with_default_namespace(A_NAMESPACE);
    builder.register(KgPack::new(runtime.clone()));
    builder.register(KnowledgePack::new(runtime.clone()));
    let registry = builder.build().expect("knowledge registry");
    registry.apply_schema_plans(runtime.backend());
    runtime.install_edge_rules(registry.all_edge_rules());

    // Reserve B's first slot in every arm, including the zero-B control, so
    // A's exact key remains slot 2 while the number of earlier B rows varies.
    let b_key = key_for_slot(1).expect("B slot");
    let a_key = key_for_slot(2).expect("A slot");
    assert_eq!(a_key.chars().count(), 3);
    let mut connection = Connection::open(&path).expect("open temporary database");
    register(&connection).expect("register prototype tokenizer before FTS DDL");
    connection
        .execute_batch(SLOT_TABLE_BASELINE_SQL)
        .expect("apply the slot-table baseline schema to the temp DB");
    connection
        .execute(
            "INSERT INTO knowledge_fts_namespace_keys(namespace_value) VALUES (?1)",
            [B_NAMESPACE],
        )
        .expect("reserve B slot even in the zero-row control");
    let transaction = connection.transaction().expect("seed transaction");
    if foreign_rows > 0 {
        // The foreign body deliberately contains A's exact key and both
        // query terms. A column-filtered slot key may see that shared posting;
        // the prototype must never emit an A-prefixed token for this B text.
        transaction
            .execute(
                "WITH RECURSIVE x(n) AS ( \
                     VALUES(1) UNION ALL SELECT n + 1 FROM x WHERE n < ?1 \
                 ) \
                 INSERT INTO knowledge_atoms ( \
                     id, namespace, slug, name, content, tags, finalized, \
                     status, created_at, updated_at \
                 ) \
                 SELECT printf('35140000-0000-4000-8000-%012d', x.n), \
                        ?2, printf('foreign-%06d', x.n), 'Foreign Match', \
                        ?3, '[]', 1, 'reviewed', 1, 1 FROM x",
                params![
                    foreign_rows,
                    B_NAMESPACE,
                    format!("{a_key}{QUERY} ordinary padding text"),
                ],
            )
            .expect("seed B before A");
    }
    // Twenty local drafts fill the deliberately lowered phase-A ceiling.
    // The two reviewed A rows follow them, so a correct scoped fallback must
    // recover the same eligible result even behind 200000 earlier B rows.
    for index in 0..20 {
        transaction
            .execute(
                "INSERT INTO knowledge_atoms ( \
                     id, namespace, slug, name, content, tags, finalized, \
                     status, created_at, updated_at \
                 ) VALUES (?1, ?2, ?3, ?4, ?5, '[]', 0, 'draft', 1, 1)",
                params![
                    format!("35140000-0000-4000-7000-{index:012}"),
                    A_NAMESPACE,
                    format!("local-draft-{index}"),
                    format!("Local Draft {index}"),
                    QUERY,
                ],
            )
            .expect("seed A draft ceiling prefix");
    }
    for index in 0..2 {
        transaction
            .execute(
                "INSERT INTO knowledge_atoms ( \
                     id, namespace, slug, name, content, tags, finalized, \
                     status, created_at, updated_at \
                 ) VALUES (?1, ?2, ?3, ?4, ?5, '[]', 1, 'reviewed', 1, 1)",
                params![
                    format!("35140000-0000-4000-9000-{index:012}"),
                    A_NAMESPACE,
                    format!("local-{index}"),
                    format!("Local Match {index}"),
                    QUERY,
                ],
            )
            .expect("seed identical A row");
    }

    // The baseline's triggers and slot view built its index while
    // seeding. Only the prefixed-tokenizer arm needs a shadow FTS table.
    transaction
        .execute_batch(
            "CREATE VIRTUAL TABLE fts_knowledge_namespace_proto USING fts5( \
                 id UNINDEXED, namespace UNINDEXED, slug, name, content, \
                 content='', \
                 tokenize='namespace_trigram_v1' \
             );",
        )
        .expect("create feature-only prefixed FTS shadow index");
    let a_envelope = envelope(&a_key, "").expect("A envelope");
    let b_envelope = envelope(&b_key, "").expect("B envelope");
    transaction
        .execute(
            "INSERT INTO fts_knowledge_namespace_proto( \
                 rowid, id, namespace, slug, name, content \
             ) SELECT rowid, id, namespace, \
                    (CASE namespace WHEN ?1 THEN ?2 ELSE ?3 END) || slug, \
                    (CASE namespace WHEN ?1 THEN ?2 ELSE ?3 END) || name, \
                    (CASE namespace WHEN ?1 THEN ?2 ELSE ?3 END) || content \
             FROM knowledge_atoms WHERE deleted_at IS NULL ORDER BY rowid",
            params![A_NAMESPACE, a_envelope, b_envelope],
        )
        .expect("populate namespace-prefixed trigram index");
    transaction.commit().expect("commit deterministic fixture");

    let poisoned_foreign_rows: i64 = connection
        .query_row(
            "SELECT count(*) FROM knowledge_atoms \
             WHERE namespace = ?1 AND instr(content, ?2) > 0",
            params![B_NAMESPACE, format!("{a_key}{QUERY}")],
            |row| row.get(0),
        )
        .expect("verify exact adversarial B text");
    assert_eq!(poisoned_foreign_rows, foreign_rows);

    let stored_a_key: String = connection
        .query_row(
            "SELECT namespace_key FROM knowledge_fts_namespace_tokens WHERE namespace = ?1",
            [A_NAMESPACE],
            |row| row.get(0),
        )
        .expect("read the baseline's assigned A key");
    assert_eq!(stored_a_key, a_key);

    let a_match = scoped_match(&a_key, TERM).expect("A MATCH expression");
    let proto_rowids: Vec<i64> = {
        let mut statement = connection
            .prepare(
                "SELECT rowid FROM fts_knowledge_namespace_proto \
                 WHERE fts_knowledge_namespace_proto MATCH ?1 ORDER BY rowid LIMIT 10",
            )
            .expect("prepare prototype rowid probe");
        statement
            .query_map([a_match], |row| row.get(0))
            .expect("run prototype rowid probe")
            .collect::<rusqlite::Result<_>>()
            .expect("collect prototype rowids")
    };
    assert_eq!(proto_rowids.len(), 10);
    assert!(
        proto_rowids.iter().all(|rowid| *rowid > foreign_rows),
        "the prototype's first postings must contain only A rows"
    );
    let slot_table_rowids: Vec<i64> = {
        let mut statement = connection
            .prepare(
                "SELECT rowid FROM fts_knowledge WHERE fts_knowledge MATCH ?1 \
                 ORDER BY rowid LIMIT 10",
            )
            .expect("prepare slot-table baseline rowid probe");
        let expression =
            format!("namespace_key : \"{a_key}\" AND {{slug name content}} : \"{TERM}\"");
        statement
            .query_map([expression], |row| row.get(0))
            .expect("run slot-table baseline rowid probe")
            .collect::<rusqlite::Result<_>>()
            .expect("collect slot-table baseline rowids")
    };
    assert_eq!(slot_table_rowids, proto_rowids);
    drop(connection);

    Fixture {
        _directory: directory,
        runtime,
        registry,
        a_key,
    }
}

async fn dispatch(fixture: &Fixture, mode: NamespaceTrigramExperiment) -> (Value, Value) {
    let mode_suffix = match &mode {
        NamespaceTrigramExperiment::SlotTable { .. } => "slot_table",
        NamespaceTrigramExperiment::Prefixed { .. } => "prefixed",
    };
    let timings: tests::PrototypePhaseTimes = Arc::new(Mutex::new(HashMap::new()));
    let started = Instant::now();
    let response = tests::with_prototype_phase_times(
        timings.clone(),
        NAMESPACE_TRIGRAM_EXPERIMENT.scope(
            mode,
            with_lexical_stage_budget_override_ms(
                120_000,
                with_phase_a_widen_ceiling_override(
                    20,
                    fixture.registry.dispatch(
                        "knowledge.search",
                        json!({
                            "namespace": A_NAMESPACE,
                            "query": QUERY,
                            "rerank": false,
                            "limit": 10,
                        }),
                    ),
                ),
            ),
        ),
    )
    .await
    .expect("real knowledge.search dispatch");
    let wall_ms = started.elapsed().as_secs_f64() * 1_000.0;
    assert_eq!(response["total"], 2, "A has exactly two eligible atoms");
    assert_eq!(response["candidate_provenance"]["lexical"], "matched");
    assert!(response.get("degraded").is_none(), "{response}");
    let stages: serde_json::Map<String, Value> = timings
        .lock()
        .expect("phase timings")
        .iter()
        .map(|(phase, (nanos, reads))| {
            (
                (*phase).to_owned(),
                json!({"reads": reads, "elapsed_ms": *nanos as f64 / 1_000_000.0}),
            )
        })
        .collect();
    assert_eq!(stages["namespace_key"]["reads"], 1);
    for phase in ["term_frequency", "phase_a_rowids", "eligibility_fallback"] {
        let label = format!("{phase}_{mode_suffix}");
        assert!(
            stages[&label]["reads"].as_u64().unwrap_or(0) > 0,
            "the {mode_suffix} arm must exercise {phase}: {stages:?}"
        );
    }
    let envelope = json!({
        "ok": true,
        "tool": "knowledge.search",
        "result": response,
    });
    (
        envelope,
        json!({"wall_ms": wall_ms, "lexical_reads": stages}),
    )
}

async fn compare_arm(foreign_rows: i64) -> (Value, Value) {
    let fixture = fixture(foreign_rows);
    let (slot_table, slot_table_latency) = dispatch(
        &fixture,
        NamespaceTrigramExperiment::SlotTable {
            key: fixture.a_key.clone(),
        },
    )
    .await;
    let (prototype, prototype_latency) = dispatch(
        &fixture,
        NamespaceTrigramExperiment::Prefixed {
            key: fixture.a_key.clone(),
        },
    )
    .await;
    assert_eq!(
        serde_json::to_vec(&slot_table).expect("serialize slot-table envelope"),
        serde_json::to_vec(&prototype).expect("serialize prototype envelope"),
        "slot-table and prefixed tokenization must preserve the search result envelope"
    );
    let record = json!({
        "foreign_rows": foreign_rows,
        "query": QUERY,
        "slot_table": slot_table_latency,
        "prototype": prototype_latency,
    });
    eprintln!("NAMESPACE_TRIGRAM_PROTO {record}");
    assert_eq!(
        fixture.runtime.config().default_namespace.as_str(),
        A_NAMESPACE
    );
    (slot_table, record)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prototype_preserves_search_result_with_earlier_foreign_rows() {
    let (zero_envelope, zero) = compare_arm(0).await;
    let (two_hundred_envelope, two_hundred) = compare_arm(200).await;
    assert_eq!(
        serde_json::to_vec(&zero_envelope).expect("serialize zero-B envelope"),
        serde_json::to_vec(&two_hundred_envelope).expect("serialize 200-B envelope"),
        "foreign postings must not change A's search result"
    );
    assert_eq!(zero["foreign_rows"], 0);
    assert_eq!(two_hundred["foreign_rows"], 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual #2404-scale, 200000-row paired latency run on a quiet machine"]
async fn measure_prototype_against_slot_table_at_200000_foreign_rows() {
    let mut baseline = None;
    for foreign_rows in [0, 200, 200_000] {
        let (envelope, _) = compare_arm(foreign_rows).await;
        let bytes = serde_json::to_vec(&envelope).expect("serialize search result envelope");
        if let Some(ref baseline) = baseline {
            assert_eq!(&bytes, baseline, "A envelope changed with B row count");
        } else {
            baseline = Some(bytes);
        }
    }
}
