use super::*;

// MUST-FAIL: bypassing syntax admission reaches the existing-file probe and
// returns its error instead. Neither URI nor relative spellings are paths
// this resolver may probe, even when the named file is missing.
#[test]
fn explicit_target_syntax_refuses_before_target_probe() {
    let tmp = tempfile::tempdir().expect("isolated target fixture");
    let missing = tmp.path().join("missing.db");
    for db in [
        format!("file:{}", missing.display()),
        format!("file:{}?mode=rw", missing.display()),
        format!("{}?mode=rw", missing.display()),
        "relative-map.db".to_string(),
        "".to_string(),
    ] {
        let error = resolve_target_db(Some(&db), tmp.path(), None, &[])
            .expect_err("non-plain or relative explicit target must refuse");
        assert!(error.contains("absolute, plain filesystem path"), "{error}");
    }
    assert!(!missing.exists());
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
}

#[test]
fn default_target_is_workspace_local() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().canonicalize().unwrap().join("some-repo");
    std::fs::create_dir(&path).expect("create ingest workspace");
    let db = resolve_target_db(None, &path, None, &[]).expect("default resolves");
    assert_eq!(db, path.join(".khive").join("code-map.db"));
}

#[test]
fn explicit_production_path_is_rejected() {
    // Reads HOME (does not mutate it), but still takes the shared lock:
    // an unguarded read here can race a concurrently-running test that
    // mutates HOME via `HomeGuard` (cargo test runs tests in the same
    // binary in parallel by default).
    let _guard = KHIVE_DB_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = std::env::var("HOME").expect("HOME set in test env");
    let prod = format!("{home}/.khive/khive.db");
    let err = resolve_target_db(Some(&prod), Path::new("/tmp/some-repo"), None, &[])
        .expect_err("must reject the shared production database");
    assert!(err.contains("shared production database"));
}

#[test]
fn explicit_dedicated_path_is_accepted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path().join("code-map.db");
    std::fs::write(&target, b"").expect("pre-create dedicated map");
    let db = resolve_target_db(
        Some(target.to_str().unwrap()),
        Path::new("/tmp/some-repo"),
        None,
        &[],
    )
    .expect("dedicated path accepted");
    assert_eq!(db, target);
}

