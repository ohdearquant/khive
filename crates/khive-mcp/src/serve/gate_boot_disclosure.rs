use std::collections::BTreeSet;
use std::path::Path;

use khive_runtime::engine_config::GateSectionConfig;
use khive_runtime::KhiveConfig;

#[cfg(test)]
#[path = "gate_boot_disclosure_tests.rs"]
mod tests;

/// Load the selected configuration and disclose which gate roster it carries.
pub(super) fn load(
    config_path: Option<&Path>,
    db_path: Option<&Path>,
) -> anyhow::Result<Option<KhiveConfig>> {
    match KhiveConfig::load_with_home_fallback_and_source(config_path, db_path)
        .map_err(|e| anyhow::anyhow!("config error: {e}"))?
    {
        Some((config, source)) => {
            emit(config.gate.as_ref(), Some(&source));
            Ok(Some(config))
        }
        None => {
            emit(None, None);
            Ok(None)
        }
    }
}

fn emit(gate: Option<&GateSectionConfig>, source: Option<&Path>) {
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
