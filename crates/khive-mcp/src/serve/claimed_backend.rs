//! Open daemon backends with the exact physical identity held by their claim.
use super::*;

#[cfg(all(test, unix))]
#[path = "claimed_backend_tests.rs"]
mod tests;

fn open_claimed_sqlite(
    path: &std::path::Path,
    max_readers: Option<usize>,
    wal_ceiling: khive_db::WalCeilingPolicy,
    read_only: bool,
    claims: Option<&[khive_runtime::daemon::DaemonStoreGuard]>,
) -> Result<StorageBackend, khive_db::SqliteError> {
    #[cfg(unix)]
    if let Some(claims) = claims {
        let expected = khive_runtime::daemon::claimed_daemon_store_identity(claims, path)
            .map_err(|error| khive_db::SqliteError::InvalidData(error.to_string()))?;
        return StorageBackend::sqlite_with_claimed_file_identity(
            path,
            max_readers,
            wal_ceiling,
            read_only,
            expected,
        );
    }
    #[cfg(not(unix))]
    if claims.is_some() {
        return Err(khive_db::SqliteError::InvalidConfig(
            "daemon store claims require Unix".into(),
        ));
    }
    if read_only {
        StorageBackend::sqlite_read_only_with_max_readers_and_wal_ceiling(
            path,
            max_readers,
            wal_ceiling,
        )
    } else {
        StorageBackend::sqlite_with_max_readers_and_wal_ceiling(path, max_readers, wal_ceiling)
    }
}

pub(super) fn open_single_backend(
    config: &mut RuntimeConfig,
    max_readers: Option<usize>,
    claims: Option<&[khive_runtime::daemon::DaemonStoreGuard]>,
) -> anyhow::Result<StorageBackend> {
    let wal_ceiling = config.resolve_wal_ceiling_policy(false)?;
    let backend = match &config.db_path {
        Some(path) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    anyhow::anyhow!(
                        "cannot create database parent directory {}: {error}",
                        parent.display()
                    )
                })?;
            }
            open_claimed_sqlite(path, max_readers, wal_ceiling, false, claims).map_err(|error| {
                let context = format!("backend main: sqlite open at {}: {error}", path.display());
                anyhow::Error::new(error).context(context)
            })?
        }
        None => StorageBackend::memory()
            .map_err(|error| anyhow::anyhow!("open single in-memory backend: {error}"))?,
    };
    Ok(backend)
}

pub(super) fn open_backend(
    cfg: &BackendConfig,
    max_readers: Option<usize>,
    wal_ceiling: khive_db::WalCeilingPolicy,
    claims: Option<&[khive_runtime::daemon::DaemonStoreGuard]>,
) -> anyhow::Result<StorageBackend> {
    match cfg.kind {
        BackendKind::Memory => StorageBackend::memory()
            .map_err(|e| anyhow::anyhow!("backend {}: memory open: {e}", cfg.name)),
        BackendKind::Sqlite => {
            let path = cfg.path.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "backend {}: sqlite backend requires a `path` field",
                    cfg.name
                )
            })?;
            let expanded = khive_runtime::expand_tilde(path);
            if !cfg.read_only {
                if let Some(parent) = expanded.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        anyhow::anyhow!(
                            "backend {}: cannot create parent dir {}: {e}",
                            cfg.name,
                            parent.display()
                        )
                    })?;
                }
            }
            if cfg.read_only {
                open_claimed_sqlite(&expanded, max_readers, wal_ceiling, true, claims).map_err(
                    |error| {
                        let context = format!(
                            "backend {}: sqlite read-only open at {}: {error}",
                            cfg.name,
                            expanded.display()
                        );
                        anyhow::Error::new(error).context(context)
                    },
                )
            } else {
                let backend =
                    open_claimed_sqlite(&expanded, max_readers, wal_ceiling, false, claims)
                        .map_err(|error| {
                            let context = format!(
                                "backend {}: sqlite open at {}: {error}",
                                cfg.name,
                                expanded.display()
                            );
                            anyhow::Error::new(error).context(context)
                        })?;
                if backend.is_read_only() {
                    anyhow::bail!(
                        "backend {}: path {} has no filesystem write bits; declare \
                         `read_only = true` so backend topology and daemon config identity \
                         describe the snapshot-inspection mode explicitly",
                        cfg.name,
                        expanded.display()
                    );
                }
                Ok(backend)
            }
        }
    }
}
