use std::path::{Path, PathBuf};
use std::sync::Arc;

use khive_db::{StorageBackend, WalCeilingPolicy};

use super::file_hardening::refuse_events_db_symlinks;
#[cfg(unix)]
use super::file_hardening::{ensure_events_db_parent_trusted, harden_events_db_sidecars};
#[cfg(unix)]
use super::forwarder::{EventsForwardingMetrics, EventsSplitClient};
use super::paths::absolutize;

/// How a runtime reaches event storage when the split is configured.
#[derive(Debug, Clone)]
pub struct EventsSplitConfig {
    /// The events database file. The events daemon is its only writer in
    /// daemon deployments; embedded mode writes it directly.
    pub db_path: PathBuf,
    /// `Some(socket)` = forward appends to the events daemon at this socket
    /// (daemon deployments). `None` = embedded mode: open `db_path` directly
    /// in-process (one-shot CLI, tests).
    pub socket_path: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Process-global handles
// ---------------------------------------------------------------------------
//
// One events-split client (or one direct backend) per process, whatever the
// number of runtimes and clones — the same shape as the daemon module's other
// process-global state. Keyed by path so tests exercising two distinct paths
// in one process stay isolated.

#[cfg(unix)]
pub(super) type ClientMap = std::collections::HashMap<PathBuf, Arc<EventsSplitClient>>;
/// One admission per canonical path. The access mode belongs to the entry,
/// so another mode cannot run raw-file preflight over a live SQLite pool.
/// Strong entries retain the existing process lifetime, including derived stores.
pub(super) type BackendMap = std::collections::HashMap<PathBuf, (bool, Arc<StorageBackend>)>;

#[cfg(unix)]
pub(super) fn client_registry() -> &'static std::sync::Mutex<ClientMap> {
    static REGISTRY: std::sync::OnceLock<std::sync::Mutex<ClientMap>> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

pub(super) fn direct_backend_registry() -> &'static std::sync::Mutex<BackendMap> {
    static REGISTRY: std::sync::OnceLock<std::sync::Mutex<BackendMap>> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Release only one test fixture's process-registry entries when its scope ends.
///
/// Borrow a unique temporary directory immediately after creating it, before
/// declaring its backend/store/client handles. Those handles then drop before
/// this guard, and the borrowed directory outlives registry cleanup. Production
/// callers retain the normal process-lifetime registry ownership.
#[cfg(any(test, feature = "test-internals"))]
#[doc(hidden)]
pub struct TestRegistryGuard<'a> {
    pub(super) root: &'a Path,
    pub(super) canonical_root: PathBuf,
}

#[cfg(any(test, feature = "test-internals"))]
impl<'a> TestRegistryGuard<'a> {
    /// Keep `root` alive until this fixture's registry references are released.
    pub fn new(root: &'a Path) -> Self {
        Self {
            root,
            canonical_root: std::fs::canonicalize(root).expect("existing unique fixture root"),
        }
    }

    pub(super) fn remove_entries<T>(
        &self,
        registry: &std::sync::Mutex<std::collections::HashMap<PathBuf, T>>,
    ) -> Vec<T> {
        let mut entries = registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let keys: Vec<_> = entries
            .keys()
            .filter(|path| path.starts_with(self.root) || path.starts_with(&self.canonical_root))
            .cloned()
            .collect();
        keys.into_iter()
            .filter_map(|key| entries.remove(&key))
            .collect()
    }
}

#[cfg(any(test, feature = "test-internals"))]
impl Drop for TestRegistryGuard<'_> {
    fn drop(&mut self) {
        // Destructors may close SQLite connections or senders. Run them only
        // after releasing each registry lock; unrelated fixtures keep serving.
        let backends = self.remove_entries(direct_backend_registry());
        #[cfg(unix)]
        let clients = self.remove_entries(client_registry());
        drop(backends);
        #[cfg(unix)]
        drop(clients);
    }
}

/// The process-wide client for `socket_path`, created (and its forwarder
/// spawned) on first use. Requires a tokio runtime context on first call.
#[cfg(unix)]
pub fn client_for(socket_path: &Path) -> crate::error::RuntimeResult<Arc<EventsSplitClient>> {
    let mut registry = client_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(existing) = registry.get(socket_path) {
        return Ok(Arc::clone(existing));
    }
    let client = EventsSplitClient::new(socket_path.to_path_buf())?;
    registry.insert(socket_path.to_path_buf(), Arc::clone(&client));
    Ok(client)
}

/// The process-wide direct (embedded-mode) backend for `db_path`, opened
/// read-write on first use.
pub fn direct_backend_for(db_path: &Path) -> crate::error::RuntimeResult<Arc<StorageBackend>> {
    direct_backend_with_max_readers(db_path, false, None)
}

/// The process-wide direct backend for `db_path`, opened READ-ONLY on first
/// use. The file must already exist — this constructor never creates or
/// schema-initializes an events database, which is what a read-only runtime's
/// no-DB-creation contract requires of its event lane.
pub fn direct_backend_read_only_for(
    db_path: &Path,
) -> crate::error::RuntimeResult<Arc<StorageBackend>> {
    direct_backend_with_max_readers(db_path, true, None)
}

/// Standalone opener: resolve its environment once before opening the event lane.
pub(crate) fn direct_backend_with_max_readers(
    db_path: &Path,
    read_only: bool,
    max_readers: Option<usize>,
) -> crate::error::RuntimeResult<Arc<StorageBackend>> {
    let mut config = crate::RuntimeConfig {
        db_path: Some(db_path.to_path_buf()),
        ..crate::RuntimeConfig::no_embeddings()
    };
    let wal_ceiling = config.resolve_wal_ceiling_policy(read_only)?;
    let disk_guard = config.resolve_disk_guard_policy(read_only)?;
    direct_backend_with_policies(
        db_path,
        read_only,
        max_readers,
        wal_ceiling,
        disk_guard,
        config.volume_lock_dir,
    )
}

