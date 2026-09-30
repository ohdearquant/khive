use super::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use khive_gate::{Gate, GateDecision, GateError, GateRequest};
use khive_pack_kg::KgPack;
use khive_runtime::{Namespace, VerbRegistryBuilder};
use khive_storage::types::PageRequest;
use khive_storage::EventFilter;
use khive_types::{EventKind, SubstrateKind};

#[derive(Debug)]
struct DenyCheckGate;

impl Gate for DenyCheckGate {
    fn check(&self, request: &GateRequest) -> Result<GateDecision, GateError> {
        if request.verb == "tool.check" {
            Ok(GateDecision::deny("test gate denial"))
        } else {
            Ok(GateDecision::allow())
        }
    }
}

struct Fixture {
    runtime: KhiveRuntime,
    registry: VerbRegistry,
    token: NamespaceToken,
}

impl Fixture {
    fn new(deny_check: bool) -> Self {
        let runtime = KhiveRuntime::memory().expect("memory runtime");
        Self::with_runtime(runtime, deny_check, true)
    }

    fn with_runtime(runtime: KhiveRuntime, deny_check: bool, audit: bool) -> Self {
        let token = runtime
            .authorize(Namespace::parse("local").expect("test namespace"))
            .expect("namespace token");
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(runtime.clone()));
        builder.register(crate::ToolPack::new(runtime.clone()));
        if audit {
            builder.with_event_store(runtime.events(&token).expect("event store"));
        }
        if deny_check {
            builder.with_gate(Arc::new(DenyCheckGate));
        }
        let registry = builder.build().expect("registry builds");
        registry.apply_schema_plans(runtime.backend());
        runtime.install_edge_rules(registry.all_edge_rules());
        Self {
            runtime,
            registry,
            token,
        }
    }

    async fn decisions(&self, since: i64) -> Vec<Value> {
        let mut offset = 0_u32;
        let mut items = Vec::new();
        loop {
            let result = self
                .registry
                .dispatch(
                    "list",
                    json!({
                        "kind": "event",
                        "event_kinds": ["tool_check_decided"],
                        "verb": "tool.check",
                        "actor": format!("{}:{}", self.token.actor().kind, self.token.actor().id),
                        "since": since,
                        "limit": 20,
                        "offset": offset,
                    }),
                )
                .await
                .expect("list decision events");
            let page = result["items"].as_array().expect("list items");
            items.extend(page.iter().cloned());
            if !result["has_more"].as_bool().expect("has_more flag") {
                return items;
            }
            assert!(!page.is_empty(), "has_more requires a nonempty page");
            offset += u32::try_from(page.len()).expect("bounded page length");
        }
    }

    async fn set_policy(&self, tool: &str, decision: &str, replaces: Option<&str>) -> String {
        let mut params = json!({
            "actor": "agent:receipt",
            "tool": tool,
            "decision": decision,
        });
        if let Some(id) = replaces {
            params["replaces"] = json!(id);
        }
        let result = self
            .registry
            .dispatch("tool.policy", params)
            .await
            .expect("policy write");
        result["policy"]["id"]
            .as_str()
            .expect("policy id")
            .to_string()
    }
}

