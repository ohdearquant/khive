use super::KhiveMcpServer;
use khive_db::{ConnectionPool, PoolConfig, StorageBackend, WalCeilingPolicy, WalCeilingSource};
use khive_runtime::{BackendId, KhiveRuntime, RuntimeConfig, VerbRegistryBuilder};
use std::{path::Path, sync::Arc};

// A private lock directory under the fixture root keeps these writable opens
// out of the user's real volume-lock namespace.
fn file_backend_with_policy(
    root: &Path,
    path: &Path,
    policy: WalCeilingPolicy,
) -> Result<StorageBackend, khive_db::SqliteError> {
    StorageBackend::sqlite_with_max_readers_and_policies(
        path,
        Some(1),
        policy,
        khive_db::DiskGuardEnvironment::default().resolve(Some(0), None)?,
        root.join("volume-locks"),
    )
}

fn file_backend(
    root: &Path,
    name: &str,
    policy: WalCeilingPolicy,
    read_only: bool,
) -> Arc<StorageBackend> {
    let path = root.join(name);
    if read_only {
        let seed = file_backend_with_policy(root, &path, WalCeilingPolicy::default())
            .expect("create main fixture database");
        seed.prepare_core_schema()
            .expect("initialize fixture schema");
        drop(seed);
        Arc::new(
            StorageBackend::sqlite_read_only_with_max_readers_and_wal_ceiling(
                &path,
                Some(1),
                policy,
            )
            .expect("open main with observable read-only policy"),
        )
    } else {
        Arc::new(
            file_backend_with_policy(root, &path, policy)
                .expect("open fixture backend with explicit policy"),
        )
    }
}

fn empty_server() -> KhiveMcpServer {
    KhiveMcpServer::from_registry(
        VerbRegistryBuilder::new()
            .build()
            .expect("build registry without packs"),
    )
}

#[tokio::test]
async fn events_wal_policy_uses_core_pool_over_checkpoint_and_secondary() {
    let root = tempfile::tempdir().expect("private backend fixture root");
    let main_policy = WalCeilingPolicy {
        bytes: 8192,
        source: WalCeilingSource::BackendField,
    };
    let secondary_policy = WalCeilingPolicy {
        bytes: 0,
        source: WalCeilingSource::Environment,
    };
    let checkpoint_policy = WalCeilingPolicy::default();
    let main = file_backend(root.path(), "main.db", main_policy, true);
    let secondary = file_backend(root.path(), "secondary.db", secondary_policy, false);
    let config = RuntimeConfig {
        backend_id: BackendId::parse("secondary").expect("secondary backend id"),
        ..RuntimeConfig::no_embeddings()
    };
    let runtime = KhiveRuntime::from_backend(secondary, config).with_core_backend(main);
    assert_eq!(
        runtime.backend().pool().config().wal_ceiling,
        secondary_policy
    );
    assert_eq!(
        runtime.core().backend().pool().config().wal_ceiling,
        main_policy
    );
    let checkpoint = Arc::new(
        ConnectionPool::new(PoolConfig {
            max_readers: 1,
            wal_ceiling: checkpoint_policy,
            ..PoolConfig::default()
        })
        .expect("independent checkpoint pool"),
    );
    let server = empty_server().with_runtime(runtime).with_pool(checkpoint);
    assert_eq!(
        server.pool.as_ref().unwrap().config().wal_ceiling,
        checkpoint_policy
    );
    assert_eq!(server.events_wal_ceiling_policy(), Some(main_policy));
}

#[tokio::test]
async fn events_wal_policy_preserves_main_explicit_zero_over_config_snapshot() {
    let root = tempfile::tempdir().expect("private backend fixture root");
    let main_policy = WalCeilingPolicy {
        bytes: 0,
        source: WalCeilingSource::BackendField,
    };
    let main = file_backend(root.path(), "main.db", main_policy, false);
    let runtime = KhiveRuntime::from_backend(
        main,
        RuntimeConfig {
            wal_ceiling_bytes: 67_108_864,
            wal_ceiling_configured_bytes: 67_108_864,
            wal_ceiling_source: WalCeilingSource::Environment,
            ..RuntimeConfig::no_embeddings()
        },
    );
    assert_eq!(runtime.config().wal_ceiling_bytes, 67_108_864);
    assert_eq!(
        runtime.config().wal_ceiling_source,
        WalCeilingSource::Environment
    );
    assert_eq!(runtime.backend().pool().config().wal_ceiling, main_policy);
    let server = empty_server().with_runtime(runtime);
    assert_eq!(server.events_wal_ceiling_policy(), Some(main_policy));
}

#[test]
fn events_wal_policy_is_absent_without_runtime_even_with_checkpoint_pool() {
    let checkpoint = Arc::new(
        ConnectionPool::new(PoolConfig {
            max_readers: 1,
            wal_ceiling: WalCeilingPolicy {
                bytes: 0,
                source: WalCeilingSource::Environment,
            },
            ..PoolConfig::default()
        })
        .expect("independent checkpoint pool"),
    );
    let server = empty_server().with_pool(checkpoint);
    assert_eq!(server.events_wal_ceiling_policy(), None);
}
