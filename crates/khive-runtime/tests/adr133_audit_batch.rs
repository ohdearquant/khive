//! ADR-133 Slice 1: mechanism tests for the audit-batch seam
//! (`crates/khive-runtime/src/audit_batch.rs`).
//!
//! Requires `--features fault-injection,test-internals`. Counters and
//! checked state prove behavior; clocks are diagnostic only
//! (`final_verification_plan_r2.md` §1-2).

#![cfg(all(feature = "fault-injection", feature = "test-internals"))]

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;

use khive_runtime::audit_batch::{
    audit_delta, fault_injection, AuditBatch, AuditBatchConfig, AuditBatchControl,
    AuditBatchSnapshot, AuditCommitOutcome, AuditProducer, AuditSnapshotError, AuditTerminalReason,
    PreparedAuditRow,
};
use khive_storage::event::{EventAppendDisposition, IdempotentEventBatchResult};
use khive_storage::types::{BatchWriteSummary, Page, PageRequest};
use khive_storage::{
    Event, EventFilter, EventStore, StorageCapability, StorageError, StorageResult,
    WriterTaskRequestState,
};
use khive_types::{EventKind, EventOutcome, SubstrateKind};
use serial_test::serial;

struct FakeStore {
    fail_next: AtomicUsize,
    /// Armed N times: the next N `append_events_idempotent` calls fail with
    /// `StorageError::Pool` — the shape the ADR-170 forwarding lane reports
    /// for an unreachable events daemon.
    fail_pool_next: AtomicUsize,
    calls: AtomicU64,
    rows: Mutex<Vec<Event>>,
    /// Armed once: the next `append_events_idempotent` call commits its rows
    /// (visible in `rows` afterward) but reports the ambiguous
    /// `SideEffectsUnknown` writer state instead of `Ok`, simulating a commit
    /// whose acknowledgement was lost (ADR-133 acceptance criterion 7).
    ambiguous_ack_once: std::sync::atomic::AtomicBool,
}

impl FakeStore {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            fail_next: AtomicUsize::new(0),
            fail_pool_next: AtomicUsize::new(0),
            calls: AtomicU64::new(0),
            rows: Mutex::new(Vec::new()),
            ambiguous_ack_once: std::sync::atomic::AtomicBool::new(false),
        })
    }
}

#[async_trait]
impl EventStore for FakeStore {
    async fn append_event(&self, event: Event) -> StorageResult<()> {
        self.rows.lock().push(event);
        Ok(())
    }
    async fn append_events(&self, events: Vec<Event>) -> StorageResult<BatchWriteSummary> {
        let n = events.len() as u64;
        self.rows.lock().extend(events);
        Ok(BatchWriteSummary {
            attempted: n,
            affected: n,
            ..BatchWriteSummary::default()
        })
    }
    async fn get_event(&self, id: uuid::Uuid) -> StorageResult<Option<Event>> {
        Ok(self.rows.lock().iter().find(|e| e.id == id).cloned())
    }
    async fn query_events(
        &self,
        _filter: EventFilter,
        _page: PageRequest,
    ) -> StorageResult<Page<Event>> {
        unimplemented!("not exercised by audit_batch tests")
    }
    async fn count_events(&self, _filter: EventFilter) -> StorageResult<u64> {
        Ok(self.rows.lock().len() as u64)
    }

    fn preflight_event(&self, event: &Event) -> StorageResult<()> {
        if event.verb.is_empty() {
            return Err(StorageError::InvalidInput {
                capability: StorageCapability::Events,
                operation: "preflight_event".into(),
                message: "empty verb".into(),
            });
        }
        Ok(())
    }

