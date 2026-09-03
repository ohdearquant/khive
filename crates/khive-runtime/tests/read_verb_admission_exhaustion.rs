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
    audit_admission_refused_obligation_count, audit_admission_unresolved_obligation_count,
    HandlerDef, PackRuntime, VerbRegistryBuilder,
};
use khive_runtime::runtime::NamespaceToken;
use khive_runtime::{KhiveRuntime, RuntimeError};
use khive_storage::types::{BatchWriteSummary, Page, PageRequest};
use khive_storage::{Event, EventFilter, EventStore, StorageResult};
use khive_types::pack::{Pack, Visibility};
use khive_types::{EventKind, EventOutcome, SubstrateKind, VerbCategory};
use serial_test::serial;

#[derive(Default)]
struct MemoryEventStore {
    events: std::sync::Mutex<Vec<Event>>,
    append_started: Option<Arc<tokio::sync::Notify>>,
    append_release: Option<Arc<tokio::sync::Notify>>,
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
        if let Some(started) = &self.append_started {
            started.notify_one();
        }
        if let Some(release) = &self.append_release {
            release.notified().await;
        }
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

/// Minimal pack exercising one allowlisted read (`get`, `Assertive`) whose
/// dispatch always fails — used to prove a FAILED allowlisted read never
/// takes the admission-degrade path (khive#2228 M1): only a *successful*
/// dispatch of an allowlisted read may degrade its audit obligation, never a
/// `DispatchFailed` one.
struct BetaPack;

impl Pack for BetaPack {
    const NAME: &'static str = "beta";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
        name: "get",
        description: "get a widget (always fails, for regression coverage)",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    }];
}

#[async_trait]
impl PackRuntime for BetaPack {
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
        _verb: &str,
        _params: Value,
        _registry: &khive_runtime::pack::VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Err(RuntimeError::NotFound(
            "widget intentionally missing for test".into(),
        ))
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

/// Minimal non-idempotent write used by khive#2256 to prove the handler
/// effect has landed before the audit generation resolves.
struct RecordingWritePack {
    effects: Arc<std::sync::atomic::AtomicUsize>,
}

impl Pack for RecordingWritePack {
    const NAME: &'static str = "recording_write";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
        name: "create",
        description: "record one committed effect",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[],
    }];
}

#[async_trait]
impl PackRuntime for RecordingWritePack {
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
        _verb: &str,
        _params: Value,
        _registry: &khive_runtime::pack::VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        self.effects
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(serde_json::json!({"created": true}))
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

/// khive#2256: a successful non-idempotent write has already committed by
/// the time its deferred audit row is submitted. If the row is enqueued but
/// its generation remains in flight past `admission_deadline`, the dispatch
/// must await the generation's real outcome rather than report the committed
/// write as a failure and invite an unsafe retry.
#[serial]
#[tokio::test]
#[serial(config_ledger)]
async fn write_verb_waits_past_audit_deadline_until_row_commits() {
    let append_started = Arc::new(tokio::sync::Notify::new());
    let append_release = Arc::new(tokio::sync::Notify::new());
    let effects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let store = Arc::new(MemoryEventStore {
        append_started: Some(Arc::clone(&append_started)),
        append_release: Some(Arc::clone(&append_release)),
        ..MemoryEventStore::default()
    });
    let mut builder = VerbRegistryBuilder::new();
    builder.register(RecordingWritePack {
        effects: Arc::clone(&effects),
    });
    builder.with_event_store(store.clone());
    builder.with_audit_batch_config(AuditBatchConfig {
        admission_deadline: std::time::Duration::from_millis(30),
        ..AuditBatchConfig::default()
    });
    let registry = Arc::new(builder.build().expect("registry builds"));

    let mut dispatch = tokio::spawn({
        let registry = Arc::clone(&registry);
        async move { registry.dispatch("create", Value::Null).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), append_started.notified())
        .await
        .expect("audit generation reaches the blocking store");
    assert_eq!(
        effects.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the domain effect must already be committed before the audit wait"
    );

    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(80), &mut dispatch)
            .await
            .is_err(),
        "an enqueued audit row crossing its bounded wait must not turn a committed write into a failure"
    );

