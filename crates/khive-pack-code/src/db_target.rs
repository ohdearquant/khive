//! `code.ingest` target-database selection (ADR-085 Amendment 2 B7).
//!
//! `code.ingest` never writes to the shared production graph: it defaults to
//! a dedicated map database colocated with the ingested path, and rejects an
//! explicit `db` that resolves to any production store known to this process,
//! including declared backends, event stores and SQLite companions.

use std::path::{Path, PathBuf};

/// The shared production database's default location. Delegates to
/// `khive_runtime::config::resolve_db_anchor(None)` — the SAME resolver
/// `kkernel`/`khive-mcp` use to anchor the production database — rather than
/// re-deriving the fallback here. A prior hand-rolled version of this
/// function only handled `HOME` being SET, returning `None` (no forbidden
/// path at all) when `HOME` was absent, while the canonical resolver falls
/// back to `./.khive/khive.db`; that divergence is exactly what let the
/// fence fail open (#1062 H2). `resolve_db_anchor(None)` always resolves to
/// `Some(_)` (see its own doc comment).
fn default_production_db_path() -> Option<PathBuf> {
    khive_runtime::config::resolve_db_anchor(None)
}

/// Normalize `path` to its deepest *existing* canonical ancestor plus the
/// still-not-yet-created suffix appended back on. This lets two lexically
/// different paths that alias the same file — a symlinked parent directory,
/// or a `db` target whose final file does not exist yet (as is normal for a
/// not-yet-created database) — compare equal, instead of falling back to raw
/// lexical equality the moment either side is missing.
fn normalize(path: &Path) -> PathBuf {
    let mut existing: &Path = path;
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if existing.exists() {
            break;
        }
        let Some(name) = existing.file_name() else {
            break;
        };
        suffix.push(name.to_os_string());
        let Some(parent) = existing.parent() else {
            break;
        };
        existing = parent;
    }
    let mut base = existing
        .canonicalize()
        .unwrap_or_else(|_| existing.to_path_buf());
    for part in suffix.into_iter().rev() {
        base.push(part);
    }
    base
}

fn same_path(a: &Path, b: &Path) -> bool {
    normalize(a) == normalize(b)
}

fn existing_metadata(path: &Path) -> Result<Option<std::fs::Metadata>, String> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "code.ingest cannot establish target database identity for {}: {error}",
            path.display()
        )),
    }
}

fn same_existing_file(a: &Path, b: &Path) -> Result<Option<bool>, String> {
    let Some(a_metadata) = existing_metadata(a)? else {
        return Ok(None);
    };
    let Some(b_metadata) = existing_metadata(b)? else {
        return Ok(None);
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(Some(
            a_metadata.dev() == b_metadata.dev() && a_metadata.ino() == b_metadata.ino(),
        ))
    }
    #[cfg(windows)]
    {
        let _ = (a_metadata, b_metadata);
        // Stable std metadata does not expose a Windows file id. The helper
        // compares volume and file-index through read-only handles.
        same_file::is_same_file(a, b).map(Some).map_err(|error| {
            format!(
                "code.ingest cannot establish target database identity for {}: {error}",
                b.display()
            )
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (a_metadata, b_metadata);
        Err("code.ingest cannot establish target database identity on this platform".to_string())
    }
}

fn matches_protected_file(candidate: &Path, protected: &Path) -> Result<bool, String> {
    Ok(
        same_existing_file(candidate, protected)?
            .unwrap_or_else(|| same_path(candidate, protected)),
    )
}

fn append_sqlite_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn protected_store_members(base: &Path) -> Vec<PathBuf> {
    let events = khive_runtime::events_split::events_db_path_beside(base);
    [base, events.as_path()]
        .into_iter()
        .flat_map(|db| {
            [
                db.to_path_buf(),
                append_sqlite_suffix(db, "-journal"),
                append_sqlite_suffix(db, "-wal"),
                append_sqlite_suffix(db, "-shm"),
            ]
        })
        .collect()
}

/// Reject SQLite URI spellings and relative explicit targets before filesystem
/// access (ADR-085 E7). This is syntax admission, not an open-time identity fence.
pub(crate) fn validate_explicit_db_path(db: &str) -> Result<(), String> {
    if db.starts_with("file:") || db.contains('?') || !Path::new(db).is_absolute() {
        return Err(
            "code.ingest refuses this database target: explicit db must be an absolute, plain \
             filesystem path (no file: URI or ? query syntax)"
                .to_string(),
        );
    }
    Ok(())
}

/// Resolve the `db` verb argument into a concrete target database path,
/// defaulting to `<path>/.khive/code-map.db` when absent, and rejecting a
/// target that resolves to a production store known to this process: the
/// default anchor, the calling runtime's configured database (or `KHIVE_DB`
/// if unresolved), and every declared backend. Each store's event database
/// and SQLite companions are protected as well.
/// An explicit target must already be a regular file. This prevents typo-driven
/// creation before runtime construction; it does not pin identity or protect
/// against concurrent unlink/replacement between this check and SQLite open.
pub(crate) fn resolve_target_db(
    db_param: Option<&str>,
    ingest_path: &Path,
    runtime_db_path: Option<&Path>,
    declared_backend_db_paths: &[PathBuf],
) -> Result<PathBuf, String> {
    let candidate = match db_param {
        Some(p) => {
            validate_explicit_db_path(p)?;
            PathBuf::from(p)
        }
        None => ingest_path.join(".khive").join("code-map.db"),
    };

    let mut forbidden: Vec<PathBuf> = Vec::new();
    if let Some(prod) = default_production_db_path() {
        forbidden.push(prod);
    }
    match runtime_db_path {
        Some(runtime_db) => forbidden.push(runtime_db.to_path_buf()),
        // `config().db_path` is unresolved (not reachable by the production
        // daemon today, which always populates it at startup) — fall back to
        // `KHIVE_DB` directly so the fence is total rather than
        // total-in-practice: an operator running with an env-only override
        // and no resolved config path is still covered (#1042).
        None => {
            if let Ok(env_db) = std::env::var("KHIVE_DB") {
                if !env_db.is_empty() {
                    forbidden.push(PathBuf::from(env_db));
                }
            }
        }
    }

    forbidden.extend_from_slice(declared_backend_db_paths);
    for base in &forbidden {
        for forbidden_path in protected_store_members(base) {
            if matches_protected_file(&candidate, &forbidden_path)? {
                return Err(format!(
                    "code.ingest refuses to target the shared production database ({}); \
                     select a different dedicated map database",
                    forbidden_path.display()
                ));
            }
        }
    }
    if db_param.is_some() {
        let metadata = std::fs::metadata(&candidate).map_err(|error| {
            format!(
                "code.ingest explicit db {candidate:?} must be an existing regular file: {error}; \
                 omit db to create the workspace-local default"
            )
        })?;
        if !metadata.is_file() {
            return Err(format!(
                "code.ingest explicit db {candidate:?} must be an existing regular file; \
                 omit db to create the workspace-local default"
            ));
        }
    }
    Ok(candidate)
}

#[cfg(test)]
mod tests {
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
        let path = Path::new("/tmp/some-repo");
        let db = resolve_target_db(None, path, None, &[]).expect("default resolves");
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
}
