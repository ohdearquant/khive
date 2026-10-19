use super::*;

use std::sync::Arc as StdArc;

use khive_db::{ConnectionPool, PoolConfig, SqlBridge};
use khive_storage::types::{SqlValue, StorageResult as StorageResultAlias};
use uuid::Uuid;

/// Owns a file-backed pool and removes its database directory after shutdown.
struct TestPool {
    pool: StdArc<ConnectionPool>,
    _temp_dir: tempfile::TempDir,
}

impl std::ops::Deref for TestPool {
    type Target = StdArc<ConnectionPool>;

    fn deref(&self) -> &Self::Target {
        &self.pool
    }
}

/// A scratch pool wired exactly like the daemon.rs / sql_bridge.rs
/// tests above it: file-backed (atomic_unit's single-writer path is
/// only reachable file-backed), `write_queue_enabled: Some(true)` so
/// `atomic_unit` routes through the real `WriterTask` + `block_on_sync`
/// seam rather than the flag-off manual-transaction fallback — the
/// suspend-trap contract only fires on this path.
fn scratch_pool(name: &str) -> TestPool {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(format!("{name}.db"));
    let pool = StdArc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(path),
            write_queue_enabled: Some(true),
            ..PoolConfig::for_test()
        })
        .expect("pool open"),
    );
    TestPool {
        pool,
        _temp_dir: dir,
    }
}

/// Minimal real schema slice (`entities`, `graph_edges`) — copied from
/// `crates/khive-db/sql/entities-ddl.sql` / `graph-ddl.sql` rather than
/// invented ad hoc, since the dangling-edge and merge-rewire tests below
/// depend on `graph_edges` having no foreign-key enforcement (ADR-099
/// D1: "`graph_edges` has no foreign-key enforcement ... SQLite will
/// happily commit the inconsistency" absent the guard/predicate rules
/// this runner implements).
fn seed_schema(pool: &ConnectionPool) {
    let writer = pool.try_writer().expect("writer");
    writer
        .autocommit(|conn| {
            Ok(conn.execute_batch(
                "CREATE TABLE entities (
                    id             TEXT PRIMARY KEY,
                    namespace      TEXT NOT NULL,
                    kind           TEXT NOT NULL,
                    entity_type    TEXT,
                    name           TEXT NOT NULL,
                    description    TEXT,
                    properties     TEXT,
                    tags           TEXT NOT NULL DEFAULT '[]',
                    created_at     INTEGER NOT NULL,
                    updated_at     INTEGER NOT NULL,
                    version        INTEGER NOT NULL DEFAULT 1,
                    deleted_at     INTEGER,
                    merged_into    TEXT,
                    merge_event_id TEXT
                );
                CREATE TABLE graph_edges (
                    namespace      TEXT NOT NULL,
                    id             TEXT NOT NULL,
                    source_id      TEXT NOT NULL,
                    target_id      TEXT NOT NULL,
                    relation       TEXT NOT NULL,
                    weight         REAL NOT NULL DEFAULT 1.0,
                    created_at     INTEGER NOT NULL,
                    updated_at     INTEGER NOT NULL,
                    deleted_at     INTEGER,
                    metadata       TEXT,
                    target_backend TEXT,
                    PRIMARY KEY (namespace, id)
                );",
            )?)
        })
        .expect("seed schema");
}

fn insert_entity(pool: &ConnectionPool, id: Uuid, name: &str) {
    let writer = pool.try_writer().expect("writer");
    writer
        .autocommit(|conn| {
            Ok(conn.execute(
                "INSERT INTO entities \
                 (id, namespace, kind, name, created_at, updated_at) \
                 VALUES (?1, 'local', 'concept', ?2, 0, 0)",
                rusqlite::params![id.to_string(), name],
            )?)
        })
        .expect("insert entity");
}

fn entity_exists(pool: &ConnectionPool, id: Uuid) -> bool {
    let writer = pool.try_writer().expect("writer");
    let count: i64 = writer
        .autocommit(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM entities WHERE id = ?1 AND deleted_at IS NULL",
                rusqlite::params![id.to_string()],
                |row| row.get(0),
            )?)
        })
        .expect("query entity");
    count > 0
}

fn edge_count(pool: &ConnectionPool, source: Uuid, target: Uuid) -> i64 {
    let writer = pool.try_writer().expect("writer");
    writer
        .autocommit(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM graph_edges \
                 WHERE source_id = ?1 AND target_id = ?2 AND deleted_at IS NULL",
                rusqlite::params![source.to_string(), target.to_string()],
                |row| row.get(0),
            )?)
        })
        .expect("query edge")
}