    append_release.notify_one();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), &mut dispatch)
        .await
        .expect("dispatch resolves after the audit generation")
        .expect("dispatch task joins")
        .expect("committed write reports success once its audit row commits");
    assert_eq!(result, serde_json::json!({"created": true}));
    assert_eq!(
        effects.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "awaiting audit resolution must not rerun the non-idempotent handler"
    );
    assert_eq!(
        store.events.lock().expect("events lock").len(),
        1,
        "the successful dispatch must have exactly one durable audit row"
    );
}

/// khive#2331: the earlier `write_verb_waits_past_audit_deadline_until_row_commits`
/// test only proves the case where the store eventually releases (80ms in,
/// well inside its huge default `resolution_deadline`). A store that never
/// returns at all must not pin the caller — and the completed write's
/// handler — forever. Once `resolution_deadline` also elapses past
/// `admission_deadline`, the dispatch must give up and report the new
/// terminal reason rather than await the stalled generation indefinitely.
/// The outer `tokio::time::timeout` wrapping the dispatch join is a test
/// safety net, not the mechanism under test: pre-fix, this would hang past
/// it and fail loudly instead of hanging the whole suite.
#[serial]
#[tokio::test]
#[serial(config_ledger)]
async fn write_verb_gives_up_after_resolution_deadline_when_store_never_returns() {
    let append_started = Arc::new(tokio::sync::Notify::new());
    let effects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let store = Arc::new(MemoryEventStore {
        append_started: Some(Arc::clone(&append_started)),
        // `append_release` is never notified: `append_events_idempotent`
        // blocks on `Notify::notified()` forever — a genuinely stalled store.
        append_release: Some(Arc::new(tokio::sync::Notify::new())),
        ..MemoryEventStore::default()
    });
    let mut builder = VerbRegistryBuilder::new();
    builder.register(RecordingWritePack {
        effects: Arc::clone(&effects),
    });
    builder.with_event_store(store.clone());
    builder.with_audit_batch_config(AuditBatchConfig {
        admission_deadline: std::time::Duration::from_millis(30),
        resolution_deadline: std::time::Duration::from_millis(100),
        ..AuditBatchConfig::default()
    });
    let registry = Arc::new(builder.build().expect("registry builds"));
    let audit_batch = registry
        .audit_batch_handle()
        .expect("event store configured, so the batch seam is too");

    let dispatch = tokio::spawn({
        let registry = Arc::clone(&registry);
        async move { registry.dispatch("create", Value::Null).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), append_started.notified())
        .await
        .expect("audit generation reaches the blocking store");
    assert_eq!(
        effects.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the domain effect must already be committed before the audit wait"
    );

    let err = tokio::time::timeout(std::time::Duration::from_secs(5), dispatch)
        .await
        .expect("dispatch resolves within the resolution deadline instead of hanging forever")
        .expect("dispatch task joins")
        .expect_err("a stalled audit store must not report the committed write as success");
    assert!(
        err.to_string().contains("ResolutionDeadlineExpired"),
        "the caller must learn the effect committed and the audit outcome is unresolved, \
         distinctly from a plain admission-deadline expiry: {err}"
    );
    assert_eq!(
        effects.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "giving up on the audit wait must not rerun the non-idempotent handler"
    );
    let snap = audit_batch.test_snapshot();
    assert!(
        snap.in_flight_generation.is_some() && snap.driver_active,
        "the stalled generation must still be owned by the driver, not abandoned or re-enqueued"
    );
    assert_eq!(
        snap.pending_rows, 0,
        "the row must not be pushed back onto the pending queue for a retry"
    );
}

/// khive#2331: the capacity-exhaustion arm — multiple concurrent committed
/// writes stalled behind the same never-returning audit store must each give
/// up at their own resolution deadline rather than piling up unbounded
/// request/audit capacity. `submitted_rows` grows by exactly N and the
/// pending queue is empty afterward: nothing was retried, duplicated, or
/// left queued.
#[serial]
#[tokio::test]
#[serial(config_ledger)]
async fn concurrent_write_verbs_all_give_up_after_resolution_deadline() {
    const N: usize = 3;
    let append_started = Arc::new(tokio::sync::Notify::new());
    let effects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let store = Arc::new(MemoryEventStore {
        append_started: Some(Arc::clone(&append_started)),
        append_release: Some(Arc::new(tokio::sync::Notify::new())),
        ..MemoryEventStore::default()
    });
    let mut builder = VerbRegistryBuilder::new();
    builder.register(RecordingWritePack {
        effects: Arc::clone(&effects),
    });
    builder.with_event_store(store.clone());
    builder.with_audit_batch_config(AuditBatchConfig {
        admission_deadline: std::time::Duration::from_millis(30),
        resolution_deadline: std::time::Duration::from_millis(100),
        ..AuditBatchConfig::default()
    });
    let registry = Arc::new(builder.build().expect("registry builds"));
    let audit_batch = registry
        .audit_batch_handle()
        .expect("event store configured, so the batch seam is too");
    let before = audit_batch.test_snapshot();

    let mut dispatches = Vec::with_capacity(N);
    for _ in 0..N {
        let registry = Arc::clone(&registry);
        dispatches.push(tokio::spawn(async move {
            registry.dispatch("create", Value::Null).await
        }));
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), append_started.notified())
        .await
        .expect("audit generation reaches the blocking store");

    for dispatch in dispatches {
        let err = tokio::time::timeout(std::time::Duration::from_secs(5), dispatch)
            .await
            .expect("each dispatch resolves within the resolution deadline instead of hanging")
            .expect("dispatch task joins")
            .expect_err("a stalled audit store must not report a committed write as success");
        assert!(
            err.to_string().contains("ResolutionDeadlineExpired"),
            "unexpected error: {err}"
        );
    }
    assert_eq!(
        effects.load(std::sync::atomic::Ordering::SeqCst),
        N,
        "every handler must have run exactly once, never rerun while awaiting audit resolution"
    );
    let after = audit_batch.test_snapshot();
    assert_eq!(
        after.submitted_rows - before.submitted_rows,
        N as u64,
        "exactly N rows were submitted; giving up on the wait must not re-enqueue or duplicate them"
    );
    assert_eq!(
        after.pending_rows, 0,
        "no row is pushed back onto the pending queue after its caller gives up"
    );
}

