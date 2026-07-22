use std::path::Path;

use clap::Parser;
use khive_runtime::{KhiveRuntime, RuntimeConfig};
use khive_storage::types::{SqlStatement, SqlValue};
use kkernel::exec::{run_exec, ExecArgs};

fn initialize_store(path: &Path) {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(path.to_path_buf()),
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::no_embeddings()
    })
    .expect("initialize isolated store");
    drop(runtime);
}

async fn count_named_entities(path: &Path, name: &str) -> i64 {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(path.to_path_buf()),
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::no_embeddings()
    })
    .expect("open isolated store");
    let sql = runtime.sql();
    let mut reader = sql.reader().await.expect("open isolated store reader");
    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT COUNT(*) AS count FROM entities WHERE name = ?1".to_string(),
            params: vec![SqlValue::Text(name.to_string())],
            label: Some("config_env_db_isolation_count".to_string()),
        })
        .await
        .expect("count named entities")
        .expect("count query returns one row");
    match row.get("count") {
        Some(SqlValue::Integer(count)) => *count,
        other => panic!("unexpected count value: {other:?}"),
    }
}

#[tokio::test]
async fn khive_config_override_keeps_create_out_of_default_store() {
    let root = tempfile::tempdir().expect("isolated test root");
    let default_home = root.path().join("default-home");
    let sandbox_dir = root.path().join("sandbox");
    std::fs::create_dir_all(default_home.join(".khive")).expect("create default store dir");
    std::fs::create_dir_all(&sandbox_dir).expect("create sandbox store dir");

    let default_db = default_home.join(".khive/khive.db");
    let overridden_db = sandbox_dir.join("khive.db");
    initialize_store(&default_db);
    initialize_store(&overridden_db);

    let config_path = sandbox_dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[[backends]]
name = "main"
kind = "sqlite"
path = "{}"
"#,
            overridden_db.display()
        ),
    )
    .expect("write sandbox config");

    std::env::set_var("HOME", &default_home);
    std::env::set_var("KHIVE_CONFIG", &config_path);
    std::env::set_var("KHIVE_NO_DAEMON", "1");
    std::env::set_var("KHIVE_LOCK", sandbox_dir.join("khived.lock"));
    std::env::set_var(
        "KHIVE_RECOVERER_LOCK",
        sandbox_dir.join("khived.recoverer.lock"),
    );
    std::env::set_var("KHIVE_PACKS", "kg");
    for key in "KHIVE_DB KHIVE_EMBEDDING_MODEL KHIVE_ADDITIONAL_EMBEDDING_MODELS \
                KHIVE_ACTOR KHIVE_REQUIRE_ATTRIBUTED_ACTOR KHIVE_BLOB_ROOT"
        .split_whitespace()
    {
        std::env::remove_var(key);
    }

    let entity_name = "config-selected-store-only";
    let args = ExecArgs::parse_from([
        "exec",
        &format!(r#"create(items=[{{"kind":"concept","name":"{entity_name}"}}])"#),
    ]);
    assert_eq!(args.config.as_deref(), Some(config_path.as_path()));
    run_exec(args).await.expect("create through kkernel exec");

    let overridden_count = count_named_entities(&overridden_db, entity_name).await;
    let default_count = count_named_entities(&default_db, entity_name).await;
    assert_eq!(
        (overridden_count, default_count),
        (1, 0),
        "create must land only in the KHIVE_CONFIG-selected store, never the HOME-derived default"
    );
}
