//! Host preparation for the node channel (C2).
//!
//! C2 only prepares the channel's host state. It starts no loop: the state is held in
//! `PreparedHostBackgroundTasks` for the loop that a later slice adds.

use std::sync::Arc;

use khive_pack_comm::node::NodeHostState;

use crate::args::Args;
use crate::server::KhiveMcpServer;

/// Refusal text when the node channel is enabled but the comm pack is not selected.
pub(super) const COMM_PACK_REQUIRED: &str = "channel.node: enabled channel requires the comm pack";

/// Refuse an enabled node channel when the selected packs do not include `comm`.
///
/// A disabled channel never refuses, whatever packs are selected.
pub(super) fn require_comm_pack<I, S>(enabled: bool, selected_packs: I) -> anyhow::Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    if !enabled {
        return Ok(());
    }
    if selected_packs
        .into_iter()
        .any(|name| name.as_ref() == "comm")
    {
        Ok(())
    } else {
        anyhow::bail!(COMM_PACK_REQUIRED)
    }
}

/// Prepare the node channel host state.
///
/// - Disabled or absent node config: prepares nothing and returns `Ok(None)`.
/// - Enabled without the `comm` pack selected: refused before any host-state lookup.
/// - State is initialized only in a resident daemon with writable comm; initialization
///   failure is returned so startup does not report success.
/// - Starts no loop.
pub(super) async fn prepare_node_channel(
    server: &KhiveMcpServer,
    args: &Args,
) -> anyhow::Result<Option<Arc<NodeHostState>>> {
    let enabled = server
        .channel_outbox_runtime_clone()
        .map(|runtime| runtime.config().node_channel.is_some())
        .unwrap_or(false);
    let registry = server.verb_registry_clone();
    // Selected packs are checked before the host-state lookup.
    require_comm_pack(enabled, registry.pack_names())?;
    if !enabled {
        return Ok(None);
    }
    let resident = args.daemon
        && args.daemon_options().lifetime != khive_runtime::daemon::DaemonLifetime::Demand;
    if !resident || server.default_runtime_is_read_only() {
        return Ok(None);
    }
    let Some(state) = registry.pack_host_state::<NodeHostState>("comm") else {
        return Ok(None);
    };
    state.services().await?;
    Ok(Some(state))
}
