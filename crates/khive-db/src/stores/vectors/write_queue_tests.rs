use std::sync::Arc;
use std::time::Duration;

use khive_storage::types::VectorRecord;
use khive_storage::VectorStore;
use khive_types::SubstrateKind;
use uuid::Uuid;

use super::*;
use crate::pool::{ConnectionPool, PoolConfig};

fn create_vec_table(pool: &Arc<ConnectionPool>, model_key: &str, dims: usize) {
    let ddl = format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS vec_{} USING vec0(\
             subject_id TEXT PRIMARY KEY, \
             namespace TEXT NOT NULL, \
             kind TEXT NOT NULL, \
             field TEXT NOT NULL, \
             embedding_model TEXT NOT NULL, \
             embedding float[{}] distance_metric=cosine)",
        model_key, dims
    );
    let writer = pool.writer().expect("writer");
    writer.conn().execute_batch(&ddl).expect("create vec table");
    writer
        .conn()
        .execute_batch(crate::migrations::VECTOR_PROVENANCE_DDL)
        .expect("create vector_provenance");
    writer
        .conn()
        .execute_batch(crate::migrations::ANN_WRITE_LOG_DDL)
        .expect("create ann_write_log");
}

/// Constructed via a `PoolConfig` literal (`write_queue_enabled: Some(true)`),
/// not the `KHIVE_WRITE_QUEUE` env var — that env var is process-global
/// and this crate's other tests are NOT `#[serial]` against it, so a
/// window where it is set here could leak into a
/// concurrently-scheduled test's own pool construction (ADR-067
/// Component A). Builds the pool inline (rather than
/// via `make_file_backed_pool`, which hardcodes `PoolConfig::default()`)
/// so `write_queue_enabled` can be set directly in the literal.
#[tokio::test]
async fn insert_batch_routes_through_writer_task_when_flag_enabled() {
    crate::extension::ensure_extensions_loaded();

    let model_key = "write_queue_flag_test";
    let dims = 4usize;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("write_queue_vectors.db");
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(path),
            write_queue_enabled: Some(true),
            ..PoolConfig::for_test()
        })
        .expect("file-backed pool"),
    );
    create_vec_table(&pool, model_key, dims);

    let store = SqliteVecStore::new(
        Arc::clone(&pool),
        true,
        model_key.to_string(),
        model_key.to_string(),
        dims,
        "ns:test".to_string(),
    )
    .expect("SqliteVecStore::new");

    let id1 = Uuid::new_v4();
    let id2 = Uuid::new_v4();
    let records = vec![
        VectorRecord {
            subject_id: id1,
            kind: SubstrateKind::Entity,
            namespace: "ns:test".to_string(),
            field: "body".to_string(),
            embedding_model: None,
            vectors: vec![vec![0.1, 0.2, 0.3, 0.4]],
            text_fingerprint: None,
            updated_at: chrono::Utc::now(),
        },
        VectorRecord {
            subject_id: id2,
            kind: SubstrateKind::Entity,
            namespace: "ns:test".to_string(),
            field: "body".to_string(),
            embedding_model: None,
            vectors: vec![vec![0.5, 0.6, 0.7, 0.8]],
            text_fingerprint: None,
            updated_at: chrono::Utc::now(),
        },
    ];

    let summary = store.insert_batch(records).await.unwrap();
    assert_eq!(summary.attempted, 2);
    assert_eq!(summary.affected, 2);
    assert_eq!(summary.failed, 0);

    let present = store
        .batch_exists(&[id1, id2], "ns:test")
        .await
        .expect("batch_exists");
    assert!(present.contains(&id1));
    assert!(present.contains(&id2));
    assert_eq!(
        pool.writer_task_spawn_count(),
        1,
        "the flag-ON path must actually spawn and use the writer task"
    );
}

