//! Acceptance coverage for operational reads when the shared audit lane is saturated.
//! The comm and GTD handlers below are the production pack implementations.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use khive_pack_comm::CommPack;
use khive_pack_gtd::GtdPack;
use khive_pack_kg::KgPack;
use khive_runtime::audit_batch::{
    fault_injection, AuditBatch, AuditBatchConfig, AuditBatchControl, AuditCommitOutcome,
    AuditProducer, AuditTerminalReason, PreparedAuditRow,
};
use khive_runtime::pack::{
    audit_admission_refused_obligation_count, audit_admission_unresolved_obligation_count,
};
use khive_runtime::{KhiveRuntime, VerbRegistry, VerbRegistryBuilder};
use khive_storage::types::{BatchWriteSummary, Page, PageRequest};
use khive_storage::{Event, EventFilter, EventStore, StorageResult};
use khive_types::{EventKind, EventOutcome, SubstrateKind};
use serde_json::{json, Value};
use serial_test::serial;

#[derive(Default)]
struct MemoryEventStore {
    events: Mutex<Vec<Event>>,
    block_first_append: AtomicBool,
    append_started: tokio::sync::Notify,
    append_release: tokio::sync::Notify,
}

#[async_trait]
impl EventStore for MemoryEventStore {
    async fn append_event(&self, event: Event) -> StorageResult<()> {
        self.events.lock().unwrap().push(event);
        Ok(())
    }

    async fn append_events(&self, events: Vec<Event>) -> StorageResult<BatchWriteSummary> {
        let count = events.len() as u64;
        self.events.lock().unwrap().extend(events);
        Ok(BatchWriteSummary {
            attempted: count,
            affected: count,
            ..BatchWriteSummary::default()
        })
    }

    async fn get_event(&self, id: uuid::Uuid) -> StorageResult<Option<Event>> {
        Ok(self
            .events
            .lock()
            .unwrap()
            .iter()
            .find(|event| event.id == id)
            .cloned())
    }

    async fn query_events(
        &self,
        _filter: EventFilter,
        _page: PageRequest,
    ) -> StorageResult<Page<Event>> {
        unimplemented!("the audit-lane fixture never queries its event store")
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
        use khive_storage::event::{EventAppendDisposition, IdempotentEventBatchResult};

        if self.block_first_append.swap(false, Ordering::SeqCst) {
            self.append_started.notify_one();
            self.append_release.notified().await;
        }
        let mut stored = self.events.lock().unwrap();
        let mut rows = Vec::with_capacity(events.len());
        for event in events {
            if let Some(existing) = stored.iter().find(|stored| stored.id == event.id) {
                rows.push(if *existing == event {
                    EventAppendDisposition::AlreadyPresentIdentical
                } else {
                    EventAppendDisposition::IdentityConflict
                });
            } else {
                stored.push(event);
                rows.push(EventAppendDisposition::Inserted);
            }
        }
        Ok(IdempotentEventBatchResult { rows })
    }

    fn supports_idempotent_audit_batch(&self) -> bool {
        true
    }
}

fn build_real_registry(runtime: &KhiveRuntime) -> VerbRegistryBuilder {
    let mut builder = VerbRegistryBuilder::new();
    builder.register_trusted(KgPack::new(runtime.clone()));
    builder.register_trusted(GtdPack::new(runtime.clone()));
    builder.register_trusted(CommPack::new(runtime.clone()));
    builder
}

async fn seeded_registry(
    config: AuditBatchConfig,
    audit_store: Arc<MemoryEventStore>,
) -> (VerbRegistry, VerbRegistry, String, String) {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let seed = build_real_registry(&runtime)
        .build()
        .expect("real packs register for seeding");
    seed.apply_schema_plans_with_map(&HashMap::new(), runtime.backend())
        .expect("real pack auxiliary schema is installed before dispatch");
    runtime.install_edge_rules(seed.all_edge_rules());

    seed.dispatch(
        "gtd.assign",
        json!({"title": "audit pressure task", "status": "next"}),
    )
    .await
    .expect("seed an actionable task before saturating audit admission");
    let sent = seed
        .dispatch(
            "comm.send",
            json!({"to": "local", "content": "audit pressure message"}),
        )
        .await
        .expect("seed a self-addressed message before saturating audit admission");
    let outbound_id = sent["full_id"]
        .as_str()
        .expect("comm.send returns a full outbound id")
        .to_owned();
    let inbox = seed
        .dispatch("comm.inbox", json!({"status": "unread"}))
        .await
        .expect("seeded inbound message is listed");
    let inbound_id = inbox["messages"][0]["full_id"]
        .as_str()
        .expect("inbox returns the inbound full id")
        .to_owned();

    let mut builder = build_real_registry(&runtime);
    builder.with_event_store(audit_store);
    builder.with_audit_batch_config(config);
    let registry = builder
        .build()
        .expect("real packs register with audit lane");
    runtime.install_edge_rules(registry.all_edge_rules());
    (seed, registry, outbound_id, inbound_id)
}

