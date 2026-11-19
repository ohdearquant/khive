use std::process::{Command, Output};

use khive_pack_gtd::GtdPack;
use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use khive_storage::{Note, SqlStatement};
use serde_json::{json, Value};
use tempfile::TempDir;
use uuid::Uuid;

struct Fixture {
    home: TempDir,
    runtime: KhiveRuntime,
    registry: VerbRegistry,
    source: Uuid,
    kept: Uuid,
}

impl Fixture {
    async fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("config.toml"),
            "[runtime]\npacks = ['kg', 'gtd']\n",
        )
        .unwrap();
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(home.path().join("duplicates.db")),
            packs: vec!["kg".into(), "gtd".into()],
            actor_id: None,
            brain_profile: None,
            ..RuntimeConfig::no_embeddings()
        })
        .unwrap();
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(runtime.clone()));
        builder.register(GtdPack::new(runtime.clone()));
        let registry = builder.build().unwrap();
        runtime.install_edge_rules(registry.all_edge_rules());
        registry
            .apply_schema_plans_with_map(&Default::default(), runtime.backend())
            .unwrap();
        let mut ids = Vec::new();
        for title in ["duplicate", "kept"] {
            let assigned = registry
                .dispatch("gtd.assign", json!({"title":title}))
                .await
                .unwrap();
            ids.push(
                assigned["full_id"]
                    .as_str()
                    .unwrap()
                    .parse::<Uuid>()
                    .unwrap(),
            );
        }
        Self {
            home,
            runtime,
            registry,
            source: ids[0],
            kept: ids[1],
        }
    }

    fn run(&self, ops: &[Value]) -> (Output, Value) {
        let path = self.home.path().join("operations.jsonl");
        std::fs::write(
            &path,
            ops.iter().map(|op| format!("{op}\n")).collect::<String>(),
        )
        .unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_kkernel"))
            .args(["exec", "--atomic", "--strict", "--ops-file"])
            .arg(path)
            .arg("--config")
            .arg(self.home.path().join("config.toml"))
            .arg("--db")
            .arg(self.home.path().join("duplicates.db"))
            .args([
                "--actor",
                "test:duplicate",
                "--expect-actor",
                "test:duplicate",
                "--namespace",
                "local",
            ])
            .current_dir(self.home.path())
            .env_clear()
            .env("HOME", self.home.path())
            .env("TMPDIR", self.home.path())
            .env(
                "KHIVE_VOLUME_LOCK_DIR",
                self.home.path().join("volume-locks"),
            )
            .env("KHIVE_NO_DAEMON", "1")
            .env("KHIVE_SOCKET", self.home.path().join("unused.sock"))
            .env("KHIVE_PACKS", "kg,gtd")
            .env("RUST_LOG", "error")
            .output()
            .expect("actual kkernel --atomic child");
        let envelope = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "non-JSON atomic response: {error}; stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            )
        });
        (output, envelope)
    }

    fn cancel(&self, verb: &str, partner: Value) -> Value {
        json!({"tool":verb,"args":{"id":self.source,"status":"cancelled","duplicate_of":partner}})
    }

    async fn source(&self) -> Note {
        let token = self.runtime.authorize(Namespace::local()).unwrap();
        self.runtime
            .notes(&token)
            .unwrap()
            .get_note(self.source)
            .await
            .unwrap()
            .unwrap()
    }

    async fn snapshot(&self) -> Value {
        let mut reader = self.runtime.sql().reader().await.unwrap();
        let mut tables = Vec::new();
        for table in ["notes", "graph_edges", "events", "gtd_lifecycle_audit"] {
            tables.push(
                reader
                    .query_all(SqlStatement {
                        sql: format!("SELECT * FROM {table} ORDER BY rowid"),
                        params: vec![],
                        label: None,
                    })
                    .await
                    .unwrap(),
            );
        }
        json!(tables)
    }
}

