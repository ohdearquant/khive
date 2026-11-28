use khive_pack_kg::KgPack;
use khive_pack_memory::MemoryPack;
use khive_runtime::{KhiveRuntime, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use khive_storage::types::{EdgeFilter, PageRequest};
use khive_storage::EdgeRelation;
use khive_types::Namespace;
use serde_json::{json, Value};
use uuid::Uuid;

#[path = "../../khive-runtime/tests/support/receipt_credentials.rs"]
mod receipt_credentials;

const BAD_BATCH: &str = "payload-who-before-key";
const KEEP_BATCH: &str = "payload-who-before-part";

fn fixture() -> (KhiveRuntime, VerbRegistry) {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        ..RuntimeConfig::no_embeddings()
    })
    .expect("private no-embedding runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("alice".to_string()));
    builder.register(KgPack::new(runtime.clone()));
    builder.register(MemoryPack::new(
        receipt_credentials::with_receipt_credentials(runtime.clone()),
    ));
    (runtime, builder.build().expect("named-actor registry"))
}

fn uuid(value: &Value) -> Uuid {
    value
        .as_str()
        .expect("public result ID")
        .parse()
        .expect("public result UUID")
}

async fn seed_owned_memory(registry: &VerbRegistry) -> Uuid {
    let seeded = registry
        .dispatch(
            "memory.remember",
            json!({"content": "reed warbler census original observation", "memory_type": "semantic"}),
        )
        .await
        .expect("public memory seed");
    let id = uuid(&seeded["id"]);
    let stored = registry
        .dispatch("get", json!({"id": id.to_string()}))
        .await
        .expect("public seed resolves");
    assert_eq!(stored["namespace"], "alice", "fixture seed is actor-owned");
    id
}

async fn note_count(runtime: &KhiveRuntime, namespace: &str) -> usize {
    let token = runtime
        .authorize(Namespace::parse(namespace).expect("fixture namespace"))
        .expect("authorize private note query");
    runtime
        .core()
        .notes(&token)
        .expect("private note store")
        .query_notes_filtered(
            namespace,
            &khive_storage::note::NoteFilter::default(),
            PageRequest {
                limit: 1000,
                offset: 0,
            },
        )
        .await
        .expect("query private notes")
        .items
        .len()
}

async fn edges(runtime: &KhiveRuntime, namespace: &str, targets: Vec<Uuid>) -> Vec<Uuid> {
    let token = runtime
        .authorize(Namespace::parse(namespace).expect("fixture namespace"))
        .expect("authorize private edge query");
    runtime
        .graph(&token)
        .expect("private graph store")
        .query_edges(
            EdgeFilter {
                target_ids: targets,
                relations: vec![EdgeRelation::Supersedes],
                ..EdgeFilter::default()
            },
            vec![],
            PageRequest {
                offset: 0,
                limit: 1000,
            },
        )
        .await
        .expect("query private supersedes edges")
        .items
        .iter()
        .map(|edge| Uuid::from(edge.id))
        .collect()
}

#[derive(Debug, PartialEq)]
struct State {
    alice_notes: usize,
    local_notes: usize,
    alice_edges: Vec<Uuid>,
    local_edges: Vec<Uuid>,
}

async fn state(runtime: &KhiveRuntime) -> State {
    State {
        alice_notes: note_count(runtime, "alice").await,
        local_notes: note_count(runtime, "local").await,
        alice_edges: edges(runtime, "alice", vec![]).await,
        local_edges: edges(runtime, "local", vec![]).await,
    }
}

fn create_row(part: u32, content: &str) -> Value {
    json!({
        "action": "create", "part": part, "content": content,
        "memory_type": "episodic", "salience": 0.3,
        "properties": {"who": "batch-tester"},
    })
}

fn supersede_row(old: Uuid) -> Value {
    let mut row = create_row(1, "reed warbler census revised observation");
    row["action"] = json!("supersede");
    row["id"] = json!(old.to_string());
    row
}

async fn apply(registry: &VerbRegistry, batch: &str, rows: Vec<Value>, dry_run: bool) -> Value {
    registry
        .dispatch(
            "memory.apply_batch",
            json!({"batch": batch, "rows": rows, "dry_run": dry_run}),
        )
        .await
        .expect("positive batch call succeeds")
}

