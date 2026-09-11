//! khive#2228 M3: the `db_diagnostics` verb handler
//! (`crates/khive-pack-kg/src/handlers/db_diagnostics.rs`) must actually
//! forward `registry.audit_batch_metrics()` into
//! `KhiveRuntime::db_diagnostics_with_audit_metrics` — not just have that
//! forwarding proven for the runtime method in isolation
//! (`khive-runtime/tests/read_verb_admission_exhaustion.rs`).
//!
//! This dispatches the real `db_diagnostics` verb through the same
//! `VerbRegistry` + `KgPack` path a caller reaches (mirroring
//! `crates/khive-pack-kg/tests/integration.rs`'s `Fixture`), after a real
//! degraded admission has made the counters nonzero, and asserts on the
//! returned wire JSON. If the handler is changed to pass `None` instead of
//! `registry.audit_batch_metrics()`, this test reddens.
//!
//! `db_diagnostics` is deliberately NOT itself in
//! `VerbRegistry::ADMISSION_DEGRADE_SAFE_VERBS` (it may do WAL-backfilling
//! physical I/O), so it hard-fails its OWN audit obligation if dispatched
//! while the audit lane is still saturated. This test therefore saturates
//! the lane with a gated `EventStore` (not the 3600s fault-injection sleep
//! `read_verb_admission_exhaustion.rs` uses, which never releases), drives
//! one degrade-safe read (`whoami`) through the saturated lane to bump the
//! refused-obligation counter, releases the gate so the lane quiesces, and
//! only then dispatches `db_diagnostics` — proving the counter it reports
//! (a process-wide cumulative counter, not reset by quiescing) is the one
//! `whoami`'s refusal actually produced.
//!
//! Lives in its own binary rather than inside `tests/integration.rs`:
//! `integration.rs` runs thousands of unrelated tests concurrently in one
//! process, and this mechanism configures its own dedicated `AuditBatch`
//! with `max_pending_rows: 1` plus a gate that blocks that batch's flush —
//! isolating it in its own test binary/process removes any risk of
//! interacting with another test's own audit-batch traffic.
//!
//! Requires `--features fault-injection,test-internals` on the
//! `khive-runtime` dependency (enabled unconditionally for this crate's
//! dev-dependency in `Cargo.toml`) for `AuditBatch::test_snapshot`/`quiesce`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use khive_pack_kg::KgPack;
use khive_runtime::audit_batch::{AuditBatchConfig, AuditBatchControl};
use khive_runtime::pack::{
    audit_admission_refused_obligation_count, audit_admission_unresolved_obligation_count,
};
use khive_runtime::{KhiveRuntime, VerbRegistryBuilder};
use khive_storage::event::{EventAppendDisposition, IdempotentEventBatchResult};
use khive_storage::types::{BatchWriteSummary, Page, PageRequest};
use khive_storage::{Event, EventFilter, EventStore, StorageResult};
use khive_types::{EventKind, EventOutcome, SubstrateKind};

/// An `EventStore` whose `append_events_idempotent` (the call
/// `AuditBatch`'s flush uses) blocks until `release()` is called — lets the
/// test hold the audit lane's in-flight generation open on demand, then let
/// it go, instead of relying on a fixed sleep.
#[derive(Default)]
struct GateEventStore {
    events: std::sync::Mutex<Vec<Event>>,
    released: Arc<AtomicBool>,
}

impl GateEventStore {
    fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
    }
}

