//! `code.ingest` target-database selection (ADR-085 Amendment 2 B7).
//!
//! `code.ingest` never writes to the shared production graph: it defaults to
//! a dedicated map database colocated with the ingested path, and rejects an
//! explicit `db` that resolves to the well-known production database path,
//! with no override.

use std::path::{Path, PathBuf};

/// The shared production database's default location
/// (`khive_runtime::config::resolve_db_anchor(None)`'s value), duplicated
/// here rather than depending on that function directly so this guard has no
/// dependency on process environment beyond `HOME` — matching the anchor
/// resolution `kkernel`/`khive-mcp` already use.
fn production_db_path() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".khive/khive.db"))
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Resolve the `db` verb argument into a concrete target database path,
/// defaulting to `<path>/.khive/code-map.db` when absent, and rejecting a
/// target that resolves to the shared production database.
pub(crate) fn resolve_target_db(
    db_param: Option<&str>,
    ingest_path: &Path,
) -> Result<PathBuf, String> {
    let candidate = match db_param {
        Some(p) => PathBuf::from(p),
        None => ingest_path.join(".khive").join("code-map.db"),
    };
    if let Some(prod) = production_db_path() {
        if same_path(&candidate, &prod) {
            return Err(format!(
                "code.ingest refuses to target the shared production database ({}); pass \
                 db=<path> pointing at a dedicated map database, or omit db to use the \
                 workspace-local default",
                prod.display()
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
        let db = resolve_target_db(None, path).expect("default resolves");
        assert_eq!(db, path.join(".khive").join("code-map.db"));
    }

    #[test]
    fn explicit_production_path_is_rejected() {
        let home = std::env::var("HOME").expect("HOME set in test env");
        let prod = format!("{home}/.khive/khive.db");
        let err = resolve_target_db(Some(&prod), Path::new("/tmp/some-repo"))
            .expect_err("must reject the shared production database");
        assert!(err.contains("shared production database"));
    }

    #[test]
    fn explicit_dedicated_path_is_accepted() {
        let db = resolve_target_db(Some("/tmp/code-ingest-map.db"), Path::new("/tmp/some-repo"))
            .expect("dedicated path accepted");
        assert_eq!(db, PathBuf::from("/tmp/code-ingest-map.db"));
    }
}
