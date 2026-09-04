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

// Every test in this file arms `fault_injection`'s process-global
// `SUPERVISOR_SLEEP_BEFORE_SPAWN` flag; running them concurrently races one
// test's arm against another supervisor loop consuming it.
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
    builder.register(CrossPackCensusProbe);
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