fn committed(output: &Output, envelope: &Value, verb: &str) {
    assert!(
        output.status.success(),
        "{envelope}; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(envelope["atomic"]["committed"], true);
    assert_eq!(envelope["summary"]["succeeded"], 1);
    let results = envelope["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["tool"], verb);
    assert_eq!(results[0]["ok"], true);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn atomic_duplicate_cancellation_forwards_both_lifecycle_verbs_and_reads_back() {
    for verb in ["gtd.transition", "gtd.complete"] {
        let fixture = Fixture::new().await;
        let before = fixture.source().await;
        let prefix = fixture.kept.simple().to_string()[..12].to_string();
        let (output, envelope) = fixture.run(&[fixture.cancel(verb, json!(prefix))]);
        committed(&output, &envelope, verb);
        let after = fixture.source().await;
        assert_eq!(
            after.properties.as_ref().unwrap()["duplicate_of"],
            fixture.kept.to_string(),
            "atomic adapter must forward the actual duplicate judgment"
        );
        assert_eq!(after.properties.as_ref().unwrap()["status"], "cancelled");
        assert_eq!(after.version, before.version + 1);
        let reverse = fixture
            .registry
            .dispatch("gtd.tasks", json!({"duplicate_of":fixture.kept}))
            .await
            .unwrap();
        assert_eq!(reverse.as_array().unwrap().len(), 1);
        assert_eq!(reverse[0]["full_id"], fixture.source.to_string());
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn atomic_duplicate_invalid_partner_and_wrong_status_refuse_without_writes() {
    for verb in ["gtd.transition", "gtd.complete"] {
        let fixture = Fixture::new().await;
        let observation = fixture
            .registry
            .dispatch(
                "create",
                json!({"kind":"observation","content":"not a task"}),
            )
            .await
            .unwrap();
        for partner in [
            json!(fixture.source),
            json!(Uuid::new_v4()),
            observation["id"].clone(),
        ] {
            let before = fixture.snapshot().await;
            let (output, envelope) = fixture.run(&[fixture.cancel(verb, partner)]);
            assert!(!output.status.success(), "{envelope}");
            assert_eq!(envelope["atomic"]["committed"], false);
            assert_eq!(envelope["atomic"]["failed_op_index"], 0);
            assert_eq!(fixture.snapshot().await, before);
        }
        let before = fixture.snapshot().await;
        let mut op = fixture.cancel(verb, json!(fixture.kept));
        op["args"]["status"] = json!("done");
        let (output, envelope) = fixture.run(&[op]);
        assert!(!output.status.success(), "{envelope}");
        assert_eq!(fixture.snapshot().await, before);
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn atomic_duplicate_partner_deleted_by_earlier_op_rolls_back_both_operations() {
    for verb in ["gtd.transition", "gtd.complete"] {
        for hard in [false, true] {
            let fixture = Fixture::new().await;
            let before = fixture.snapshot().await;
            let (output, envelope) = fixture.run(&[
                json!({"tool":"delete","args":{"id":fixture.kept,"hard":hard}}),
                fixture.cancel(verb, json!(fixture.kept)),
            ]);
            assert!(!output.status.success(), "{envelope}");
            assert_eq!(envelope["atomic"]["rolled_back"], true);
            assert_eq!(envelope["atomic"]["failed_op_index"], 1);
            assert_eq!(envelope["summary"]["succeeded"], 0);
            assert!(
                envelope["atomic"]["error"]
                    .as_str()
                    .unwrap()
                    .contains("guard failed"),
                "{envelope}"
            );
            assert_eq!(
                fixture.snapshot().await,
                before,
                "partner delete and duplicate cancellation must both roll back"
            );
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn atomic_duplicate_recancel_asserts_exact_judgment_and_partner_liveness() {
    let fixture = Fixture::new().await;
    let op = fixture.cancel("gtd.transition", json!(fixture.kept));
    let (output, envelope) = fixture.run(std::slice::from_ref(&op));
    committed(&output, &envelope, "gtd.transition");
    let before = fixture.snapshot().await;
    let (output, envelope) = fixture.run(std::slice::from_ref(&op));
    committed(&output, &envelope, "gtd.transition");
    assert_eq!(envelope["results"][0]["result"]["transitioned"], false);
    assert_eq!(fixture.snapshot().await, before);
    let (output, envelope) = fixture.run(&[
        json!({"tool":"delete","args":{"id":fixture.kept,"hard":true}}),
        op,
    ]);
    assert!(!output.status.success(), "{envelope}");
    assert_eq!(envelope["atomic"]["rolled_back"], true);
    assert_eq!(envelope["atomic"]["failed_op_index"], 1);
    assert_eq!(fixture.snapshot().await, before);
    let other = fixture
        .registry
        .dispatch("gtd.assign", json!({"title":"different kept"}))
        .await
        .unwrap();
    let before = fixture.snapshot().await;
    let (output, envelope) =
        fixture.run(&[fixture.cancel("gtd.transition", other["full_id"].clone())]);
    assert!(!output.status.success(), "{envelope}");
    assert_eq!(fixture.snapshot().await, before);
}