fn entities_snapshot(pool: &ConnectionPool) -> Vec<String> {
    let writer = pool.try_writer().expect("writer");
    writer
        .autocommit(|conn| {
            let mut stmt = conn
                .prepare("SELECT id, name, deleted_at FROM entities ORDER BY id")
                .expect("prepare snapshot");
            let rows = stmt
                .query_map([], |row| {
                    let id: String = row.get(0)?;
                    let name: String = row.get(1)?;
                    let deleted_at: Option<i64> = row.get(2)?;
                    Ok(format!("{id}:{name}:{deleted_at:?}"))
                })
                .expect("query snapshot");
            Ok(rows
                .collect::<Result<Vec<_>, _>>()
                .expect("collect snapshot"))
        })
        .expect("snapshot scope")
}

fn delete_plan(id: Uuid, label: &str) -> AtomicOpPlan {
    AtomicOpPlan::Delete(DeletePlan {
        target_id: id,
        statements: vec![PlanStatement {
            statement: SqlStatement {
                sql: "DELETE FROM entities WHERE id = ?1 AND deleted_at IS NULL".to_string(),
                params: vec![SqlValue::Text(id.to_string())],
                label: Some(label.to_string()),
            },
            guard: Some(AffectedRowGuard::exactly(1)),
        }],
        post_commit: PostCommitEffect::None,
    })
}

fn rename_plan(id: Uuid, new_name: &str, label: &str) -> AtomicOpPlan {
    AtomicOpPlan::Update(Box::new(UpdatePlan {
        graph_effects: Vec::new(),
        note_vector_purge: None,
        note_embedding_inheritance: None,
        entity_guard: None,
        note_guard: None,
        target_id: id,
        statements: vec![PlanStatement {
            statement: SqlStatement {
                sql: "UPDATE entities SET version = version + 1, name = ?1, updated_at = 1 \
                          WHERE id = ?2 AND deleted_at IS NULL"
                    .to_string(),
                params: vec![
                    SqlValue::Text(new_name.to_string()),
                    SqlValue::Text(id.to_string()),
                ],
                label: Some(label.to_string()),
            },
            guard: Some(AffectedRowGuard::exactly(1)),
        }],
        post_commit: PostCommitEffect::None,
        edge_natural_key: None,
        idempotent_noop: false,
    }))
}

/// The guarded `INSERT ... SELECT ... WHERE EXISTS` shape `LinkPlan`
/// documents: the affected-row count of this ONE statement doubles as
/// the in-transaction endpoint-existence probe (ADR-099 acceptance
/// criteria: dangling-edge).
fn link_plan(edge_id: Uuid, source: Uuid, target: Uuid) -> AtomicOpPlan {
    AtomicOpPlan::Link(LinkPlan {
            source_id: source,
            target_id: target,
            statements: vec![PlanStatement {
                statement: SqlStatement {
                    sql: "INSERT INTO graph_edges \
                          (namespace, id, source_id, target_id, relation, created_at, updated_at) \
                          SELECT 'local', ?1, ?2, ?3, 'annotates', 0, 0 \
                          WHERE EXISTS (SELECT 1 FROM entities WHERE id = ?2 AND deleted_at IS NULL) \
                            AND EXISTS (SELECT 1 FROM entities WHERE id = ?3 AND deleted_at IS NULL)"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(edge_id.to_string()),
                        SqlValue::Text(source.to_string()),
                        SqlValue::Text(target.to_string()),
                    ],
                    label: Some("insert-edge-where-exists".to_string()),
                },
                guard: Some(AffectedRowGuard::exactly(1)),
            }],
            disposition: khive_storage::EdgeUpsertDisposition::Created,
            dependency_kind_inference_hint: None,
        })
}

fn merge_plan(into_id: Uuid, from_id: Uuid) -> AtomicOpPlan {
    AtomicOpPlan::Merge(MergePlan {
        into_id,
        from_id,
        rewires: vec![crate::atomic_plan::PlanPredicate {
            description: "source_id = :from".to_string(),
            statement: SqlStatement {
                sql: "UPDATE graph_edges SET source_id = ?1, updated_at = 1 \
                          WHERE source_id = ?2"
                    .to_string(),
                params: vec![
                    SqlValue::Text(into_id.to_string()),
                    SqlValue::Text(from_id.to_string()),
                ],
                label: Some("merge-rewire".to_string()),
            },
        }],
        lifecycle: vec![PlanStatement {
            statement: SqlStatement {
                sql: "UPDATE entities SET version = version + 1, deleted_at = 1, merged_into = ?1 \
                          WHERE id = ?2 AND deleted_at IS NULL"
                    .to_string(),
                params: vec![
                    SqlValue::Text(into_id.to_string()),
                    SqlValue::Text(from_id.to_string()),
                ],
                label: Some("tombstone-from-entity".to_string()),
            },
            guard: Some(AffectedRowGuard::exactly(1)),
        }],
    })
}