    async fn append_events_idempotent(
        &self,
        events: Vec<Event>,
    ) -> StorageResult<IdempotentEventBatchResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_next.load(Ordering::SeqCst) > 0 {
            self.fail_next.fetch_sub(1, Ordering::SeqCst);
            return Err(StorageError::WriterTaskRequestFailed {
                request_state: WriterTaskRequestState::TransactionRolledBack,
                source: Box::new(StorageError::Internal(
                    "simulated operation error after proven rollback".into(),
                )),
            });
        }
        if self.fail_pool_next.load(Ordering::SeqCst) > 0 {
            self.fail_pool_next.fetch_sub(1, Ordering::SeqCst);
            return Err(StorageError::Pool {
                operation: "append_events_idempotent".into(),
                message: "events daemon unreachable (simulated transient outage)".into(),
            });
        }
        let ambiguous = self
            .ambiguous_ack_once
            .swap(false, std::sync::atomic::Ordering::SeqCst);
        let mut rows = self.rows.lock();
        let mut dispositions = Vec::with_capacity(events.len());
        for event in events {
            if let Some(existing) = rows.iter().find(|e| e.id == event.id) {
                if *existing == event {
                    dispositions.push(EventAppendDisposition::AlreadyPresentIdentical);
                } else {
                    dispositions.push(EventAppendDisposition::IdentityConflict);
                }
            } else {
                rows.push(event);
                dispositions.push(EventAppendDisposition::Inserted);
            }
        }
        if ambiguous {
            // The rows above are already committed (visible in `rows`), but
            // the caller's acknowledgement is lost — exactly the fixture
            // ADR-133 acceptance criterion 7 requires: a commit that
            // succeeds in the store while the driver observes ambiguity and
            // must retry rather than duplicate.
            return Err(StorageError::WriterTaskTerminated {
                request_state: WriterTaskRequestState::SideEffectsUnknown,
            });
        }
        Ok(IdempotentEventBatchResult { rows: dispositions })
    }

    fn supports_idempotent_audit_batch(&self) -> bool {
        true
    }
}

fn mk_event(verb: &str) -> Event {
    Event::new(
        "local",
        verb,
        EventKind::Audit,
        SubstrateKind::Event,
        "test:actor",
    )
    .with_outcome(EventOutcome::Success)
}

/// An accounting-bearing event carrying the same `resource.units`/
/// `resource.cost_unit` payload shape a real successful dispatch emits
/// (`khive_runtime::cost_unit::resource_payload`, wired into the audit row
/// at `crates/khive-runtime/src/pack.rs`), so a test built on this fixture
/// can observe whether the accounting projection is duplicated or lost on
/// an ambiguous-ack retry — a bare `mk_event` carries no such payload to
/// lose.
fn mk_accounting_event(verb: &str) -> Event {
    let resource = khive_runtime::cost_unit::resource_payload(
        verb,
        &serde_json::json!({}),
        &serde_json::json!({ "done": true }),
        || 0,
        None,
    );
    Event::new(
        "local",
        verb,
        EventKind::Audit,
        SubstrateKind::Event,
        "test:actor",
    )
    .with_outcome(EventOutcome::Success)
    .with_payload(serde_json::json!({ "resource": resource }))
}

/// The accounting consumer's projection: sum `resource.cost_unit` across a
/// set of persisted rows, mirroring `khive-pack-brain`'s `brain.event_counts`
/// aggregation (`event_cost_unit` in `crates/khive-pack-brain/src/handlers.rs`)
/// without depending on that pack. A duplicated or dropped row changes this
/// total; a row replayed as `AlreadyPresentIdentical` must not.
fn total_cost_unit(rows: &[Event]) -> i64 {
    rows.iter()
        .filter_map(|e| e.payload.get("resource")?.get("cost_unit")?.as_i64())
        .sum()
}

#[serial]
#[tokio::test]
async fn d1_idle_submit_commits_before_response() {
    let store = FakeStore::new();
    let batch = AuditBatch::new(store.clone(), AuditBatchConfig::default());
    let outcome = batch
        .submit(PreparedAuditRow {
            event: mk_event("kg.create"),
            producer: AuditProducer::DispatchSucceeded,
        })
        .await
        .expect("commits");
    assert_eq!(outcome, AuditCommitOutcome::Committed);
    assert_eq!(store.calls.load(Ordering::SeqCst), 1);
    batch.quiesce().await.expect("idle");
    assert!(batch.test_snapshot().is_idle());
}

