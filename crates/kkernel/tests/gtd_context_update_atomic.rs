//! Public CLI atomic context updates must keep the task property and its
//! annotation consistent without exposing internal companions as extra ops.

use std::process::{Command, Output};

use khive_pack_gtd::GtdPack;
use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, VerbRegistryBuilder};
use khive_storage::{Edge, EdgeFilter, EdgeRelation, Note, PageRequest, SqlStatement};
use serde_json::{json, Value};
use tempfile::TempDir;
use uuid::Uuid;

struct Fixture {
    home: TempDir,
    runtime: KhiveRuntime,
    task: Uuid,
    a: Uuid,
    b: Uuid,
    unrelated: Uuid,
}

impl Fixture {
    async fn new(with_context: bool) -> Self {
        let home = tempfile::tempdir().expect("isolated CLI fixture");
        std::fs::write(
            home.path().join("config.toml"),
            "[runtime]\npacks = ['kg', 'gtd']\n",
        )
        .expect("explicit fixture config");
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(home.path().join("context.db")),
            packs: vec!["kg".into(), "gtd".into()],
            actor_id: None,
            brain_profile: None,
            ..RuntimeConfig::no_embeddings()
        })
        .expect("model-less seed runtime");
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(runtime.clone()));
        builder.register(GtdPack::new(runtime.clone()));
        let registry = builder.build().expect("KG and GTD registry");
        runtime.install_edge_rules(registry.all_edge_rules());
        registry
            .apply_schema_plans_with_map(&Default::default(), runtime.backend())
            .expect("real pack schemas");
        let token = runtime.authorize(Namespace::local()).expect("seed token");
        let mut ids = Vec::new();
        for name in ["Context A", "Context B", "Independent annotation"] {
            ids.push(
                runtime
                    .create_entity(&token, "concept", None, name, None, None, vec![])
                    .await
                    .expect("seed context entity")
                    .id,
            );
        }
        let [a, b, unrelated] = <[Uuid; 3]>::try_from(ids).expect("three entities");
        let mut args = json!({"title": "Task context atomic contract"});
        if with_context {
            args["context_entity_id"] = json!(a);
        }
        let assigned = registry
            .dispatch("gtd.assign", args)
            .await
            .expect("canonical GTD seed task");
        let task = Uuid::parse_str(assigned["full_id"].as_str().expect("full task ID"))
            .expect("task UUID");
        runtime
            .link(
                &token,
                task,
                unrelated,
                EdgeRelation::Annotates,
                0.75,
                Some(json!({"purpose": "independent annotation"})),
            )
            .await
            .expect("unrelated annotation control");
        Self {
            home,
            runtime,
            task,
            a,
            b,
            unrelated,
        }
    }

    fn run(&self, ops: &[Value]) -> (Output, Value) {
        let contents: String = ops.iter().map(|op| format!("{op}\n")).collect();
        let ops_file = self.home.path().join("operations.jsonl");
        std::fs::write(&ops_file, contents).expect("write JSONL operations");
        let output = Command::new(env!("CARGO_BIN_EXE_kkernel"))
            .args(["exec", "--atomic", "--strict", "--ops-file"])
            .arg(ops_file)
            .arg("--config")
            .arg(self.home.path().join("config.toml"))
            .arg("--db")
            .arg(self.home.path().join("context.db"))
            .args([
                "--actor",
                "test:context",
                "--expect-actor",
                "test:context",
                "--namespace",
                "local",
            ])
            .current_dir(self.home.path())
            .env_clear()
            .env("HOME", self.home.path())
            .env("TMPDIR", self.home.path())
            .env("KHIVE_NO_DAEMON", "1")
            .env("KHIVE_SOCKET", self.home.path().join("unused.sock"))
            .env("KHIVE_PACKS", "kg,gtd")
            .env("RUST_LOG", "error")
            .output()
            .expect("run actual kkernel --atomic binary");
        let envelope = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "atomic output is not JSON: {error}; stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            )
        });
        (output, envelope)
    }

    fn update(&self, context: Value) -> Value {
        json!({"tool": "update", "args": {
            "id": self.task, "properties": {"context_entity_id": context}
        }})
    }

    async fn note(&self) -> Note {
        let token = self
            .runtime
            .authorize(Namespace::local())
            .expect("read token");
        self.runtime
            .notes(&token)
            .expect("live fixture note store")
            .get_note(self.task)
            .await
            .expect("read task")
            .expect("task remains present")
    }

    async fn annotations(&self) -> Vec<Edge> {
        let token = self
            .runtime
            .authorize(Namespace::local())
            .expect("read token");
        let mut edges = self
            .runtime
            .graph(&token)
            .expect("live fixture graph store")
            .query_edges(
                EdgeFilter {
                    source_ids: vec![self.task],
                    relations: vec![EdgeRelation::Annotates],
                    ..EdgeFilter::default()
                },
                vec![],
                PageRequest {
                    offset: 0,
                    limit: 100,
                },
            )
            .await
            .expect("read live annotation edges")
            .items;
        edges.sort_by_key(|edge| edge.id.to_string());
        edges
    }

    async fn assert_targets(&self, expected: &[Uuid]) {
        let mut actual: Vec<Uuid> = self
            .annotations()
            .await
            .iter()
            .map(|edge| edge.target_id)
            .collect();
        let mut expected = expected.to_vec();
        actual.sort();
        expected.sort();
        assert_eq!(actual, expected);
    }

    async fn snapshot(&self) -> Value {
        // Keep the original writable pool alive; a WAL file is not a frozen
        // read-only snapshot. Include tombstones so rollback cannot hide an edge.
        let token = self
            .runtime
            .authorize(Namespace::local())
            .expect("read token");
        let mut entities = Vec::new();
        for id in [self.a, self.b, self.unrelated] {
            entities.push(
                self.runtime
                    .entities(&token)
                    .expect("entity store")
                    .get_entity_including_deleted(id)
                    .await
                    .expect("read entity including tombstone"),
            );
        }
        let edges = self
            .runtime
            .sql()
            .reader()
            .await
            .expect("live SQL reader")
            .query_all(SqlStatement {
                sql: "SELECT * FROM graph_edges ORDER BY namespace, id".into(),
                params: vec![],
                label: Some("gtd-context-atomic-domain-snapshot".into()),
            })
            .await
            .expect("snapshot every edge column, including deleted rows");
        json!({"task": self.note().await, "entities": entities, "edges": edges})
    }
}

