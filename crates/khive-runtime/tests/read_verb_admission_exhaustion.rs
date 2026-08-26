//! khive#2147/khive#2217: a read dispatch must still return its result once
//! the shared audit-lane's own admission is exhausted, instead of discarding
//! an already-computed read to protect an audit obligation the read never
//! needed as strictly as a write does.
//!
//! Lives in its own binary (mirroring `tests/adr133_audit_batch.rs`) rather
//! than inside `khive-runtime/src/pack.rs`'s `mod tests`: that module
//! compiles into the crate's single `--lib` unit-test binary alongside
//! ~1300 other tests running concurrently across many OS threads, and this
//! mechanism needs a background supervisor task to actually get scheduled
//! within a bounded wait — under heavy suite-wide parallelism that wait is
//! not reliable. A dedicated binary with no unrelated concurrent tests
//! removes that source of flakiness.
//!
//! Requires `--features fault-injection,test-internals` (same as
//! `tests/adr133_audit_batch.rs`).

#![cfg(all(feature = "fault-injection", feature = "test-internals"))]

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::audit_batch::{
    fault_injection, AuditBatchConfig, AuditBatchControl, AuditProducer, PreparedAuditRow,
};
use khive_runtime::pack::{
    audit_admission_degraded_obligation_count, HandlerDef, PackRuntime, VerbRegistryBuilder,
};
use khive_runtime::runtime::NamespaceToken;
use khive_runtime::RuntimeError;
use khive_storage::types::{BatchWriteSummary, Page, PageRequest};
use khive_storage::{Event, EventFilter, EventStore, StorageResult};
use khive_types::pack::{Pack, Visibility};
use khive_types::{EventKind, EventOutcome, SubstrateKind, VerbCategory};
use serial_test::serial;

#[derive(Default)]
struct MemoryEventStore {
    events: std::sync::Mutex<Vec<Event>>,
}

#[async_trait]
impl EventStore for MemoryEventStore {
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
            failed: 0,
            first_error: String::new(),
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
    ) -> StorageResult<khive_storage::event::IdempotentEventBatchResult> {
        let mut store = self.events.lock().unwrap();
        let mut rows = Vec::with_capacity(events.len());
        for event in events {
            if let Some(existing) = store.iter().find(|e| e.id == event.id) {
                if *existing == event {
                    rows.push(
                        khive_storage::event::EventAppendDisposition::AlreadyPresentIdentical,
                    );
                } else {
                    rows.push(khive_storage::event::EventAppendDisposition::IdentityConflict);
                }
            } else {
                store.push(event);
                rows.push(khive_storage::event::EventAppendDisposition::Inserted);
            }
        }
        Ok(khive_storage::event::IdempotentEventBatchResult { rows })
    }
    fn supports_idempotent_audit_batch(&self) -> bool {
        true
    }
}

/// Minimal pack exercising one read (`list`, `Assertive`) and one write
/// (`create`, `Commissive`) verb, neither doing any real domain work.
struct AlphaPack;

impl Pack for AlphaPack {
    const NAME: &'static str = "alpha";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[
        HandlerDef {
            name: "create",
            description: "create a widget",
            visibility: Visibility::Verb,
            category: VerbCategory::Commissive,
            params: &[],
        },
        HandlerDef {
            name: "list",
            description: "list widgets",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
    ];
}

#[async_trait]
impl PackRuntime for AlphaPack {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        Self::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        Self::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        Self::HANDLERS
    }
    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &khive_runtime::pack::VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Ok(serde_json::json!({ "pack": "alpha", "verb": verb }))
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