// ------------------------------------------------------------------
// 1. Rollback end-to-end
// ------------------------------------------------------------------

#[tokio::test]
async fn rollback_end_to_end_leaves_zero_partial_state() {
    let pool = scratch_pool("rollback_end_to_end");
    seed_schema(&pool);
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    insert_entity(&pool, a, "alpha");
    insert_entity(&pool, b, "bravo");
    let before = entities_snapshot(&pool);

    let bridge = SqlBridge::new(StdArc::clone(&pool), true);
    let plans = vec![
        rename_plan(a, "alpha-renamed", "rename-a"),
        // Induced failure: b does not exist under this id, so the
        // guard (expects exactly 1 affected row) fails.
        delete_plan(Uuid::new_v4(), "delete-nonexistent"),
        rename_plan(b, "bravo-renamed", "rename-b"),
    ];

    let outcome = run_atomic_unit(&bridge, plans).await.expect("seam call ok");
    match outcome {
        AtomicRunOutcome::RolledBack {
            failed_op_index,
            failure,
        } => {
            assert_eq!(failed_op_index, 1);
            assert!(matches!(failure, AtomicOpFailure::GuardFailed { .. }));
        }
        other => panic!("expected RolledBack, got {other:?}"),
    }

    let after = entities_snapshot(&pool);
    assert_eq!(
        before, after,
        "a mid-unit failure must leave the database byte-for-byte unchanged"
    );
}

// ------------------------------------------------------------------
// 2. Suspend-trap paired: the runner's happy path resolves on first
//    poll (commits); a hand-built suspending closure through the SAME
//    `atomic_unit` seam fails loudly instead of silently succeeding.
// ------------------------------------------------------------------

#[tokio::test]
async fn commit_pass_resolves_on_first_poll_and_commits() {
    let pool = scratch_pool("suspend_trap_happy_path");
    seed_schema(&pool);
    let a = Uuid::new_v4();
    insert_entity(&pool, a, "alpha");

    let bridge = SqlBridge::new(StdArc::clone(&pool), true);
    let plans = vec![rename_plan(a, "alpha-v2", "rename-a")];

    let outcome = run_atomic_unit(&bridge, plans).await.expect("seam call ok");
    assert!(
        matches!(outcome, AtomicRunOutcome::Committed { .. }),
        "the real commit-pass closure (SAVEPOINT + guarded DML only) must \
             resolve on block_on_sync's first poll and commit: {outcome:?}"
    );

    let writer = pool.try_writer().expect("writer");
    let name: String = writer
        .autocommit(|conn| {
            Ok(conn.query_row(
                "SELECT name FROM entities WHERE id = ?1",
                rusqlite::params![a.to_string()],
                |row| row.get(0),
            )?)
        })
        .expect("query renamed entity");
    assert_eq!(name, "alpha-v2");
}

#[tokio::test]
async fn hand_built_suspending_closure_fails_loudly_through_the_same_seam() {
    // The exact misuse the module-doc "suspend-trap" promise guards
    // against: a closure sent through this crate's OWN integration of
    // `SqlAccess::atomic_unit` (the same `bridge` type `run_atomic_unit`
    // uses) that reaches a real suspension point instead of staying
    // synchronous DML — a stand-in for "a future edit admits a verb
    // whose apply forgot to hoist its embedding out of the transaction"
    // (ADR-099 acceptance criteria). `tokio::task::yield_now` returns
    // `Poll::Pending` on its first poll by construction, so this is a
    // real suspension, not `std::future::pending`'s permanent one.
    let pool = scratch_pool("suspend_trap_misuse");
    seed_schema(&pool);

    let bridge = SqlBridge::new(StdArc::clone(&pool), true);
    let suspending_op: AtomicUnitOp = Box::new(|_writer| {
        Box::pin(async move {
            tokio::task::yield_now().await;
            Ok(Box::new(()) as Box<dyn Any + Send>)
        })
    });

    let result: StorageResultAlias<Box<dyn Any + Send>> =
        khive_storage::SqlAccess::atomic_unit(&bridge, suspending_op).await;
    assert!(
        result.is_err(),
        "a closure that suspends on first poll must fail loudly through \
             `atomic_unit`, never silently succeed or wedge; got {result:?}"
    );
}

