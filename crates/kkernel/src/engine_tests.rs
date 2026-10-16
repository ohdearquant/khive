use super::*;
use tempfile::TempDir;

// Engine tests must be hermetic: with `db: None` the command resolves to the
// operator's real `~/.khive/khive.db` and runs migration writes against
// it. Always point tests at a throwaway temp DB (review #531). Keep the
// returned TempDir alive for the test's duration.
fn temp_db() -> (TempDir, Option<std::path::PathBuf>) {
    let tmp = TempDir::new().expect("temp dir");
    let path = tmp.path().join("engine_test.db");
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(path.clone()),
        volume_lock_dir: Some(tmp.path().join("volume-locks")),
        ..RuntimeConfig::no_embeddings()
    })
    .expect("create and migrate engine test database");
    drop(runtime);
    // SQLite connection close is deferred, so the drop above can leave
    // writable `-wal`/`-shm` sidecars behind; the commands under test
    // open this database read-only, which accepts only the frozen
    // snapshot form. No engine test writes after this point.
    #[cfg(unix)]
    khive_storage::test_support::freeze_snapshot_sidecars(&path);
    (tmp, Some(path))
}

#[tokio::test]
async fn engine_list_empty_ok() {
    let (_tmp, db) = temp_db();
    let args = EngineListArgs { human: false, db };
    // Should not panic even when no models are registered yet.
    cmd_engine_list(args)
        .await
        .expect("engine list succeeds on empty registry");
}

#[tokio::test]
async fn engine_status_empty_ok() {
    let (_tmp, db) = temp_db();
    let args = EngineStatusArgs {
        engine: "mE5-small".into(),
        human: false,
        db,
    };
    cmd_engine_status(args)
        .await
        .expect("engine status succeeds on empty registry");
}

#[tokio::test]
async fn registered_model_keeps_the_seven_field_cli_projection() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("registered-model.db");
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(path.clone()),
        volume_lock_dir: Some(temp.path().join("volume-locks")),
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    runtime
        .backend()
        .register_embedding_model("cli-engine", "cli-model", "v1", 384)
        .unwrap();
    let registry = runtime.list_embedding_models(None).await.unwrap();
    assert_eq!(registry.len(), 1);
    let activated_at = registry[0].activated_at;
    assert!(activated_at.is_some());
    drop(runtime);
    #[cfg(unix)]
    khive_storage::test_support::freeze_snapshot_sidecars(&path);

    let records = fetch_model_records(Some(&path), Some("cli-engine"))
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&records).unwrap(),
        serde_json::json!([{
            "engine_name": "cli-engine",
            "model_id": "cli-model",
            "key_version": "v1",
            "dimensions": 384,
            "status": "active",
            "activated_at": activated_at,
            "superseded_at": null,
        }]),
    );
}

#[test]
fn engine_migrate_returns_not_implemented() {
    let (_tmp, db) = temp_db();
    let args = EngineMigrateArgs {
        engine: "mE5-small".into(),
        to: Some("bge-small-en-v1.5".into()),
        resume: false,
        abort: false,
        db,
    };
    let err = cmd_engine_migrate(args).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("not yet implemented"),
        "expected 'not yet implemented' in error, got: {msg}"
    );
    assert!(
        msg.contains("#2873"),
        "expected follow-up issue reference in error, got: {msg}"
    );
}

#[test]
fn engine_migrate_resume_returns_not_implemented() {
    let (_tmp, db) = temp_db();
    let args = EngineMigrateArgs {
        engine: "mE5-small".into(),
        to: None,
        resume: true,
        abort: false,
        db,
    };
    let err = cmd_engine_migrate(args).unwrap_err();
    assert!(err.to_string().contains("not yet implemented"));
}

#[test]
fn engine_migrate_abort_returns_not_implemented() {
    let (_tmp, db) = temp_db();
    let args = EngineMigrateArgs {
        engine: "mE5-small".into(),
        to: None,
        resume: false,
        abort: true,
        db,
    };
    let err = cmd_engine_migrate(args).unwrap_err();
    assert!(err.to_string().contains("not yet implemented"));
}

#[test]
fn drift_check_returns_not_implemented() {
    let (_tmp, db) = temp_db();
    let args = EngineDriftCheckArgs {
        engine: "mE5-small".into(),
        sample: 500,
        human: false,
        db,
    };
    let err = cmd_engine_drift_check(args).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("not yet implemented"),
        "expected 'not yet implemented' in error, got: {msg}"
    );
    assert!(
        msg.contains("#2873"),
        "expected follow-up issue reference in error, got: {msg}"
    );
}