// Both tests in this file arm `fault_injection`'s process-global
// `SUPERVISOR_SLEEP_BEFORE_SPAWN` flag; running them concurrently races one
// test's arm against the other's supervisor loop consuming it.
#[serial]
#[tokio::test]
#[serial(config_ledger)]
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
    // already-computed result. A queue refusal is a confirmed, terminal
    // loss — the row never enqueued — so it counts on the "refused" counter,
    // not the "unresolved" one used by the deadline-expiry test below.
    let before_refused = audit_admission_refused_obligation_count();
    let before_unresolved = audit_admission_unresolved_obligation_count();
    let result = registry
        .dispatch("list", Value::Null)
        .await
        .expect("a read verb must not fail on audit-lane admission exhaustion");
    assert_eq!(
        result,
        serde_json::json!({ "pack": "alpha", "verb": "list" })
    );
    assert_eq!(
        audit_admission_refused_obligation_count(),
        before_refused + 1,
        "a queue-refusal degrade must count on its own dedicated counter, not \
         AUDIT_APPEND_FAILURES or AUDIT_OBLIGATION_APPEND_FAILURES"
    );
    assert_eq!(
        audit_admission_unresolved_obligation_count(),
        before_unresolved,
        "a queue refusal must never be counted on the deadline-expiry counter"
    );
    // The raw process-wide counter is an internal detail; what an operator
    // actually reads is `db_diagnostics`, fed by `VerbRegistry::audit_batch_metrics()`
    // (ADR-103 Amendment 3 / ADR-133 Amendment 1). Prove the threading, not
    // just the counter increment: this assertion reddens if
    // `RuntimeAuditBatchMetrics::admission_refused_obligations` regresses to
    // a hardcoded 0 or is dropped from the struct.
    let metrics = registry
        .audit_batch_metrics()
        .expect("event store configured, so the batch seam is too");
    assert_eq!(
        metrics.admission_refused_obligations,
        audit_admission_refused_obligation_count(),
        "VerbRegistry::audit_batch_metrics() must surface the same admission-refused count \
         the production db_diagnostics verb reports"
    );

    // Drive the SAME production path the `db_diagnostics` verb handler uses
    // (`crates/khive-pack-kg/src/handlers/db_diagnostics.rs` calls exactly
    // this `KhiveRuntime` method with exactly this `registry.audit_batch_metrics()`
    // value) so this assertion reddens if that forwarding is ever dropped in
    // favor of passing `None`.
    let rt = KhiveRuntime::memory().expect("memory runtime should create");
    let report = rt
        .db_diagnostics_with_audit_metrics(Some(metrics))
        .await
        .expect("diagnostics succeed");
    assert_eq!(
        report.writer_contention.audit_admission_refused_obligations,
        Some(audit_admission_refused_obligation_count()),
        "the real db_diagnostics handler path must surface the refused-obligation count"
    );
    assert_eq!(
        report
            .writer_contention
            .audit_admission_unresolved_obligations,
        Some(audit_admission_unresolved_obligation_count()),
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
#[serial(config_ledger)]
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
    // its own already-computed result rather than discarding it. Unlike a
    // queue refusal, the row was already enqueued and may still commit, so
    // this must count on the "unresolved" counter, not "refused".
    let before_refused = audit_admission_refused_obligation_count();
    let before_unresolved = audit_admission_unresolved_obligation_count();
    let result = registry
        .dispatch("list", Value::Null)
        .await
        .expect("a read verb must not fail when its own audit row's admission deadline elapses");
    assert_eq!(
        result,
        serde_json::json!({ "pack": "alpha", "verb": "list" })
    );
    assert_eq!(
        audit_admission_unresolved_obligation_count(),
        before_unresolved + 1,
        "a deadline-expiry degrade must count on its own dedicated counter too, not just \
         the queue-refusal arm"
    );
    assert_eq!(
        audit_admission_refused_obligation_count(),
        before_refused,
        "a deadline expiry must never be counted on the queue-refusal counter"
    );

    // Drive the same production forwarding path as the queue-refusal test
    // above: this reddens if `db_diagnostics_with_audit_metrics` stops
    // forwarding the unresolved count into `writer_contention`.
    let metrics = registry
        .audit_batch_metrics()
        .expect("event store configured, so the batch seam is too");
    let rt = KhiveRuntime::memory().expect("memory runtime should create");
    let report = rt
        .db_diagnostics_with_audit_metrics(Some(metrics))
        .await
        .expect("diagnostics succeed");
    assert_eq!(
        report
            .writer_contention
            .audit_admission_unresolved_obligations,
        Some(audit_admission_unresolved_obligation_count()),
        "the real db_diagnostics handler path must surface the unresolved-obligation count"
    );

    drop(occupant);
}