fn audit_event(verb: &str) -> Event {
    Event::new(
        "local",
        verb,
        EventKind::Audit,
        SubstrateKind::Event,
        "test:actor",
    )
    .with_outcome(EventOutcome::Success)
}

type Submission = tokio::task::JoinHandle<Result<AuditCommitOutcome, AuditTerminalReason>>;

async fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if condition() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "audit-lane condition did not become true within {timeout:?}"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

async fn occupy_generation(registry: &VerbRegistry) -> (Arc<AuditBatch>, Submission) {
    let batch = registry
        .audit_batch_handle()
        .expect("configured audit lane");
    fault_injection::arm_supervisor_sleep_before_spawn();
    let occupant_batch = Arc::clone(&batch);
    let occupant = tokio::spawn(async move {
        occupant_batch
            .submit(PreparedAuditRow {
                event: audit_event("acceptance.occupant"),
                producer: AuditProducer::ConfigLocked,
            })
            .await
    });
    wait_until(Duration::from_secs(5), || {
        let snapshot = batch.test_snapshot();
        snapshot.pending_rows == 0 && snapshot.in_flight_generation.is_some()
    })
    .await;
    (batch, occupant)
}

fn assert_normal_read_shape(verb: &str, result: &Value, outbound_id: &str) {
    match verb {
        "gtd.tasks" | "gtd.next" => {
            let tasks = result
                .as_array()
                .expect("unclamped GTD read returns an array");
            assert_eq!(tasks.len(), 1, "{verb}: {result}");
            assert_eq!(tasks[0]["title"], "audit pressure task");
        }
        "comm.inbox" => {
            let messages = result["messages"].as_array().expect("inbox messages");
            assert_eq!(messages.len(), 1, "{result}");
            assert_eq!(result["count"], 1);
        }
        "comm.unread" => {
            assert_eq!(result["count"], 1);
            assert_eq!(result["count_saturated"], false);
        }
        "comm.thread" => {
            assert!(result["messages"].as_array().is_some(), "{result}");
            assert!(result["count"].as_u64().is_some_and(|count| count >= 1));
        }
        "comm.delivered" => {
            assert_eq!(result["id"], outbound_id);
            assert_eq!(result["delivered"], true);
            assert_eq!(result["inbound_count"], 1);
        }
        "comm.probe" => {
            assert!(result["new_messages"].as_array().is_some(), "{result}");
            assert!(result["stale_unread_count"].as_i64().is_some());
        }
        "comm.health" => {
            assert_eq!(result["role"], "client");
            assert!(result["channels"].as_array().is_some(), "{result}");
        }
        _ => panic!("unexpected operational read: {verb}"),
    }
}

#[tokio::test]
#[serial]
#[serial(config_ledger)]
async fn real_operational_reads_survive_queue_refusal() {
    let (_seed, registry, outbound_id, _inbound_id) = seeded_registry(
        AuditBatchConfig {
            max_pending_rows: NonZeroUsize::new(1).unwrap(),
            ..AuditBatchConfig::default()
        },
        Arc::new(MemoryEventStore::default()),
    )
    .await;
    let (batch, occupant) = occupy_generation(&registry).await;
    let filler_batch = Arc::clone(&batch);
    let filler = tokio::spawn(async move {
        filler_batch
            .submit(PreparedAuditRow {
                event: audit_event("acceptance.filler"),
                producer: AuditProducer::ConfigLocked,
            })
            .await
    });
    wait_until(Duration::from_secs(5), || {
        batch.test_snapshot().pending_rows == 1
    })
    .await;

    let cases = [
        ("gtd.tasks", json!({"status": "next"})),
        ("gtd.next", json!({})),
        ("comm.inbox", json!({"status": "unread"})),
        ("comm.unread", json!({})),
        ("comm.thread", json!({"id": outbound_id.clone()})),
        ("comm.delivered", json!({"id": outbound_id.clone()})),
        ("comm.probe", json!({"actor": "local"})),
        ("comm.health", json!({})),
    ];
    for (verb, params) in cases {
        let before_refused = audit_admission_refused_obligation_count();
        let before_unresolved = audit_admission_unresolved_obligation_count();
        let result = registry
            .dispatch_with_disposition(verb, params, None)
            .await
            .unwrap_or_else(|error| panic!("{verb} failed under audit queue refusal: {error}"));
        assert_normal_read_shape(verb, &result, &outbound_id);
        assert_eq!(
            audit_admission_refused_obligation_count(),
            before_refused + 1,
            "{verb} must count exactly one refused audit obligation"
        );
        assert_eq!(
            audit_admission_unresolved_obligation_count(),
            before_unresolved,
            "{verb} must not move the deadline-expiry counter"
        );
    }
    let metrics = registry.audit_batch_metrics().expect("audit metrics");
    assert_eq!(
        metrics.admission_refused_obligations,
        audit_admission_refused_obligation_count()
    );
    assert_eq!(
        metrics.admission_unresolved_obligations,
        audit_admission_unresolved_obligation_count()
    );

    occupant.abort();
    filler.abort();
}

