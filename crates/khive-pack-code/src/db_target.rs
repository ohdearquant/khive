//! `code.ingest` target-database selection (ADR-085 Amendment 2 B7).
//!
//! `code.ingest` never writes to the shared production graph: it defaults to
//! a dedicated map database colocated with the ingested path, and rejects an
//! explicit `db` that resolves to the well-known production database path or
//! to the calling runtime's actual configured database, with no override.

use std::path::{Path, PathBuf};

/// The shared production database's default location
/// (`khive_runtime::config::resolve_db_anchor(None)`'s value), duplicated
/// here rather than depending on that function directly so this guard has no
/// dependency on process environment beyond `HOME` — matching the anchor
/// resolution `kkernel`/`khive-mcp` already use.
fn default_production_db_path() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".khive/khive.db"))
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

    let mut forbidden: Vec<PathBuf> = Vec::new();
    if let Some(prod) = default_production_db_path() {
        forbidden.push(prod);
    }
    if let Some(runtime_db) = runtime_db_path {
        forbidden.push(runtime_db.to_path_buf());
    }

    for forbidden_path in &forbidden {
        if same_path(&candidate, forbidden_path) {
            return Err(format!(
                "code.ingest refuses to target the shared production database ({}); pass \
                 db=<path> pointing at a dedicated map database, or omit db to use the \
                 workspace-local default",
                forbidden_path.display()
            ));
        }
    }
    Ok(candidate)
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
}
