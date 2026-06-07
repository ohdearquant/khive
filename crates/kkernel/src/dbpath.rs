//! Shared `--db` / `KHIVE_DB` resolution for `kkernel` subcommands.

use std::path::PathBuf;

/// Resolve the `--db`/`KHIVE_DB` value into a `db_path` override, mirroring
/// `kkernel mcp`: an explicit `:memory:` means the ephemeral in-memory db
/// (`None`), not a file literally named ":memory:" (which SQLite treats as a
/// per-connection file → empty schema). `None` leaves the default in place.
///
/// Returns `Some(override)` when the caller supplied `--db`/`KHIVE_DB`, where
/// the inner value is the `RuntimeConfig.db_path` to set; `None` means no
/// override (keep `RuntimeConfig::default().db_path`).
pub fn resolve_db_override(db: Option<&str>) -> Option<Option<PathBuf>> {
    match db {
        Some(":memory:") => Some(None),
        Some(path) => Some(Some(PathBuf::from(path))),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_sentinel_maps_to_none() {
        assert_eq!(resolve_db_override(Some(":memory:")), Some(None));
    }

    #[test]
    fn explicit_path_maps_to_some() {
        assert_eq!(
            resolve_db_override(Some("/tmp/kkernel-test.db")),
            Some(Some(PathBuf::from("/tmp/kkernel-test.db")))
        );
    }

    #[test]
    fn absent_db_leaves_default() {
        assert_eq!(resolve_db_override(None), None);
    }
}
