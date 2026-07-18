//! `code.ingest` target-database selection (ADR-085 Amendment 2 B7).
//!
//! `code.ingest` never writes to the shared production graph: it defaults to
//! a dedicated map database colocated with the ingested path, and rejects an
//! explicit `db` that resolves to the well-known production database path or
//! to the calling runtime's actual configured database, with no override.

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

/// `(device, inode)` for `path` and, when present, its SQLite WAL/SHM
/// companion files — a hard link to a protected database shares the target's
/// inode even though `normalize`'s canonicalized *path* differs (hard links
/// are distinct directory entries pointing at one inode; there is no
/// symlink for `canonicalize` to resolve through). Absent files are simply
/// omitted, not an error: a not-yet-created candidate has no identity to
/// compare here and falls back to `normalize`'s path-alias check instead.
#[cfg(unix)]
fn file_identities(path: &Path) -> Vec<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;

    let mut ids = Vec::new();
    let mut probe = |p: &Path| {
        if let Ok(meta) = std::fs::metadata(p) {
            ids.push((meta.dev(), meta.ino()));
        }
    };
    probe(path);
    for suffix in ["-wal", "-shm"] {
        let mut companion = path.as_os_str().to_os_string();
        companion.push(suffix);
        probe(Path::new(&companion));
    }
    ids
}

#[cfg(not(unix))]
fn file_identities(_path: &Path) -> Vec<(u64, u64)> {
    Vec::new()
}

/// True when `a` and `b` name the same database file: either the same path
/// once symlinked-parent aliasing is normalized away, or — the hard-link
/// case `normalize` cannot see, since a hard link introduces no symlink for
/// canonicalization to follow — the same `(device, inode)` identity on disk,
/// checked against both files' main path and any present `-wal`/`-shm`
/// companion.
fn same_path(a: &Path, b: &Path) -> bool {
    if normalize(a) == normalize(b) {
        return true;
    }
    let a_ids = file_identities(a);
    if a_ids.is_empty() {
        return false;
    }
    let b_ids = file_identities(b);
    a_ids.iter().any(|id| b_ids.contains(id))
}

/// The production-database paths a target must never alias: the well-known
/// `$HOME/.khive/khive.db` default plus whichever of `runtime_db_path` or
/// `KHIVE_DB` names the actually-configured production database (see
/// `resolve_target_db`'s doc comment for why both are needed).
fn forbidden_db_paths(runtime_db_path: Option<&Path>) -> Vec<PathBuf> {
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
    forbidden
}

fn reject_if_forbidden(candidate: &Path, forbidden: &[PathBuf]) -> Result<(), String> {
    for forbidden_path in forbidden {
        if same_path(candidate, forbidden_path) {
            return Err(format!(
                "code.ingest refuses to target the shared production database ({}); pass \
                 db=<path> pointing at a dedicated map database, or omit db to use the \
                 workspace-local default",
                forbidden_path.display()
            ));
        }
    }
    Ok(())
}

/// Resolve the `db` verb argument into a concrete target database path,
/// defaulting to `<path>/.khive/code-map.db` when absent, and rejecting a
/// target that resolves to the shared production database — either its
/// well-known `$HOME/.khive/khive.db` default, or `runtime_db_path`, the
/// database the calling `KhiveRuntime` was actually constructed against
/// (`self.runtime.config().db_path` at the call site), so an operator running
/// a non-default production location (`--db` / `KHIVE_DB`) is covered too.
pub(crate) fn resolve_target_db(
    db_param: Option<&str>,
    ingest_path: &Path,
    runtime_db_path: Option<&Path>,
) -> Result<PathBuf, String> {
    let candidate = match db_param {
        Some(p) => PathBuf::from(p),
        None => ingest_path.join(".khive").join("code-map.db"),
    };
    reject_if_forbidden(&candidate, &forbidden_db_paths(runtime_db_path))?;
    Ok(candidate)
}

/// Re-verify `opened_db_path` against the same forbidden set immediately
/// after the target `KhiveRuntime` has been opened (or created), before any
/// write runs against it. `resolve_target_db`'s check and the actual open are
/// two separate filesystem operations with a window between them (create a
/// fresh `db_path` after the check observed no existing file, alias it to a
/// protected database in that window); re-stating the now-guaranteed-to-exist
/// path closes most of that window by adding a metadata-identity check the
/// first pass could not perform against a not-yet-created file. This is a
/// narrowing, not a full close — a true race-resistant fence requires a
/// VFS-level check (ADR-085 Amendment 4, in review).
pub(crate) fn verify_opened_target(
    opened_db_path: &Path,
    runtime_db_path: Option<&Path>,
) -> Result<(), String> {
    reject_if_forbidden(opened_db_path, &forbidden_db_paths(runtime_db_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_target_is_workspace_local() {
        let path = Path::new("/tmp/some-repo");
        let db = resolve_target_db(None, path, None).expect("default resolves");
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
        let err = resolve_target_db(Some(&prod), Path::new("/tmp/some-repo"), None)
            .expect_err("must reject the shared production database");
        assert!(err.contains("shared production database"));
    }

    #[test]
    fn explicit_dedicated_path_is_accepted() {
        let db = resolve_target_db(
            Some("/tmp/code-ingest-map.db"),
            Path::new("/tmp/some-repo"),
            None,
        )
        .expect("dedicated path accepted");
        assert_eq!(db, PathBuf::from("/tmp/code-ingest-map.db"));
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
        )
        .expect_err("symlinked-parent alias of the configured db must be rejected");
        assert!(err.contains("shared production database"));
    }

    /// A hard link to the configured production database file
    /// has a DIFFERENT literal path from the original (no symlink involved,
    /// so `normalize`'s canonicalize-based comparison sees two distinct,
    /// already-canonical paths) but shares its `(device, inode)` identity —
    /// the fence must catch this via metadata, not path text.
    #[test]
    #[cfg(unix)]
    fn hard_link_to_configured_db_is_rejected() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let configured = tmp.path().join("main.db");
        std::fs::write(&configured, b"").expect("create sentinel file");
        let candidate = tmp.path().join("map.db");
        std::fs::hard_link(&configured, &candidate).expect("hard link");

        let err = resolve_target_db(
            Some(candidate.to_str().unwrap()),
            Path::new("/tmp/some-repo"),
            Some(&configured),
        )
        .expect_err("hard-linked alias of the configured db must be rejected");
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
        let db = resolve_target_db(
            Some("/tmp/code-ingest-map-2.db"),
            Path::new("/tmp/some-repo"),
            None,
        )
        .expect("dedicated path accepted with no env override");
        assert_eq!(db, PathBuf::from("/tmp/code-ingest-map-2.db"));
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
        let err = resolve_target_db(
            Some(prod.to_str().unwrap()),
            Path::new("/tmp/some-repo"),
            None, // config().db_path unresolved — the #1042/#1062 gap
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
        let result = resolve_target_db(
            Some(prod.to_str().unwrap()),
            Path::new("/tmp/some-repo"),
            None,
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