// ------------------------------------------------------------------
// 3. Staleness both directions
// ------------------------------------------------------------------

#[tokio::test]
async fn concurrent_mutation_between_prepare_and_apply_trips_guard_and_rolls_back() {
    let pool = scratch_pool("staleness_tripped");
    seed_schema(&pool);
    let a = Uuid::new_v4();
    insert_entity(&pool, a, "alpha");

    // Plan prepared against pre-transaction state (a exists).
    let plan = rename_plan(a, "alpha-v2", "rename-a");

    // Concurrent mutation between prepare and apply: a is deleted by a
    // separate writer before the commit pass runs.
    {
        let writer = pool.try_writer().expect("writer");
        writer
            .autocommit(|conn| {
                Ok(conn.execute(
                    "DELETE FROM entities WHERE id = ?1",
                    rusqlite::params![a.to_string()],
                )?)
            })
            .expect("concurrent delete");
    }

    let bridge = SqlBridge::new(StdArc::clone(&pool), true);
    let outcome = run_atomic_unit(&bridge, vec![plan])
        .await
        .expect("seam call ok");
    match outcome {
        AtomicRunOutcome::RolledBack {
            failed_op_index,
            failure,
        } => {
            assert_eq!(failed_op_index, 0);
            assert!(matches!(
                failure,
                AtomicOpFailure::GuardFailed { observed: 0, .. }
            ));
        }
        other => panic!("expected RolledBack from the stale guard, got {other:?}"),
    }
}

#[tokio::test]
async fn no_mutation_twin_commits() {
    let pool = scratch_pool("staleness_untripped");
    seed_schema(&pool);
    let a = Uuid::new_v4();
    insert_entity(&pool, a, "alpha");

    let plan = rename_plan(a, "alpha-v2", "rename-a");
    let bridge = SqlBridge::new(StdArc::clone(&pool), true);
    let outcome = run_atomic_unit(&bridge, vec![plan])
        .await
        .expect("seam call ok");
    assert!(matches!(outcome, AtomicRunOutcome::Committed { .. }));
    assert!(entity_exists(&pool, a));
}

// ------------------------------------------------------------------
// 4. Dangling-edge
// ------------------------------------------------------------------

#[tokio::test]
async fn dangling_edge_from_delete_then_link_rolls_back_whole_unit() {
    let pool = scratch_pool("dangling_edge");
    seed_schema(&pool);
    let x = Uuid::new_v4();
    let a = Uuid::new_v4();
    insert_entity(&pool, x, "x");
    insert_entity(&pool, a, "a");
    let before = entities_snapshot(&pool);

    let bridge = SqlBridge::new(StdArc::clone(&pool), true);
    let edge_id = Uuid::new_v4();
    let plans = vec![delete_plan(x, "delete-x"), link_plan(edge_id, a, x)];

    let outcome = run_atomic_unit(&bridge, plans).await.expect("seam call ok");
    match outcome {
        AtomicRunOutcome::RolledBack {
            failed_op_index,
            failure,
        } => {
            assert_eq!(
                failed_op_index, 1,
                "delete(x) succeeds; link(a, x) is the op whose \
                     endpoint-existence guard fails once x is gone"
            );
            assert!(matches!(
                failure,
                AtomicOpFailure::GuardFailed { observed: 0, .. }
            ));
        }
        other => panic!("expected RolledBack, got {other:?}"),
    }

    assert_eq!(
        edge_count(&pool, a, x),
        0,
        "no dangling edge may be committed"
    );
    assert_eq!(
        entities_snapshot(&pool),
        before,
        "x must still exist — the whole unit rolled back, including the delete"
    );
}

// ------------------------------------------------------------------
// 5. Merge-rewire
// ------------------------------------------------------------------