#[cfg(any(unix, windows))]
#[test]
fn final_component_symlinks_refuse_while_an_independent_copy_is_admitted() {
    fn link_file(target: &Path, link: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).expect("create file symlink");
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(target, link).expect(
            "Windows CI needs Developer Mode or symlink privilege; do not skip this witness",
        );
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let protected = tmp.path().join("protected.db");
    let dedicated = tmp.path().join("dedicated.db");
    std::fs::write(&protected, b"protected bytes").unwrap();
    std::fs::write(&dedicated, b"dedicated bytes").unwrap();

    let explicit_link = tmp.path().join("explicit-link.db");
    link_file(&dedicated, &explicit_link);
    let error = resolve_target_db(
        Some(explicit_link.to_str().unwrap()),
        tmp.path(),
        Some(&protected),
        &[],
    )
    .expect_err("an explicit final-component symlink must refuse");
    assert!(error.contains("final-component symlink"), "{error}");

    let ingest = tmp.path().join("source");
    let default_dir = ingest.join(".khive");
    std::fs::create_dir_all(&default_dir).unwrap();
    let default_link = default_dir.join("code-map.db");
    link_file(&dedicated, &default_link);
    let error = resolve_target_db(None, &ingest, Some(&protected), &[])
        .expect_err("an existing default final-component symlink must refuse");
    assert!(error.contains("final-component symlink"), "{error}");

    let protected_link = tmp.path().join("protected-link.db");
    link_file(&protected, &protected_link);
    assert!(
        resolve_target_db(
            Some(protected_link.to_str().unwrap()),
            tmp.path(),
            Some(&protected),
            &[],
        )
        .is_err(),
        "a symlink to production must also refuse"
    );

    let independent_copy = tmp.path().join("independent-copy.db");
    std::fs::copy(&protected, &independent_copy).unwrap();
    assert_eq!(
        resolve_target_db(
            Some(independent_copy.to_str().unwrap()),
            tmp.path(),
            Some(&protected),
            &[],
        ),
        Ok(independent_copy)
    );
    assert_eq!(std::fs::read(&protected).unwrap(), b"protected bytes");
    assert_eq!(std::fs::read(&dedicated).unwrap(), b"dedicated bytes");
    assert!(std::fs::symlink_metadata(&explicit_link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(std::fs::symlink_metadata(&default_link)
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn declared_backend_and_its_event_companions_are_protected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let backend = tmp.path().join("secondary.db");
    let events = khive_runtime::events_split::events_db_path_beside(&backend);
    for target in [
        backend.clone(),
        append_sqlite_suffix(&backend, "-journal"),
        append_sqlite_suffix(&backend, "-wal"),
        append_sqlite_suffix(&backend, "-shm"),
        events.clone(),
        append_sqlite_suffix(&events, "-journal"),
        append_sqlite_suffix(&events, "-wal"),
        append_sqlite_suffix(&events, "-shm"),
    ] {
        std::fs::write(&target, b"unchanged").expect("sentinel file");
        let error = resolve_target_db(
            Some(target.to_str().expect("utf8 temp path")),
            tmp.path(),
            None,
            std::slice::from_ref(&backend),
        )
        .expect_err("known production member must refuse");
        assert!(error.contains(&target.display().to_string()), "{error}");
        assert_eq!(std::fs::read(&target).unwrap(), b"unchanged");
    }
}

#[cfg(unix)]
#[test]
fn symlinked_backend_physical_wal_companion_is_protected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let declared_dir = tmp.path().join("declared");
    let physical_dir = tmp.path().join("physical");
    std::fs::create_dir_all(&declared_dir).unwrap();
    std::fs::create_dir_all(&physical_dir).unwrap();
    let physical = physical_dir.join("real.db");
    let declared = declared_dir.join("khive.db");
    let physical_wal = append_sqlite_suffix(&physical, "-wal");
    std::fs::write(&physical, b"backend").unwrap();
    std::fs::write(&physical_wal, b"wal sentinel").unwrap();
    std::os::unix::fs::symlink(&physical, &declared).unwrap();

    let error = resolve_target_db(
        Some(physical_wal.to_str().unwrap()),
        tmp.path(),
        None,
        &[declared],
    )
    .expect_err("the physical SQLite WAL companion must be protected");
    assert!(
        error.contains(&physical_wal.display().to_string()),
        "{error}"
    );
    assert_eq!(std::fs::read(&physical_wal).unwrap(), b"wal sentinel");
}

#[cfg(unix)]
#[test]
fn dangling_backend_physical_wal_companion_is_protected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let physical = tmp.path().join("physical.db");
    let declared = tmp.path().join("declared.db");
    let physical_wal = append_sqlite_suffix(&physical, "-wal");
    std::fs::write(&physical_wal, b"wal sentinel").unwrap();
    std::os::unix::fs::symlink(&physical, &declared).unwrap();
    assert!(!physical.exists());

    let error = resolve_target_db(
        Some(physical_wal.to_str().unwrap()),
        tmp.path(),
        None,
        &[declared],
    )
    .expect_err("a dangling backend must protect the physical WAL companion");
    assert!(
        error.contains(&physical_wal.display().to_string()),
        "{error}"
    );
    assert_eq!(std::fs::read(&physical_wal).unwrap(), b"wal sentinel");
    assert!(!physical.exists());
}

#[cfg(unix)]
#[test]
fn dangling_production_backend_and_default_map_share_destination_is_refused() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let physical = tmp.path().join("physical.db");
    let declared = tmp.path().join("declared.db");
    std::os::unix::fs::symlink(&physical, &declared).unwrap();

    let ingest = tmp.path().join("source");
    let default_dir = ingest.join(".khive");
    std::fs::create_dir_all(&default_dir).unwrap();
    let default_map = default_dir.join("code-map.db");
    std::os::unix::fs::symlink(&physical, &default_map).unwrap();
    assert!(!physical.exists());

    let error = resolve_target_db(None, &ingest, Some(&declared), &[])
        .expect_err("a default map must not create a dangling production destination");
    assert!(error.contains(&physical.display().to_string()), "{error}");
    assert!(!physical.exists());
    assert_eq!(std::fs::read_link(&default_map).unwrap(), physical);
}

