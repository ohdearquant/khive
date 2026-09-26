//! ADR-133 / #1406: a replayable concurrent writer-acquisition measurement.
//!
//! The measured window contains only the census's known WRITER control,
//! `comm.read`. Each of four calls patches a different inbound message. The
//! same fixed rows and calls run against two fresh in-memory pools. Scheduling
//! may change their order, but the complete counter delta must replay exactly.
//! In-memory reads share the writer connection, so the per-call upper bound
//! includes target validation, the guarded patch, and the response refresh.

use std::sync::Arc;
use std::time::Duration;

use khive_db::pool::WriterAcquisitionSnapshot;
use khive_pack_comm::CommPack;
use khive_runtime::{KhiveRuntime, Namespace, VerbRegistryBuilder};
use khive_storage::Note;
use serde_json::json;
use tokio::sync::Barrier;
use tokio::task::JoinSet;
use uuid::Uuid;

const CALLS: usize = 4;
const MAX_POOLED_ACQUISITIONS_PER_READ: u64 = 3;

fn assert_known_writer_control() {
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../../../scripts/data/writer-census-v1.json"))
            .expect("pinned writer census manifest is valid JSON");
    assert_eq!(manifest["control"]["verb"], "comm.read");
    assert_eq!(manifest["control"]["required_classification"], "WRITER");
    assert_eq!(
        manifest["overrides"]["comm.read"]["classification"],
        "WRITER"
    );
    assert_eq!(manifest["defaults"]["classification"], "UNKNOWN");
}

fn counter_delta(
    before: WriterAcquisitionSnapshot,
    after: WriterAcquisitionSnapshot,
) -> WriterAcquisitionSnapshot {
    let subtract = |new: u64, old: u64| new.checked_sub(old).expect("writer counter is monotonic");
    WriterAcquisitionSnapshot {
        acquisitions: subtract(after.acquisitions, before.acquisitions),
        pooled_acquisitions: subtract(after.pooled_acquisitions, before.pooled_acquisitions),
        standalone_acquisitions: subtract(
            after.standalone_acquisitions,
            before.standalone_acquisitions,
        ),
        writer_task_acquisitions: subtract(
            after.writer_task_acquisitions,
            before.writer_task_acquisitions,
        ),
        timeouts: subtract(after.timeouts, before.timeouts),
        writer_task_begin_busy: subtract(
            after.writer_task_begin_busy,
            before.writer_task_begin_busy,
        ),
        writer_task_begin_busy_absorbed: subtract(
            after.writer_task_begin_busy_absorbed,
            before.writer_task_begin_busy_absorbed,
        ),
        writer_task_begin_errors: subtract(
            after.writer_task_begin_errors,
            before.writer_task_begin_errors,
        ),
        writer_task_request_failures: subtract(
            after.writer_task_request_failures,
            before.writer_task_request_failures,
        ),
        writer_task_side_effects_unknown: subtract(
            after.writer_task_side_effects_unknown,
            before.writer_task_side_effects_unknown,
        ),
    }
}

async fn replay_once() -> WriterAcquisitionSnapshot {
    let runtime = KhiveRuntime::memory().expect("isolated in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(CommPack::new(runtime.clone()));
    let registry = Arc::new(builder.build().expect("kg+comm registry"));
    assert!(
        registry.audit_batch_handle().is_none(),
        "the conditional dispatch-audit path must be disabled for this direct writer measurement"
    );

    let token = runtime.authorize(Namespace::local()).expect("local token");
    let notes = runtime.notes(&token).expect("note store");
    let ids: Vec<Uuid> = (0..CALLS)
        .map(|index| Uuid::from_u128(0x1406_0000 + index as u128))
        .collect();
    for (index, id) in ids.iter().copied().enumerate() {
        notes
            .upsert_note(Note {
                version: 1,
                key: None,
                id,
                namespace: "local".into(),
                kind: "message".into(),
                status: "active".into(),
                name: None,
                content: format!("writer census inbound {index}"),
                salience: None,
                decay_factor: None,
                expires_at: None,
                properties: Some(json!({
                    "direction": "inbound",
                    "to_actor": "local",
                    "from_actor": "local",
                    "read": false,
                })),
                created_at: 1_000_000 + index as i64,
                updated_at: 1_000_000 + index as i64,
                deleted_at: None,
            })
            .await
            .expect("seed one fixed inbound message");
    }

    // Schema creation and input seeding finish before the first snapshot. No
    // UNKNOWN verb or conditional audit write runs inside this interval.
    let before = runtime.backend().pool().writer_acquisition_snapshot();
    let barrier = Arc::new(Barrier::new(CALLS + 1));
    let mut joins = JoinSet::new();
    for id in ids {
        let registry = Arc::clone(&registry);
        let barrier = Arc::clone(&barrier);
        joins.spawn(async move {
            barrier.wait().await;
            registry
                .dispatch("comm.read", json!({ "id": id.to_string() }))
                .await
        });
    }
    barrier.wait().await;

    for _ in 0..CALLS {
        let joined = match tokio::time::timeout(Duration::from_secs(10), joins.join_next()).await {
            Ok(Some(joined)) => joined,
            other => {
                joins.abort_all();
                panic!("concurrent comm.read workload did not finish: {other:?}");
            }
        };
        let response = joined
            .expect("comm.read task did not panic")
            .expect("comm.read dispatch succeeded");
        assert_eq!(response["status"], "success");
        assert_eq!(response["read"], true);
    }

    let after = runtime.backend().pool().writer_acquisition_snapshot();
    counter_delta(before, after)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fixed_concurrent_comm_read_writer_delta_replays_exactly() {
    assert_known_writer_control();

    let first = replay_once().await;
    let second = replay_once().await;
    assert_eq!(first, second, "identical inputs must have an exact delta");
    let pooled_only = WriterAcquisitionSnapshot {
        acquisitions: first.pooled_acquisitions,
        pooled_acquisitions: first.pooled_acquisitions,
        ..WriterAcquisitionSnapshot::default()
    };
    assert_eq!(
        first, pooled_only,
        "the workload must have only successful pooled writer acquisitions"
    );
    assert!(
        (CALLS as u64..=MAX_POOLED_ACQUISITIONS_PER_READ * CALLS as u64)
            .contains(&first.pooled_acquisitions),
        "each successful comm.read must account for its patch without an extra \
         per-message writer acquisition: {first:?}"
    );
}