/// Open the direct event lane with the policies already resolved for its main
/// backend. A writable open also needs the disk policy and the volume lock
/// directory; a read-only open uses neither.
pub(crate) fn direct_backend_with_policies(
    db_path: &Path,
    read_only: bool,
    max_readers: Option<usize>,
    wal_ceiling: WalCeilingPolicy,
    disk_guard: Option<khive_db::EffectiveDiskGuardConfig>,
    volume_lock_dir: Option<PathBuf>,
) -> crate::error::RuntimeResult<Arc<StorageBackend>> {
    wal_ceiling.validate_static(true, true, read_only)?;
    if !read_only {
        disk_guard
            .ok_or_else(|| {
                crate::error::RuntimeError::Internal("missing events disk policy".into())
            })?
            .validate()?;
        khive_db::require_volume_lock_dir(volume_lock_dir.clone())?;
    }
    let mut registry = direct_backend_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let absolute_path = absolutize(db_path);
    let db_path = absolute_path.as_path();
    refuse_events_db_symlinks(db_path)
        .map_err(|e| crate::error::RuntimeError::Internal(e.to_string()))?;
    #[cfg(unix)]
    if !read_only {
        if let Some(parent) = db_path.parent().filter(|p| !p.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    // Directory trust comes after creation (a fresh parent in a trusted
    // ancestor must be statable) and before any open: SQLite's open is
    // path-based, so the only defense against a component swapped between
    // validation and open is that no untrusted local user can write the
    // directories the path traverses.
    #[cfg(unix)]
    ensure_events_db_parent_trusted(db_path)
        .map_err(|e| crate::error::RuntimeError::Internal(e.to_string()))?;
    let key = std::fs::canonicalize(db_path)
        .or_else(|error| {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(error);
            }
            let parent = db_path.parent().ok_or(error)?;
            let name = db_path.file_name().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "events path has no file name",
                )
            })?;
            std::fs::canonicalize(parent).map(|parent| parent.join(name))
        })
        .map_err(|e| crate::error::RuntimeError::Internal(e.to_string()))?;
    if let Some((existing_read_only, existing)) = registry.get(&key) {
        if *existing_read_only != read_only {
            let existing_mode = if *existing_read_only {
                "read-only"
            } else {
                "writable"
            };
            let requested_mode = if read_only { "read-only" } else { "writable" };
            return Err(crate::error::RuntimeError::InvalidInput(format!(
                "events database {} is already open {existing_mode} in this process; cannot \
                 open it {requested_mode}; read-only events access requires a separate frozen snapshot",
                key.display()
            )));
        }
        let numbers = |p: Option<khive_db::EffectiveDiskGuardConfig>| {
            p.map(|p| (p.reserve_bytes, p.guard_deadline_ms))
        };
        if numbers(existing.pool().effective_disk_guard_config()) != numbers(disk_guard) {
            return Err(crate::error::RuntimeError::Internal(
                "events database is already open with a different disk reserve/deadline policy"
                    .into(),
            ));
        }
        if !read_only && existing.pool().config().volume_lock_dir != volume_lock_dir {
            return Err(crate::error::RuntimeError::Internal(
                "events database is already open with a different volume-lock directory".into(),
            ));
        }
        let existing_bytes = existing.pool().config().wal_ceiling.bytes;
        if existing_bytes != wal_ceiling.bytes {
            return Err(khive_db::SqliteError::InvalidConfig(format!(
                "events database {} is already open with wal_ceiling_bytes={existing_bytes}; \
                 requested {}; drain and restart before changing the WAL ceiling",
                key.display(),
                wal_ceiling.bytes
            ))
            .into());
        }
        return Ok(Arc::clone(existing));
    }
    let db_path = key.as_path();
    // Embedded writable mode holds the events database to the daemon's own
    // contract: owner-only from the first byte, never at the process umask,
    // and — because event rows carry the same audit payloads either way — a
    // pre-existing database or -wal/-shm sidecar is tightened to 0600 too,
    // fail-closed. A create race just means SQLite finds the file present.
    #[cfg(unix)]
    if !read_only {
        use std::os::unix::fs::OpenOptionsExt;
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(db_path);
        // Before the open, and only before it: see the precondition on
        // `harden_events_db_sidecars`.
        let _before_open = harden_events_db_sidecars(db_path)
            .map_err(|e| crate::error::RuntimeError::Internal(e.to_string()))?;
    }
    let backend = Arc::new(if read_only {
        StorageBackend::sqlite_read_only_with_max_readers_and_wal_ceiling(
            db_path,
            max_readers,
            wal_ceiling,
        )?
    } else {
        StorageBackend::sqlite_with_max_readers_and_policies(
            db_path,
            max_readers,
            wal_ceiling,
            disk_guard.ok_or_else(|| {
                crate::error::RuntimeError::Internal("missing events disk policy".into())
            })?,
            khive_db::require_volume_lock_dir(volume_lock_dir)?,
        )?
    });
    registry.insert(key, (read_only, Arc::clone(&backend)));
    Ok(backend)
}

/// Forwarding metrics for the process-wide client at `socket_path`, if one
/// exists. `None` means the split never initialized in this process.
#[cfg(unix)]
pub fn forwarding_metrics(socket_path: &Path) -> Option<EventsForwardingMetrics> {
    let registry = client_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    registry.get(socket_path).map(|client| client.metrics())
}
