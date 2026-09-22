//! Actual composed-request execution through SQL-backed audit and domain events.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use khive_runtime::{
    KhiveRuntime, Namespace, PackRuntime, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::event::IdempotentEventBatchResult;
use khive_storage::{
    BatchWriteSummary, Event, EventFilter, EventStore, Page, PageRequest, StorageResult,
};
use khive_types::{EventKind, HandlerDef, RefResolution, SubstrateKind};
use serde_json::{json, Value};
use tokio::sync::Notify;
use uuid::Uuid;

use super::KhiveMcpServer;
use crate::tools::request::RequestParams;

const PROBE: &str = "attribution.probe";
const DOMAIN: &str = "attribution.domain";

/// The first operation stays blocked until the second audit is committed.
/// The release key is its target UUID, deliberately independent of op_index
/// so an insertion-order mutant produces a failing assertion rather than a hang.
struct OrderedAuditStore {
    inner: Arc<dyn EventStore>,
    second: Uuid,
    release_first: Arc<Notify>,
    committed: Mutex<Vec<Uuid>>,
}

#[async_trait]
impl EventStore for OrderedAuditStore {
    async fn append_event(&self, event: Event) -> StorageResult<()> {
        self.inner.append_event(event).await
    }

    async fn append_events(&self, events: Vec<Event>) -> StorageResult<BatchWriteSummary> {
        self.inner.append_events(events).await
    }

    async fn get_event(&self, id: Uuid) -> StorageResult<Option<Event>> {
        self.inner.get_event(id).await
    }

    async fn query_events(
        &self,
        filter: EventFilter,
        page: PageRequest,
    ) -> StorageResult<Page<Event>> {
        self.inner.query_events(filter, page).await
    }

    async fn count_events(&self, filter: EventFilter) -> StorageResult<u64> {
        self.inner.count_events(filter).await
    }

    fn preflight_event(&self, event: &Event) -> StorageResult<()> {
        self.inner.preflight_event(event)
    }

    async fn append_events_idempotent(
        &self,
        events: Vec<Event>,
    ) -> StorageResult<IdempotentEventBatchResult> {
        let targets: Vec<Uuid> = events
            .iter()
            .filter(|event| event.verb == PROBE)
            .filter_map(|event| event.target_id)
            .collect();
        let result = self.inner.append_events_idempotent(events).await?;
        for target in targets {
            self.committed.lock().unwrap().push(target);
            if target == self.second {
                self.release_first.notify_one();
            }
        }
        Ok(result)
    }

    fn supports_idempotent_audit_batch(&self) -> bool {
        self.inner.supports_idempotent_audit_batch()
    }
}

struct ProbePack {
    runtime: KhiveRuntime,
    blocked_target: Option<Uuid>,
    release_first: Arc<Notify>,
}

impl khive_types::Pack for ProbePack {
    const NAME: &'static str = "attribution";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
        name: PROBE,
        description: "emit a domain event and return its target",
        visibility: khive_types::Visibility::Verb,
        category: khive_types::VerbCategory::Commissive,
        params: &[khive_types::ParamDef {
            name: "target_id",
            param_type: "string",
            required: true,
            description: "target identifying one fixture operation",
            resolution_mode: khive_types::IdResolutionMode::NotApplicable,
        }],
    }];
}

#[async_trait]
impl PackRuntime for ProbePack {
    fn name(&self) -> &str {
        <Self as khive_types::Pack>::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        <Self as khive_types::Pack>::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        <Self as khive_types::Pack>::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        <Self as khive_types::Pack>::HANDLERS
    }

    async fn dispatch(
        &self,
        _verb: &str,
        params: Value,
        _registry: &VerbRegistry,
        token: &khive_runtime::NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        let target = Uuid::parse_str(params["target_id"].as_str().expect("fixture target"))
            .expect("fixture UUID");
        if self.blocked_target == Some(target) {
            self.release_first.notified().await;
        }
        self.runtime
            .events(token)?
            .append_event(
                Event::new(
                    token.namespace().as_str(),
                    DOMAIN,
                    EventKind::Audit,
                    SubstrateKind::Event,
                    "fixture",
                )
                .with_target(target),
            )
            .await?;
        Ok(json!({"id": target}))
    }
}

fn fixture(blocked_target: Option<Uuid>, second: Uuid) -> (KhiveMcpServer, Arc<OrderedAuditStore>) {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let token = runtime
        .authorize(Namespace::local())
        .expect("fixture token");
    let release_first = Arc::new(Notify::new());
    let store = Arc::new(OrderedAuditStore {
        inner: runtime.events(&token).expect("SQL event store"),
        second,
        release_first: release_first.clone(),
        committed: Mutex::new(Vec::new()),
    });
    let mut builder = VerbRegistryBuilder::new();
    builder.with_default_namespace("local".to_string());
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(ProbePack {
        runtime,
        blocked_target,
        release_first,
    });
    builder.with_event_store(store.clone());
    (
        KhiveMcpServer::from_registry(builder.build().expect("fixture registry")),
        store,
    )
}