#[serial]
#[tokio::test]
async fn d1_arrivals_during_commit_share_next_generation() {
    let store = FakeStore::new();
    let batch = AuditBatch::new(store.clone(), AuditBatchConfig::default());
    let mut handles = Vec::new();
    for i in 0..8 {
        let batch = batch.clone();
        handles.push(tokio::spawn(async move {
            batch
                .submit(PreparedAuditRow {
                    event: mk_event(&format!("verb.{i}")),
                    producer: AuditProducer::DispatchSucceeded,
                })
                .await
        }));
    }
    for h in handles {
        h.await.unwrap().expect("commits");
    }
    batch.quiesce().await.expect("idle");
    let snap = batch.test_snapshot();
    assert!(snap.store_batch_calls >= 1);
    assert!(
        snap.store_batch_calls < 8,
        "concurrent arrivals must batch, not each take their own acquisition"
    );
    assert_eq!(snap.committed_rows, 8);
}

/// Also exercises `d1_mid_commit_arrival_needs_no_third_generation_without_split_or_retry`:
/// a burst that lands entirely while idle collapses to few generations, never
/// one-per-row and never re-splitting an already-formed generation.
#[serial]
#[tokio::test]
async fn d1_mid_commit_arrival_needs_no_third_generation_without_split_or_retry() {
    let store = FakeStore::new();
    let batch = AuditBatch::new(store.clone(), AuditBatchConfig::default());
    let mut handles = Vec::new();
    for i in 0..16 {
        let batch = batch.clone();
        handles.push(tokio::spawn(async move {
            batch
                .submit(PreparedAuditRow {
                    event: mk_event(&format!("verb.{i}")),
                    producer: AuditProducer::DispatchSucceeded,
                })
                .await
        }));
    }
    for h in handles {
        h.await.unwrap().expect("commits");
    }
    batch.quiesce().await.expect("idle");
    let snap = batch.test_snapshot();
    assert_eq!(snap.committed_rows, 16);
    assert!(
        snap.store_batch_calls < 16,
        "must not degrade to one acquisition per row"
    );
}

#[serial]
#[tokio::test]
async fn d1_preflight_failure_does_not_poison_other_caller() {
    let store = FakeStore::new();
    let batch = AuditBatch::new(store.clone(), AuditBatchConfig::default());
    let bad = batch
        .submit(PreparedAuditRow {
            event: mk_event(""),
            producer: AuditProducer::DispatchSucceeded,
        })
        .await;
    assert_eq!(bad, Err(AuditTerminalReason::PreflightRejected));
    let good = batch
        .submit(PreparedAuditRow {
            event: mk_event("kg.create"),
            producer: AuditProducer::DispatchSucceeded,
        })
        .await;
    assert_eq!(good, Ok(AuditCommitOutcome::Committed));
    assert_eq!(batch.test_snapshot().submitted_rows, 1);
}

#[serial]
#[tokio::test]
async fn d1_retry_table_uses_typed_request_state() {
    let store = FakeStore::new();
    store.fail_next.store(1, Ordering::SeqCst);
    let batch = AuditBatch::new(store.clone(), AuditBatchConfig::default());
    let outcome = batch
        .submit(PreparedAuditRow {
            event: mk_event("kg.create"),
            producer: AuditProducer::DispatchSucceeded,
        })
        .await;
    assert_eq!(outcome, Ok(AuditCommitOutcome::Committed));
    assert_eq!(
        store.calls.load(Ordering::SeqCst),
        2,
        "a proven rollback makes the idempotent batch replay effect-safe even when the source error is not independently transient"
    );
}

/// A transient forwarding failure (`StorageError::Pool`, the shape the
/// ADR-170 events-daemon lane reports while the daemon restarts) must use the
/// configured bounded retries, not abandon the generation on first failure.
#[serial]
#[tokio::test]
async fn d1_transient_forwarding_failure_is_retried_not_terminal() {
    let store = FakeStore::new();
    store.fail_pool_next.store(1, Ordering::SeqCst);
    let batch = AuditBatch::new(store.clone(), AuditBatchConfig::default());
    let outcome = batch
        .submit(PreparedAuditRow {
            event: mk_event("kg.create"),
            producer: AuditProducer::DispatchSucceeded,
        })
        .await;
    assert_eq!(outcome, Ok(AuditCommitOutcome::Committed));
    assert_eq!(
        store.calls.load(Ordering::SeqCst),
        2,
        "one transient Pool failure then one successful retry"
    );
}

