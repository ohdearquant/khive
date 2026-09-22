//! Finding null-clear policy must reach the public CLI atomic preparation path.
//! Persisted key membership distinguishes deletion from storing JSON null.

use std::process::{Command, Output};

use khive_pack_code::CodePack;
use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, VerbRegistryBuilder};
use khive_storage::{Note, SqlStatement};
use serde_json::{json, Value};
use tempfile::TempDir;
use uuid::Uuid;

struct Fixture {
    home: TempDir,
    runtime: KhiveRuntime,
    finding: Uuid,
    observation: Uuid,
}

impl Fixture {
    async fn new() -> Self {
        let home = tempfile::tempdir().expect("isolated CLI fixture");
        std::fs::write(
            home.path().join("config.toml"),
            "[runtime]\npacks = ['kg', 'code']\n",
        )
        .expect("explicit fixture config");
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(home.path().join("findings.db")),
            packs: vec!["kg".into(), "code".into()],
            actor_id: None,
            brain_profile: None,
            ..RuntimeConfig::no_embeddings()
        })
        .expect("model-less seed runtime");
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(runtime.clone()));
        builder.register(CodePack::new(runtime.clone()));
        let registry = builder.build().expect("KG and code registry");
        runtime.install_edge_rules(registry.all_edge_rules());
        registry
            .apply_schema_plans_with_map(&Default::default(), runtime.backend())
            .expect("real pack schemas");
        let mut ids = Vec::new();
        for kind in ["finding", "observation"] {
            let created = registry
                .dispatch(
                    "create",
                    json!({
                        "kind": kind,
                        "title": "Finding null-clear contract",
                        "content": "Persisted optional-field control",
                        "properties": {
                            "severity": "high", "confidence": "medium",
                            "kind_status": "open", "evidence": ["src/lib.rs:42"],
                            "custom": "before", "keep": 42
                        }
                    }),
                )
                .await
                .expect("canonical seed create");
            ids.push(
                Uuid::parse_str(created["id"].as_str().expect("created note ID"))
                    .expect("full note UUID"),
            );
        }
        let [finding, observation] = <[Uuid; 2]>::try_from(ids).expect("two seed notes");
        Self {
            home,
            runtime,
            finding,
            observation,
        }
    }

    fn run(&self, ops: &[Value], atomic: bool) -> (Output, Value) {
        let contents: String = ops.iter().map(|op| format!("{op}\n")).collect();
        let ops_file = self.home.path().join("operations.jsonl");
        std::fs::write(&ops_file, contents).expect("write JSONL operations");
        let mut command = Command::new(env!("CARGO_BIN_EXE_kkernel"));
        command.args(["exec", "--strict", "--ops-file"]);
        command.arg(ops_file);
        if atomic {
            command.arg("--atomic");
        }
        let output = command
            .arg("--config")
            .arg(self.home.path().join("config.toml"))
            .arg("--db")
            .arg(self.home.path().join("findings.db"))
            .args([
                "--actor",
                "test:finding-clear",
                "--expect-actor",
                "test:finding-clear",
                "--namespace",
                "local",
            ])
            .current_dir(self.home.path())
            .env_clear()
            .env("HOME", self.home.path())
            .env("TMPDIR", self.home.path())
            .env("KHIVE_NO_DAEMON", "1")
            .env("KHIVE_SOCKET", self.home.path().join("unused.sock"))
            .env("KHIVE_PACKS", "kg,code")
            .env("RUST_LOG", "error")
            .output()
            .expect("run actual kkernel binary");
        let envelope = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "CLI output is not JSON: {error}; stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            )
        });
        (output, envelope)
    }

    async fn note(&self, id: Uuid) -> Note {
        let token = self
            .runtime
            .authorize(Namespace::local())
            .expect("read token");
        self.runtime
            .notes(&token)
            .expect("live fixture note store")
            .get_note(id)
            .await
            .expect("read persisted note")
            .expect("seed note remains present")
    }

    async fn snapshot(&self) -> Value {
        // Read through the still-live writable fixture pool. Reopening a WAL
        // database read-only is not a reliable post-child persistence check.
        let rows = self
            .runtime
            .sql()
            .reader()
            .await
            .expect("live SQL reader")
            .query_all(SqlStatement {
                sql: "SELECT * FROM notes ORDER BY namespace, id".into(),
                params: vec![],
                label: Some("finding-null-clear-domain-snapshot".into()),
            })
            .await
            .expect("snapshot all note columns, revisions and tombstones");
        json!(rows)
    }
}

fn update(id: Uuid, properties: Value) -> Value {
    json!({"tool": "update", "args": {"id": id, "properties": properties}})
}

