//! Pass the held store descriptor's identity into SQLite's pre-write admission.
use super::*;
use khive_db::file_identity::{database_file_identity_from_file, DatabaseFileIdentity};
use std::path::Path;

/// Resolve a bound daemon claim by its prepared canonical path.
/// The identity comes from the held descriptor, rather than a fresh path stat.
pub fn claimed_daemon_store_identity(
    guards: &[DaemonStoreGuard],
    database: &Path,
) -> anyhow::Result<DatabaseFileIdentity> {
    let guard = guards
        .iter()
        .find(|guard| guard.database == database)
        .ok_or_else(|| {
            anyhow::anyhow!("database {} has no daemon store claim", database.display())
        })?;
    ensure_claimed_parent_identity(&guard.parent_dir, &guard.database)?;
    let file = guard
        ._bound_database
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("claimed database {} was not bound", database.display()))?;
    database_file_identity_from_file(file).map_err(Into::into)
}