/// Create the three core live-subject tables used by the anti-join.
/// Mirrors `orphan_sweep_tests::create_substrate_tables`;
/// duplicated here (rather than shared) because that helper is private to
/// its own sibling module — same convention as this module's own
/// `create_vec_table` duplicate.
fn create_substrate_tables(pool: &Arc<ConnectionPool>) {
    pool.try_writer()
        .expect("writer")
        .conn()
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS entities \
                     (id TEXT PRIMARY KEY, deleted_at INTEGER); \
                 CREATE TABLE IF NOT EXISTS notes \
                     (id TEXT PRIMARY KEY, deleted_at INTEGER); \
                 CREATE TABLE IF NOT EXISTS knowledge_atoms \
                     (id TEXT PRIMARY KEY, deleted_at INTEGER);",
        )
        .expect("create substrate tables");
}

/// Insert a substrate row into `entities`. `deleted_at = None` → live.
fn insert_entity(pool: &Arc<ConnectionPool>, id: Uuid, deleted_at: Option<i64>) {
    let id_str = id.to_string();
    pool.try_writer()
        .expect("writer")
        .conn()
        .execute(
            "INSERT INTO entities (id, deleted_at) VALUES (?1, ?2)",
            rusqlite::params![id_str, deleted_at],
        )
        .expect("insert entity");
}