#[tokio::test]
async fn gate_refusal_has_no_decision_receipt_after_cursor() {
    let fixture = Fixture::new(true);
    let cursor = now_micros().saturating_sub(1);
    let error = fixture
        .registry
        .dispatch(
            "tool.check",
            json!({"tool": "unregistered", "actor": "agent:receipt"}),
        )
        .await
        .expect_err("gate must refuse before the handler");
    assert!(matches!(error, RuntimeError::PermissionDenied { .. }));
    assert!(fixture.decisions(cursor).await.is_empty());

    let audit = fixture
        .runtime
        .list_events(
            &fixture.token,
            EventFilter {
                kinds: vec![EventKind::Audit],
                verbs: vec!["tool.check".into()],
                ..EventFilter::default()
            },
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .expect("gate audit query");
    assert_eq!(audit.items.len(), 1);
}

#[tokio::test]
async fn allowed_gate_yields_one_typed_receipt_with_call_data() {
    let fixture = Fixture::new(false);
    let cursor = now_micros().saturating_sub(1);
    let result = fixture
        .registry
        .dispatch(
            "tool.check",
            json!({"tool": "unregistered", "actor": "agent:receipt"}),
        )
        .await
        .expect("tool.check");
    assert_eq!(result["decision"], "ask");
    let rows = fixture.decisions(cursor).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["kind"], "tool_check_decided");
    assert_eq!(rows[0]["namespace"], "local");
    let stored_actor = format!(
        "{}:{}",
        fixture.token.actor().kind,
        fixture.token.actor().id
    );
    assert_eq!(rows[0]["actor"].as_str(), Some(stored_actor.as_str()));
    assert_eq!(rows[0]["payload"]["actor"], "agent:receipt");
    assert_eq!(rows[0]["payload"]["tool"], "unregistered");
    assert_eq!(rows[0]["payload"]["registered"], false);
    assert_eq!(rows[0]["payload"]["decision"], "ask");
    assert_eq!(rows[0]["payload"]["source"], "default");
    assert_eq!(rows[0]["payload"]["id"], Value::Null);
    assert_eq!(rows[0]["payload"]["scope"], Value::Null);
    assert_eq!(rows[0]["payload"]["caller_verb"], "tool.check");
}