#[serial]
#[tokio::test]
async fn d1_dropped_waiter_does_not_cancel_generation() {
    let store = FakeStore::new();
    let batch = AuditBatch::new(store.clone(), AuditBatchConfig::default());
    let batch2 = batch.clone();
    let dropped = tokio::spawn(async move {
        batch2
            .submit(PreparedAuditRow {
                event: mk_event("kg.create"),
                producer: AuditProducer::DispatchSucceeded,
            })
            .await
    });
    dropped.abort();
    let survivor = batch
        .submit(PreparedAuditRow {
            event: mk_event("kg.list"),
            producer: AuditProducer::DispatchSucceeded,
        })
        .await;
    assert_eq!(survivor, Ok(AuditCommitOutcome::Committed));
}

#[serial]
#[tokio::test]
async fn d1_close_rejects_new_and_drains_accepted() {
    let store = FakeStore::new();
    let batch = AuditBatch::new(store.clone(), AuditBatchConfig::default());
    batch
        .submit(PreparedAuditRow {
            event: mk_event("kg.create"),
            producer: AuditProducer::DispatchSucceeded,
        })
        .await
        .expect("commits");
    batch.close_and_drain().await.expect("clean close");
    let rejected = batch
        .submit(PreparedAuditRow {
            event: mk_event("kg.list"),
            producer: AuditProducer::DispatchSucceeded,
        })
        .await;
    assert_eq!(rejected, Err(AuditTerminalReason::AdmissionClosed));
}

#[serial]
#[tokio::test]
async fn d1_child_driver_panic_fails_waiters() {
    let store = FakeStore::new();
    let batch = AuditBatch::new(store.clone(), AuditBatchConfig::default());
    fault_injection::arm_child_panic();
    let result = batch
        .submit(PreparedAuditRow {
            event: mk_event("kg.create"),
            producer: AuditProducer::DispatchSucceeded,
        })
        .await;
    assert_eq!(result, Err(AuditTerminalReason::DriverPanicked));
    assert_eq!(
        batch.quiesce().await,
        Err(AuditTerminalReason::DriverPanicked)
    );
    let second = batch
        .submit(PreparedAuditRow {
            event: mk_event("kg.list"),
            producer: AuditProducer::DispatchSucceeded,
        })
        .await;
    assert_eq!(
        second,
        Err(AuditTerminalReason::DriverPanicked),
        "Failed wins once and rejects further admission"
    );
}

#[serial]
#[tokio::test]
async fn d1_child_driver_cancellation_fails_waiters() {
    let store = FakeStore::new();
    let batch = AuditBatch::new(store.clone(), AuditBatchConfig::default());
    fault_injection::arm_child_cancel();
    let result = batch
        .submit(PreparedAuditRow {
            event: mk_event("kg.create"),
            producer: AuditProducer::DispatchSucceeded,
        })
        .await;
    assert_eq!(result, Err(AuditTerminalReason::DriverCancelled));
}

#[serial]
#[tokio::test]
async fn d1_supervisor_panic_fails_waiters_before_background_baseline() {
    let store = FakeStore::new();
    let batch = AuditBatch::new(store.clone(), AuditBatchConfig::default());
    fault_injection::arm_supervisor_panic();
    let result = batch
        .submit(PreparedAuditRow {
            event: mk_event("kg.create"),
            producer: AuditProducer::DispatchSucceeded,
        })
        .await;
    assert_eq!(result, Err(AuditTerminalReason::DriverPanicked));
    let metrics = batch.metrics_snapshot();
    assert_eq!(metrics.flush_failures, 1);
}