/// ADR-067 Amendment 1: `orphan_sweep`'s flag-on path must route through
/// the pool-wide `WriterTask` (not `with_writer_unmanaged`'s pool-mutex
/// path) when the write queue is enabled — mirrors
/// `insert_batch_routes_through_writer_task_when_flag_enabled` above.
#[tokio::test]
async fn orphan_sweep_routes_through_writer_task_when_flag_enabled() {
    crate::extension::ensure_extensions_loaded();

    let model_key = "write_queue_orphan_sweep";
    let dims = 4usize;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("write_queue_orphan_sweep.db");
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(path),
            write_queue_enabled: Some(true),
            ..PoolConfig::for_test()
        })
        .expect("file-backed pool"),
    );
    create_substrate_tables(&pool);
    create_vec_table(&pool, model_key, dims);

    let store = SqliteVecStore::new(
        Arc::clone(&pool),
        true,
        model_key.to_string(),
        model_key.to_string(),
        dims,
        "ns:test".to_string(),
    )
    .expect("SqliteVecStore::new");

    let live_id = Uuid::new_v4();
    insert_entity(&pool, live_id, None); // live subject
    let orphan_id = Uuid::new_v4(); // no substrate row -> orphaned vector

    store
        .insert(
            live_id,
            SubstrateKind::Entity,
            "ns:test",
            "body",
            vec![vec![0.1, 0.2, 0.3, 0.4]],
        )
        .await
        .expect("insert live vector");
    store
        .insert(
            orphan_id,
            SubstrateKind::Entity,
            "ns:test",
            "body",
            vec![vec![0.5, 0.6, 0.7, 0.8]],
        )
        .await
        .expect("insert orphan vector");

    // Dry run: reports the orphan without deleting it.
    let dry = store
        .orphan_sweep(&OrphanSweepConfig {
            subject_id_allowlist: None,
            namespaces: vec![],
            substrate_kinds: vec![],
            max_delete: 100,
            dry_run: true,
        })
        .await
        .expect("dry-run sweep");
    assert_eq!(dry.scanned, 2);
    assert_eq!(dry.would_delete, 1);
    assert_eq!(dry.deleted, 0);
    assert!(!dry.max_delete_hit);

    // Real sweep: deletes the orphan, keeps the live vector.
    let real = store
        .orphan_sweep(&OrphanSweepConfig {
            subject_id_allowlist: None,
            namespaces: vec![],
            substrate_kinds: vec![],
            max_delete: 100,
            dry_run: false,
        })
        .await
        .expect("real sweep");
    assert_eq!(real.scanned, 2);
    assert_eq!(real.would_delete, 1);
    assert_eq!(real.deleted, 1);
    assert!(!real.max_delete_hit);

    let present = store
        .batch_exists(&[live_id, orphan_id], "ns:test")
        .await
        .expect("batch_exists");
    assert!(
        present.contains(&live_id),
        "live vector must survive the sweep"
    );
    assert!(
        !present.contains(&orphan_id),
        "orphaned vector must be swept"
    );

    // `writer_task_spawn_count() == 1` alone does not discriminate the
    // fix from a regression: `SqliteVecStore::new` and the two setup
    // `store.insert(..)` calls above already spawn and use the writer
    // task, so that counter would read 1 even if `orphan_sweep` itself
    // had reverted to the legacy `with_writer_unmanaged` path. Prove
    // routing directly instead, mirroring
    // `upsert_entity_routes_through_writer_task_when_flag_enabled`
    // (entity_tests.rs): hold the writer task's single drain slot open
    // with an occupier parked on a oneshot (`blocking_recv`, valid
    // inside the writer task's `spawn_blocking`), then call
    // `orphan_sweep` on a separate task and poll
    // `WriterTaskHandle::queue_depth()`. A version that genuinely
    // routes through `writer_task.send(..)` must show the request
    // sitting in the channel (`queue_depth() >= 1`) while the occupier
    // holds the slot; a version that fell back to
    // `with_writer_unmanaged`'s pool-mutex path never touches this
    // channel, so `queue_depth()` would stay `0` for the whole poll
    // window — the failure mode this test exists to catch.
    let writer_task = pool
        .writer_task_handle()
        .expect("writer task handle")
        .expect("writer task must be spawned for a file-backed pool with the flag on");

    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let occupier = {
        let writer_task = writer_task.clone();
        tokio::spawn(async move {
            writer_task
                .send(move |_conn| {
                    let _ = started_tx.send(());
                    let _ = release_rx.blocking_recv();
                    Ok::<(), StorageError>(())
                })
                .await
        })
    };

    started_rx
        .await
        .expect("occupier must signal it has started running inside the writer task");
    assert_eq!(
        writer_task.queue_depth(),
        0,
        "channel must start empty once the occupier has been dequeued and is running"
    );

    let sweep_task = tokio::spawn(async move {
        store
            .orphan_sweep(&OrphanSweepConfig {
                subject_id_allowlist: None,
                namespaces: vec![],
                substrate_kinds: vec![],
                max_delete: 100,
                dry_run: true,
            })
            .await
    });

    let mut saw_enqueued = false;
    for _ in 0..100 {
        if writer_task.queue_depth() >= 1 {
            saw_enqueued = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        saw_enqueued,
        "orphan_sweep's write request never appeared in the writer task's channel \
             while the occupier held the single drain slot — orphan_sweep is not routing \
             through the shared writer task"
    );

    release_tx
        .send(())
        .expect("occupier must still be waiting on the release signal");
    occupier
        .await
        .expect("occupier task must not panic")
        .expect("occupier write must succeed");
    let post_sweep = sweep_task
        .await
        .expect("sweep task must not panic")
        .expect("orphan_sweep must succeed once unblocked");
    assert_eq!(
        post_sweep.scanned, 1,
        "only the surviving live vector remains after the earlier real sweep"
    );
}

