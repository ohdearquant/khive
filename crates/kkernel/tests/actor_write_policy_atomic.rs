//! Exercise the public CLI atomic path, which mints a broad namespace token.
//!
//! Atomic v1 rejects create before authorization. Update existing entities so
//! this regression reaches the gate, with a successful unrestricted control.

use std::path::Path;
use std::process::{Command, Output};

use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};
use serde_json::{json, Value};
use tempfile::TempDir;
use uuid::Uuid;

fn fixture_config(db: &Path) -> RuntimeConfig {
    RuntimeConfig {
        db_path: Some(db.to_path_buf()),
        packs: vec!["kg".to_owned()],
        actor_id: None,
        brain_profile: None,
        ..RuntimeConfig::no_embeddings()
    }
}

fn run_atomic(home: &TempDir, actor: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kkernel"))
        .args(["exec", "--atomic", "--strict", "--ops-file"])
        .arg(home.path().join("updates.jsonl"))
        .arg("--config")
        .arg(home.path().join("config.toml"))
        .arg("--db")
        .arg(home.path().join("gate.db"))
        .args([
            "--actor",
            actor,
            "--expect-actor",
            actor,
            "--namespace",
            "local",
        ])
        .current_dir(home.path())
        .env_clear()
        .env("HOME", home.path())
        .env("TMPDIR", home.path())
        .env("KHIVE_NO_DAEMON", "1")
        .env("KHIVE_SOCKET", home.path().join("unused.sock"))
        .env("KHIVE_PACKS", "kg")
        .env("RUST_LOG", "error")
        .output()
        .expect("run the actual kkernel binary")
}

async fn entity_snapshot(runtime: &KhiveRuntime, ids: &[Uuid]) -> Vec<Value> {
    // Query through the live fixture pool. A database with a writable WAL
    // sidecar is not a frozen read-only snapshot, even after one handle drops.
    let token = runtime
        .authorize(Namespace::local())
        .expect("fixture token");
    let mut rows = Vec::new();
    for &id in ids {
        let entity = runtime
            .get_entity(&token, id)
            .await
            .expect("fixture entity");
        rows.push(serde_json::to_value(entity).expect("serialize complete entity row"));
    }
    rows
}

#[tokio::test]
async fn atomic_updates_deny_restricted_actor_without_domain_changes_and_allow_writer() {
    let home = tempfile::tempdir().expect("isolated CLI home");
    let db = home.path().join("gate.db");
    std::fs::write(
        home.path().join("config.toml"),
        "[gate]\ngranted_actors = ['seat:duty', 'seat:writer']\ndeny_writes_for = ['*:duty']\n",
    )
    .expect("write enrollment and write restrictions");

    let runtime = KhiveRuntime::new(fixture_config(&db)).expect("model-less seed runtime");
    let token = runtime.authorize(Namespace::local()).expect("seed token");
    let mut ids = Vec::new();
    for name in ["Atomic policy first", "Atomic policy second"] {
        let entity = runtime
            .create_entity(
                &token,
                "concept",
                None,
                name,
                None,
                None,
                vec!["before".to_owned()],
            )
            .await
            .expect("seed existing entity");
        ids.push(entity.id);
    }
    let before = entity_snapshot(&runtime, &ids).await;
    // Tag-only updates commit domain changes without invoking an embedding model.
    let operations: String = ids
        .iter()
        .map(|id| {
            format!(
                "{}\n",
                json!({"tool": "update", "args": {"id": id, "tags": ["after"]}})
            )
        })
        .collect();
    std::fs::write(home.path().join("updates.jsonl"), operations).expect("write atomic batch");

    let denied = run_atomic(&home, "seat:duty");
    let denied_stderr = String::from_utf8_lossy(&denied.stderr);
    assert!(
        !denied.status.success(),
        "restricted atomic batch must fail"
    );
    assert!(
        denied_stderr.contains("authorize namespace for --atomic")
            && denied_stderr.contains("deny_writes_for"),
        "must refuse at broad-token authorization, not parsing or enrollment: {denied_stderr}"
    );
    assert_eq!(
        entity_snapshot(&runtime, &ids).await,
        before,
        "neither domain row, including timestamps, may change after refusal"
    );

    let allowed = run_atomic(&home, "seat:writer");
    assert!(
        allowed.status.success(),
        "unrestricted enrolled writer must succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&allowed.stdout),
        String::from_utf8_lossy(&allowed.stderr)
    );
    let envelope: Value = serde_json::from_slice(&allowed.stdout).expect("atomic JSON envelope");
    assert_eq!(envelope["atomic"]["committed"], true, "{envelope}");
    assert_eq!(envelope["summary"]["succeeded"], 2, "{envelope}");
    let after = entity_snapshot(&runtime, &ids).await;
    for (old, new) in before.iter().zip(after) {
        assert_eq!(new["tags"], json!(["after"]));
        assert_eq!(new["id"], old["id"]);
        assert_eq!(new["name"], old["name"]);
    }
}