#[serial]
#[tokio::test]
async fn d1_supervisor_cancellation_fails_waiters_before_background_baseline() {
    let store = FakeStore::new();
    let batch = AuditBatch::new(store.clone(), AuditBatchConfig::default());
    fault_injection::arm_supervisor_sleep_before_spawn();
    let batch2 = batch.clone();
    let submitted = tokio::spawn(async move {
        batch2
            .submit(PreparedAuditRow {
                event: mk_event("kg.create"),
                producer: AuditProducer::DispatchSucceeded,
            })
            .await
    });
    // Give the supervisor time to enter the armed sleep, then abort its
    // retained handle — simulating a shutdown abort landing mid-generation.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        batch.test_abort_supervisor(),
        "a supervisor must be in flight to abort"
    );
    let result = submitted.await.unwrap();
    assert_eq!(result, Err(AuditTerminalReason::DriverCancelled));
}

#[serial]
#[tokio::test]
async fn d1_lost_join_maps_to_driver_join_lost() {
    let store = FakeStore::new();
    let batch = AuditBatch::new(store.clone(), AuditBatchConfig::default());
    fault_injection::arm_join_lost();
    let result = batch
        .submit(PreparedAuditRow {
            event: mk_event("kg.create"),
            producer: AuditProducer::DispatchSucceeded,
        })
        .await;
    assert_eq!(result, Err(AuditTerminalReason::DriverJoinLost));
}

#[serial]
#[tokio::test]
async fn d1_success_with_nonterminal_state_maps_to_driver_exited_inconsistent() {
    let store = FakeStore::new();
    let batch = AuditBatch::new(store.clone(), AuditBatchConfig::default());
    fault_injection::arm_inconsistent_exit();
    let result = batch
        .submit(PreparedAuditRow {
            event: mk_event("kg.create"),
            producer: AuditProducer::DispatchSucceeded,
        })
        .await;
    assert_eq!(result, Err(AuditTerminalReason::DriverExitedInconsistent));
}

#[serial]
#[tokio::test]
async fn d2_d3_pure_observability_degrades_instead_of_blocking() {
    let store = FakeStore::new();
    let batch = AuditBatch::new(store.clone(), AuditBatchConfig::default());
    fault_injection::arm_child_panic();
    let result = batch
        .submit(PreparedAuditRow {
            event: mk_event("memory.recall"),
            producer: AuditProducer::RecallExecuted,
        })
        .await;
    assert_eq!(result, Err(AuditTerminalReason::DriverPanicked));
    let metrics = batch.metrics_snapshot();
    assert_eq!(metrics.degraded_rows, 1);
    assert!(metrics.degraded);
}

#[serial]
#[tokio::test]
async fn d2_d3_config_locked_degrades_instead_of_blocking() {
    let store = FakeStore::new();
    let batch = AuditBatch::new(store.clone(), AuditBatchConfig::default());
    fault_injection::arm_child_panic();
    let result = batch
        .submit(PreparedAuditRow {
            event: mk_event("config.lock"),
            producer: AuditProducer::ConfigLocked,
        })
        .await;
    assert_eq!(result, Err(AuditTerminalReason::DriverPanicked));
    assert_eq!(batch.metrics_snapshot().degraded_rows, 1);
}

#[serial]
#[tokio::test]
async fn d2_d3_every_obligation_producer_fails_closed_on_persistent_commit_failure() {
    for producer in [
        AuditProducer::GateDenied,
        AuditProducer::DispatchSucceeded,
        AuditProducer::DispatchFailed,
        AuditProducer::UnknownVerb,
        AuditProducer::GitDigestReceipt,
    ] {
        let store = FakeStore::new();
        let batch = AuditBatch::new(store.clone(), AuditBatchConfig::default());
        fault_injection::arm_child_panic();
        let result = batch
            .submit(PreparedAuditRow {
                event: mk_event("kg.create"),
                producer,
            })
            .await;
        assert_eq!(
            result,
            Err(AuditTerminalReason::DriverPanicked),
            "producer {producer:?} must fail closed"
        );
        assert_eq!(
            batch.metrics_snapshot().degraded_rows,
            0,
            "producer {producer:?} is an obligation, not a degraded pure-observability row"
        );
    }
}