/// Revert-and-confirm-fails companion (mirrors the pattern in
/// `crates/khive-vcs/src/sync.rs::checkpoint_wal_write_queue_tests`): the
/// OLD `orphan_sweep` shape — a closure that opens its own
/// `Transaction::new_unchecked`/`BEGIN IMMEDIATE` — must fail if routed
/// through the WriterTask channel. `run_writer_task`'s drain loop already
/// wraps every request in its own `BEGIN IMMEDIATE` before invoking the
/// closure, so a second `BEGIN IMMEDIATE` issued from inside the closure
/// violates SQLite's nested-transaction rule. This proves the fix's
/// DML-only extraction (`orphan_sweep_dml`, no inner `BEGIN`) is
/// required — naively forwarding the old closure to `writer_task.send()`
/// would not have worked.
#[tokio::test]
async fn orphan_sweep_old_unmanaged_shape_nests_transaction_under_write_queue() {
    crate::extension::ensure_extensions_loaded();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("write_queue_orphan_sweep_regression.db");
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(path),
            write_queue_enabled: Some(true),
            ..PoolConfig::for_test()
        })
        .expect("file-backed pool"),
    );
    create_substrate_tables(&pool);
    create_vec_table(&pool, "write_queue_orphan_sweep_regression", 4);

    let writer_task = pool
        .writer_task_handle()
        .expect("writer task handle")
        .expect("writer task must spawn for a file-backed pool with the flag on");

    let result: Result<(), StorageError> = writer_task
        .send(move |conn| {
            // The OLD orphan_sweep shape: opens its own BEGIN IMMEDIATE via
            // `Transaction::new_unchecked`. Under the write queue this
            // closure already runs inside the drain loop's own open
            // transaction, so this must fail with SQLite's
            // nested-transaction error.
            let tx = rusqlite::Transaction::new_unchecked(
                conn,
                rusqlite::TransactionBehavior::Immediate,
            )
            .map_err(|e| map_err(e, "orphan_sweep_old_shape"))?;
            tx.commit()
                .map_err(|e| map_err(e, "orphan_sweep_old_shape"))?;
            Ok(())
        })
        .await;

    let err = result.expect_err(
        "routing the OLD orphan_sweep closure (its own BEGIN IMMEDIATE) through the \
             WriterTask must fail under KHIVE_WRITE_QUEUE — if this now succeeds, re-audit \
             whether the WriterTask still owns the sole BEGIN IMMEDIATE for this connection",
    );
    let msg = err.to_string();
    assert!(
        msg.contains("cannot start a transaction within a transaction"),
        "expected the deterministic nested-transaction failure (SQLite's own message \
             for a second BEGIN issued inside an already-open transaction), got: {msg}"
    );
}

/// ADR-136 D1 gate 2/4: `vec_delete_subjects`'s flag-on path must route
/// through the pool-wide `WriterTask`, not `with_writer_unmanaged`'s
/// pool-mutex path, when the write queue is enabled — same occupier /
/// `queue_depth()` technique as
/// `orphan_sweep_routes_through_writer_task_when_flag_enabled` above (a
/// `writer_task_spawn_count() == 1` assertion alone is a false positive:
/// `SqliteVecStore::new` and the setup insert already spawn/use the
/// task). Red-proof: reverting the
/// `current_writer_task("vec_delete_subjects")` branch (forcing every
/// call through `with_writer_unmanaged`) makes `saw_enqueued` stay
/// `false` and this test fail — see the impl report for the exact
/// revert/run/restore transcript.
#[tokio::test]
async fn vec_delete_subjects_routes_through_writer_task_when_flag_enabled() {
    crate::extension::ensure_extensions_loaded();

    let model_key = "write_queue_vec_delete_subjects";
    let dims = 4usize;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("write_queue_vec_delete_subjects.db");
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(path),
            write_queue_enabled: Some(true),
            ..PoolConfig::for_test()
        })
        .expect("file-backed pool"),
    );
    create_vec_table(&pool, model_key, dims);

    let store = SqliteVecStore::new(
        Arc::clone(&pool),
        true,
        model_key.to_string(),
        model_key.to_string(),
        dims,
        "ns:test".to_string(),
    )
    .expect("SqliteVecStore::new");

    let id = Uuid::new_v4();
    store
        .insert(
            id,
            SubstrateKind::Entity,
            "ns:test",
            "body",
            vec![vec![0.1, 0.2, 0.3, 0.4]],
        )
        .await
        .expect("insert vector");

    let writer_task = pool
        .writer_task_handle()
        .expect("writer task handle")
        .expect("writer task must be spawned for a file-backed pool with the flag on");

    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let occupier = {
        let writer_task = writer_task.clone();
        tokio::spawn(async move {
            writer_task
                .send(move |_conn| {
                    let _ = started_tx.send(());
                    let _ = release_rx.blocking_recv();
                    Ok::<(), StorageError>(())
                })
                .await
        })
    };

    started_rx
        .await
        .expect("occupier must signal it has started running inside the writer task");
    assert_eq!(
        writer_task.queue_depth(),
        0,
        "channel must start empty once the occupier has been dequeued and is running"
    );

    let delete_task = tokio::spawn(async move { store.delete_subjects(&[id]).await });

    let mut saw_enqueued = false;
    for _ in 0..100 {
        if writer_task.queue_depth() >= 1 {
            saw_enqueued = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        saw_enqueued,
        "vec_delete_subjects's write request never appeared in the writer task's channel \
             while the occupier held the single drain slot — vec_delete_subjects is not \
             routing through the shared writer task"
    );

    release_tx
        .send(())
        .expect("occupier must still be waiting on the release signal");
    occupier
        .await
        .expect("occupier task must not panic")
        .expect("occupier write must succeed");
    let deleted = delete_task
        .await
        .expect("delete task must not panic")
        .expect("vec_delete_subjects must succeed once unblocked");
    assert_eq!(deleted, 1);
}

