#[test]
fn resolve_blob_root_prefers_env_var() {
    if crate::test_process::run_in_child(|command| {
        command.env("KHIVE_BLOB_ROOT", "/tmp/env-override-root");
    }) {
        return;
    }
    let _guard = ENV_LOCK.lock().unwrap();
    let resolved = resolve_blob_root(Some(Path::new("/db/dir")), Some(Path::new("/cfg/root")));
    assert_eq!(resolved.unwrap(), PathBuf::from("/tmp/env-override-root"));
}

#[test]
fn resolve_blob_root_prefers_config_over_default() {
    if crate::test_process::run_in_child(|command| {
        command.env_remove("KHIVE_BLOB_ROOT");
    }) {
        return;
    }
    let _guard = ENV_LOCK.lock().unwrap();
    let resolved = resolve_blob_root(Some(Path::new("/db/dir")), Some(Path::new("/cfg/root")));
    assert_eq!(resolved.unwrap(), PathBuf::from("/cfg/root"));
}

#[test]
fn resolve_blob_root_defaults_beside_db_dir() {
    if crate::test_process::run_in_child(|command| {
        command.env_remove("KHIVE_BLOB_ROOT");
    }) {
        return;
    }
    let _guard = ENV_LOCK.lock().unwrap();
    let resolved = resolve_blob_root(Some(Path::new("/db/dir")), None);
    assert_eq!(resolved.unwrap(), PathBuf::from("/db/dir/blobs"));
}

#[test]
fn resolve_blob_root_errors_with_no_env_config_or_db_dir() {
    if crate::test_process::run_in_child(|command| {
        command.env_remove("KHIVE_BLOB_ROOT");
    }) {
        return;
    }
    let _guard = ENV_LOCK.lock().unwrap();
    let resolved = resolve_blob_root(None, None);
    assert!(resolved.is_err());
}

// Retain the original fixture lock within each isolated child.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
