//! Disk-reserve policy contribution to the daemon compatibility id, and the
//! policy the events daemon inherits from the main backend.
use super::*;

/// Fold only effective numeric policy, never the source, free-space sample,
/// or volume identifier. Every writable SQLite backend contributes one row:
/// each declared backend, plus the implicit main when no declared backend is
/// named `main`. Invalid environments cannot open a writable pool; the
/// `invalid` marker keeps this string-returning compatibility API
/// deterministic until that open returns its typed configuration error.
pub(super) fn disk_guard_policy_fingerprint(
    config: &RuntimeConfig,
    khive_cfg: Option<&khive_runtime::KhiveConfig>,
    storage_read_only: bool,
) -> String {
    let declared = khive_cfg
        .map(|cfg| cfg.backends.as_slice())
        .unwrap_or_default();
    let mut rows: Vec<(String, u64, u64)> = Vec::new();
    for backend in declared {
        // Read-only storage has no writer, so main's row follows the same
        // rule as its WAL ceiling above.
        let main_is_read_only = storage_read_only && backend.name == khive_runtime::BackendId::MAIN;
        if backend.kind != khive_runtime::BackendKind::Sqlite
            || backend.read_only
            || main_is_read_only
        {
            continue;
        }
        let policy = match config
            .disk_guard_environment
            .resolve(backend.disk_reserve_bytes, backend.disk_guard_deadline_ms)
        {
            Ok(policy) => policy,
            Err(_) => return ";sqlite_disk_guard=invalid".to_string(),
        };
        rows.push((
            backend.name.clone(),
            policy.reserve_bytes,
            policy.guard_deadline_ms,
        ));
    }
    let main_is_declared = declared
        .iter()
        .any(|backend| backend.name == khive_runtime::BackendId::MAIN);
    if !main_is_declared && config.db_path.is_some() && !storage_read_only {
        let policy = match config
            .disk_guard_config
            .map(Ok)
            .unwrap_or_else(|| config.disk_guard_environment.resolve(None, None))
        {
            Ok(policy) => policy,
            Err(_) => return ";sqlite_disk_guard=invalid".to_string(),
        };
        rows.push((
            khive_runtime::BackendId::MAIN.to_string(),
            policy.reserve_bytes,
            policy.guard_deadline_ms,
        ));
    }
    encode_disk_guard_policy_rows(rows)
}

fn encode_disk_guard_policy_rows(mut rows: Vec<(String, u64, u64)>) -> String {
    if rows.is_empty() {
        return String::new();
    }
    rows.sort();
    format!(
        ";sqlite_disk_guard={}",
        serde_json::to_string(&rows).expect("numeric disk-guard rows serialize")
    )
}

impl KhiveMcpServer {
    /// The disk policy and lock directory the events daemon inherits from the
    /// main backend. `None` is logged: the supervisor is then not started.
    pub(crate) fn events_disk_policy(
        &self,
    ) -> Option<(khive_db::EffectiveDiskGuardConfig, std::path::PathBuf)> {
        let runtime = self.runtime.as_ref()?;
        let policy = match runtime.events_disk_guard_policy() {
            Ok(policy) => policy,
            Err(error) => {
                tracing::warn!(%error, "events disk policy unresolved; events daemon unsupervised");
                return None;
            }
        };
        let Some(volume_lock_dir) = runtime.events_volume_lock_dir() else {
            tracing::warn!("no SQLite volume-lock directory; events daemon unsupervised");
            return None;
        };
        Some((policy, volume_lock_dir))
    }
}