#[tokio::test]
async fn merge_rewire_sees_earlier_in_file_edge_write() {
    let pool = scratch_pool("merge_rewire");
    seed_schema(&pool);
    let z = Uuid::new_v4();
    let from = Uuid::new_v4();
    let into = Uuid::new_v4();
    insert_entity(&pool, z, "z");
    insert_entity(&pool, from, "from-entity");
    insert_entity(&pool, into, "into-entity");

    let bridge = SqlBridge::new(StdArc::clone(&pool), true);
    let edge_id = Uuid::new_v4();
    // [link(from, z), merge(into, from)] — the rewire's predicate-based
    // UPDATE must see the edge the earlier op in THIS SAME unit
    // inserted (ADR-099 acceptance criteria: "merge rewires see earlier
    // in-file writes").
    let plans = vec![link_plan(edge_id, from, z), merge_plan(into, from)];

    let outcome = run_atomic_unit(&bridge, plans).await.expect("seam call ok");
    assert!(
        matches!(outcome, AtomicRunOutcome::Committed { .. }),
        "expected the unit to commit: {outcome:?}"
    );

    assert_eq!(
        edge_count(&pool, from, z),
        0,
        "no live edge may remain sourced from the merged-away entity"
    );
    assert_eq!(
        edge_count(&pool, into, z),
        1,
        "the edge must be rewired onto `into`"
    );
    assert!(
        !entity_exists(&pool, from),
        "the `from` entity must be tombstoned (soft-deleted)"
    );
}

// ------------------------------------------------------------------
// 6. Zero-row
// ------------------------------------------------------------------

#[tokio::test]
async fn zero_row_apply_fails_the_whole_unit() {
    let pool = scratch_pool("zero_row");
    seed_schema(&pool);
    let x = Uuid::new_v4();
    insert_entity(&pool, x, "x");
    let before = entities_snapshot(&pool);

    let bridge = SqlBridge::new(StdArc::clone(&pool), true);
    // [delete(x, hard), update(x)] — update's affected-row guard
    // observes zero rows once x is already gone.
    let plans = vec![
        delete_plan(x, "delete-x"),
        rename_plan(x, "x-v2", "update-x"),
    ];

    let outcome = run_atomic_unit(&bridge, plans).await.expect("seam call ok");
    match outcome {
        AtomicRunOutcome::RolledBack {
            failed_op_index,
            failure,
        } => {
            assert_eq!(failed_op_index, 1);
            assert_eq!(
                failure,
                AtomicOpFailure::GuardFailed {
                    statement_label: Some("update-x".to_string()),
                    expected: AffectedRowGuard::exactly(1),
                    observed: 0,
                }
            );
        }
        other => panic!("expected RolledBack, got {other:?}"),
    }

    assert_eq!(
        entities_snapshot(&pool),
        before,
        "x must be restored — the whole unit rolled back"
    );
}

// ------------------------------------------------------------------
// Post-commit effect collection (UpdatePlan's reindex caveat, D3)
// ------------------------------------------------------------------

// ------------------------------------------------------------------
// 7. Daemon coexistence (ADR-099 B3 acceptance test 5)
// ------------------------------------------------------------------

