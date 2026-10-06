use std::collections::BTreeSet;
use std::path::Path;

use khive_runtime::engine_config::GateSectionConfig;

pub(super) fn emit(gate: Option<&GateSectionConfig>, source: Option<&Path>) {
    let attributed_count = gate.map_or(0, |gate| {
        gate.granted_actors.iter().collect::<BTreeSet<_>>().len()
    });
    let grant_unattributed = gate.is_some_and(|gate| gate.grant_unattributed);
    let principal_count = attributed_count + usize::from(grant_unattributed);
    tracing::info!(
        target: "khive.boot",
        config_source = ?source,
        roster_configured = gate.is_some(),
        configured_attributed_principal_count = attributed_count,
        grant_unattributed,
        configured_principal_count = principal_count,
        "gate configuration selection resolved"
    );
}