#[tokio::test]
async fn opposite_policy_sequences_are_distinguishable_from_rows() {
    let fixture = Fixture::new(false);
    let cursor = now_micros().saturating_sub(1);
    let first = fixture
        .registry
        .dispatch(
            "tool.policy",
            json!({"actor": "agent:receipt", "tool": "deny-first", "decision": "deny"}),
        )
        .await
        .expect("deny policy");
    fixture
        .registry
        .dispatch(
            "tool.check",
            json!({"tool": "deny-first", "actor": "agent:receipt"}),
        )
        .await
        .expect("first check");
    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    fixture
        .set_policy("deny-first", "allow", first["policy"]["id"].as_str())
        .await;
    fixture
        .registry
        .dispatch(
            "tool.check",
            json!({"tool": "deny-first", "actor": "agent:receipt"}),
        )
        .await
        .expect("second check");

    let allow_id = fixture.set_policy("allow-first", "allow", None).await;
    fixture
        .registry
        .dispatch(
            "tool.check",
            json!({"tool": "allow-first", "actor": "agent:receipt"}),
        )
        .await
        .expect("third check");
    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    let rows = fixture.decisions(cursor).await;
    assert_eq!(rows.len(), 3);
    fixture
        .set_policy("allow-first", "deny", Some(&allow_id))
        .await;
    fixture
        .registry
        .dispatch(
            "tool.check",
            json!({"tool": "allow-first", "actor": "agent:receipt"}),
        )
        .await
        .expect("fourth check");

    let rows = fixture.decisions(cursor).await;
    assert_eq!(rows.len(), 4);
    let sequence = |tool: &str| {
        rows.iter()
            .rev()
            .filter(|row| row["payload"]["tool"] == tool)
            .map(|row| row["payload"]["decision"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        sequence("deny-first"),
        vec!["deny".to_string(), "allow".to_string()]
    );
    assert_eq!(
        sequence("allow-first"),
        vec!["allow".to_string(), "deny".to_string()]
    );
}

#[tokio::test]
async fn same_microsecond_rows_list_newest_id_first() {
    let fixture = Fixture::new(false);
    let timestamp = now_micros();
    let events = fixture.runtime.events(&fixture.token).expect("event store");
    for id in [Uuid::from_u128(1), Uuid::from_u128(2)] {
        let mut event = khive_storage::Event::new(
            "local",
            "tool.check",
            EventKind::ToolCheckDecided,
            SubstrateKind::Event,
            actor_label(&fixture.token),
        )
        .with_payload(json!({"actor":"agent:receipt","tool":"tie"}));
        event.id = id;
        event.created_at = timestamp;
        events.append_event(event).await.expect("tie event append");
    }
    let page = fixture
        .runtime
        .list_events(
            &fixture.token,
            EventFilter {
                kinds: vec![EventKind::ToolCheckDecided],
                after: Some(timestamp.saturating_sub(1).max(0)),
                ..EventFilter::default()
            },
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .expect("tie page");
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.items[0].id, Uuid::from_u128(2));
    assert_eq!(page.items[1].id, Uuid::from_u128(1));
}

/// A credential-shaped value built at run time from parts.
fn credential_shaped() -> String {
    format!("{}{}", ["gh", "p_"].concat(), "A".repeat(36))
}

#[tokio::test]
async fn credential_shaped_check_input_leaves_no_receipt_carrying_it() {
    let secret = credential_shaped();
    for field in ["actor", "tool"] {
        let fixture = Fixture::new(false);
        let mut params = json!({"tool": "unregistered", "actor": "agent:receipt"});
        params[field] = json!(secret);

        let error = fixture
            .registry
            .dispatch("tool.check", params)
            .await
            .expect_err("credential-shaped input must be refused");
        assert!(
            matches!(error, RuntimeError::SecretDetected(_)),
            "{field}: {error:?}"
        );
        assert!(
            !error.to_string().contains(&secret),
            "{field}: the refusal must not echo the value"
        );

        let events = fixture
            .runtime
            .list_events(
                &fixture.token,
                EventFilter::default(),
                PageRequest {
                    limit: 100,
                    offset: 0,
                },
            )
            .await
            .expect("list all events");
        for event in &events.items {
            assert!(
                !serde_json::to_string(event)
                    .expect("event serializes")
                    .contains(&secret),
                "{field}: an event row stores the credential-shaped value"
            );
        }
        assert!(fixture.decisions(0).await.is_empty(), "{field}");
    }
}

/// Caller side of the catch-up procedure in the ADR: keep the exclusive floor
/// fixed and deduplicate every returned id for the observation horizon.
struct DecisionPoller {
    since: i64,
    seen: HashMap<Uuid, i64>,
}

impl DecisionPoller {
    fn new(since: i64) -> Self {
        Self {
            since,
            seen: HashMap::new(),
        }
    }

    async fn poll(&mut self, fixture: &Fixture) -> Vec<Uuid> {
        let since = self.since;
        let mut fresh = Vec::new();
        for row in fixture.decisions(since).await {
            let at = khive_runtime::rfc3339_to_utc_micros(row["created_at"].as_str().unwrap())
                .expect("created_at parses");
            let id = Uuid::parse_str(row["id"].as_str().unwrap()).expect("event id");
            if self.seen.insert(id, at).is_none() {
                fresh.push((at, id));
            }
        }
        fresh.sort();
        fresh.into_iter().map(|(_, id)| id).collect()
    }
}

fn receipt(fixture: &Fixture, id: Uuid, created_at: i64) -> khive_storage::Event {
    let mut event = khive_storage::Event::new(
        "local",
        "tool.check",
        EventKind::ToolCheckDecided,
        SubstrateKind::Event,
        actor_label(&fixture.token),
    )
    .with_payload(json!({"actor":"agent:receipt","tool":"late"}));
    event.id = id;
    event.created_at = created_at;
    event
}

async fn append_receipt_event(fixture: &Fixture, event: khive_storage::Event) {
    fixture
        .runtime
        .events(&fixture.token)
        .expect("event store")
        .append_event(event)
        .await
        .expect("receipt append");
}

async fn append_receipt(fixture: &Fixture, id: Uuid, created_at: i64) {
    append_receipt_event(fixture, receipt(fixture, id, created_at)).await;
}

#[tokio::test]
async fn late_arrival_sharing_the_cursor_microsecond_is_returned() {
    let fixture = Fixture::new(false);
    let timestamp = now_micros();
    let first = Uuid::from_u128(2);
    let mut poller = DecisionPoller::new(timestamp.saturating_sub(1));

    append_receipt(&fixture, first, timestamp).await;
    assert_eq!(poller.poll(&fixture).await, vec![first]);

    // Appended after the cursor was saved: same microsecond, lower id.
    let late = Uuid::from_u128(1);
    append_receipt(&fixture, late, timestamp).await;
    assert_eq!(poller.poll(&fixture).await, vec![late]);
    assert!(poller.poll(&fixture).await.is_empty());
}

#[tokio::test]
async fn receipt_visible_after_the_old_window_is_recovered() {
    let fixture = Fixture::new(false);
    let old_window = 60_000_000;
    let timestamp = now_micros();
    let delayed = Uuid::from_u128(4);
    let delayed_at = timestamp - old_window * 2;
    let mut poller = DecisionPoller::new(delayed_at.saturating_sub(1));
    let held_receipt = receipt(&fixture, delayed, delayed_at);
    let events = fixture.runtime.events(&fixture.token).expect("event store");
    let (release, wait_for_release) = tokio::sync::oneshot::channel();
    let (started, wait_for_start) = tokio::sync::oneshot::channel();
    let pending_append = tokio::spawn(async move {
        started.send(()).expect("signal held append started");
        wait_for_release.await.expect("release delayed receipt");
        events
            .append_event(held_receipt)
            .await
            .expect("delayed receipt append");
    });
    wait_for_start.await.expect("held append started");

    append_receipt(&fixture, Uuid::from_u128(5), timestamp).await;
    assert_eq!(poller.poll(&fixture).await, vec![Uuid::from_u128(5)]);

    // Hold the append before it enters storage to model delayed visibility.
    // Its timestamp is beyond the former 60-second cutoff, and the append
    // future stays pending longer than this short scan-lag window.
    let scan_lag_window = std::time::Duration::from_millis(5);
    let wait_started = std::time::Instant::now();
    tokio::time::sleep(scan_lag_window + std::time::Duration::from_millis(5)).await;
    assert!(
        wait_started.elapsed() > scan_lag_window,
        "the append must remain invisible for longer than W"
    );
    assert!(!pending_append.is_finished());
    assert!(poller.poll(&fixture).await.is_empty());
    release.send(()).expect("release pending append");
    pending_append.await.expect("delayed append task");
    assert_eq!(poller.poll(&fixture).await, vec![delayed]);
    assert!(poller.poll(&fixture).await.is_empty());
}

/// The receipt append enters the real file-backed event writer's queue while
/// another write occupies its only drain slot. The older timestamp survives
/// a poll that has already seen a later-stamped receipt.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn queued_decision_receipt_is_recovered_after_the_poll_window() {
    let dir = tempfile::tempdir().expect("temporary database directory");
    let backend = Arc::new(
        khive_db::StorageBackend::sqlite_for_test_with_journal_mode(
            dir.path().join("decision-receipt-admission.db"),
            true,
            Duration::from_secs(5),
        )
        .expect("file-backed writer queue"),
    );
    backend.prepare_core_schema().expect("core schema");
    let runtime =
        KhiveRuntime::from_backend(backend, khive_runtime::RuntimeConfig::no_embeddings());
    // Keep list polls read-only while the writer is intentionally occupied.
    let fixture = Fixture::with_runtime(runtime, false, false);
    let writer = fixture
        .runtime
        .backend()
        .pool()
        .writer_task_handle()
        .expect("writer task lookup")
        .expect("file-backed event writes must use a writer task");

    let window = Duration::from_millis(20);
    let delayed_id = Uuid::new_v4();
    let delayed_at = now_micros();
    let delayed = receipt(&fixture, delayed_id, delayed_at);
    let mut poller = DecisionPoller::new(delayed_at.saturating_sub(1));

    tokio::time::sleep(window + Duration::from_millis(10)).await;
    let newer_id = Uuid::new_v4();
    let newer_at = now_micros();
    assert!(
        newer_at - delayed_at > i64::try_from(window.as_micros()).expect("window fits i64"),
        "the delayed receipt must be older than the newest row minus the poll window"
    );
    append_receipt(&fixture, newer_id, newer_at).await;
    assert_eq!(poller.poll(&fixture).await, vec![newer_id]);
    // Mutation control: a since floor advanced to newest_at - window now
    // excludes delayed_at, so the post-release assertion below must fail.

    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    // The update commits before parking the drain slot, leaving WAL readers
    // and the list verb's idempotent schema check free to run during the wait.
    let occupier = {
        let writer = writer.clone();
        tokio::spawn(async move {
            writer
                .send_top_level(move |conn| {
                    let changed = conn
                        .execute(
                            "UPDATE events SET duration_us = duration_us + 1 WHERE id = ?1",
                            [newer_id.to_string()],
                        )
                        .expect("occupier updates the visible receipt");
                    assert_eq!(changed, 1);
                    started_tx.send(()).expect("signal occupied writer slot");
                    release_rx.blocking_recv().expect("release writer slot");
                    Ok::<(), khive_storage::StorageError>(())
                })
                .await
        })
    };
    started_rx
        .await
        .expect("writer must enter the occupied slot");

    let events = fixture.runtime.events(&fixture.token).expect("event store");
    let pending_append = tokio::spawn(async move { events.append_event(delayed).await });
    let queue_deadline = Instant::now() + Duration::from_secs(5);
    while writer.queue_depth() == 0 && Instant::now() < queue_deadline {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(
        writer.queue_depth() >= 1,
        "the delayed receipt must enter the event writer's queue"
    );
    assert!(
        delayed_at < now_micros(),
        "created_at must have been stamped before writer admission"
    );
    let queued_since = Instant::now();
    tokio::time::sleep(window + Duration::from_millis(10)).await;
    assert!(
        queued_since.elapsed() > window,
        "the receipt must wait in the writer queue longer than the poll window"
    );
    assert!(writer.queue_depth() >= 1, "the receipt is still queued");
    assert!(
        !pending_append.is_finished(),
        "the receipt has not appended"
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(5), poller.poll(&fixture))
            .await
            .expect("the read poll must finish while the writer is occupied")
            .is_empty(),
        "the poll during writer admission must not see the delayed receipt"
    );

    release_tx.send(()).expect("release occupied writer slot");
    occupier
        .await
        .expect("occupier task")
        .expect("occupier write");
    pending_append
        .await
        .expect("receipt task")
        .expect("receipt append");
    let stored = fixture
        .runtime
        .events(&fixture.token)
        .expect("event store")
        .get_event(delayed_id)
        .await
        .expect("stored receipt read")
        .expect("stored receipt");
    assert_eq!(stored.created_at, delayed_at);
    assert_eq!(poller.poll(&fixture).await, vec![delayed_id]);
    assert!(poller.poll(&fixture).await.is_empty());
}

#[tokio::test]
async fn poller_retains_ids_for_the_whole_observation_horizon() {
    let fixture = Fixture::new(false);
    let timestamp = now_micros();
    let old_window = 60_000_000;
    let older = timestamp - old_window * 10;
    let mut poller = DecisionPoller::new(older.saturating_sub(1));

    append_receipt(&fixture, Uuid::from_u128(7), older).await;
    append_receipt(&fixture, Uuid::from_u128(8), timestamp).await;
    assert_eq!(poller.poll(&fixture).await.len(), 2);
    assert_eq!(poller.seen.len(), 2);
    assert!(poller.seen.contains_key(&Uuid::from_u128(7)));
    assert!(poller.seen.contains_key(&Uuid::from_u128(8)));
    assert!(poller.poll(&fixture).await.is_empty());
}

#[tokio::test]
async fn poller_drains_every_page_from_the_fixed_floor() {
    let fixture = Fixture::new(false);
    let timestamp = now_micros();
    let mut poller = DecisionPoller::new(timestamp.saturating_sub(1));
    for id in 1..=25 {
        append_receipt(&fixture, Uuid::from_u128(id), timestamp).await;
    }
    assert_eq!(poller.poll(&fixture).await.len(), 25);
    assert!(poller.poll(&fixture).await.is_empty());
}