#[cfg(unix)]
#[test]
fn dangling_default_symlink_to_protected_events_store_is_refused() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let backend = tmp.path().join("backend.db");
    let events = khive_runtime::events_split::events_db_path_beside(&backend);
    let ingest = tmp.path().join("source");
    let default_dir = ingest.join(".khive");
    std::fs::create_dir_all(&default_dir).unwrap();
    let default_map = default_dir.join("code-map.db");
    let relative_events = Path::new("../..").join(events.file_name().unwrap());
    std::os::unix::fs::symlink(&relative_events, &default_map).unwrap();
    assert!(!events.exists());

    let error = resolve_target_db(None, &ingest, Some(&backend), &[])
        .expect_err("a dangling default link must not create a protected event store");
    assert!(error.contains(&events.display().to_string()), "{error}");
    assert!(!events.exists());
    assert_eq!(std::fs::read_link(&default_map).unwrap(), relative_events);
}

#[cfg(any(unix, windows))]
#[test]
fn hardlink_alias_is_protected_but_byte_copy_is_a_distinct_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let backend = tmp.path().join("secondary.db");
    let alias = tmp.path().join("alias.db");
    let copy = tmp.path().join("copy.db");
    std::fs::write(&backend, b"same bytes").unwrap();
    std::fs::hard_link(&backend, &alias).unwrap();
    std::fs::copy(&backend, &copy).unwrap();
    let known = [backend];
    let error = resolve_target_db(Some(alias.to_str().unwrap()), tmp.path(), None, &known)
        .expect_err("hard link has the same file identity");
    assert!(error.contains(&known[0].display().to_string()), "{error}");
    assert_eq!(
        resolve_target_db(Some(copy.to_str().unwrap()), tmp.path(), None, &known),
        Ok(copy)
    );
}

#[cfg(unix)]
#[test]
fn identity_probe_error_refuses_instead_of_falling_back_to_pathnames() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path().join("dedicated.db");
    let backend = tmp.path().join("backend.db");
    let loop_target = tmp.path().join("loop.db");
    std::fs::write(&target, b"").unwrap();
    std::os::unix::fs::symlink(&loop_target, &backend).unwrap();
    std::os::unix::fs::symlink(&backend, &loop_target).unwrap();

    let error = resolve_target_db(Some(target.to_str().unwrap()), tmp.path(), None, &[backend])
        .expect_err("an uninspectable production member cannot be ignored");
    assert!(
        error.contains("cannot establish target database identity"),
        "{error}"
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"");
}

#[test]
fn nondefault_configured_production_db_is_rejected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let prod = tmp.path().join("srv-main.db");
    std::fs::write(&prod, b"").expect("create sentinel file");
    let err = resolve_target_db(
        Some(prod.to_str().unwrap()),
        Path::new("/tmp/some-repo"),
        Some(&prod),
        &[],
    )
    .expect_err("must reject the runtime's actual configured production db");
    assert!(err.contains("shared production database"));
}

#[test]
fn symlinked_parent_alias_of_configured_db_is_rejected_even_before_file_exists() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let real_dir = tmp.path().join("real");
    std::fs::create_dir_all(&real_dir).expect("mkdir");
    let link_dir = tmp.path().join("link");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real_dir, &link_dir).expect("symlink");
    #[cfg(not(unix))]
    std::fs::create_dir_all(&link_dir).expect("mkdir fallback");

    let configured = real_dir.join("main.db");
    // Neither the configured db file nor the aliased candidate exists yet
    // — both parents do, and the alias must still be caught.
    let candidate = link_dir.join("main.db");
    let err = resolve_target_db(
        Some(candidate.to_str().unwrap()),
        Path::new("/tmp/some-repo"),
        Some(&configured),
        &[],
    )
    .expect_err("symlinked-parent alias of the configured db must be rejected");
    assert!(err.contains("shared production database"));
}

/// Serializes tests that mutate the process-wide `KHIVE_DB` env var —
/// `std::env::set_var`/`remove_var` race across parallel `cargo test`
/// threads otherwise.
static KHIVE_DB_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn env_khive_db_is_fenced_when_config_db_path_is_unresolved() {
    let _guard = KHIVE_DB_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let env_db = tmp.path().join("env-configured.db");
    std::fs::write(&env_db, b"").expect("create sentinel file");
    // SAFETY: serialized by KHIVE_DB_ENV_LOCK above.
    unsafe {
        std::env::set_var("KHIVE_DB", &env_db);
    }
    let result = resolve_target_db(
        Some(env_db.to_str().unwrap()),
        Path::new("/tmp/some-repo"),
        None, // config().db_path unresolved — the #1042 gap
        &[],
    );
    unsafe {
        std::env::remove_var("KHIVE_DB");
    }
    let err = result.expect_err("KHIVE_DB must be fenced even when runtime_db_path is None");
    assert!(err.contains("shared production database"));
}