/// ADR-136 D1 gate 3/4: with `KHIVE_WRITE_ROUTING=strict` and no writer
/// task available, `vec_delete_subjects` must error instead of silently
/// falling back to `with_writer_unmanaged`'s pool-mutex path.
#[tokio::test]
async fn vec_delete_subjects_strict_routing_fails_closed_without_writer_task() {
    crate::extension::ensure_extensions_loaded();

    let model_key = "strict_vec_delete_subjects";
    let dims = 4usize;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("strict_vec_delete_subjects.db");
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(path),
            write_queue_enabled: Some(false),
            write_routing_strict: true,
            ..PoolConfig::for_test()
        })
        .expect("file-backed pool"),
    );
    create_vec_table(&pool, model_key, dims);

    let store = SqliteVecStore::new(
        Arc::clone(&pool),
        true,
        model_key.to_string(),
        model_key.to_string(),
        dims,
        "ns:test".to_string(),
    )
    .expect("SqliteVecStore::new");

    let id = Uuid::new_v4();
    let err = store.delete_subjects(&[id]).await.expect_err(
        "KHIVE_WRITE_ROUTING=strict with no writer task must fail closed, not silently \
             fall back to with_writer_unmanaged",
    );
    assert!(
        err.to_string().contains("strict"),
        "error must name strict routing, got: {err}"
    );
}

/// ADR-136 D1 gate 3/4: same fail-closed contract as
/// `vec_delete_subjects_strict_routing_fails_closed_without_writer_task`,
/// for `orphan_sweep`'s own `with_writer_unmanaged` fallback.
#[tokio::test]
async fn orphan_sweep_strict_routing_fails_closed_without_writer_task() {
    crate::extension::ensure_extensions_loaded();

    let model_key = "strict_orphan_sweep";
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("strict_orphan_sweep.db");
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(path),
            write_queue_enabled: Some(false),
            write_routing_strict: true,
            ..PoolConfig::for_test()
        })
        .expect("file-backed pool"),
    );
    create_substrate_tables(&pool);
    create_vec_table(&pool, model_key, 4);

    let store = SqliteVecStore::new(
        Arc::clone(&pool),
        true,
        model_key.to_string(),
        model_key.to_string(),
        4,
        "ns:test".to_string(),
    )
    .expect("SqliteVecStore::new");

    let err = store
        .orphan_sweep(&OrphanSweepConfig {
            subject_id_allowlist: None,
            namespaces: vec![],
            substrate_kinds: vec![],
            max_delete: 100,
            dry_run: true,
        })
        .await
        .expect_err(
            "KHIVE_WRITE_ROUTING=strict with no writer task must fail closed, not \
                 silently fall back to with_writer_unmanaged",
        );
    assert!(
        err.to_string().contains("strict"),
        "error must name strict routing, got: {err}"
    );
}