#[serial]
#[tokio::test]
async fn audit_delta_rejects_regressed_counters() {
    let before = AuditBatchSnapshot {
        pending_rows: 0,
        in_flight_generation: None,
        driver_active: false,
        next_generation_id: 2,
        submitted_rows: 5,
        committed_rows: 5,
        store_batch_calls: 1,
        per_generation: vec![],
    };
    let mut after = before.clone();
    after.submitted_rows = 4;
    assert_eq!(
        audit_delta(&before, &after),
        Err(AuditSnapshotError::CounterRegressed)
    );
}

#[serial]
#[tokio::test]
async fn identity_conflict_reported_per_row_and_isolated() {
    let store = FakeStore::new();
    let batch = AuditBatch::new(store.clone(), AuditBatchConfig::default());
    let base = mk_event("kg.create");
    store.rows.lock().push(
        Event {
            id: base.id,
            ..base.clone()
        }
        .with_payload(serde_json::json!({"different": true})),
    );
    let result = batch
        .submit(PreparedAuditRow {
            event: base,
            producer: AuditProducer::DispatchSucceeded,
        })
        .await;
    assert_eq!(result, Err(AuditTerminalReason::IdentityConflict));
}

/// ADR-133 acceptance criterion 7: an ambiguous commit acknowledgement
/// followed by a retry persists exactly one accounting-bearing record. The
/// fixture injects the ambiguity itself (the store commits, then reports
/// `SideEffectsUnknown`) rather than a clean pre-write failure, since a
/// clean failure is the easy case and cannot duplicate a row.
#[serial]
#[tokio::test]
async fn d1c_ambiguous_ack_retry_persists_exactly_one_accounting_row() {
    let store = FakeStore::new();
    store.ambiguous_ack_once.store(true, Ordering::SeqCst);
    let batch = AuditBatch::new(store.clone(), AuditBatchConfig::default());
    let event = mk_accounting_event("kg.create");
    let event_id = event.id;
    let expected_cost_unit = event.payload["resource"]["cost_unit"]
        .as_i64()
        .expect("the fixture carries a real resource.cost_unit payload");

    let result = batch
        .submit(PreparedAuditRow {
            event,
            producer: AuditProducer::DispatchSucceeded,
        })
        .await;

    // The retry replays the identical row against a store that already
    // holds it, so the accounting consumer sees a fresh commit's twin
    // (`AlreadyPresentIdentical`), not a second row.
    assert_eq!(result, Ok(AuditCommitOutcome::AlreadyPresentIdentical));
    let rows = store.rows.lock();
    assert_eq!(
        rows.iter().filter(|e| e.id == event_id).count(),
        1,
        "the ambiguous commit followed by retry must persist exactly one row, not zero or two"
    );
    assert_eq!(
        total_cost_unit(&rows),
        expected_cost_unit,
        "the accounting consumer's projected total must reflect exactly one dispatch's \
         cost_unit — an ambiguous ack retried into a duplicate row would double it, and a \
         retried loss would zero it"
    );
    drop(rows);
    assert_eq!(
        store.calls.load(Ordering::SeqCst),
        2,
        "one ambiguous attempt, then one retry that observes the row already committed"
    );
}

