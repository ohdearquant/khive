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
//! Gated on `--features fault-injection,test-internals` (same as
//! `tests/adr133_audit_batch.rs`), but this crate's own `[dev-dependencies]`
//! entry on itself (`crates/khive-runtime/Cargo.toml`) already requests both
//! features, so plain `cargo test -p khive-runtime` builds and runs this
//! file with no extra flags — the `#![cfg]` below is what makes the file a
//! no-op for anyone building khive-runtime as a plain dependency, not an
//! opt-in a caller of `cargo test -p khive-runtime` has to remember.

#![cfg(all(feature = "fault-injection", feature = "test-internals"))]

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::audit_batch::{
    fault_injection, AuditBatch, AuditBatchConfig, AuditBatchControl, AuditCommitOutcome,
    AuditProducer, AuditTerminalReason, PreparedAuditRow,
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

/// khive#2331 cap-shedding coverage: per-call release gates, keyed by
/// arrival order, each holding the sender half of a oneshot pair the call
/// itself created and is awaiting the receiver half of. Lets a test release
/// one specific blocked `append_events_idempotent` call while an earlier
/// one stays blocked — a single shared `Notify`/`Semaphore` always wakes its
/// oldest waiter first, which cannot express "release the newest blocked
/// call while an older one stays blocked".
type CallGates = Arc<std::sync::Mutex<Vec<Option<tokio::sync::oneshot::Sender<()>>>>>;

fn release_call_gate(gates: &CallGates, index: usize) {
    let tx = gates.lock().unwrap()[index]
        .take()
        .expect("call gate exists and has not already been released");
    let _ = tx.send(());
}

#[derive(Default)]
struct MemoryEventStore {
    events: std::sync::Mutex<Vec<Event>>,
    append_started: Option<Arc<tokio::sync::Notify>>,
    append_release: Option<Arc<tokio::sync::Notify>>,
    call_gates: Option<CallGates>,
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
    ) -> StorageResult<khive_storage::event::IdempotentEventBatchResult> {
        if let Some(started) = &self.append_started {
            started.notify_one();
        }
        if let Some(gates) = &self.call_gates {
            let (tx, rx) = tokio::sync::oneshot::channel();
            gates.lock().unwrap().push(Some(tx));
            let _ = rx.await;
        } else if let Some(release) = &self.append_release {
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
///
/// `Pack::NAME` is `"kg"`, matching `VerbRegistry::ADMISSION_DEGRADE_SAFE_VERBS`'s
/// `("kg", "get")` entry: admission-degrade eligibility is bound to the
/// owning pack, not the verb name alone, so a stand-in pack under any other
/// name would make `get` ineligible regardless of category.
struct BetaPack;

impl Pack for BetaPack {
    const NAME: &'static str = "kg";
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
///
/// `Pack::NAME` is `"kg"`, matching `VerbRegistry::ADMISSION_DEGRADE_SAFE_VERBS`'s
/// `("kg", "list")` entry — see `BetaPack`'s doc for why this must match
/// exactly.
struct AlphaPack;

impl Pack for AlphaPack {
    const NAME: &'static str = "kg";
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
        Ok(serde_json::json!({ "pack": "kg", "verb": verb }))
    }
}

/// Rogue-pack probe: declares Assertive handlers under the verb names of
/// several *other* real packs' allowlisted reads (`gtd.tasks`, `gtd.next`,
/// `comm.inbox`) alongside the known-incidental-write names
/// (`memory.recall`, `db_diagnostics`, `knowledge.*`), all under a pack name
/// (`cross-pack-census-probe`) that matches none of
/// `VerbRegistry::ADMISSION_DEGRADE_SAFE_VERBS`'s `(pack, verb)` entries.
///
/// This is exactly the shape of pack that admission-degrade eligibility must
/// reject: a verb name alone is not a sound key, because any pack registered
/// through the same `VerbRegistryBuilder` path can declare a handler under a
/// name that collides with an allowlisted one while actually performing a
/// durable write of its own. Binding eligibility to `(pack, verb)` — not
/// `verb` alone — means every handler here must stay fail-closed regardless
/// of verb name, proven by
/// `cross_pack_reads_stay_strict_when_pack_identity_does_not_match_allowlist`
/// below. The handlers are deliberately no-ops: this test exercises the
/// registry's reviewed name/category/pack classification under a forced
/// audit refusal, not real domain behavior (which the source census in
/// `khive-runtime/src/pack.rs`'s `mod tests` audits separately).
struct CrossPackCensusProbe;

impl Pack for CrossPackCensusProbe {
    const NAME: &'static str = "cross-pack-census-probe";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[
        HandlerDef {
            name: "gtd.tasks",
            description: "reported side-effect-free GTD read",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
        HandlerDef {
            name: "gtd.next",
            description: "reported side-effect-free GTD read",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
        HandlerDef {
            name: "comm.inbox",
            description: "reported side-effect-free comm read",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
        HandlerDef {
            name: "memory.recall",
            description: "Assertive handler with serve-accounting writes",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
        HandlerDef {
            name: "db_diagnostics",
            description: "Assertive handler with PASSIVE checkpoint I/O",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
        HandlerDef {
            name: "knowledge.search",
            description: "Assertive handler with ANN maintenance",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
        HandlerDef {
            name: "knowledge.suggest",
            description: "Assertive handler with ANN maintenance",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
        HandlerDef {
            name: "knowledge.compose",
            description: "Assertive handler that can invoke ANN maintenance",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
    ];
}

#[async_trait]
impl PackRuntime for CrossPackCensusProbe {
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
        Ok(serde_json::json!({
            "pack": "cross-pack-census-probe",
            "verb": verb,
        }))
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

/// khive#2331: `AdmissionDeadlineExpired`/`ResolutionDeadlineExpired` bound
/// only how long a *caller* waits for a row's outcome — the row itself
/// stays exactly where the driver holds it, by design (see both tests
/// above). Before this fix, that meant a permanently stalled
/// `EventStore::append_events_idempotent()` call could pin the driver
/// inside one generation's `child.await` forever: `supervisor_loop` never
/// loops back to drain `state.pending` again, so every row still arriving
/// piles up there until `max_pending_rows` is reached, and every
/// submission after that gets `QueueAdmissionExhausted` — including ones
/// that have nothing to do with the original stalled generation. Giving up
/// as a caller does not free the slot: the row is left enqueued regardless.
///
/// Red before the fix: the driver never abandons the stuck generation, so
/// `pending_rows` never returns to 0 and the final `submit()` below hangs
/// at `QueueAdmissionExhausted` forever (this test's own 5s outer timeouts
/// would fail it instead of letting it hang the suite).
#[serial]
#[tokio::test]
#[serial(config_ledger)]
async fn driver_deadline_recovers_admission_capacity_from_a_stalled_generation() {
    let append_started = Arc::new(tokio::sync::Notify::new());
    let store = Arc::new(MemoryEventStore {
        append_started: Some(Arc::clone(&append_started)),
        // Never notified: every `append_events_idempotent` call blocks
        // forever, for every generation the driver ever spawns, not just
        // the first.
        append_release: Some(Arc::new(tokio::sync::Notify::new())),
        ..MemoryEventStore::default()
    });
    let batch = AuditBatch::new(
        store,
        AuditBatchConfig {
            admission_deadline: std::time::Duration::from_millis(20),
            resolution_deadline: std::time::Duration::from_millis(30),
            max_pending_rows: std::num::NonZeroUsize::new(2).unwrap(),
            ..AuditBatchConfig::default()
        },
    );

    // Occupant: drained into generation 1 immediately, which then stalls
    // forever on the store call.
    let occupant_batch = batch.clone();
    let occupant = tokio::spawn(async move {
        occupant_batch
            .submit(PreparedAuditRow {
                event: mk_event("kg.occupant"),
                producer: AuditProducer::DispatchSucceeded,
            })
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), append_started.notified())
        .await
        .expect("generation 1 reaches the blocking store");

    // Two more rows fill `pending` to its configured 2-row capacity while
    // generation 1 is stuck. (a): each times out on its own
    // `admission_deadline` — a caller giving up — but per the documented
    // contract stays enqueued for the driver to resolve.
    let mut fillers = Vec::new();
    for i in 0..2 {
        let b = batch.clone();
        fillers.push(tokio::spawn(async move {
            b.submit(PreparedAuditRow {
                event: mk_event(&format!("kg.filler{i}")),
                producer: AuditProducer::DispatchSucceeded,
            })
            .await
        }));
    }
    for f in fillers {
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), f)
            .await
            .expect("filler resolves within its own admission deadline instead of hanging")
            .expect("filler task joins");
        assert_eq!(
            result,
            Err(AuditTerminalReason::AdmissionDeadlineExpired),
            "each filler gives up on its own admission_deadline, but its row is left enqueued"
        );
    }

    // Capacity is still exhausted by rows the stuck driver has not drained,
    // even though both fillers' own callers already gave up above — proving
    // a caller giving up does not, by itself, free anything.
    let refused = batch
        .submit(PreparedAuditRow {
            event: mk_event("kg.refused"),
            producer: AuditProducer::DispatchSucceeded,
        })
        .await;
    assert_eq!(
        refused,
        Err(AuditTerminalReason::QueueAdmissionExhausted),
        "pending is still full of rows the stuck driver has not drained"
    );

    // (b): once the driver's own per-generation bound elapses (3x
    // `resolution_deadline` = 90ms here), it abandons generation 1 and loops
    // back to drain the two filler rows into generation 2 — which stalls
    // the same way, but draining `pending` is what frees admission.
    wait_until(std::time::Duration::from_secs(5), || {
        batch.test_snapshot().pending_rows == 0
    })
    .await;
    let abandoned = batch
        .test_snapshot()
        .per_generation
        .iter()
        .filter(|g| g.terminal_reason == Some(AuditTerminalReason::DriverAppendAbandoned))
        .count();
    assert!(
        abandoned >= 1,
        "generation 1 must be recorded as DriverAppendAbandoned once the driver gives up on it"
    );

    // (a) continued: a fresh row is admitted (pushed onto `pending`) again,
    // not synchronously refused — the bug this test guards against.
    let fresh_batch = batch.clone();
    let fresh = tokio::spawn(async move {
        fresh_batch
            .submit(PreparedAuditRow {
                event: mk_event("kg.fresh"),
                producer: AuditProducer::DispatchSucceeded,
            })
            .await
    });
    wait_until(std::time::Duration::from_secs(5), || {
        let snap = batch.test_snapshot();
        snap.pending_rows >= 1 || snap.in_flight_generation.is_some()
    })
    .await;

    drop(occupant);
    drop(fresh);
}

/// Control for the fix above: an append that completes well inside the
/// driver's own per-generation bound must be entirely unaffected by the new
/// timeout wrapper around it — it still commits normally, through the same
/// code path as every pre-existing (non-stalling) test in this crate, and
/// leaves no `DriverAppendAbandoned` generation behind.
#[serial]
#[tokio::test]
#[serial(config_ledger)]
async fn normal_append_inside_driver_deadline_is_unaffected() {
    let store = Arc::new(MemoryEventStore::default());
    let batch = AuditBatch::new(
        store,
        AuditBatchConfig {
            admission_deadline: std::time::Duration::from_millis(20),
            resolution_deadline: std::time::Duration::from_millis(30),
            ..AuditBatchConfig::default()
        },
    );
    let before = batch.test_snapshot();
    let result = batch
        .submit(PreparedAuditRow {
            event: mk_event("kg.normal"),
            producer: AuditProducer::DispatchSucceeded,
        })
        .await;
    assert_eq!(result, Ok(AuditCommitOutcome::Committed));
    let after = batch.test_snapshot();
    assert_eq!(after.pending_rows, 0);
    let new_generations = &after.per_generation[before.per_generation.len()..];
    assert!(
        !new_generations.is_empty(),
        "the committed row must have produced a generation record"
    );
    assert!(
        new_generations.iter().all(|g| g.terminal_reason.is_none()),
        "a normal, fast append must commit cleanly, never as DriverAppendAbandoned: \
         {new_generations:?}"
    );
}

/// khive#2331: `driver_append_deadline` alone stops the driver from holding
/// one stalled generation forever, but it does not cap how many detached
/// appends can be outstanding at once — a store whose append never returns
/// would otherwise mint one detached task per `driver_append_deadline`
/// indefinitely, and each retains up to `max_rows_per_generation` events
/// until it (maybe) finally returns.
/// `AuditBatchConfig::max_abandoned_appends` caps the outstanding count:
/// once reached, further generations are shed with
/// `AuditTerminalReason::StoreWedged` without ever calling the store, and
/// the driver resumes attempting real appends as soon as an outstanding one
/// returns and the count drops back below the cap.
///
/// Red before the fix: there is no cap, `outstanding_abandoned_appends` does
/// not exist on the snapshot, and all five generations below resolve
/// `DriverAppendAbandoned` after their own `driver_append_deadline` instead
/// of the last three shedding immediately as `StoreWedged`.
#[serial]
#[tokio::test]
#[serial(config_ledger)]
async fn driver_sheds_generations_once_the_abandoned_append_cap_is_reached() {
    let append_started = Arc::new(tokio::sync::Notify::new());
    let call_gates: CallGates = Arc::new(std::sync::Mutex::new(Vec::new()));
    let store = Arc::new(MemoryEventStore {
        append_started: Some(Arc::clone(&append_started)),
        call_gates: Some(Arc::clone(&call_gates)),
        ..MemoryEventStore::default()
    });
    // Generous relative to `admission_deadline` so the state-polling below
    // (a handful of lock/notify round trips) never races a caller's own
    // bounded wait; the mechanism under test is driven entirely off
    // `test_snapshot()`/`health_metrics()`, never off a `submit()` call's
    // own return value, for exactly that reason.
    let resolution_deadline = std::time::Duration::from_millis(100);
    // Mirrors `driver_append_deadline()` in `audit_batch.rs` (3x
    // `resolution_deadline`); that function is private, so this is a
    // parallel derivation, not a call into it.
    let driver_append_deadline = resolution_deadline.saturating_mul(3);
    let batch = AuditBatch::new(
        store,
        AuditBatchConfig {
            admission_deadline: std::time::Duration::from_millis(80),
            resolution_deadline,
            max_abandoned_appends: std::num::NonZeroUsize::new(2).unwrap(),
            ..AuditBatchConfig::default()
        },
    );

    // Generation 1: submitted and immediately blocked on call gate index 0.
    // The caller's own bounded `submit()` may give up at `admission_deadline`
    // before the driver's own, much larger bound — that is expected and
    // irrelevant here (its `JoinHandle` is dropped, unawaited), because the
    // row itself is left with the driver regardless (documented non-removal
    // contract) and this test observes the driver's behavior directly
    // through `test_snapshot()`, not through the caller's wait.
    let b = batch.clone();
    let g1 = tokio::spawn(async move {
        b.submit(PreparedAuditRow {
            event: mk_event("kg.g1"),
            producer: AuditProducer::DispatchSucceeded,
        })
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), append_started.notified())
        .await
        .expect("generation 1 reaches the blocking store");
    drop(g1);

    // Generation 1 must abandon (past driver_append_deadline) before the
    // driver loops back to drain generation 2.
    wait_until(driver_append_deadline * 2, || {
        batch.test_snapshot().outstanding_abandoned_appends >= 1
    })
    .await;

    // Generation 2: submitted once generation 1 has abandoned, so it forms
    // its own generation (outstanding == 1 < cap == 2, not shed) and blocks
    // on call gate index 1.
    let b = batch.clone();
    let g2 = tokio::spawn(async move {
        b.submit(PreparedAuditRow {
            event: mk_event("kg.g2"),
            producer: AuditProducer::DispatchSucceeded,
        })
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), append_started.notified())
        .await
        .expect("generation 2 reaches the blocking store");
    drop(g2);

    // Generation 2 must also abandon before the cap (2) is reached.
    wait_until(driver_append_deadline * 2, || {
        batch.test_snapshot().outstanding_abandoned_appends >= 2
    })
    .await;
    let snap = batch.test_snapshot();
    let abandoned = snap
        .per_generation
        .iter()
        .filter(|g| g.terminal_reason == Some(AuditTerminalReason::DriverAppendAbandoned))
        .count();
    assert_eq!(
        abandoned, 2,
        "exactly generations 1 and 2 must have abandoned before the cap takes effect: {:?}",
        snap.per_generation
    );

    // (a): with the cap reached, three more generations shed immediately —
    // no store call is ever attempted (the call-gate count stays at 2),
    // well under `driver_append_deadline`.
    for i in 0..3 {
        let start = std::time::Instant::now();
        let result = batch
            .submit(PreparedAuditRow {
                event: mk_event(&format!("kg.wedged{i}")),
                producer: AuditProducer::DispatchSucceeded,
            })
            .await;
        let elapsed = start.elapsed();
        assert_eq!(
            result,
            Err(AuditTerminalReason::StoreWedged),
            "generation must shed once the abandoned-append cap is reached"
        );
        assert!(
            elapsed < driver_append_deadline / 2,
            "a shed generation must resolve without waiting anywhere near \
             driver_append_deadline: took {elapsed:?}"
        );
    }
    assert_eq!(
        call_gates.lock().unwrap().len(),
        2,
        "shedding must never attempt a real append: the store must never see a third call"
    );
    assert_eq!(
        batch.test_snapshot().outstanding_abandoned_appends,
        2,
        "shedding must not spawn or retain any additional detached appends"
    );

    // (d): admission capacity was never at risk — `pending` stayed far below
    // `max_pending_rows`, and every submission above resolved to either
    // `DriverAppendAbandoned` (observed via the snapshot) or `StoreWedged`
    // (asserted directly), never `QueueAdmissionExhausted`.
    assert!(
        batch.test_snapshot().pending_rows < AuditBatchConfig::default().max_pending_rows.get(),
        "pending must never approach max_pending_rows while generations are being shed"
    );

    // (b): recovery. Releasing generation 1's call gate (index 0) lets that
    // append finally return a commit, dropping the outstanding count to 1 —
    // below the cap. `notify_one`/a shared `Semaphore` would instead always
    // wake the OLDEST blocked call first, which is exactly why this test
    // uses per-call gates: generation 2's call (index 1) must stay blocked
    // and untouched by this release.
    release_call_gate(&call_gates, 0);
    wait_until(std::time::Duration::from_secs(5), || {
        batch.test_snapshot().outstanding_abandoned_appends == 1
    })
    .await;
    assert_eq!(
        batch.health_metrics().late_append_commits,
        1,
        "the released append must be recorded as a late commit, proving the wedged \
         store later drained"
    );

    // Below the cap again (1 < 2), a fresh generation attempts a real
    // append — no timer needed for recovery.
    let before_recovery = batch.test_snapshot().per_generation.len();
    let recovered = batch.clone();
    let recovered_task = tokio::spawn(async move {
        recovered
            .submit(PreparedAuditRow {
                event: mk_event("kg.recovered"),
                producer: AuditProducer::DispatchSucceeded,
            })
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), append_started.notified())
        .await
        .expect("recovery generation attempts a real append, proving it was not shed");
    wait_until(std::time::Duration::from_secs(5), || {
        call_gates.lock().unwrap().len() == 3
    })
    .await;
    release_call_gate(&call_gates, 2);
    drop(recovered_task);

    wait_until(std::time::Duration::from_secs(5), || {
        let snap = batch.test_snapshot();
        snap.per_generation.len() > before_recovery
            && snap.per_generation[before_recovery..]
                .iter()
                .any(|g| g.terminal_reason.is_none() && g.committed_rows >= 1)
    })
    .await;
    assert_eq!(
        batch.test_snapshot().outstanding_abandoned_appends,
        1,
        "the recovery generation must commit inline (never detach); generation 2's \
         still-blocked append remains the only outstanding one"
    );
}

// Every test below this point arms `fault_injection`'s process-global
// `SUPERVISOR_SLEEP_BEFORE_SPAWN` flag; running them concurrently races one
// test's arm against another supervisor loop consuming it. (The
// `MemoryEventStore`-driven tests above this comment never arm it — their
// unnamed `#[serial]` is not for this reason.)
#[serial]
#[tokio::test]
#[serial(config_ledger)]
async fn read_verb_dispatch_survives_audit_lane_admission_exhaustion() {
    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    // `register_trusted` stands in for the real composition root
    // (`PackRegistry::register_packs`, which registers only
    // `inventory`-discovered factories): this test's whole point is
    // eligibility for a pack the registry actually vouches for, so it must
    // not use the untrusted `register` path.
    builder.register_trusted(AlphaPack);
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
    assert_eq!(result, serde_json::json!({ "pack": "kg", "verb": "list" }));
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

/// khive-oss#2311: `pack.name()` is a value the `PackRuntime` trait object
/// reports about itself, not something the registry verifies — so before
/// this fix, any pack registered through the ordinary, untrusted
/// `VerbRegistryBuilder::register` path could self-report an allowlisted
/// pack name (`AlphaPack` here claims `"kg"`, exactly like the previous
/// test) and inherit admission-degrade eligibility it never earned, as long
/// as the real `kg` pack was not also loaded (verb names are unique per
/// registry, so this is the only shape in which an impostor's same-named
/// handler is reachable at all). This is the exact mirror of the previous
/// test — same pack type, same verb, same admission-pressure harness — with
/// the one load-bearing difference being the registration path: `register`
/// here instead of `register_trusted`. Before the fix, `admission_degrade_safe_probe`
/// returned `true` for this registration and the read below degraded
/// successfully instead of hard-failing.
#[serial]
#[tokio::test]
#[serial(config_ledger)]
async fn allowlisted_read_stays_strict_when_pack_is_registered_untrusted() {
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

    assert!(
        !registry.admission_degrade_safe_probe("list"),
        "AlphaPack's \"list\" is Assertive and (\"kg\", \"list\") is allowlisted, but \
         AlphaPack was registered through the untrusted `register` path — it must not read \
         as admission-degrade-safe regardless of its self-reported pack name"
    );

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

    // Unlike the trusted-registration test above, "list" must now hard-fail
    // exactly like "create" (a real write) already does when the audit lane
    // is saturated — it is no longer eligible to drop its own audit row.
    let before_refused = audit_admission_refused_obligation_count();
    let before_unresolved = audit_admission_unresolved_obligation_count();
    let error = registry.dispatch("list", Value::Null).await.expect_err(
        "an untrusted-registration read must hard-fail under audit-lane admission \
             exhaustion, the same as a write",
    );
    let message = error.to_string();
    assert!(
        message.contains("audit obligation commit failed")
            && message.contains("QueueAdmissionExhausted"),
        "expected the strict obligation-failure path, got: {message}"
    );
    assert_eq!(
        audit_admission_refused_obligation_count(),
        before_refused,
        "an untrusted-pack read must never take the counted admission-degrade path"
    );
    assert_eq!(
        audit_admission_unresolved_obligation_count(),
        before_unresolved,
        "an untrusted-pack read must never move the deadline-expiry counter either"
    );

    drop(occupant);
    drop(filler);
}

/// Admission-degrade eligibility is bound to `(pack, verb)`, not `verb`
/// alone: every handler `CrossPackCensusProbe` declares is `Assertive` and
/// several of its names (`gtd.tasks`, `gtd.next`, `comm.inbox`) are on
/// `VerbRegistry::ADMISSION_DEGRADE_SAFE_VERBS`, but the pack that declares
/// them here (`cross-pack-census-probe`) matches none of the allowlist's
/// `(pack, verb)` pairs. Every handler must therefore stay fail-closed under
/// a forced `QueueAdmissionExhausted` outcome — the previously-"safe" names
/// included, since a bare name match without pack verification is exactly
/// the gap this binding closes. Before the fix, the first three names in
/// this test's loop degraded successfully (accepted as `true` by
/// `admission_degrade_safe`), which is the failure mode the direct probe
/// assertion below reproduces.
#[serial]
#[tokio::test]
#[serial(config_ledger)]
async fn cross_pack_reads_stay_strict_when_pack_identity_does_not_match_allowlist() {
    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    // Trusted registration isolates pack-identity mismatch as the sole
    // cause of ineligibility here — untrusted registration would reject
    // these handlers too, but for the different reason this test does not
    // exercise (see `allowlisted_read_stays_strict_when_pack_is_registered_untrusted`).
    builder.register_trusted(CrossPackCensusProbe);
    builder.with_event_store(store);
    builder.with_audit_batch_config(AuditBatchConfig {
        max_pending_rows: std::num::NonZeroUsize::new(1).unwrap(),
        ..AuditBatchConfig::default()
    });
    let registry = builder.build().expect("registry builds");
    let audit_batch = registry
        .audit_batch_handle()
        .expect("event store configured, so the batch seam is too");

    // Direct white-box check, independent of the audit-pressure mechanics
    // below: a handler named like an allowlisted verb but declared by the
    // wrong pack must never read as admission-degrade-safe.
    for verb in ["gtd.tasks", "gtd.next", "comm.inbox"] {
        assert!(
            !registry.admission_degrade_safe_probe(verb),
            "{verb} is on ADMISSION_DEGRADE_SAFE_VERBS under a different pack; \
             cross-pack-census-probe's handler of the same name must not inherit \
             its degrade-safety"
        );
    }

    fault_injection::arm_supervisor_sleep_before_spawn();
    let occupant_batch = audit_batch.clone();
    let occupant = tokio::spawn(async move {
        occupant_batch
            .submit(PreparedAuditRow {
                event: mk_event("census.occupant"),
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
                event: mk_event("census.filler"),
                producer: AuditProducer::ConfigLocked,
            })
            .await
    });
    wait_until(std::time::Duration::from_secs(5), || {
        audit_batch.test_snapshot().pending_rows == 1
    })
    .await;

    let before_refused = audit_admission_refused_obligation_count();
    let before_unresolved = audit_admission_unresolved_obligation_count();
    for verb in [
        "gtd.tasks",
        "gtd.next",
        "comm.inbox",
        "memory.recall",
        "db_diagnostics",
        "knowledge.search",
        "knowledge.suggest",
        "knowledge.compose",
    ] {
        let error = match registry.dispatch(verb, Value::Null).await {
            Ok(result) => panic!(
                "{verb} must stay fail-closed under forced audit admission refusal — its \
                 owning pack here does not match the allowlist's entry for this verb name; \
                 unexpected result: {result}"
            ),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(
            message.contains("audit obligation commit failed")
                && message.contains("QueueAdmissionExhausted"),
            "{verb} must fail for the saturated audit obligation, got: {message}"
        );
    }
    assert_eq!(
        audit_admission_refused_obligation_count(),
        before_refused,
        "a pack-identity mismatch must never take the counted admission-degrade path"
    );
    assert_eq!(
        audit_admission_unresolved_obligation_count(),
        before_unresolved,
        "forced queue refusal must not move the deadline-expiry counter"
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
#[serial]
#[tokio::test]
#[serial(config_ledger)]
async fn read_verb_dispatch_survives_audit_lane_admission_deadline_expiry() {
    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register_trusted(AlphaPack);
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
    assert_eq!(result, serde_json::json!({ "pack": "kg", "verb": "list" }));
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
    builder.register_trusted(BetaPack);
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
    builder.register_trusted(BetaPack);
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
