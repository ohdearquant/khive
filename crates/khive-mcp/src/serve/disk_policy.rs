//! Disk-reserve policy checks and the policy-carrying backend opener.
use super::*;

pub(super) fn disk_guard_numbers(
    policy: Option<khive_db::EffectiveDiskGuardConfig>,
) -> Option<(u64, u64)> {
    policy.map(|p| (p.reserve_bytes, p.guard_deadline_ms))
}

/// Validate every disk policy from the same captured snapshot before any open.
pub(super) fn validate_disk_guard_topology(
    config: &RuntimeConfig,
    backends: &[BackendConfig],
    force_memory: bool,
) -> anyhow::Result<()> {
    let effective = effective_backend_configs(backends, force_memory);
    // A read-only alias resolves no disk policy, so a read-only and a writable alias of one file
    // would otherwise refuse here as a policy conflict; the access-mode refusal is the real one.
    validate_effective_backend_alias_modes(&effective)?;
    let mut policies: HashMap<BackendAliasIdentity, (String, Option<(u64, u64)>)> = HashMap::new();
    for backend in effective {
        let policy = backend.resolve_disk_guard(&config.disk_guard_environment)?;
        let Some(path) = canonical_backend_path(&backend)? else {
            continue;
        };
        let identity = backend_alias_identity(&backend.name, &path)?;
        let numbers = disk_guard_numbers(policy);
        if let Some((first_name, first_policy)) = policies.get(&identity) {
            if *first_policy != numbers {
                return Err(khive_runtime::ConfigError::DiskGuardAliasConflict {
                    first_backend: first_name.clone(),
                    second_backend: backend.name.clone(),
                }
                .into());
            }
        } else {
            policies.insert(identity, (backend.name, numbers));
        }
    }
    Ok(())
}

pub(super) fn open_backend_with_policies(
    cfg: &BackendConfig,
    max_readers: Option<usize>,
    wal_ceiling: khive_db::WalCeilingPolicy,
    disk_guard: Option<khive_db::EffectiveDiskGuardConfig>,
    volume_lock_dir: Option<&std::path::Path>,
) -> anyhow::Result<StorageBackend> {
    claimed_backend::open_backend(
        cfg,
        max_readers,
        wal_ceiling,
        None,
        disk_guard,
        volume_lock_dir,
    )
}