/// khive#2117/khive#2208: `QueueAdmissionExhausted` must mean only the
/// pre-enqueue refusal — `state.pending.len() >= max_pending_rows` — never
/// the deadline-elapsed case covered by the next test. A row refused here
/// was never pushed onto `state.pending` and never incremented
/// `submitted_rows`, so retrying it applies the obligation at most once.
///
/// The armed sleep fault holds the supervisor inside its first (and only)
/// generation for the test's duration, so nothing ever drains
/// `state.pending` after the one-row capacity fills — the refusal below is
/// deterministic, not a timing race against the driver.
#[serial]
#[tokio::test]
async fn queue_admission_exhausted_row_is_never_enqueued() {
    let store = FakeStore::new();
    let batch = AuditBatch::new(
        store.clone(),
        AuditBatchConfig {
            max_pending_rows: std::num::NonZeroUsize::new(1).unwrap(),
            ..AuditBatchConfig::default()
        },
    );
    fault_injection::arm_supervisor_sleep_before_spawn();
    let occupant_batch = batch.clone();
    let occupant = tokio::spawn(async move {
        occupant_batch
            .submit(PreparedAuditRow {
                event: mk_event("kg.occupant"),
                producer: AuditProducer::DispatchSucceeded,
            })
            .await
    });
    // Let the supervisor drain the occupant row into the armed sleep,
    // freeing `state.pending` back to 0 before the fill row claims the
    // configured one-row capacity.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    let filler_batch = batch.clone();
    let filler = tokio::spawn(async move {
        filler_batch
            .submit(PreparedAuditRow {
                event: mk_event("kg.filler"),
                producer: AuditProducer::DispatchSucceeded,
            })
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;

    let before = batch.test_snapshot();
    let refused = batch
        .submit(PreparedAuditRow {
            event: mk_event("kg.refused"),
            producer: AuditProducer::DispatchSucceeded,
        })
        .await;
    assert_eq!(refused, Err(AuditTerminalReason::QueueAdmissionExhausted));
    let after = batch.test_snapshot();
    assert_eq!(
        after.submitted_rows, before.submitted_rows,
        "a queue-full refusal must never increment submitted_rows — the row was never enqueued"
    );
    assert_eq!(
        after.pending_rows, before.pending_rows,
        "a queue-full refusal must not grow state.pending"
    );

    // Safe to retry: refused again, deterministically, with no side effect
    // from the first refusal.
    let retried = batch
        .submit(PreparedAuditRow {
            event: mk_event("kg.refused"),
            producer: AuditProducer::DispatchSucceeded,
        })
        .await;
    assert_eq!(retried, Err(AuditTerminalReason::QueueAdmissionExhausted));

    drop(occupant);
    drop(filler);
}

/// khive#2117/khive#2208: a caller whose `admission_deadline` elapses on a
/// row that was already enqueued gets a distinct reason from queue-full —
/// `AdmissionDeadlineExpired` — and the row itself is left in
/// `state.pending`/`submitted_rows` exactly as if the caller had not timed
/// out, because the generation driver (once it eventually runs) drains and
/// commits it independently of this caller's wait. The two prior assertions
/// against `queue_admission_exhausted_row_is_never_enqueued` above are the
/// negative space this test fills in: same seam, opposite enqueue outcome.
#[serial]
#[tokio::test]
async fn admission_deadline_expired_row_stays_enqueued() {
    let store = FakeStore::new();
    let batch = AuditBatch::new(
        store.clone(),
        AuditBatchConfig {
            admission_deadline: std::time::Duration::from_millis(30),
            ..AuditBatchConfig::default()
        },
    );
    fault_injection::arm_supervisor_sleep_before_spawn();
    let occupant_batch = batch.clone();
    let occupant = tokio::spawn(async move {
        occupant_batch
            .submit(PreparedAuditRow {
                event: mk_event("kg.occupant"),
                producer: AuditProducer::DispatchSucceeded,
            })
            .await
    });
    // Let the supervisor drain the occupant row into the armed sleep before
    // this test's row is submitted, so the row below is the one waiting
    // behind the stuck generation rather than racing to be drained itself.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;

    let before = batch.test_snapshot();
    let stuck = batch
        .submit(PreparedAuditRow {
            event: mk_event("kg.stuck"),
            producer: AuditProducer::DispatchSucceeded,
        })
        .await;
    assert_eq!(stuck, Err(AuditTerminalReason::AdmissionDeadlineExpired));
    let after = batch.test_snapshot();
    assert_eq!(
        after.submitted_rows,
        before.submitted_rows + 1,
        "a deadline-expired row WAS enqueued and counted — unlike a queue-full refusal"
    );
    assert_eq!(
        after.pending_rows,
        before.pending_rows + 1,
        "the row remains queued after the caller's own wait times out; the driver, not this \
         arm, is responsible for eventually draining it"
    );

    drop(occupant);
}