#[test]
fn dedicated_path_still_accepted_with_no_khive_db_env_and_unresolved_config() {
    let _guard = KHIVE_DB_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: serialized by KHIVE_DB_ENV_LOCK above.
    unsafe {
        std::env::remove_var("KHIVE_DB");
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path().join("code-map.db");
    std::fs::write(&target, b"").expect("pre-create dedicated map");
    let db = resolve_target_db(
        Some(target.to_str().unwrap()),
        Path::new("/tmp/some-repo"),
        None,
        &[],
    )
    .expect("dedicated path accepted with no env override");
    assert_eq!(db, target);
}

/// RAII guard: clears `HOME` for the test body and restores the prior
/// value on drop (including on panic/unwind) — mirrors the `HomeGuard`
/// pattern in `khive-mcp/src/serve.rs`. Callers must hold
/// `KHIVE_DB_ENV_LOCK` for the guard's whole lifetime; this type does not
/// take the lock itself.
struct HomeGuard {
    original: Option<std::ffi::OsString>,
}

impl HomeGuard {
    fn clear() -> Self {
        let original = std::env::var_os("HOME");
        // SAFETY: caller holds KHIVE_DB_ENV_LOCK for this guard's lifetime.
        unsafe {
            std::env::remove_var("HOME");
        }
        Self { original }
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        // SAFETY: caller holds KHIVE_DB_ENV_LOCK for this guard's lifetime.
        unsafe {
            match &self.original {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
    }
}

/// #1062 H2 regression: with `HOME` unset AND no `KHIVE_DB` override, the
/// canonical resolver (`khive_runtime::config::resolve_db_anchor(None)`)
/// still falls back to `./.khive/khive.db` — the fence's default
/// forbidden path must be derived the SAME way, or this exact
/// unresolved-config branch resolves the production db and lets it
/// through (the prior HOME-only `default_production_db_path` returned
/// `None` here, disarming the fence entirely).
#[test]
fn production_default_is_fenced_when_home_unset_and_khive_db_absent() {
    let _guard = KHIVE_DB_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home_guard = HomeGuard::clear();
    // SAFETY: serialized by KHIVE_DB_ENV_LOCK above.
    unsafe {
        std::env::remove_var("KHIVE_DB");
    }
    let prod = khive_runtime::config::resolve_db_anchor(None)
        .expect("resolve_db_anchor(None) always resolves to Some(_)");
    // Explicit targets now require absolute spelling; retain this test's
    // separate assertion that the HOME-less canonical default is fenced.
    let prod = std::env::current_dir().unwrap().join(prod);
    let err = resolve_target_db(
        Some(prod.to_str().unwrap()),
        Path::new("/tmp/some-repo"),
        None, // config().db_path unresolved — the #1042/#1062 gap
        &[],
    )
    .expect_err("must reject the canonical production db even with HOME unset");
    assert!(err.contains("shared production database"));
}

/// Same #1062 H2 gap, with `KHIVE_DB` present but empty rather than
/// absent — the empty-string guard on the `KHIVE_DB` fallback (added for
/// #1042) must not be mistaken for "no override, so allow it through";
/// the canonical-default fence below it still has to fire.
#[test]
fn production_default_is_fenced_when_home_unset_and_khive_db_empty() {
    let _guard = KHIVE_DB_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home_guard = HomeGuard::clear();
    // SAFETY: serialized by KHIVE_DB_ENV_LOCK above.
    unsafe {
        std::env::set_var("KHIVE_DB", "");
    }
    let prod = khive_runtime::config::resolve_db_anchor(None)
        .expect("resolve_db_anchor(None) always resolves to Some(_)");
    let prod = std::env::current_dir().unwrap().join(prod);
    let result = resolve_target_db(
        Some(prod.to_str().unwrap()),
        Path::new("/tmp/some-repo"),
        None,
        &[],
    );
    // SAFETY: serialized by KHIVE_DB_ENV_LOCK above.
    unsafe {
        std::env::remove_var("KHIVE_DB");
    }
    let err = result
        .expect_err("must reject the canonical production db with HOME unset + empty KHIVE_DB");
    assert!(err.contains("shared production database"));
}
