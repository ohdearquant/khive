use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use khive_storage::{SqlAccess, StorageCapability, StorageError, StorageResult};

use super::{map_io_err, DATABASE_GC_LOCK_SUFFIX};

pub(super) fn database_gc_lock_path(database_path: &Path) -> PathBuf {
    let mut lock_path = database_path.as_os_str().to_os_string();
    lock_path.push(DATABASE_GC_LOCK_SUFFIX);
    PathBuf::from(lock_path)
}

pub(super) fn acquire_database_gc_lock(
    database_path: Option<&Path>,
) -> StorageResult<Option<fs::File>> {
    let Some(database_path) = database_path else {
        // An in-memory database cannot be shared by another process. The
        // process-local lock below is therefore the complete owner fence.
        return Ok(None);
    };
    let lock_path = database_gc_lock_path(database_path);
    let lock_file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| map_io_err(e, "database_gc_lock_open"))?;
    fs4::FileExt::lock(&lock_file).map_err(|e| map_io_err(e, "database_gc_lock_acquire"))?;
    Ok(Some(lock_file))
}

/// Process-wide database owner fence for transactional blob sweeps.
///
/// Claims live in the database and their attachment triggers are database-global,
/// so a root-only lock is insufficient: two differently configured roots for
/// one database must not recover each other's live claims. File-backed pools
/// additionally take [`acquire_database_gc_lock`] for cross-process exclusion.
type SweepLockMap = HashMap<Option<PathBuf>, Arc<DatabaseGcProcessLock>>;

#[derive(Debug, Default)]
pub(super) struct DatabaseGcProcessLock {
    held: StdMutex<bool>,
    released: std::sync::Condvar,
    #[cfg(test)]
    waiters: std::sync::atomic::AtomicUsize,
}

impl DatabaseGcProcessLock {
    pub(super) fn acquire(self: &Arc<Self>) -> DatabaseGcProcessGuard {
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *held {
            #[cfg(test)]
            self.waiters
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            held = self
                .released
                .wait(held)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            #[cfg(test)]
            self.waiters
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
        *held = true;
        DatabaseGcProcessGuard {
            lock: Arc::clone(self),
        }
    }

    pub(super) fn try_acquire(self: &Arc<Self>) -> Option<DatabaseGcProcessGuard> {
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *held {
            return None;
        }
        *held = true;
        Some(DatabaseGcProcessGuard {
            lock: Arc::clone(self),
        })
    }
}

#[derive(Debug)]
pub(super) struct DatabaseGcProcessGuard {
    lock: Arc<DatabaseGcProcessLock>,
}

impl Drop for DatabaseGcProcessGuard {
    fn drop(&mut self) {
        let mut held = self
            .lock
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        debug_assert!(*held, "database GC process owner released twice");
        *held = false;
        self.lock.released.notify_one();
    }
}

fn database_sweep_locks() -> &'static StdMutex<SweepLockMap> {
    static REGISTRY: OnceLock<StdMutex<SweepLockMap>> = OnceLock::new();
    REGISTRY.get_or_init(|| StdMutex::new(HashMap::new()))
}

pub(super) fn sweep_lock_for_database(database_path: Option<&Path>) -> Arc<DatabaseGcProcessLock> {
    let key = database_path.map(Path::to_path_buf);
    let mut locks = database_sweep_locks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    locks
        .entry(key)
        .or_insert_with(|| Arc::new(DatabaseGcProcessLock::default()))
        .clone()
}

#[cfg(test)]
pub(crate) fn database_gc_waiter_count(database_path: Option<&Path>) -> usize {
    sweep_lock_for_database(database_path)
        .waiters
        .load(std::sync::atomic::Ordering::SeqCst)
}

/// Exclusive canonical-database ownership shared by transactional blob GC and
/// the boot-gated V21 attachment cutover.
///
/// The process-local mutex is acquired first and retained while the advisory
/// file lock is acquired on a blocking thread. Moving the owned mutex guard
/// into that closure makes cancellation safe: dropping the outer future cannot
/// release process ownership while a blocking advisory acquisition continues.
pub struct DatabaseGcOwnerGuard {
    _process_guard: DatabaseGcProcessGuard,
    _advisory_guard: Option<fs::File>,
    database_path: Option<PathBuf>,
}

pub(crate) fn acquire_database_gc_owner_for_path_blocking(
    database_path: Option<PathBuf>,
) -> StorageResult<DatabaseGcOwnerGuard> {
    let process_guard = sweep_lock_for_database(database_path.as_deref()).acquire();
    let advisory_guard = acquire_database_gc_lock(database_path.as_deref())?;
    Ok(DatabaseGcOwnerGuard {
        _process_guard: process_guard,
        _advisory_guard: advisory_guard,
        database_path,
    })
}

/// Try to acquire canonical database-GC ownership without waiting.
///
/// This is the fail-closed bridge for the legacy raw-connection migration API:
/// a caller may already hold an opaque pooled writer guard, so waiting here
/// could invert the canonical owner-before-writer order used by sweeps. The
/// production backend boot path uses the blocking helper before writer
/// checkout instead.
pub(crate) fn try_acquire_database_gc_owner_for_path(
    database_path: PathBuf,
) -> StorageResult<DatabaseGcOwnerGuard> {
    let process_guard = sweep_lock_for_database(Some(&database_path))
        .try_acquire()
        .ok_or_else(|| {
            StorageError::Internal(format!(
                "database GC owner for {} is already held; retry schema migration through the \
                 coordinated backend boot path",
                database_path.display()
            ))
        })?;
    let lock_path = database_gc_lock_path(&database_path);
    let advisory_guard = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|error| map_io_err(error, "database_gc_lock_open"))?;
    fs4::FileExt::try_lock(&advisory_guard)
        .map_err(|error| map_io_err(error.into(), "database_gc_lock_try_acquire"))?;
    Ok(DatabaseGcOwnerGuard {
        _process_guard: process_guard,
        _advisory_guard: Some(advisory_guard),
        database_path: Some(database_path),
    })
}

impl std::fmt::Debug for DatabaseGcOwnerGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DatabaseGcOwnerGuard")
            .field("database_path", &self.database_path)
            .finish_non_exhaustive()
    }
}

impl DatabaseGcOwnerGuard {
    /// Canonical database path that keys this owner, or `None` for an
    /// in-memory database whose process-local mutex is the complete fence.
    pub fn database_path(&self) -> Option<&Path> {
        self.database_path.as_deref()
    }
}

/// Acquire the canonical database owner used by both V21 boot cutover and
/// transactional blob sweep. Callers must retain the returned guard across
/// every stage that must exclude the other protocol.
pub async fn acquire_database_gc_owner(sql: &dyn SqlAccess) -> StorageResult<DatabaseGcOwnerGuard> {
    let database_path = sql.database_path();
    tokio::task::spawn_blocking(move || acquire_database_gc_owner_for_path_blocking(database_path))
        .await
        .map_err(|error| {
            StorageError::driver(StorageCapability::Blob, "acquire_database_gc_owner", error)
        })?
}