// Both tests in this file arm `fault_injection`'s process-global
// `SUPERVISOR_SLEEP_BEFORE_SPAWN` flag; running them concurrently races one
// test's arm against the other's supervisor loop consuming it.
#[serial]
#[tokio::test]
async fn read_verb_dispatch_survives_audit_lane_admission_exhaustion() {
    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_event_store(store);
    builder.with_audit_batch_config(AuditBatchConfig {
        max_pending_rows: std::num::NonZeroUsize::new(1).unwrap(),
        ..AuditBatchConfig::default()
    });
    let registry = builder.build().expect("registry builds");
    let audit_batch = registry
        .audit_batch_handle()
        .expect("event store configured, so the batch seam is too");

    // Hold the supervisor inside its first generation for the test's
    // duration, so submitting up to `max_pending_rows` (1) directly through
    // the shared `AuditBatch` leaves admission synchronously refused
    // (`QueueAdmissionExhausted`, no wait) for any further row.
    fault_injection::arm_supervisor_sleep_before_spawn();
    let occupant_batch = audit_batch.clone();
    let occupant = tokio::spawn(async move {
        occupant_batch
            .submit(PreparedAuditRow {
                event: mk_event("kg.occupant"),
                producer: AuditProducer::ConfigLocked,
            })
            .await
    });
    wait_until(std::time::Duration::from_secs(5), || {
        let snap = audit_batch.test_snapshot();
        snap.pending_rows == 0 && snap.in_flight_generation.is_some()
    })
    .await;

    let filler_batch = audit_batch.clone();
    let filler = tokio::spawn(async move {
        filler_batch
            .submit(PreparedAuditRow {
                event: mk_event("kg.filler"),
                producer: AuditProducer::ConfigLocked,
            })
            .await
    });
    wait_until(std::time::Duration::from_secs(5), || {
        audit_batch.test_snapshot().pending_rows == 1
    })
    .await;

    // The audit lane is now saturated (`state.pending.len() ==
    // max_pending_rows`): a `create` (write) dispatched now still fails —
    // the existing write-side hard-fail obligation semantics are unchanged.
    registry
        .dispatch("create", Value::Null)
        .await
        .expect_err("a write's obligation failure still fails the dispatch when saturated");

    // The read must still succeed: its own audit row is refused on
    // admission (immediate `QueueAdmissionExhausted`, no wait), but that
    // failure degrades to best-effort instead of discarding the read's
    // already-computed result.
    let before_degraded = audit_admission_degraded_obligation_count();
    let result = registry
        .dispatch("list", Value::Null)
        .await
        .expect("a read verb must not fail on audit-lane admission exhaustion");
    assert_eq!(
        result,
        serde_json::json!({ "pack": "alpha", "verb": "list" })
    );
    assert_eq!(
        audit_admission_degraded_obligation_count(),
        before_degraded + 1,
        "a queue-refusal degrade must count on its own dedicated counter, not \
         AUDIT_APPEND_FAILURES or AUDIT_OBLIGATION_APPEND_FAILURES"
    );

    drop(occupant);
    drop(filler);
}

/// khive#2117/khive#2208 + khive#2147/khive#2217 combined: a read dispatch
/// must also survive its own audit row's admission *deadline* elapsing, not
/// just an immediate queue-full refusal — the two `AuditTerminalReason`
/// variants are distinct call sites in `append_audit_event_best_effort` and
/// this exercises the one `read_verb_dispatch_survives_audit_lane_admission_exhaustion`
/// above does not: `AdmissionDeadlineExpired` on a row that was already
/// enqueued (`state.pending`), rather than `QueueAdmissionExhausted` before
/// enqueue.
// Both tests in this file arm `fault_injection`'s process-global
// `SUPERVISOR_SLEEP_BEFORE_SPAWN` flag; running them concurrently races one
// test's arm against the other's supervisor loop consuming it.
#[serial]
#[tokio::test]
async fn read_verb_dispatch_survives_audit_lane_admission_deadline_expiry() {
    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_event_store(store);
    builder.with_audit_batch_config(AuditBatchConfig {
        // Room for the occupant row plus this test's own "list" row, so the
        // "list" row is never refused on admission — it must enqueue and
        // then wait out its own short deadline instead.
        max_pending_rows: std::num::NonZeroUsize::new(4).unwrap(),
        admission_deadline: std::time::Duration::from_millis(30),
        ..AuditBatchConfig::default()
    });
    let registry = builder.build().expect("registry builds");
    let audit_batch = registry
        .audit_batch_handle()
        .expect("event store configured, so the batch seam is too");

    // Hold the supervisor inside its first generation for the test's
    // duration, mirroring the queue-full test above: an occupant row is
    // drained into the armed sleep, so any row submitted afterward sits in
    // `state.pending`, undrained, until its own `admission_deadline` elapses.
    fault_injection::arm_supervisor_sleep_before_spawn();
    let occupant_batch = audit_batch.clone();
    let occupant = tokio::spawn(async move {
        occupant_batch
            .submit(PreparedAuditRow {
                event: mk_event("kg.occupant"),
                producer: AuditProducer::ConfigLocked,
            })
            .await
    });
    wait_until(std::time::Duration::from_secs(5), || {
        let snap = audit_batch.test_snapshot();
        snap.pending_rows == 0 && snap.in_flight_generation.is_some()
    })
    .await;

    // The read's own audit row now enqueues behind the stalled generation
    // (the queue has room, so this is not `QueueAdmissionExhausted`) and
    // waits out its 30ms `admission_deadline` — `AdmissionDeadlineExpired`.
    // That failure must still degrade to best-effort: the dispatch reports
    // its own already-computed result rather than discarding it.
    let before_degraded = audit_admission_degraded_obligation_count();
    let result = registry
        .dispatch("list", Value::Null)
        .await
        .expect("a read verb must not fail when its own audit row's admission deadline elapses");
    assert_eq!(
        result,
        serde_json::json!({ "pack": "alpha", "verb": "list" })
    );
    assert_eq!(
        audit_admission_degraded_obligation_count(),
        before_degraded + 1,
        "a deadline-expiry degrade must count on its own dedicated counter too, not just \
         the queue-refusal arm"
    );

    drop(occupant);
}
