//! ADR-115 reservation through real code.ingest dispatch. The wire schema has
//! no caller properties field, so explicitly seed a legacy map preimage through
//! the storage fixture boundary; reingest must not carry its unverified stamp.

use std::path::PathBuf;
use std::process::Command;

use khive_pack_code::{CodePack, CODE_INGEST_NAMESPACE};
use khive_pack_kg::KgPack;
use khive_runtime::{
    entity_fts_document, KhiveRuntime, Namespace, NamespaceToken, RuntimeConfig, RuntimeError,
    VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::types::SqlStatement;
use khive_storage::Entity;
use serde_json::{json, Value};
use tempfile::TempDir;
use uuid::Uuid;

const PROJECT: &str = "reservation_fixture";

/// Keep the host's production database outside the code-map fixture. A
/// symlinked HOME parent must remain a refusal for the production VFS guard.
fn run_with_private_home_in_child() -> bool {
    const CHILD_TEST: &str = "KHIVE_RESERVED_PROPERTIES_CHILD";
    let thread = std::thread::current();
    let name = thread.name().expect("libtest names its test threads");
    if std::env::var(CHILD_TEST).ok().as_deref() == Some(name) {
        return false;
    }
    let physical_temp = std::env::temp_dir()
        .canonicalize()
        .expect("physical temporary directory");
    let home = tempfile::tempdir_in(physical_temp).expect("private child HOME");
    let output = Command::new(std::env::current_exe().expect("integration test executable"))
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(CHILD_TEST, name)
        .env("HOME", home.path())
        .env("KHIVE_DB", "")
        .output()
        .expect("spawn isolated reservation test");
    assert!(
        output.status.success()
            && String::from_utf8_lossy(&output.stdout)
                .contains("test result: ok. 1 passed; 0 failed;"),
        "isolated reservation test failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

struct Fixture {
    _root: TempDir,
    source: PathBuf,
    target: PathBuf,
    map: KhiveRuntime,
    token: NamespaceToken,
    registry: VerbRegistry,
    id: Uuid,
}

impl Fixture {
    async fn new(properties: Value) -> Self {
        // The code-map VFS requires plain parent components. macOS's default
        // temporary path can traverse /var -> /private/var.
        let physical_temp = std::env::temp_dir()
            .canonicalize()
            .expect("physical temporary directory");
        let root = tempfile::tempdir_in(physical_temp).expect("isolated reservation fixture");
        let source = root.path().join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(
            source.join("Cargo.toml"),
            format!("[package]\nname = \"{PROJECT}\"\nversion = \"0.1.0\"\n"),
        )
        .unwrap();
        let target = root.path().join("map.db");
        // Keep the retained fixture runtime in rollback DELETE mode so the
        // later ingest does not need a quiescent transition from its live pool.
        let map = KhiveRuntime::new_code_map(
            RuntimeConfig {
                db_path: Some(target.clone()),
                actor_id: Some("test:code-reservation-map".into()),
                packs: vec![],
                ..RuntimeConfig::no_embeddings()
            },
            vec![],
            vec![],
        )
        .unwrap();
        let token = map.authorize(Namespace::local()).unwrap();
        let id = Uuid::new_v5(
            &CODE_INGEST_NAMESPACE,
            &serde_json::to_vec(&json!({
                "kind": "code-source-project", "source_project": PROJECT
            }))
            .unwrap(),
        );
        let mut entity = Entity::new(token.namespace().as_str(), "project", PROJECT);
        entity.id = id;
        entity.properties = Some(properties);
        // Deliberate legacy/preimage fixture: bypass normal admission only to
        // represent a previously stored forged/null stamp in the dedicated map.
        map.entities(&token)
            .unwrap()
            .upsert_entity(entity.clone())
            .await
            .unwrap();
        map.text(&token)
            .unwrap()
            .upsert_document(entity_fts_document(&entity))
            .await
            .unwrap();

        let caller = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            actor_id: Some("test:code-reservation-caller".into()),
            packs: vec![],
            ..RuntimeConfig::no_embeddings()
        })
        .unwrap();
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(caller.clone()));
        builder.register(CodePack::new(caller.clone()));
        let registry = builder.build().unwrap();
        caller.install_edge_rules(registry.all_edge_rules());
        Self {
            _root: root,
            source,
            target,
            map,
            token,
            registry,
            id,
        }
    }

    async fn ingest(&self) -> Result<Value, RuntimeError> {
        self.registry
            .dispatch(
                "code.ingest",
                json!({
                    "path": self.source,
                    "db": self.target,
                    "languages": ["rust"],
                    "tiers": ["l1"]
                }),
            )
            .await
    }

    async fn snapshot(&self) -> Value {
        let sql = self.map.sql();
        let mut reader = sql.reader().await.unwrap();
        let mut state = serde_json::Map::new();
        for (name, query) in [
            ("entities", "SELECT * FROM entities ORDER BY namespace, id"),
            ("edges", "SELECT * FROM graph_edges ORDER BY namespace, id"),
            ("fts", "SELECT rowid, * FROM fts_entities ORDER BY rowid"),
            (
                "fts_rowids",
                "SELECT * FROM fts_entities_rowids ORDER BY namespace, subject_id",
            ),
        ] {
            let rows = reader
                .query_all(SqlStatement {
                    sql: query.into(),
                    params: vec![],
                    label: Some("code_reservation_snapshot".into()),
                })
                .await
                .unwrap();
            state.insert(name.into(), serde_json::to_value(rows).unwrap());
        }
        Value::Object(state)
    }
}

// MUST-FAIL: removing source_ingest::gate_check's reservation call allows the
// real handler to refresh this preimage (including FTS) instead of refusing.
#[tokio::test]
async fn code_ingest_refuses_reserved_preimage_without_mutating_map() {
    if run_with_private_home_in_child() {
        return;
    }
    for stamp in [json!("forged"), Value::Null, json!({"copied": true})] {
        let fixture =
            Fixture::new(json!({"khive:secret_gate": stamp, "ordinary": "retained"})).await;
        let before = fixture.snapshot().await;
        let error = fixture
            .ingest()
            .await
            .expect_err("reserved preimage must refuse");
        assert!(
            matches!(&error, RuntimeError::InvalidInput(message)
                if message.contains("khive:secret_gate") && message.contains("runtime-owned")),
            "{error:?}"
        );
        assert_eq!(fixture.snapshot().await, before);
    }
}

// MUST-FAIL: recursively reserving nested keys breaks the second positive arm.
#[tokio::test]
async fn code_ingest_preserves_ordinary_and_nested_reserved_data() {
    if run_with_private_home_in_child() {
        return;
    }
    for properties in [
        json!({"ordinary": "retained"}),
        json!({"ordinary": {"khive:secret_gate": "ordinary nested data"}}),
    ] {
        let fixture = Fixture::new(properties.clone()).await;
        let response = fixture
            .ingest()
            .await
            .expect("ordinary data remains writable");
        assert_eq!(response["projects_updated"], 1);
        assert_eq!(response["fts_indexed"], 1);
        let entity = fixture
            .map
            .entities(&fixture.token)
            .unwrap()
            .get_entity(fixture.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            entity.properties.as_ref().unwrap()["ordinary"],
            properties["ordinary"]
        );
        assert_eq!(
            entity.properties.as_ref().unwrap()["source_project"],
            PROJECT
        );
        assert!(fixture
            .map
            .text(&fixture.token)
            .unwrap()
            .get_document(fixture.token.namespace().as_str(), fixture.id)
            .await
            .unwrap()
            .is_some());
    }
}