async fn run(server: &KhiveMcpServer, ops: String, count: u64) {
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        server.dispatch_request_local(RequestParams {
            ops,
            request_id: Some(2049),
            presentation: Some("verbose".into()),
            ..Default::default()
        }),
    )
    .await
    .expect("deterministic request fixture must finish")
    .expect("request dispatch");
    let value: Value = serde_json::from_str(&result).expect("JSON response");
    assert_eq!(value["summary"]["succeeded"], count, "{value}");
}

async fn rows(store: &dyn EventStore, verb: &str) -> Vec<Event> {
    store
        .query_events(
            EventFilter {
                verbs: vec![verb.into()],
                ..Default::default()
            },
            PageRequest {
                limit: 100,
                offset: 0,
            },
        )
        .await
        .expect("read event rows")
        .items
}

fn assert_fields(event: &Event, index: u32, resolution: RefResolution) {
    assert_eq!(event.op_index, Some(index));
    assert_eq!(event.ref_resolution, Some(resolution));
    if event.verb == PROBE {
        // This is the serialized AuditEvent carrier, independent of the SQL
        // columns exposed by the event query surface.
        assert_eq!(event.payload["op_index"], index);
        assert_eq!(event.payload["ref_resolution"], resolution.name());
        assert_eq!(event.payload["resource"]["request_id"], 2049);
    }
}

#[tokio::test]
async fn batch_operation_attribution_uses_parser_position_when_second_commits_first() {
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let (server, store) = fixture(Some(first), second);
    run(
        &server,
        format!(r#"[{PROBE}(target_id="{first}"), {PROBE}(target_id="{second}")]"#),
        2,
    )
    .await;
    assert_eq!(*store.committed.lock().unwrap(), vec![second, first]);
    for verb in [PROBE, DOMAIN] {
        let events = rows(store.as_ref(), verb).await;
        assert_eq!(events.len(), 2);
        for event in events {
            let index = if event.target_id == Some(first) {
                0
            } else {
                assert_eq!(event.target_id, Some(second));
                1
            };
            assert_fields(&event, index, RefResolution::Literal);
        }
    }
}

#[tokio::test]
async fn chain_operation_attribution_marks_only_consumed_references_resolved() {
    let target = Uuid::new_v4();
    let (server, store) = fixture(None, Uuid::new_v4());
    run(&server, format!(r#"{PROBE}(target_id="{target}") | {PROBE}(target_id=$prev.id) | {PROBE}(target_id="{target}")"#), 3).await;
    for verb in [PROBE, DOMAIN] {
        let mut events = rows(store.as_ref(), verb).await;
        events.sort_by_key(|event| event.op_index);
        assert_eq!(events.len(), 3);
        assert_fields(&events[0], 0, RefResolution::Literal);
        assert_fields(&events[1], 1, RefResolution::Resolved);
        assert_fields(&events[2], 2, RefResolution::Literal);
    }
}

#[tokio::test]
async fn single_operation_and_direct_event_have_distinct_provenance_in_same_store() {
    let target = Uuid::new_v4();
    let (server, store) = fixture(None, Uuid::new_v4());
    let direct = Event::new(
        "local",
        DOMAIN,
        EventKind::Audit,
        SubstrateKind::Event,
        "fixture",
    );
    assert_eq!((direct.op_index, direct.ref_resolution), (None, None));
    store.append_event(direct.clone()).await.unwrap();
    run(&server, format!(r#"{PROBE}(target_id="{target}")"#), 1).await;
    let audit = rows(store.as_ref(), PROBE).await;
    assert_eq!(audit.len(), 1);
    assert_fields(&audit[0], 0, RefResolution::Literal);
    let stored_direct = store.get_event(direct.id).await.unwrap().unwrap();
    assert_eq!(
        (stored_direct.op_index, stored_direct.ref_resolution),
        (None, None)
    );
    let domain = rows(store.as_ref(), DOMAIN).await;
    assert_eq!(domain.len(), 2);
    assert_fields(
        domain
            .iter()
            .find(|event| event.target_id == Some(target))
            .unwrap(),
        0,
        RefResolution::Literal,
    );

    // ADR-022 projects the fields without a new filtering contract.
    let listed = server
        .registry
        .dispatch("list", json!({"kind": "event", "verb": PROBE}))
        .await
        .unwrap();
    let item = &listed["items"][0];
    assert_eq!(item["op_index"], 0);
    assert_eq!(item["ref_resolution"], "literal");
    let got = server
        .registry
        .dispatch("get", json!({"id": audit[0].id}))
        .await
        .unwrap();
    assert_eq!(got["op_index"], 0);
    assert_eq!(got["ref_resolution"], "literal");
}
