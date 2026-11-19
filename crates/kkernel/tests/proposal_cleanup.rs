//! Real operator execution, write-policy denial, and pre-mutation atomic refusal.
use std::process::{Command, Output};

use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, VerbRegistryBuilder};
use khive_storage::{SqlStatement, SqlValue};
use serde_json::{json, Value};
use tempfile::TempDir;

fn command(home: &TempDir, actor: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kkernel"));
    command
        .args(["exec", "--strict", "--output-format", "json"])
        .arg("--config")
        .arg(home.path().join("config.toml"))
        .arg("--db")
        .arg(home.path().join("kg.db"))
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
        .env("KHIVE_VOLUME_LOCK_DIR", home.path().join("volume-locks"))
        .env("KHIVE_NO_DAEMON", "1")
        .env("KHIVE_SOCKET", home.path().join("unused.sock"))
        .env("KHIVE_PACKS", "kg")
        .env("RUST_LOG", "error");
    command
}

fn succeeded(output: Output) -> Value {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

async fn projection(runtime: &KhiveRuntime) -> Value {
    serde_json::to_value(
        runtime
            .sql()
            .reader()
            .await
            .unwrap()
            .query_all(SqlStatement::new(
                "SELECT * FROM proposals_open ORDER BY proposal_id",
                vec![],
            ))
            .await
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn operator_cleanup_obeys_write_policy_and_atomic_preflight() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        "[gate]\ngranted_actors = ['seat:duty', 'seat:writer']\ndeny_writes_for = ['*:duty']\n",
    )
    .unwrap();
    let runtime = KhiveRuntime::new_for_test(RuntimeConfig {
        db_path: Some(home.path().join("kg.db")),
        volume_lock_dir: Some(home.path().join("volume-locks")),
        packs: vec!["kg".into()],
        brain_profile: None,
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("local".into()));
    builder.register(KgPack::new(runtime.clone()));
    let registry = builder.build().unwrap();
    let proposal = registry.dispatch("propose", json!({
        "title":"Operator archive", "description":"CLI cleanup witness",
        "changeset":{"kind":"add_entity","entity":{"kind":"concept","name":"Operator archive"}}
    })).await.unwrap();
    let id = proposal["id"].as_str().unwrap();
    registry
        .dispatch("review", json!({"id":id,"decision":"reject"}))
        .await
        .unwrap();
    runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute(SqlStatement::new(
            "UPDATE proposals_open SET updated_at=1 WHERE proposal_id=?1",
            vec![SqlValue::Text(id.into())],
        ))
        .await
        .unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let entity = runtime
        .create_entity_with_embedding_report(
            &token,
            "concept",
            None,
            "Atomic sibling witness",
            None,
            None,
            vec!["before".into()],
        )
        .await
        .unwrap()
        .0;
    let before_entity = serde_json::to_value(&entity).unwrap();
    let before_projection = projection(&runtime).await;

    let denied = command(&home, "seat:duty")
        .arg(r#"kg.proposal_cleanup(older_than="1d")"#)
        .output()
        .unwrap();
    assert!(!denied.status.success());
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&denied.stdout),
        String::from_utf8_lossy(&denied.stderr)
    );
    assert!(
        diagnostic.contains("deny_writes_for"),
        "must reach policy, not fail parsing/enrollment: {diagnostic}"
    );
    assert_eq!(projection(&runtime).await, before_projection);

    std::fs::write(
        home.path().join("atomic.jsonl"),
        format!(
            "{}\n{}\n",
            json!({"tool":"update","args":{"id":entity.id.to_string(),"tags":["after"]}}),
            json!({"tool":"kg.proposal_cleanup","args":{"older_than":"1d"}})
        ),
    )
    .unwrap();
    let atomic = command(&home, "seat:writer")
        .arg("--atomic")
        .arg("--ops-file")
        .arg(home.path().join("atomic.jsonl"))
        .output()
        .unwrap();
    assert!(!atomic.status.success());
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&atomic.stdout),
        String::from_utf8_lossy(&atomic.stderr)
    );
    assert!(
        diagnostic.contains("kg.proposal_cleanup") && diagnostic.contains("atomic-admissible"),
        "{diagnostic}"
    );
    assert_eq!(projection(&runtime).await, before_projection);
    assert_eq!(
        serde_json::to_value(runtime.get_entity(&token, entity.id).await.unwrap()).unwrap(),
        before_entity
    );

    let help = succeeded(
        command(&home, "seat:writer")
            .arg("kg.proposal_cleanup(help=true)")
            .output()
            .unwrap(),
    );
    assert_eq!(help["results"][0]["result"]["visibility"], "internal");
    assert_eq!(
        projection(&runtime).await,
        before_projection,
        "help must not run the operator"
    );
    let first = succeeded(
        command(&home, "seat:writer")
            .arg(r#"kg.proposal_cleanup(older_than="1d")"#)
            .output()
            .unwrap(),
    );
    assert_eq!(first["results"][0]["result"], json!({"archived":1}));
    let second = succeeded(
        command(&home, "seat:writer")
            .arg(r#"kg.proposal_cleanup(older_than="1d")"#)
            .output()
            .unwrap(),
    );
    assert_eq!(second["results"][0]["result"], json!({"archived":0}));
}
