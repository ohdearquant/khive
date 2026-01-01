//! Selected-only schema migration with opened-file topology identity validation.

use super::*;

/// Open only the selected backend, retaining the topology's pre-open identities.
/// Creating an absent SQLite file can reveal a case or firmlink alias that the
/// planner could not identify. Refuse that changed topology before migration.
fn open_selected_backend_bound_to_topology_with<F>(
    selected: &BackendConfig,
    effective_backends: &[BackendConfig],
    max_readers: Option<usize>,
    opener: F,
) -> anyhow::Result<StorageBackend>
where
    F: FnOnce(&BackendConfig, Option<usize>) -> anyhow::Result<StorageBackend>,
{
    let identities = effective_backends
        .iter()
        .map(|cfg| {
            canonical_backend_path(cfg)?.map_or(Ok(None), |canonical| {
                backend_alias_identity(&cfg.name, &canonical)
                    .map(|identity| Some((identity, canonical)))
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let selected_index = effective_backends
        .iter()
        .position(|cfg| cfg.name == selected.name)
        .expect("the planner validated the selected backend");
    let Some((snapshot, canonical)) = &identities[selected_index] else {
        return opener(selected, max_readers);
    };
    let (backend, opened) = open_backend_bound_to_alias_identity_with(
        selected,
        max_readers,
        snapshot,
        canonical,
        opener,
    )?;
    for (configured, identity) in effective_backends.iter().zip(&identities) {
        let Some((previous, _)) = identity else {
            continue;
        };
        if configured.name == selected.name || previous == snapshot {
            continue;
        }
        let Some(now) = canonical_backend_path(configured)? else {
            continue;
        };
        if backend_alias_identity(&configured.name, &now)? == opened {
            anyhow::bail!(
                "backend {}: opened database identity also matches backend {} after the \
                 topology snapshot; refusing targeted migration",
                selected.name,
                configured.name,
            );
        }
    }
    Ok(backend)
}

/// Execute the selected-only schema path after whole-topology preflight.
/// The opener seam keeps file-creation races testable at the migration boundary.
pub(super) async fn migrate_selected_storage_backend_with<F>(
    base_config: &RuntimeConfig,
    effective_backends: &[BackendConfig],
    selected: &BackendConfig,
    opener: F,
) -> anyhow::Result<BackendSchemaMigrationStatus>
where
    F: FnOnce(
        &BackendConfig,
        Option<usize>,
        khive_db::WalCeilingPolicy,
    ) -> anyhow::Result<StorageBackend>,
{
    let policy = wal_ceiling_policy_for_backend(base_config, selected)?;
    let backend = Arc::new(open_selected_backend_bound_to_topology_with(
        selected,
        effective_backends,
        None,
        |cfg, max_readers| opener(cfg, max_readers, policy),
    )?);
    prepare_core_schema_for_boot(Arc::clone(&backend), format!("backend {}", selected.name))
        .await?;
    crate::attachment_cutover::require_secondary_attachment_empty(
        Arc::clone(&backend),
        &selected.name,
    )
    .await?;
    crate::attachment_cutover::coordinate_empty_secondary_attachment_cutover(
        Arc::clone(&backend),
        &selected.name,
    )
    .await?;
    Ok(BackendSchemaMigrationStatus {
        backend: selected.name.clone(),
        applied_version: read_applied_schema_version(backend.sql().as_ref()).await?,
        prerequisite: false,
    })
}