/// khive#2228 M1: a FAILED dispatch of an allowlisted read verb must stay on
/// the strict obligation path even when the audit lane is under the exact
/// same `QueueAdmissionExhausted` pressure that degrades a *successful*
/// allowlisted read's obligation in
/// `read_verb_dispatch_survives_audit_lane_admission_exhaustion` above. The
/// dispatch itself still surfaces its original handler error (unrelated to
/// audit accounting), but the admission-degrade counters must not move for
/// this row — a `DispatchFailed` producer is never admission-degrade
/// eligible, no matter what `VerbRegistry::admission_degrade_safe` says
/// about the verb name alone.
#[serial]
#[tokio::test]
#[serial(config_ledger)]
async fn failed_allowlisted_read_does_not_degrade_on_admission_exhaustion() {
    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(BetaPack);
    builder.with_event_store(store);
    builder.with_audit_batch_config(AuditBatchConfig {
        max_pending_rows: std::num::NonZeroUsize::new(1).unwrap(),
        ..AuditBatchConfig::default()
    });
    let registry = builder.build().expect("registry builds");
    let audit_batch = registry
        .audit_batch_handle()
        .expect("event store configured, so the batch seam is too");

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

    // The audit lane is now saturated. `get` is on `ADMISSION_DEGRADE_SAFE_VERBS`
    // and registered `Assertive`, so `admission_degrade_safe("get")` is true —
    // but BetaPack's handler always returns `Err`, so the producer is
    // `DispatchFailed`. That must stay obligation-bearing: the dispatch fails
    // with its own handler error (via `fold_audit_obligation`/the direct
    // handler error path), and the admission-degrade counters must not move.
    let before_refused = audit_admission_refused_obligation_count();
    let before_unresolved = audit_admission_unresolved_obligation_count();
    let err = registry
        .dispatch("get", Value::Null)
        .await
        .expect_err("a failed allowlisted read must still surface its handler error");
    assert!(
        err.to_string().contains("widget intentionally missing"),
        "the original handler error must be preserved, not replaced by an audit error: {err}"
    );
    assert_eq!(
        audit_admission_refused_obligation_count(),
        before_refused,
        "a DispatchFailed row must never be counted on the admission-degrade \
         refused counter, even for an allowlisted verb name"
    );
    assert_eq!(
        audit_admission_unresolved_obligation_count(),
        before_unresolved,
        "a DispatchFailed row must never be counted on the admission-degrade \
         unresolved counter, even for an allowlisted verb name"
    );

    drop(occupant);
    drop(filler);
}