/// An atomic unit must go through the SAME `WriterTaskHandle` queue as
/// an ordinary single-row write — not a separate connection that would
/// let it silently bypass single-writer serialization (ADR-067
/// Component A). This reuses the deterministic occupier pattern from
/// `khive-db::stores::graph_tests::upsert_edge_routes_through_writer_task_when_flag_enabled`
/// (Slice A): an occupier closure parks on a oneshot inside the writer
/// task's one drain slot, so both the atomic unit's `atomic_unit` call
/// and a concurrent ordinary `upsert_entity` call are forced to queue
/// behind it. `queue_depth` reaching 2 while both are pending, then
/// draining to 0 once the occupier releases and both complete, is the
/// proof that `run_atomic_unit` "discriminates" through the real queue
/// gauge rather than opening a competing connection.
#[tokio::test]
async fn atomic_unit_and_concurrent_normal_write_share_the_same_writer_queue() {
    let pool = scratch_pool("daemon_coexistence");
    seed_schema(&pool);
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    insert_entity(&pool, a, "alpha");

    let writer_task = pool
        .writer_task_handle()
        .expect("writer task lookup")
        .expect("writer task must be spawned with the flag on for a file-backed pool");

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

    // The atomic unit: renames `a`.
    let bridge = SqlBridge::new(StdArc::clone(&pool), true);
    let atomic_plans = vec![rename_plan(a, "alpha-atomic", "rename-a-atomic")];
    let atomic_task = tokio::spawn(async move { run_atomic_unit(&bridge, atomic_plans).await });

    // A concurrent ORDINARY write, routed through the SAME
    // `WriterTaskHandle::send` the real `with_writer` path uses
    // (khive-db's own `graph_tests` occupier test asserts this for
    // `upsert_edge`) — NOT a second `pool.try_writer()` connection,
    // which would open a competing SQLite handle and contend on the
    // file lock the occupier's `BEGIN IMMEDIATE` already holds,
    // producing a `DatabaseBusy` false failure unrelated to the queue
    // this test is actually proving something about.
    let normal_write_task = {
        let writer_task = writer_task.clone();
        let b_str = b.to_string();
        tokio::spawn(async move {
            writer_task
                .send(move |conn| {
                    conn.execute(
                        "INSERT INTO entities \
                             (id, namespace, kind, name, created_at, updated_at) \
                             VALUES (?1, 'local', 'concept', ?2, 0, 0)",
                        rusqlite::params![b_str, "bravo"],
                    )
                    .map_err(|e| StorageError::Internal(e.to_string()))
                })
                .await
        })
    };

    // Both the atomic unit's `atomic_unit` call and (best-effort) the
    // ordinary write must appear in the SAME queue while the occupier
    // holds the slot — this is the coexistence proof: neither one opens
    // a side-channel connection that would let it skip the queue.
    let mut saw_both_enqueued = false;
    for _ in 0..200 {
        if writer_task.queue_depth() >= 2 {
            saw_both_enqueued = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(
        saw_both_enqueued,
        "expected BOTH the atomic unit's atomic_unit() call and the \
             concurrent ordinary write to appear in the writer task's queue \
             while the occupier held the single drain slot (queue_depth \
             should have reached 2) — observed {}; run_atomic_unit is not \
             sharing the same queue as an ordinary write",
        writer_task.queue_depth()
    );

    release_tx.send(()).expect("release occupier");
    occupier
        .await
        .expect("occupier task join")
        .expect("occupier op ok");

    let atomic_outcome = atomic_task
        .await
        .expect("atomic task join")
        .expect("seam call ok");
    assert!(
        matches!(atomic_outcome, AtomicRunOutcome::Committed { .. }),
        "atomic unit must commit once the occupier releases: {atomic_outcome:?}"
    );
    normal_write_task
        .await
        .expect("normal write task join")
        .expect("normal write op ok");

    assert!(
        entity_exists(&pool, a),
        "a must exist (renamed) after commit"
    );
    {
        // Scoped: the `WriterGuard` returned by `try_writer()` holds a
        // non-reentrant `parking_lot::Mutex` for as long as it lives.
        // Left unscoped, it would still be held when `entity_exists`
        // below tries to check out the same writer, deadlocking (in
        // practice, timing out after `checkout_timeout`) against itself
        // rather than against anything the writer task/occupier hold.
        let writer = pool.try_writer().expect("writer");
        let renamed: String = writer
            .autocommit(|conn| {
                Ok(conn.query_row(
                    "SELECT name FROM entities WHERE id = ?1",
                    rusqlite::params![a.to_string()],
                    |row| row.get(0),
                )?)
            })
            .expect("query renamed entity");
        assert_eq!(renamed, "alpha-atomic");
    }
    assert!(
        entity_exists(&pool, b),
        "the concurrent ordinary write must also have landed"
    );
}

#[tokio::test]
async fn post_commit_effects_are_collected_in_op_order_and_only_for_update() {
    let pool = scratch_pool("post_commit_collection");
    seed_schema(&pool);
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    insert_entity(&pool, a, "alpha");
    insert_entity(&pool, b, "bravo");

    let bridge = SqlBridge::new(StdArc::clone(&pool), true);
    let mut update_a = match rename_plan(a, "alpha-v2", "rename-a") {
        AtomicOpPlan::Update(p) => p,
        _ => unreachable!(),
    };
    update_a.post_commit = PostCommitEffect::ReindexEntity { entity_id: a };
    let plans = vec![
        AtomicOpPlan::Update(update_a),
        delete_plan(b, "delete-b"), // no post-commit effect for delete
    ];

    let outcome = run_atomic_unit(&bridge, plans).await.expect("seam call ok");
    match outcome {
        AtomicRunOutcome::Committed { post_commit } => {
            fn require_commit_provenance(_: &CommittedPostCommitEffects) {}
            require_commit_provenance(&post_commit);
            assert_eq!(
                post_commit.as_slice(),
                &[PostCommitEffect::ReindexEntity { entity_id: a }]
            );
        }
        other => panic!("expected Committed, got {other:?}"),
    }
}

mod acknowledgement {
    use super::*;
    use khive_storage::{SqlReader, WriterTaskRequestState};

    #[derive(Clone, Copy, Debug)]
    enum Fault {
        Request(WriterTaskRequestState),
        Terminated(WriterTaskRequestState),
        EvidenceRequest(WriterTaskRequestState),
        EvidenceTerminated(WriterTaskRequestState),
    }

    impl Fault {
        fn error(self) -> StorageError {
            let error = match self {
                Self::Request(request_state) | Self::EvidenceRequest(request_state) => {
                    StorageError::WriterTaskRequestFailed {
                        request_state,
                        source: Box::new(StorageError::Internal(
                            "lost atomic acknowledgement".into(),
                        )),
                    }
                }
                Self::Terminated(request_state) | Self::EvidenceTerminated(request_state) => {
                    StorageError::writer_task_terminated(request_state)
                }
            };
            if matches!(self, Self::EvidenceRequest(_) | Self::EvidenceTerminated(_)) {
                error.with_sqlite_write_failure(khive_storage::error::SqliteWriteFailure {
                    stage: khive_storage::error::SqliteWriteStage::Commit,
                    primary_code: rusqlite::ffi::SQLITE_IOERR,
                    extended_code: rusqlite::ffi::SQLITE_IOERR_FSYNC,
                    settlement_unknown: false,
                })
            } else {
                error
            }
        }

        fn assert_preserved(self, error: StorageError) {
            assert_eq!(
                error.sqlite_write_failure().is_some(),
                matches!(self, Self::EvidenceRequest(_) | Self::EvidenceTerminated(_))
            );
            match (self, error.without_sqlite_write_stage()) {
                (
                    Self::Request(expected) | Self::EvidenceRequest(expected),
                    StorageError::WriterTaskRequestFailed {
                        request_state,
                        source,
                    },
                ) => {
                    assert_eq!(*request_state, expected);
                    assert!(matches!(source.as_ref(), StorageError::Internal(message)
                            if message == "lost atomic acknowledgement"));
                }
                (
                    Self::Terminated(expected) | Self::EvidenceTerminated(expected),
                    StorageError::WriterTaskTerminated { request_state, .. },
                ) => assert_eq!(*request_state, expected),
                (expected, actual) => panic!("expected {expected:?}, got {actual:?}"),
            }
        }
    }

    struct FaultAccess {
        bridge: SqlBridge,
        fault: Fault,
        invoke: bool,
        callback_result: StdArc<Mutex<Option<bool>>>,
    }

    #[async_trait::async_trait]
    impl SqlAccess for FaultAccess {
        async fn reader(&self) -> StorageResultAlias<Box<dyn SqlReader>> {
            self.bridge.reader().await
        }

        async fn writer(&self) -> StorageResultAlias<Box<dyn SqlWriter>> {
            self.bridge.writer().await
        }

        async fn atomic_unit(&self, op: AtomicUnitOp) -> StorageResultAlias<Box<dyn Any + Send>> {
            if self.invoke {
                let callback_result = StdArc::clone(&self.callback_result);
                let result = self
                    .bridge
                    .atomic_unit(Box::new(move |writer| {
                        Box::pin(async move {
                            let result = op(writer).await;
                            *callback_result.lock().expect("callback result") =
                                Some(result.is_ok());
                            result
                        })
                    }))
                    .await;
                assert_eq!(
                    Some(result.is_ok()),
                    *self.callback_result.lock().expect("callback result"),
                    "the real transaction must finish before replacing its acknowledgement"
                );
            }
            Err(self.fault.error())
        }
    }

    fn fault_access(pool: &TestPool, fault: Fault, invoke: bool) -> FaultAccess {
        FaultAccess {
            bridge: SqlBridge::new(StdArc::clone(pool), true),
            fault,
            invoke,
            callback_result: StdArc::new(Mutex::new(None)),
        }
    }

    fn effect_plan(id: Uuid) -> AtomicOpPlan {
        let AtomicOpPlan::Update(mut plan) = rename_plan(id, "changed", "rename") else {
            unreachable!()
        };
        plan.post_commit = PostCommitEffect::ReindexEntity { entity_id: id };
        AtomicOpPlan::Update(plan)
    }

    #[tokio::test]
    async fn recorded_failure_recovers_only_after_confirmed_rollback() {
        let pool = scratch_pool("recorded_rollback");
        seed_schema(&pool);
        let id = Uuid::new_v4();
        insert_entity(&pool, id, "original");
        let before = entities_snapshot(&pool);
        let access = fault_access(
            &pool,
            Fault::Request(WriterTaskRequestState::TransactionRolledBack),
            true,
        );
        let result = run_atomic_unit(
            &access,
            vec![
                effect_plan(id),
                rename_plan(Uuid::new_v4(), "missing", "missing-target"),
            ],
        )
        .await
        .expect("confirmed rollback recovers the recorded refusal");
        assert_eq!(
            result,
            AtomicRunOutcome::RolledBack {
                failed_op_index: 1,
                failure: AtomicOpFailure::GuardFailed {
                    statement_label: Some("missing-target".into()),
                    expected: AffectedRowGuard::exactly(1),
                    observed: 0,
                },
            },
        );
        assert_eq!(*access.callback_result.lock().unwrap(), Some(false));
        assert_eq!(entities_snapshot(&pool), before);
    }

    #[tokio::test]
    async fn recorded_failure_does_not_mask_unknown_or_terminal_outcomes() {
        for fault in [
            Fault::Request(WriterTaskRequestState::SideEffectsUnknown),
            Fault::Terminated(WriterTaskRequestState::SideEffectsUnknown),
            Fault::EvidenceRequest(WriterTaskRequestState::SideEffectsUnknown),
            Fault::EvidenceTerminated(WriterTaskRequestState::SideEffectsUnknown),
            Fault::Terminated(WriterTaskRequestState::TransactionRolledBack),
        ] {
            let pool = scratch_pool("recorded_uncertain");
            seed_schema(&pool);
            let id = Uuid::new_v4();
            insert_entity(&pool, id, "original");
            let before = entities_snapshot(&pool);
            let access = fault_access(&pool, fault, true);
            let error = run_atomic_unit(
                &access,
                vec![
                    effect_plan(id),
                    rename_plan(Uuid::new_v4(), "missing", "missing-target"),
                ],
            )
            .await
            .expect_err("a recorded refusal cannot replace the outer outcome");
            fault.assert_preserved(error.0);
            assert_eq!(*access.callback_result.lock().unwrap(), Some(false));
            assert_eq!(entities_snapshot(&pool), before);
        }
    }

    #[tokio::test]
    async fn failure_before_callback_cannot_manufacture_recorded_refusal() {
        for fault in [
            Fault::Request(WriterTaskRequestState::NotStarted),
            Fault::Request(WriterTaskRequestState::TransactionRolledBack),
            Fault::Terminated(WriterTaskRequestState::NotStarted),
        ] {
            let pool = scratch_pool("empty_failure_slot");
            seed_schema(&pool);
            let id = Uuid::new_v4();
            insert_entity(&pool, id, "original");
            let before = entities_snapshot(&pool);
            let access = fault_access(&pool, fault, false);
            let error = run_atomic_unit(&access, vec![effect_plan(id)])
                .await
                .expect_err("an empty slot must preserve the outer failure");
            fault.assert_preserved(error.0);
            assert_eq!(*access.callback_result.lock().unwrap(), None);
            assert_eq!(entities_snapshot(&pool), before);
        }
    }

    #[tokio::test]
    async fn successful_callback_with_lost_acknowledgement_returns_no_commit_token() {
        for fault in [
            Fault::Request(WriterTaskRequestState::SideEffectsUnknown),
            Fault::Terminated(WriterTaskRequestState::SideEffectsUnknown),
            Fault::EvidenceRequest(WriterTaskRequestState::SideEffectsUnknown),
            Fault::EvidenceTerminated(WriterTaskRequestState::SideEffectsUnknown),
        ] {
            let pool = scratch_pool("committed_ack_lost");
            seed_schema(&pool);
            let id = Uuid::new_v4();
            insert_entity(&pool, id, "original");
            let access = fault_access(&pool, fault, true);
            let error = run_atomic_unit(&access, vec![effect_plan(id)])
                .await
                .expect_err("callback success is not a committed-effects token");
            fault.assert_preserved(error.0);
            assert_eq!(*access.callback_result.lock().unwrap(), Some(true));
            let writer = pool.try_writer().expect("writer");
            let name: String = writer
                .autocommit(|conn| {
                    Ok(conn.query_row(
                        "SELECT name FROM entities WHERE id=?1",
                        rusqlite::params![id.to_string()],
                        |row| row.get(0),
                    )?)
                })
                .expect("persisted write");
            assert_eq!(
                name, "changed",
                "the lost acknowledgement does not imply rollback"
            );
        }
    }
}