fn assert_slots(envelope: &Value, tools: &[&str]) {
    let results = envelope["results"].as_array().expect("result slots");
    assert_eq!(results.len(), tools.len(), "{envelope}");
    assert_eq!(
        envelope["summary"]["total"],
        json!(tools.len()),
        "{envelope}"
    );
    for (index, tool) in tools.iter().enumerate() {
        assert_eq!(results[index]["op_index"], json!(index), "{envelope}");
        assert_eq!(results[index]["tool"], *tool, "{envelope}");
    }
}

fn assert_one_committed_update(output: &Output, envelope: &Value) {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_slots(envelope, &["update"]);
    assert_eq!(envelope["atomic"]["committed"], true, "{envelope}");
    assert_eq!(envelope["atomic"]["rolled_back"], false, "{envelope}");
    assert_eq!(envelope["summary"]["succeeded"], 1, "{envelope}");
    assert_eq!(envelope["results"][0]["ok"], true, "{envelope}");
    assert!(envelope["results"][0]["result"].is_object(), "{envelope}");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn atomic_context_a_to_b_replaces_only_the_context_annotation_in_one_result_slot() {
    let fixture = Fixture::new(true).await;
    fixture
        .assert_targets(&[fixture.a, fixture.unrelated])
        .await;
    let before = fixture.note().await;
    let independent = fixture
        .annotations()
        .await
        .into_iter()
        .find(|edge| edge.target_id == fixture.unrelated)
        .unwrap();
    let (output, envelope) = fixture.run(&[fixture.update(json!(fixture.b))]);
    assert_one_committed_update(&output, &envelope);
    let after = fixture.note().await;
    assert_eq!(
        after.properties.as_ref().unwrap()["context_entity_id"],
        json!(fixture.b)
    );
    assert_eq!(after.version, before.version + 1);
    fixture
        .assert_targets(&[fixture.b, fixture.unrelated])
        .await;
    let preserved = fixture
        .annotations()
        .await
        .into_iter()
        .find(|edge| edge.target_id == fixture.unrelated)
        .unwrap();
    assert_eq!(
        serde_json::to_value(preserved).unwrap(),
        serde_json::to_value(independent).unwrap()
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn atomic_context_a_to_null_removes_only_the_context_annotation() {
    let fixture = Fixture::new(true).await;
    fixture
        .assert_targets(&[fixture.a, fixture.unrelated])
        .await;
    let before = fixture.note().await;
    let (output, envelope) = fixture.run(&[fixture.update(Value::Null)]);
    assert_one_committed_update(&output, &envelope);
    let after = fixture.note().await;
    assert!(after.properties.as_ref().unwrap()["context_entity_id"].is_null());
    assert_eq!(after.version, before.version + 1);
    fixture.assert_targets(&[fixture.unrelated]).await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn atomic_same_context_preserves_edge_ids_timestamps_and_metadata() {
    let fixture = Fixture::new(true).await;
    let before = serde_json::to_value(fixture.annotations().await).unwrap();
    let (output, envelope) =
        fixture.run(&[fixture.update(json!(fixture.a.to_string().to_uppercase()))]);
    assert_one_committed_update(&output, &envelope);
    assert_eq!(
        serde_json::to_value(fixture.annotations().await).unwrap(),
        before
    );
    assert_eq!(
        fixture.note().await.properties.unwrap()["context_entity_id"],
        json!(fixture.a)
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn atomic_first_context_creates_one_annotation_without_an_extra_result_slot() {
    let fixture = Fixture::new(false).await;
    fixture.assert_targets(&[fixture.unrelated]).await;
    let before = fixture.note().await;
    let (output, envelope) = fixture.run(&[fixture.update(json!(fixture.b))]);
    assert_one_committed_update(&output, &envelope);
    let after = fixture.note().await;
    assert_eq!(
        after.properties.as_ref().unwrap()["context_entity_id"],
        json!(fixture.b)
    );
    assert_eq!(after.version, before.version + 1);
    fixture
        .assert_targets(&[fixture.b, fixture.unrelated])
        .await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn atomic_invalid_context_refuses_before_any_domain_change() {
    let fixture = Fixture::new(true).await;
    let before = fixture.snapshot().await;
    let missing = Uuid::new_v4();
    let (output, envelope) = fixture.run(&[fixture.update(json!(missing))]);
    assert!(!output.status.success(), "{envelope}");
    assert_slots(&envelope, &["update"]);
    assert_eq!(envelope["atomic"]["committed"], false, "{envelope}");
    assert_eq!(envelope["atomic"]["rolled_back"], false, "{envelope}");
    assert_eq!(envelope["atomic"]["failed_op_index"], 0, "{envelope}");
    assert!(
        envelope["results"][0]["error"]
            .as_str()
            .unwrap()
            .contains("context_entity_id"),
        "{envelope}"
    );
    assert_eq!(
        fixture.snapshot().await,
        before,
        "invalid context must not change note revision or edges"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn atomic_context_link_companion_missing_endpoint_rolls_back_the_whole_batch() {
    let fixture = Fixture::new(true).await;
    fixture
        .assert_targets(&[fixture.a, fixture.unrelated])
        .await;
    let before = fixture.snapshot().await;
    // Preparation sees B. The first operation removes it inside the unit;
    // the update's link companion must recheck the endpoint during execution.
    let (output, envelope) = fixture.run(&[
        json!({"tool": "delete", "args": {"id": fixture.b, "hard": true}}),
        fixture.update(json!(fixture.b)),
    ]);
    // Atomic execution reports a completed rollback with exit 0, even under
    // --strict. The envelope and persisted snapshot below establish failure.
    assert!(output.status.success(), "{envelope}");
    assert_slots(&envelope, &["delete", "update"]);
    assert_eq!(envelope["atomic"]["committed"], false, "{envelope}");
    assert_eq!(envelope["atomic"]["rolled_back"], true, "{envelope}");
    assert_eq!(envelope["atomic"]["failed_op_index"], 1, "{envelope}");
    assert_eq!(envelope["summary"]["succeeded"], 0, "{envelope}");
    assert_eq!(envelope["summary"]["failed"], 2, "{envelope}");
    assert!(
        envelope["atomic"]["error"]
            .as_str()
            .unwrap()
            .contains("guard failed"),
        "{envelope}"
    );
    assert!(envelope["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|result| result["ok"] == false));
    assert_eq!(
        fixture.snapshot().await,
        before,
        "rollback must restore B, task properties/revision, and every old edge byte-for-byte"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn atomic_existing_context_annotation_rechecks_endpoint_and_preserves_the_edge_on_success() {
    let fixture = Fixture::new(true).await;
    let token = fixture
        .runtime
        .authorize(Namespace::local())
        .expect("seed token");
    fixture
        .runtime
        .link(
            &token,
            fixture.task,
            fixture.b,
            EdgeRelation::Annotates,
            0.35,
            Some(json!({"purpose": "separately authored B annotation"})),
        )
        .await
        .expect("seed an existing B annotation alongside context A");
    fixture
        .assert_targets(&[fixture.a, fixture.b, fixture.unrelated])
        .await;
    let before = fixture.snapshot().await;
    let retained: Vec<Edge> = fixture
        .annotations()
        .await
        .into_iter()
        .filter(|edge| edge.target_id != fixture.a)
        .collect();
    let update = fixture.update(json!(fixture.b));
    let (output, envelope) = fixture.run(&[
        json!({"tool": "delete", "args": {"id": fixture.b, "hard": true}}),
        update.clone(),
    ]);
    // Atomic execution reports a completed rollback with exit 0, even under
    // --strict. The envelope and persisted snapshot below establish failure.
    assert!(output.status.success(), "{envelope}");
    assert_slots(&envelope, &["delete", "update"]);
    assert_eq!(envelope["atomic"]["committed"], false, "{envelope}");
    assert_eq!(envelope["atomic"]["rolled_back"], true, "{envelope}");
    assert_eq!(envelope["atomic"]["failed_op_index"], 1, "{envelope}");
    assert_eq!(envelope["summary"]["succeeded"], 0, "{envelope}");
    assert_eq!(envelope["summary"]["failed"], 2, "{envelope}");
    assert!(
        envelope["atomic"]["error"]
            .as_str()
            .unwrap()
            .contains("guard failed"),
        "{envelope}"
    );
    assert!(envelope["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|result| result["ok"] == false));
    assert_eq!(
        fixture.snapshot().await,
        before,
        "reusing an existing B annotation must not bypass the execution-time endpoint guard"
    );

    // With B intact, the exact same update succeeds without rewriting the
    // independently authored B edge or creating a replacement for it.
    let (output, envelope) = fixture.run(&[update]);
    assert_one_committed_update(&output, &envelope);
    assert_eq!(
        fixture.note().await.properties.unwrap()["context_entity_id"],
        json!(fixture.b)
    );
    assert_eq!(
        serde_json::to_value(fixture.annotations().await).unwrap(),
        serde_json::to_value(retained).unwrap(),
        "successful context reuse preserves both B and unrelated C edge rows exactly"
    );
}
