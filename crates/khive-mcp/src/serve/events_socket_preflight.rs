//! Events socket preflight for the boot topology, run before any store is
//! claimed or opened.

use khive_runtime::{BackendConfig, BackendId, BackendKind, RuntimeConfig};

use super::{prepare_daemon_store_plan, DaemonStorePlan};

/// Validate the socket that the resolved boot topology will actually use.
/// Daemon hosts call this after freezing backend paths but before binding any
/// store files; the public builders repeat it before opening their backends.
pub fn preflight_events_socket_for_boot(
    config: &RuntimeConfig,
    backends: &[BackendConfig],
    force_memory: bool,
) -> anyhow::Result<()> {
    if force_memory {
        return Ok(());
    }
    let Some(socket) = config
        .events_split
        .as_ref()
        .and_then(|split| split.socket_path.as_deref())
    else {
        return Ok(());
    };
    if backends.is_empty() {
        if config.db_path.is_some() {
            khive_runtime::events_split::validate_events_socket_path(socket)?;
        }
    } else if let Some(main_path) = backends
        .iter()
        .find(|backend| backend.name == BackendId::MAIN && backend.kind == BackendKind::Sqlite)
        .and_then(|backend| backend.path.as_ref())
    {
        // Match prepare_configured_storage_topology's reanchor, including the
        // canonicalized paths supplied by prepare_daemon_store_plan.
        let expanded = khive_runtime::expand_tilde(main_path);
        let db_path = khive_runtime::events_split::events_db_path_beside(&expanded);
        let socket_path = khive_runtime::events_split::events_socket_path_beside(&db_path);
        khive_runtime::events_split::validate_events_socket_path(&socket_path)?;
    }
    Ok(())
}

/// Freeze the daemon's store paths, then preflight the events socket of that
/// frozen topology, before the boot guard or any store claim is taken.
pub fn prepare_preflighted_daemon_store_plan(
    config: &mut RuntimeConfig,
    db_anchor: &mut Option<std::path::PathBuf>,
    backends: &mut [BackendConfig],
    force_memory: bool,
) -> anyhow::Result<DaemonStorePlan> {
    let plan = prepare_daemon_store_plan(&mut config.db_path, db_anchor, backends, force_memory)?;
    preflight_events_socket_for_boot(config, backends, force_memory)?;
    Ok(plan)
}
