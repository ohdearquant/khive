use super::*;

/// Resolve every writer policy a standalone events-daemon entry needs from its
/// own environment. The error names the setting that cannot be resolved.
#[cfg(unix)]
pub(super) fn standalone_daemon_policies(
    db_path: &Path,
) -> crate::error::RuntimeResult<(
    WalCeilingPolicy,
    khive_db::EffectiveDiskGuardConfig,
    PathBuf,
)> {
    let mut config = crate::RuntimeConfig {
        db_path: Some(db_path.to_path_buf()),
        ..crate::RuntimeConfig::no_embeddings()
    };
    let wal_ceiling = config.resolve_wal_ceiling_policy(false)?;
    let disk_guard = config
        .resolve_disk_guard_policy(false)?
        .ok_or_else(|| crate::error::RuntimeError::Internal("missing events disk policy".into()))?;
    let volume_lock_dir = khive_db::require_volume_lock_dir(config.volume_lock_dir)?;
    Ok((wal_ceiling, disk_guard, volume_lock_dir))
}

/// Supervise every events-daemon child with the main backend's resolved WAL
/// ceiling; the disk policy and lock directory come from this process's environment.
#[cfg(unix)]
pub async fn supervise_events_daemon_with_wal_ceiling(
    db_path: PathBuf,
    socket_path: PathBuf,
    wal_ceiling: WalCeilingPolicy,
) {
    let config = crate::RuntimeConfig::no_embeddings();
    let resolved = config
        .disk_guard_environment
        .resolve(None, None)
        .and_then(|policy| {
            Ok((
                policy,
                khive_db::require_volume_lock_dir(config.volume_lock_dir)?,
            ))
        });
    match resolved {
        Ok((disk_guard, volume_lock_dir)) => {
            supervise_events_daemon_with_policies(
                db_path,
                socket_path,
                wal_ceiling,
                disk_guard,
                volume_lock_dir,
            )
            .await;
        }
        Err(error) => {
            tracing::warn!(%error, "invalid events daemon policy; events supervisor not started");
        }
    }
}

/// Serve the events daemon with a WAL ceiling already resolved by its host; the
/// disk policy and lock directory come from this process's environment.
#[cfg(unix)]
pub async fn run_events_daemon_with_wal_ceiling(
    db_path: &Path,
    socket_path: &Path,
    wal_ceiling: WalCeilingPolicy,
) -> anyhow::Result<()> {
    let config = crate::RuntimeConfig::no_embeddings();
    let disk_guard = config.disk_guard_environment.resolve(None, None)?;
    let volume_lock_dir = khive_db::require_volume_lock_dir(config.volume_lock_dir)?;
    run_events_daemon_with_policies(
        db_path,
        socket_path,
        wal_ceiling,
        disk_guard,
        volume_lock_dir,
    )
    .await
}
