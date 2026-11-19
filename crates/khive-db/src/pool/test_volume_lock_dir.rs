//! Default volume-lock namespaces for test fixture pools.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// Explicit test-only lock namespace, a new one for every fixture config:
/// each call names its own subdirectory of this process's private directory.
/// Under the test harness the in-process lease slot is keyed by lock
/// directory, so a fixture's writes wait only on pools built from the same
/// config, never on an unrelated test in the same binary. Fixtures that must
/// contend share one directory explicitly. `PoolConfig::default()` in an
/// ordinary build never calls this.
pub(super) fn test_volume_lock_dir() -> PathBuf {
    static DIRECTORY: OnceLock<PathBuf> = OnceLock::new();
    static NEXT_NAMESPACE: AtomicU64 = AtomicU64::new(0);
    DIRECTORY
        .get_or_init(|| {
            tempfile::Builder::new()
                .prefix("khive-db-volume-lock-test-")
                .tempdir()
                .expect("private test volume-lock directory")
                .keep()
        })
        .join(format!(
            "pool-{}",
            NEXT_NAMESPACE.fetch_add(1, Ordering::Relaxed)
        ))
}

#[cfg(test)]
mod tests {
    use crate::pool::{ConnectionPool, PoolConfig};
    use std::time::{Duration, Instant};

    #[test]
    fn each_default_test_config_names_its_own_lock_namespace() {
        let first = PoolConfig::for_test().volume_lock_dir.unwrap();
        let second = PoolConfig::for_test().volume_lock_dir.unwrap();
        assert_ne!(first, second);
        assert_eq!(
            first.parent(),
            second.parent(),
            "both namespaces sit under this process's private directory"
        );
    }

    #[test]
    fn a_writer_held_by_one_test_pool_does_not_hold_up_another() {
        assert!(
            crate::disk_guard::harness_scoped_process_leases(),
            "the in-process lease is keyed by lock directory only under the test marker"
        );
        let dir = tempfile::tempdir().unwrap();
        let open = |name: &str| {
            ConnectionPool::new(PoolConfig {
                path: Some(dir.path().join(name)),
                ..PoolConfig::for_test()
            })
            .unwrap()
        };
        let holder = open("holder.db");
        let other = open("other.db");
        let _held = holder.writer().unwrap();

        let started = Instant::now();
        let acquired = std::thread::scope(|scope| {
            scope
                .spawn(|| other.writer().map(drop))
                .join()
                .expect("writer thread")
        });
        let waited = started.elapsed();
        acquired.expect("another test's database must not wait on this lease");
        assert!(
            waited < Duration::from_millis(1_000),
            "waited {waited:?} for a lease held by an unrelated fixture"
        );
    }
}