/// ADR-136 D1 gate 3 amendment: a store built on a thread with no
/// ambient Tokio runtime caches `writer_task: None` at construction —
/// the pool returns `Err(WriterTaskNoRuntime)`, which `SqliteVecStore::
/// new` collapses via `.ok().flatten()` (a documented, deliberate
/// best-effort degrade). The bug this guards against: without
/// `with_writer`'s write-time re-lookup (`current_writer_task`), that
/// construction-time `None` would stick forever, so a *normal* vector
/// write (`insert`, routed through the general `with_writer` helper, not
/// a maintenance path) issued later inside a real runtime would silently
/// bypass the queue via the direct-connection path instead of routing
/// through the shared `WriterTask` like every other write on this pool.
/// Same occupier / `queue_depth()` discriminator as
/// `vec_delete_subjects_routes_through_writer_task_when_flag_enabled`
/// above, proving genuine queue routing rather than a
/// `writer_task_spawn_count() == 1` false positive.
///
/// Deliberately `#[test]`, not `#[tokio::test]`: construction must
/// happen with no ambient runtime, which a `#[tokio::test]` function
/// body would not give it (the whole test body already runs on a Tokio
/// worker thread). Red-proof: reverting `with_writer`'s
/// `self.current_writer_task(operation)` check back to the cached handle
/// makes `saw_enqueued` stay `false` and this test fail — the write takes
/// the direct-connection path immediately instead of ever appearing in
/// the writer task's channel.
#[test]
fn general_write_routes_through_writer_task_when_store_built_outside_runtime() {
    crate::extension::ensure_extensions_loaded();

    let model_key = "general_write_no_runtime_construction";
    let dims = 4usize;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("general_write_no_runtime_construction.db");
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(path),
            write_queue_enabled: Some(true),
            ..PoolConfig::for_test()
        })
        .expect("file-backed pool"),
    );
    create_vec_table(&pool, model_key, dims);

    assert!(
        tokio::runtime::Handle::try_current().is_err(),
        "sanity: this test body must not already be running inside a Tokio runtime"
    );
    // Construction happens here, outside any runtime — reproduces the
    // permanent-`None`-cache scenario `writer_task_handle()`'s doc
    // comment describes.
    let store = SqliteVecStore::new(
        Arc::clone(&pool),
        true,
        model_key.to_string(),
        model_key.to_string(),
        dims,
        "ns:test".to_string(),
    )
    .expect("SqliteVecStore::new");

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async move {
        let writer_task = pool
            .writer_task_handle()
            .unwrap()
            .expect("writer task must be available now that a runtime exists");

        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let occupier = {
            let writer_task = writer_task.clone();
            tokio::spawn(async move {
                writer_task
                    .send(move |_conn| {
                        let _ = started_tx.send(());
                        let _ = release_rx.blocking_recv();
                        Ok::<(), StorageError>(())
                    })
                    .await
            })
        };
        started_rx
            .await
            .expect("occupier must signal it has started running inside the writer task");
        assert_eq!(
            writer_task.queue_depth(),
            0,
            "channel must start empty once the occupier has been dequeued and is running"
        );

        let id = Uuid::new_v4();
        let write_task = tokio::spawn(async move {
            store
                .insert(
                    id,
                    SubstrateKind::Entity,
                    "ns:test",
                    "body",
                    vec![vec![0.1, 0.2, 0.3, 0.4]],
                )
                .await
        });

        let mut saw_enqueued = false;
        for _ in 0..100 {
            if writer_task.queue_depth() >= 1 {
                saw_enqueued = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            saw_enqueued,
            "insert's write request never appeared in the writer task's channel while \
                 the occupier held the single drain slot — a store built outside a runtime \
                 is not re-checking writer-task availability at write time"
        );

        release_tx
            .send(())
            .expect("occupier must still be waiting on the release signal");
        occupier
            .await
            .expect("occupier task must not panic")
            .expect("occupier write must succeed");
        write_task
            .await
            .expect("write task must not panic")
            .expect("insert must succeed once unblocked");
    });
}
