// MUST-FAIL: resolving the lock directory only inside the open closure lets
// the constructor create the database's parent directory before refusing.
#[test]
fn file_backed_runtime_without_a_lock_directory_creates_no_parent_directory() {
    let root = tempfile::tempdir().expect("fixture root");
    let top = root.path().join("missing");
    let config = RuntimeConfig {
        db_path: Some(top.join("nested").join("main.db")),
        disk_guard_config: Some(khive_db::EffectiveDiskGuardConfig::default()),
        volume_lock_dir: None,
        ..RuntimeConfig::no_embeddings()
    };
    let error = KhiveRuntime::new(config)
        .err()
        .expect("a writable open needs a lock directory");
    assert!(
        error.to_string().contains("KHIVE_VOLUME_LOCK_DIR"),
        "{error}"
    );
    assert!(!top.exists());
}
