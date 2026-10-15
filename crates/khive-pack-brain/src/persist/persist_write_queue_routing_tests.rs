use super::*;
use khive_brain_core::BrainState;
use khive_runtime::{KhiveRuntime, Namespace};

/// Proves `persist_brain_state_mutation` is actually enqueued on the
/// pool's shared `WriterTaskHandle` channel when the write queue is
/// enabled, mirroring `fold_gate.rs`'s equivalent routing test (same
/// `queue_depth` + occupier-parked-on-oneshot technique; see that test's
/// doc comment for why a wall-clock test can't distinguish this).
#[tokio::test]
async fn persist_brain_state_mutation_routes_through_writer_task_when_flag_enabled() {
    // Token minted from an in-memory runtime: never touches the write
    // queue / env var, used only for its `NamespaceToken`.
    let token_rt = KhiveRuntime::memory().expect("memory runtime for token minting");
    let token = token_rt
        .authorize(Namespace::local())
        .expect("authorize local namespace");

    // The actual storage this test exercises: a bare, file-backed,
    // write-queue-enabled pool/bridge — no KhiveRuntime, no env var.
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("persist-write-queue-routing.db");
    let pool_cfg = khive_db::PoolConfig {
        path: Some(db_path),
        write_queue_enabled: Some(true),
        ..khive_db::PoolConfig::for_test()
    };
    let pool = std::sync::Arc::new(khive_db::ConnectionPool::new(pool_cfg).expect("pool"));
    pool.run_migrations().expect("migrations");
    let sql: std::sync::Arc<dyn SqlAccess> =
        std::sync::Arc::new(khive_db::SqlBridge::new(std::sync::Arc::clone(&pool), true));

    let writer_task = pool
        .writer_task_handle()
        .unwrap()
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
                    Ok::<(), khive_storage::StorageError>(())
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

    let tracker: Mutex<PersistenceTracker> = Mutex::new(PersistenceTracker::new());
    let state: Mutex<BrainState> = Mutex::new(BrainState::new(16));

    let persist_task = tokio::spawn(async move {
        persist_brain_state_mutation(
            sql.as_ref(),
            &token,
            &tracker,
            &state,
            BrainMutationEvent {
                profile_id: "write-queue-routing-profile".to_string(),
                event_kind: "brain.test_mutation".to_string(),
                payload: serde_json::json!({"probe": true}),
            },
            16,
            |_state: &mut BrainState| -> Result<(), RuntimeError> { Ok(()) },
        )
        .await
    });

    let mut saw_enqueued = false;
    for _ in 0..100 {
        if writer_task.queue_depth() >= 1 {
            saw_enqueued = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(
        saw_enqueued,
        "persist_brain_state_mutation's atomic_unit request never appeared in the \
             writer task's channel while the occupier held the single drain slot — \
             atomic_unit is not routing this call through the shared writer task"
    );

    release_tx
        .send(())
        .expect("occupier must still be waiting on the release signal");
    occupier
        .await
        .expect("occupier task must not panic")
        .expect("occupier write must succeed");

    persist_task
        .await
        .expect("persist_task must not panic")
        .expect("persist_brain_state_mutation must succeed once unblocked");
}