/// khive#2228 M1: the `AdmissionDeadlineExpired` counterpart of
/// `failed_allowlisted_read_does_not_degrade_on_admission_exhaustion` above —
/// a FAILED allowlisted read's own audit row enqueues and then times out
/// waiting on its admission deadline, rather than being refused before
/// enqueue. That must also stay obligation-bearing: no admission-degrade
/// counter may move for a `DispatchFailed` producer.
#[serial]
#[tokio::test]
#[serial(config_ledger)]
async fn failed_allowlisted_read_does_not_degrade_on_admission_deadline_expiry() {
    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(BetaPack);
    builder.with_event_store(store);
    builder.with_audit_batch_config(AuditBatchConfig {
        max_pending_rows: std::num::NonZeroUsize::new(4).unwrap(),
        admission_deadline: std::time::Duration::from_millis(30),
        ..AuditBatchConfig::default()
    });
    let registry = builder.build().expect("registry builds");
    let audit_batch = registry
        .audit_batch_handle()
        .expect("event store configured, so the batch seam is too");

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

    let before_refused = audit_admission_refused_obligation_count();
    let before_unresolved = audit_admission_unresolved_obligation_count();
    let err = registry
        .dispatch("get", Value::Null)
        .await
        .expect_err("a failed allowlisted read must still surface its handler error");
    assert!(
        err.to_string().contains("widget intentionally missing"),
        "the original handler error must be preserved, not replaced by an audit error: {err}"
    );
    assert_eq!(
        audit_admission_refused_obligation_count(),
        before_refused,
        "a DispatchFailed row must never be counted on the admission-degrade \
         refused counter, even for an allowlisted verb name"
    );
    assert_eq!(
        audit_admission_unresolved_obligation_count(),
        before_unresolved,
        "a DispatchFailed row must never be counted on the admission-degrade \
         unresolved counter, even for an allowlisted verb name"
    );

    drop(occupant);
}