#[tokio::test]
#[serial]
#[serial(config_ledger)]
async fn real_inbox_and_gtd_next_survive_admission_deadline_expiry() {
    let (_seed, registry, outbound_id, _inbound_id) = seeded_registry(
        AuditBatchConfig {
            max_pending_rows: NonZeroUsize::new(4).unwrap(),
            admission_deadline: Duration::from_millis(50),
            ..AuditBatchConfig::default()
        },
        Arc::new(MemoryEventStore::default()),
    )
    .await;
    let (batch, occupant) = occupy_generation(&registry).await;

    for (verb, params) in [
        ("comm.inbox", json!({"status": "unread"})),
        ("gtd.next", json!({})),
    ] {
        let before_refused = audit_admission_refused_obligation_count();
        let before_unresolved = audit_admission_unresolved_obligation_count();
        let result = registry
            .dispatch_with_disposition(verb, params, None)
            .await
            .unwrap_or_else(|error| panic!("{verb} failed after audit deadline: {error}"));
        assert_normal_read_shape(verb, &result, &outbound_id);
        assert_eq!(
            audit_admission_unresolved_obligation_count(),
            before_unresolved + 1,
            "{verb} must count exactly one unresolved audit obligation"
        );
        assert_eq!(
            audit_admission_refused_obligation_count(),
            before_refused,
            "{verb} must not move the queue-refusal counter"
        );
    }
    assert_eq!(batch.test_snapshot().pending_rows, 2);
    occupant.abort();
}

#[tokio::test]
#[serial]
#[serial(config_ledger)]
async fn comm_read_waits_past_admission_deadline_for_its_audit_commit() {
    let audit_store = Arc::new(MemoryEventStore {
        block_first_append: AtomicBool::new(true),
        ..MemoryEventStore::default()
    });
    let (seed, registry, _outbound_id, inbound_id) = seeded_registry(
        AuditBatchConfig {
            max_pending_rows: NonZeroUsize::new(4).unwrap(),
            admission_deadline: Duration::from_millis(40),
            resolution_deadline: Duration::from_secs(5),
            ..AuditBatchConfig::default()
        },
        Arc::clone(&audit_store),
    )
    .await;
    let registry = Arc::new(registry);
    let batch = registry
        .audit_batch_handle()
        .expect("configured audit lane");
    let occupant_batch = Arc::clone(&batch);
    let occupant = tokio::spawn(async move {
        occupant_batch
            .submit(PreparedAuditRow {
                event: audit_event("acceptance.read_occupant"),
                producer: AuditProducer::ConfigLocked,
            })
            .await
    });
    tokio::time::timeout(
        Duration::from_secs(5),
        audit_store.append_started.notified(),
    )
    .await
    .expect("first audit generation reaches the held store append");

    let before_refused = audit_admission_refused_obligation_count();
    let before_unresolved = audit_admission_unresolved_obligation_count();
    let mut read = tokio::spawn({
        let registry = Arc::clone(&registry);
        let inbound_id = inbound_id.clone();
        async move {
            registry
                .dispatch_with_disposition("comm.read", json!({"id": inbound_id}), None)
                .await
        }
    });
    wait_until(Duration::from_secs(5), || {
        batch.test_snapshot().pending_rows == 1
    })
    .await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut read)
            .await
            .is_err(),
        "comm.read must still await its enqueued audit row after admission_deadline"
    );
    assert_eq!(audit_admission_refused_obligation_count(), before_refused);
    assert_eq!(
        audit_admission_unresolved_obligation_count(),
        before_unresolved
    );

    audit_store.append_release.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(5), &mut read)
        .await
        .expect("comm.read resolves after the held append releases")
        .expect("comm.read task joins")
        .expect("comm.read succeeds once its audit row commits");
    assert_eq!(result["status"], "success");
    assert_eq!(result["read"], true);
    assert_eq!(result["full_id"], inbound_id);

    let read_inbox = seed
        .dispatch("comm.inbox", json!({"status": "read"}))
        .await
        .expect("read-only inbox confirms the persisted mark");
    let messages = read_inbox["messages"]
        .as_array()
        .expect("inbox returns messages");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["full_id"], inbound_id);
    assert_eq!(messages[0]["read"], true);

    let events = audit_store.events.lock().unwrap();
    let read_events: Vec<_> = events
        .iter()
        .filter(|event| event.verb == "comm.read")
        .collect();
    assert_eq!(read_events.len(), 1, "comm.read must commit one audit row");
    assert_eq!(read_events[0].kind, EventKind::Audit);
    assert_eq!(read_events[0].outcome, EventOutcome::Success);
    assert_eq!(audit_admission_refused_obligation_count(), before_refused);
    assert_eq!(
        audit_admission_unresolved_obligation_count(),
        before_unresolved
    );
    occupant.abort();
}