#[async_trait]
impl EventStore for GateEventStore {
    async fn append_event(&self, event: Event) -> StorageResult<()> {
        self.events.lock().unwrap().push(event);
        Ok(())
    }
    async fn append_events(&self, events: Vec<Event>) -> StorageResult<BatchWriteSummary> {
        let n = events.len() as u64;
        self.events.lock().unwrap().extend(events);
        Ok(BatchWriteSummary {
            attempted: n,
            affected: n,
            ..BatchWriteSummary::default()
        })
    }
    async fn get_event(&self, id: uuid::Uuid) -> StorageResult<Option<Event>> {
        Ok(self
            .events
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.id == id)
            .cloned())
    }
    async fn query_events(
        &self,
        _filter: EventFilter,
        _page: PageRequest,
    ) -> StorageResult<Page<Event>> {
        unimplemented!("not exercised by this test")
    }
    async fn count_events(&self, _filter: EventFilter) -> StorageResult<u64> {
        Ok(self.events.lock().unwrap().len() as u64)
    }
    fn preflight_event(&self, _event: &Event) -> StorageResult<()> {
        Ok(())
    }
    async fn append_events_idempotent(
        &self,
        events: Vec<Event>,
    ) -> StorageResult<IdempotentEventBatchResult> {
        while !self.released.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        let mut store = self.events.lock().unwrap();
        let mut rows = Vec::with_capacity(events.len());
        for event in events {
            if let Some(existing) = store.iter().find(|e| e.id == event.id) {
                if *existing == event {
                    rows.push(EventAppendDisposition::AlreadyPresentIdentical);
                } else {
                    rows.push(EventAppendDisposition::IdentityConflict);
                }
            } else {
                store.push(event);
                rows.push(EventAppendDisposition::Inserted);
            }
        }
        Ok(IdempotentEventBatchResult { rows })
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

/// Polls `condition` every 2ms until it holds or `timeout` elapses
/// (panicking on elapse) — a correctness-driven wait instead of a
/// wall-clock guess.
async fn wait_until(timeout: std::time::Duration, mut condition: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if condition() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "condition did not become true within {timeout:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn db_diagnostics_verb_reports_real_admission_refused_obligations() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime must succeed");
    let store = Arc::new(GateEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.with_event_store(store.clone());
    builder.with_audit_batch_config(AuditBatchConfig {
        max_pending_rows: std::num::NonZeroUsize::new(1).unwrap(),
        ..AuditBatchConfig::default()
    });
    // The real composition root registers built-in packs through the trusted
    // path; this test stands in for it, so `whoami` keeps its best-effort
    // audit obligation under admission pressure. The ordinary `register`
    // path is untrusted and would hard-fail the read instead.
    builder.register_trusted(KgPack::new(rt));
    let registry = builder.build().expect("registry builds");
    let audit_batch = registry
        .audit_batch_handle()
        .expect("event store configured, so the batch seam is too");

    // Occupant row: submitted directly through the shared `AuditBatch`
    // (bypassing verb dispatch), it gets picked into the first generation
    // and then blocks in `GateEventStore::append_events_idempotent` until
    // released below.
    let occupant_batch = audit_batch.clone();
    let occupant = tokio::spawn(async move {
        occupant_batch
            .submit(khive_runtime::audit_batch::PreparedAuditRow {
                event: mk_event("kg.occupant"),
                producer: khive_runtime::audit_batch::AuditProducer::ConfigLocked,
            })
            .await
    });
    wait_until(std::time::Duration::from_secs(5), || {
        let snap = audit_batch.test_snapshot();
        snap.pending_rows == 0 && snap.in_flight_generation.is_some()
    })
    .await;

    // Filler row: with the occupant already drained into the in-flight
    // generation, this one lands in `state.pending` and stays there
    // (`max_pending_rows == 1`) until the gate above releases.
    let filler_batch = audit_batch.clone();
    let filler = tokio::spawn(async move {
        filler_batch
            .submit(khive_runtime::audit_batch::PreparedAuditRow {
                event: mk_event("kg.filler"),
                producer: khive_runtime::audit_batch::AuditProducer::ConfigLocked,
            })
            .await
    });
    wait_until(std::time::Duration::from_secs(5), || {
        audit_batch.test_snapshot().pending_rows == 1
    })
    .await;

    // The audit lane is now saturated. Dispatch a real read verb
    // (`whoami`, in `ADMISSION_DEGRADE_SAFE_VERBS`) through the registry:
    // its own best-effort audit row is refused on admission, incrementing
    // the process-wide refused-obligation counter, while the dispatch
    // itself still succeeds.
    let before_refused = audit_admission_refused_obligation_count();
    let before_unresolved = audit_admission_unresolved_obligation_count();
    registry
        .dispatch("whoami", json!({}))
        .await
        .expect("a read verb must not fail on audit-lane admission exhaustion");
    assert_eq!(
        audit_admission_refused_obligation_count(),
        before_refused + 1,
        "whoami's own best-effort audit row must be refused on admission while the lane is saturated"
    );

    // Release the gate and let the lane quiesce. `db_diagnostics` is NOT
    // in `ADMISSION_DEGRADE_SAFE_VERBS` (ADR-103/ADR-133: it may do
    // WAL-backfilling physical I/O), so dispatching it while still
    // saturated would hard-fail its own audit obligation instead of
    // reporting anything — this test cares about what it *reports*, so the
    // lane must be healthy again before that dispatch.
    store.release();
    audit_batch
        .quiesce()
        .await
        .expect("the audit lane must quiesce once the gate is released");
    drop(occupant);
    drop(filler);

    // Now dispatch the ACTUAL `db_diagnostics` verb through the SAME
    // registry — the path a real caller reaches, through
    // `KgPack::handle_db_diagnostics`, not a direct call to
    // `KhiveRuntime::db_diagnostics_with_audit_metrics` constructed by the
    // test. This is what closes khive#2228: the handler must forward
    // `registry.audit_batch_metrics()`, and this assertion reddens if it
    // is changed to pass `None` instead. The refused-obligation count is
    // process-wide and cumulative, so it still reflects whoami's refusal
    // above even though the lane itself is healthy again.
    let report: Value = registry
        .dispatch("db_diagnostics", json!({}))
        .await
        .expect("db_diagnostics must succeed against an in-memory backend");
    let writer_contention = report
        .get("writer_contention")
        .expect("writer_contention section must be present");
    assert_eq!(
        writer_contention.get("audit_admission_refused_obligations"),
        Some(&Value::from(before_refused + 1)),
        "the real db_diagnostics verb dispatch must surface the refused-obligation count \
         accumulated by whoami's admission refusal above: {writer_contention:?}"
    );
    assert_eq!(
        writer_contention.get("audit_admission_unresolved_obligations"),
        Some(&Value::from(before_unresolved)),
        "no admission-deadline expiry occurred, so the unresolved counter must be unchanged: \
         {writer_contention:?}"
    );
}