async fn assert_plan_write_and_replay(
    runtime: &KhiveRuntime,
    registry: &VerbRegistry,
    batch: &str,
    row: Value,
    old: Option<Uuid>,
) {
    let before = state(runtime).await;
    let rows = vec![row.clone()];
    let dry = apply(registry, batch, rows.clone(), true).await;
    assert_eq!(dry["entries"][0]["outcome"], "planned", "{dry}");
    assert_eq!(dry["counts"]["planned"], 1, "{dry}");
    assert_eq!(dry["counts"]["refused"], 0, "{dry}");
    assert_eq!(dry["counts"]["failed"], 0, "{dry}");
    assert_eq!(
        state(runtime).await,
        before,
        "positive dry run writes nothing"
    );

    let written = apply(registry, batch, rows.clone(), false).await;
    assert_eq!(written["entries"][0]["outcome"], "written", "{written}");
    assert_eq!(written["counts"]["written"], 1, "{written}");
    assert_eq!(written["counts"]["refused"], 0, "{written}");
    assert_eq!(written["counts"]["failed"], 0, "{written}");
    let new_id = uuid(&written["entries"][0]["new_id"]);
    let stored = registry
        .dispatch("get", json!({"id": new_id.to_string()}))
        .await
        .expect("positive written row resolves");
    assert_eq!(stored["namespace"], "alice");
    assert_eq!(stored["content"], row["content"]);
    assert_eq!(stored["properties"]["who"], "batch-tester");
    let after = state(runtime).await;
    assert_eq!(after.alice_notes, before.alice_notes + 1);
    assert_eq!(after.local_notes, before.local_notes);
    assert_eq!(
        after.alice_edges.len(),
        before.alice_edges.len() + usize::from(old.is_some())
    );
    assert_eq!(after.local_edges, before.local_edges);
    if let Some(old) = old {
        assert_eq!(edges(runtime, "alice", vec![old]).await.len(), 1);
    }

    let replay = apply(registry, batch, rows, false).await;
    assert_eq!(replay["entries"][0]["outcome"], "deduped", "{replay}");
    assert_eq!(uuid(&replay["entries"][0]["new_id"]), new_id);
    assert_eq!(replay["counts"]["deduped"], 1, "{replay}");
    assert_eq!(replay["counts"]["written"], 0, "{replay}");
    assert_eq!(replay["counts"]["refused"], 0, "{replay}");
    assert_eq!(replay["counts"]["failed"], 0, "{replay}");
    assert_eq!(state(runtime).await, after, "replay adds no note or edge");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn batch_name_secret_key_shape_is_refused_at_intake_for_dry_and_real_calls() {
    let (runtime, registry) = fixture();
    let old = seed_owned_memory(&registry).await;
    let before = state(&runtime).await;
    assert_eq!(
        before.alice_notes, 1,
        "private seed is visible to note queries"
    );
    assert!(before.alice_edges.is_empty() && before.local_edges.is_empty());
    let rows = vec![
        create_row(7, "curlew shoreline tally fresh observation"),
        supersede_row(old),
    ];
    for dry_run in [true, false] {
        let result = registry
            .dispatch(
                "memory.apply_batch",
                json!({"batch": BAD_BATCH, "rows": rows.clone(), "dry_run": dry_run}),
            )
            .await;
        assert!(
            matches!(&result, Err(RuntimeError::InvalidInput(message))
                if message.contains("batch name")
                    && message.contains("hex-credential-token")
                    && message.contains("choose another")),
            "batch_name_gate_refuses_derived_secret_key_at_intake: dry_run={dry_run}; result={result:?}"
        );
        assert_eq!(
            state(&runtime).await,
            before,
            "batch name intake refusal writes no row or edge"
        );
        if let Err(RuntimeError::InvalidInput(message)) = &result {
            assert!(
                !message.contains(BAD_BATCH),
                "intake error omits raw batch/key text"
            );
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn safe_supersede_batch_name_plans_writes_and_replays_the_same_row() {
    let (runtime, registry) = fixture();
    let old = seed_owned_memory(&registry).await;
    assert_plan_write_and_replay(
        &runtime,
        &registry,
        KEEP_BATCH,
        supersede_row(old),
        Some(old),
    )
    .await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn create_only_batch_name_uses_create_key_shape_and_replays_the_same_row() {
    let (runtime, registry) = fixture();
    assert_eq!(
        state(&runtime).await.alice_notes,
        0,
        "private create fixture is empty"
    );
    assert_plan_write_and_replay(
        &runtime,
        &registry,
        "rotate-api-key",
        create_row(1, "curlew shoreline tally newly created observation"),
        None,
    )
    .await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn invalid_supersede_without_a_key_keeps_create_only_batch_compatibility() {
    let (runtime, registry) = fixture();
    let before = state(&runtime).await;
    let mut invalid = create_row(1, "reed warbler observation missing its prior row");
    invalid["action"] = json!("supersede");
    let rows = vec![
        invalid,
        create_row(2, "curlew shoreline tally valid created observation"),
    ];
    for dry_run in [true, false] {
        let result = registry
            .dispatch(
                "memory.apply_batch",
                json!({"batch": "rotate-api-key", "rows": rows.clone(), "dry_run": dry_run}),
            )
            .await;
        assert!(
            result.is_ok(),
            "batch_name_gate_preserves_invalid_row_key_compatibility: dry_run={dry_run}; result={result:?}"
        );
        let result = result.expect("compatibility call was accepted");
        assert_eq!(result["entries"][0]["outcome"], "refused", "{result}");
        assert_eq!(
            result["entries"][0]["reason"]["code"], "id-missing",
            "{result}"
        );
        assert_eq!(
            result["entries"][1]["outcome"],
            if dry_run { "planned" } else { "written" },
            "{result}"
        );
        assert_eq!(result["counts"]["refused"], 1, "{result}");
        assert_eq!(result["counts"]["failed"], 0, "{result}");
        if dry_run {
            assert_eq!(result["counts"]["planned"], 1, "{result}");
            assert_eq!(
                state(&runtime).await,
                before,
                "compatibility dry run writes nothing"
            );
        } else {
            assert_eq!(result["counts"]["written"], 1, "{result}");
            let created = uuid(&result["entries"][1]["new_id"]);
            let stored = registry
                .dispatch("get", json!({"id": created.to_string()}))
                .await
                .expect("compatible create row resolves");
            assert_eq!(stored["namespace"], "alice");
            let after = state(&runtime).await;
            assert_eq!(after.alice_notes, before.alice_notes + 1);
            assert_eq!(after.local_notes, before.local_notes);
            assert_eq!(after.alice_edges, before.alice_edges);
            assert_eq!(after.local_edges, before.local_edges);
        }
    }
}
