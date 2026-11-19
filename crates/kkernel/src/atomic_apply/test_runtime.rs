/// Owns a file-backed runtime and removes its database directory after shutdown.
struct TestRuntime {
    runtime: KhiveRuntime,
    _temp_dir: tempfile::TempDir,
}

impl std::ops::Deref for TestRuntime {
    type Target = KhiveRuntime;

    fn deref(&self) -> &Self::Target {
        &self.runtime
    }
}

fn scratch_runtime() -> TestRuntime {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("atomic_apply_gtd.db");
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(path),
        volume_lock_dir: Some(dir.path().join("volume-locks")),
        embedding_model: None,
        additional_embedding_models: vec![],
        ..RuntimeConfig::default()
    })
    .expect("runtime");
    TestRuntime {
        runtime,
        _temp_dir: dir,
    }
}

#[test]
fn scratch_runtimes_own_distinct_volume_lock_namespaces() {
    let first = scratch_runtime();
    let second = scratch_runtime();
    let first_dir = first.config().volume_lock_dir.as_ref().unwrap();
    let second_dir = second.config().volume_lock_dir.as_ref().unwrap();
    assert_ne!(first_dir, second_dir);
    assert_eq!(first_dir.parent(), Some(first._temp_dir.path()));
    assert_eq!(second_dir.parent(), Some(second._temp_dir.path()));
    assert!(first_dir.is_dir());
    assert!(second_dir.is_dir());
    let cloned = first.config().clone();
    assert_eq!(cloned.volume_lock_dir.as_ref(), Some(first_dir));
    assert_eq!(cloned.db_path, first.config().db_path);
}