fn assert_committed(output: &Output, envelope: &Value, count: usize) {
    assert!(
        output.status.success(),
        "{envelope}; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(envelope["atomic"]["committed"], true, "{envelope}");
    assert_eq!(envelope["atomic"]["rolled_back"], false, "{envelope}");
    assert_eq!(envelope["summary"]["succeeded"], json!(count), "{envelope}");
    let results = envelope["results"].as_array().expect("per-op results");
    assert_eq!(results.len(), count, "{envelope}");
    for (index, entry) in results.iter().enumerate() {
        assert_eq!(entry["op_index"], json!(index), "{envelope}");
        assert_eq!(entry["tool"], "update", "{envelope}");
        assert_eq!(entry["ok"], true, "{envelope}");
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn atomic_finding_null_clears_only_declared_optional_keys() {
    let fixture = Fixture::new().await;
    let before = fixture.note(fixture.finding).await;
    let properties = before.properties.as_ref().unwrap().as_object().unwrap();
    assert!(properties.contains_key("severity"));
    assert!(properties.contains_key("confidence"));
    let (output, envelope) = fixture.run(
        &[
            update(
                fixture.finding,
                json!({"severity": null, "confidence": null, "custom": null}),
            ),
            update(
                fixture.observation,
                json!({"severity": null, "confidence": null, "custom": null}),
            ),
        ],
        true,
    );
    assert_committed(&output, &envelope, 2);
    for (id, finding) in [(fixture.finding, true), (fixture.observation, false)] {
        let note = fixture.note(id).await;
        let properties = note.properties.as_ref().unwrap().as_object().unwrap();
        for field in ["severity", "confidence"] {
            assert_eq!(properties.contains_key(field), !finding, "{note:?}");
            if !finding {
                assert_eq!(properties.get(field), Some(&Value::Null), "{note:?}");
            }
        }
        assert!(properties.contains_key("custom"), "{note:?}");
        assert_eq!(properties.get("custom"), Some(&Value::Null));
        assert_eq!(properties.get("keep"), Some(&json!(42)));
        assert_eq!(properties.get("kind_status"), Some(&json!("open")));
        assert_eq!(properties.get("evidence"), Some(&json!(["src/lib.rs:42"])));
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn atomic_finding_single_clear_preserves_omitted_fields() {
    for (cleared, retained, expected) in [
        ("severity", "confidence", "medium"),
        ("confidence", "severity", "high"),
    ] {
        let fixture = Fixture::new().await;
        let mut properties = json!({});
        properties[cleared] = Value::Null;
        let (output, envelope) = fixture.run(&[update(fixture.finding, properties)], true);
        assert_committed(&output, &envelope, 1);
        let note = fixture.note(fixture.finding).await;
        let properties = note.properties.as_ref().unwrap().as_object().unwrap();
        assert!(!properties.contains_key(cleared), "{note:?}");
        assert!(properties.contains_key(retained), "{note:?}");
        assert_eq!(properties.get(retained), Some(&json!(expected)));
        assert_eq!(properties.get("custom"), Some(&json!("before")));
        assert_eq!(properties.get("keep"), Some(&json!(42)));
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn atomic_finding_required_null_refuses_without_any_write() {
    let fixture = Fixture::new().await;
    for field in ["kind_status", "evidence"] {
        let before = fixture.snapshot().await;
        let mut invalid = json!({});
        invalid[field] = Value::Null;
        let (output, envelope) = fixture.run(
            &[
                update(fixture.observation, json!({"keep": 99})),
                update(fixture.finding, invalid),
            ],
            true,
        );
        assert!(!output.status.success(), "{envelope}");
        assert_eq!(envelope["atomic"]["committed"], false, "{envelope}");
        assert_eq!(envelope["atomic"]["failed_op_index"], 1, "{envelope}");
        assert_eq!(envelope["results"][1]["ok"], false, "{envelope}");
        assert!(
            envelope["results"][1]["error"].to_string().contains(field),
            "{envelope}"
        );
        assert_eq!(
            fixture.snapshot().await,
            before,
            "{field} refusal wrote notes"
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn create_finding_null_remains_refused_without_any_write() {
    let fixture = Fixture::new().await;
    for field in ["severity", "confidence"] {
        let before = fixture.snapshot().await;
        let mut properties = json!({});
        properties[field] = Value::Null;
        // Create is outside the atomic v1 allowlist. Exercise its ordinary CLI
        // path so refusal proves owner validation, not atomic admissibility.
        let (output, envelope) = fixture.run(
            &[json!({"tool": "create", "args": {
                "kind": "finding", "title": "Invalid null enum", "properties": properties
            }})],
            false,
        );
        assert!(!output.status.success(), "{envelope}");
        assert_eq!(envelope["failed"], 1, "{envelope}");
        assert_eq!(envelope["succeeded"], 0, "{envelope}");
        assert!(
            envelope["failures"][0]["error"]["message"]
                .to_string()
                .contains(&format!("{field} must be a string")),
            "{envelope}"
        );
        assert_eq!(
            fixture.snapshot().await,
            before,
            "invalid create wrote a note"
        );
    }
}
