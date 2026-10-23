//! Success-audit acceptance through production handlers and stored-event readers.
#![cfg(unix)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rmcp::{
    model::{CallToolRequestParams, ClientInfo},
    ClientHandler, ServiceExt,
};
#[derive(Clone, Default)]
struct ReaderClient;
impl ClientHandler for ReaderClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }
}
use khive_mcp::server::KhiveMcpServer;
use khive_mcp::tools::request::RequestParams;
use khive_pack_brain::BrainPack;
use khive_pack_comm::CommPack;
use khive_pack_kg::KgPack;
use khive_runtime::pack::{AuditSuccessRowsMode, RequestIdentity};
use khive_runtime::{
    KhiveRuntime, Namespace, Resolved, RuntimeConfig, RuntimeError, VerbRegistry,
    VerbRegistryBuilder,
};
use khive_storage::event::{EventGroupBy, EventPageQuery, IdempotentEventBatchResult};
use khive_storage::note::{Note, NoteFilter};
use khive_storage::{
    BatchWriteSummary, Event, EventFilter, EventStore, Page, PageRequest, StorageError,
    StorageResult,
};
use khive_types::{EventKind, EventOutcome, SubstrateKind};
use serde_json::{json, Value};
use uuid::Uuid;

const HEARTBEAT: &str = "comm.heartbeat";
const CLEANUP: &str = "comm.cleanup_expired_quarantine";
const SELECTED: [u64; 8] = [0, 2, 8, 12, 34, 47, 77, 90];
const CHILD: &str = "KHIVE_SUCCESS_AUDIT_ACCEPTANCE_CHILD";

fn isolated(name: &str, cases: &[&str]) -> Option<String> {
    if std::env::var(CHILD).ok().as_deref() == Some(name) {
        return Some(std::env::var("KHIVE_SUCCESS_AUDIT_CASE").unwrap());
    }
    for case in cases {
        let directory = tempfile::Builder::new()
            .prefix("kh-as-")
            .tempdir_in("/tmp")
            .unwrap();
        let log = directory.path().join("output");
        let output = std::fs::File::create(&log).unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("KHIVE_") {
                command.env_remove(key);
            }
        }
        let mut child = command
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env(CHILD, name)
            .env("KHIVE_SUCCESS_AUDIT_CASE", case)
            .env("KHIVE_TEST_HARNESS", "1")
            .env("KHIVE_EVENTS_SPLIT", "0")
            .env("KHIVE_SUCCESS_AUDIT_ROOT", directory.path())
            .env("KHIVE_SOCKET", directory.path().join("s"))
            .env("KHIVE_PID", directory.path().join("p"))
            .env("KHIVE_LOCK", directory.path().join("l"))
            .env("KHIVE_RECOVERER_LOCK", directory.path().join("r"))
            .stdin(Stdio::null())
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(120);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "native fixture deadline: {}",
                    std::fs::read_to_string(&log).unwrap()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let output = std::fs::read_to_string(log).unwrap();
        assert!(
            status.success()
                && output.contains("1 passed")
                && output.contains("SUCCESS_AUDIT_ACCEPTANCE_VERIFIED"),
            "{name}/{case}: {output}"
        );
    }
    None
}
fn run(future: impl std::future::Future<Output = ()>) {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(future);
    println!("SUCCESS_AUDIT_ACCEPTANCE_VERIFIED");
}

struct AuditSink {
    inner: Arc<dyn EventStore>,
    fail: AtomicBool,
}
#[async_trait]
impl EventStore for AuditSink {
    async fn append_event(&self, event: Event) -> StorageResult<()> {
        self.inner.append_event(event).await
    }
    async fn append_events(&self, events: Vec<Event>) -> StorageResult<BatchWriteSummary> {
        self.inner.append_events(events).await
    }
    async fn get_event(&self, id: Uuid) -> StorageResult<Option<Event>> {
        self.inner.get_event(id).await
    }
    async fn query_events(&self, f: EventFilter, p: PageRequest) -> StorageResult<Page<Event>> {
        self.inner.query_events(f, p).await
    }
    async fn count_events(&self, f: EventFilter) -> StorageResult<u64> {
        self.inner.count_events(f).await
    }
    fn preflight_event(&self, e: &Event) -> StorageResult<()> {
        self.inner.preflight_event(e)
    }
    fn supports_idempotent_audit_batch(&self) -> bool {
        true
    }
    async fn append_events_idempotent(
        &self,
        events: Vec<Event>,
    ) -> StorageResult<IdempotentEventBatchResult> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(StorageError::Internal(
                "injected audit persistence failure".into(),
            ));
        }
        self.inner.append_events_idempotent(events).await
    }
}
struct Fixture {
    runtime: KhiveRuntime,
    registry: VerbRegistry,
    sink: Arc<AuditSink>,
    server: KhiveMcpServer,
}
fn fixture(mode: AuditSuccessRowsMode) -> Fixture {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let sink = Arc::new(AuditSink {
        inner: runtime.events(&token).unwrap(),
        fail: AtomicBool::new(false),
    });
    let registry = registry(&runtime, mode, Some(sink.clone()));
    let server = KhiveMcpServer::from_registry(registry.clone());
    Fixture {
        runtime,
        registry,
        sink,
        server,
    }
}
fn registry(
    runtime: &KhiveRuntime,
    mode: AuditSuccessRowsMode,
    sink: Option<Arc<dyn EventStore>>,
) -> VerbRegistry {
    let mut b = VerbRegistryBuilder::new();
    b.with_audit_success_rows_mode(mode)
        .with_actor_id(runtime.config().actor_id.clone())
        .with_visible_namespaces(runtime.config().visible_namespaces.clone());
    b.register_trusted(KgPack::new(runtime.clone()))
        .register_trusted(CommPack::new(runtime.clone()))
        .register_trusted(BrainPack::new(runtime.clone()));
    if let Some(sink) = sink {
        b.with_event_store(sink);
    } else {
        b.with_runtime_event_store(runtime).unwrap();
    }
    let registry = b.build().unwrap();
    registry
        .apply_schema_plans_with_map(&HashMap::new(), runtime.backend())
        .unwrap();
    runtime.install_edge_rules(registry.all_edge_rules());
    registry
}
fn args(verb: &str) -> Value {
    match verb {
        HEARTBEAT => {
            json!({"channel_kind":"email","channel_slug":"acceptance","outcome":"success"})
        }
        CLEANUP => json!({"channel_kind":"email","channel_slug":"acceptance"}),
        "comm.probe" => json!({"actor":"local"}),
        "comm.cursor_get" => json!({"channel_kind":"email","channel_slug":"acceptance"}),
        _ => json!({}),
    }
}
async fn invoke(registry: &VerbRegistry, verb: &str, args: Value, id: u64) -> Value {
    registry
        .dispatch_with_identity(
            verb,
            args,
            Some(RequestIdentity {
                namespace: "local".into(),
                request_id: Some(id),
                ..RequestIdentity::default()
            }),
        )
        .await
        .unwrap()
}
async fn envelope(server: &KhiveMcpServer, verb: &str, args: Value) -> Value {
    let text = server
        .dispatch_request_local(RequestParams {
            ops: json!([{"tool":verb,"args":args}]).to_string(),
            presentation: Some("verbose".into()),
            format: Some("json".into()),
            ..RequestParams::default()
        })
        .await
        .unwrap();
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["results"][0]["ok"], true, "{body}");
    body["results"][0]["result"].clone()
}
async fn rows(f: &Fixture) -> Vec<Event> {
    f.sink
        .query_events(
            EventFilter {
                kinds: vec![EventKind::Audit],
                ..Default::default()
            },
            PageRequest {
                offset: 0,
                limit: 1000,
            },
        )
        .await
        .unwrap()
        .items
}
fn field<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    if let Some(value) = value.get(key) {
        return Some(value);
    }
    match value {
        Value::Object(m) => m.values().find_map(|v| field(v, key)),
        Value::Array(a) => a.iter().find_map(|v| field(v, key)),
        _ => None,
    }
}
async fn diagnostics(f: &Fixture) -> Value {
    envelope(
        &f.server,
        "db_diagnostics",
        json!({"size_composition":false}),
    )
    .await
}
fn counter(value: &Value, kind: &str, verb: &str) -> u64 {
    field(value, kind).expect("available decision map")[verb]
        .as_u64()
        .unwrap()
}
fn sampling(row: &Event) -> &Value {
    &row.payload["resource"]["sampling"]
}
async fn health(f: &Fixture) -> Vec<Note> {
    let token = f.runtime.authorize(Namespace::local()).unwrap();
    f.runtime
        .notes(&token)
        .unwrap()
        .query_notes_filtered_count_free(
            "local",
            &NoteFilter {
                kind: Some("channel_health".into()),
                ..Default::default()
            },
            PageRequest {
                offset: 0,
                limit: 100,
            },
        )
        .await
        .unwrap()
        .items
}

#[test]
fn real_poll_state_changes_force_audit_rows() {
    if isolated("real_poll_state_changes_force_audit_rows", &["effects"]).is_none() {
        return;
    }
    run(async {
        let f = fixture(AuditSuccessRowsMode::Reduced);
        invoke(&f.registry, HEARTBEAT, args(HEARTBEAT), 1).await;
        let failure = json!({"channel_kind":"email","channel_slug":"acceptance","outcome":"failure","error_class":"temporary","error_message":"connection unavailable"});
        let result = invoke(&f.registry, HEARTBEAT, failure, 2).await;
        assert_eq!(result["ok"], true);
        assert_eq!(result["outcome"], "failure");
        let stored = health(&f).await;
        assert_eq!(stored.len(), 1);
        let props = stored[0].properties.as_ref().unwrap();
        assert_eq!(props["consecutive_failures"], 1);
        assert_eq!(props["last_error"]["class"], "temporary");
        invoke(&f.registry, CLEANUP, args(CLEANUP), 3).await;
        let token = f.runtime.authorize(Namespace::local()).unwrap();
        let mut note = Note::new("local", "message", "expired quarantine").with_properties(
            json!({"channel_kind":"email","channel_slug":"acceptance","quarantined":true}),
        );
        note.expires_at = Some(1);
        let id = note.id;
        f.runtime
            .backend()
            .notes()
            .unwrap()
            .upsert_note(note)
            .await
            .unwrap();
        let result = invoke(
            &f.registry,
            CLEANUP,
            json!({"channel_kind":"email","channel_slug":"acceptance","as_of_micros":100}),
            4,
        )
        .await;
        assert_eq!(result["deleted"], 1);
        assert!(f
            .runtime
            .notes(&token)
            .unwrap()
            .get_note_including_deleted(id)
            .await
            .unwrap()
            .is_none());
        let rows = rows(&f).await;
        assert_eq!(rows.len(), 4, "one row per selected or forced success");
        assert_eq!(
            rows.iter()
                .map(|e| e.payload["resource"]["request_id"].as_u64().unwrap())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([1, 2, 3, 4])
        );
        for id in [2, 4] {
            let row = rows
                .iter()
                .find(|e| e.payload["resource"]["request_id"] == id)
                .unwrap();
            assert_eq!(
                sampling(row),
                &json!({"every":10,"seq":1,"selected_by":"forced"})
            );
        }
        let d = diagnostics(&f).await;
        assert_eq!(counter(&d, "audit_success_rows_forced", HEARTBEAT), 1);
        assert_eq!(counter(&d, "audit_success_rows_forced", CLEANUP), 1);
    });
}

#[test]
fn real_poll_errors_keep_audits_without_sequences() {
    if isolated(
        "real_poll_errors_keep_audits_without_sequences",
        &["errors"],
    )
    .is_none()
    {
        return;
    }
    run(async {
        let f = fixture(AuditSuccessRowsMode::Reduced);
        for verb in [
            "comm.inbox",
            "comm.probe",
            "comm.cursor_get",
            HEARTBEAT,
            CLEANUP,
        ] {
            assert!(
                f.registry
                    .dispatch(verb, json!({"unknown_argument":true}))
                    .await
                    .is_err(),
                "{verb}"
            );
        }
        let stored = rows(&f).await;
        assert_eq!(stored.len(), 5);
        for row in stored {
            assert_eq!(row.outcome, EventOutcome::Error);
            assert!(sampling(&row).is_null());
            assert!(row.payload["error_class"].is_string());
        }
        for (id, verb) in [(10, HEARTBEAT), (11, CLEANUP)] {
            invoke(&f.registry, verb, args(verb), id).await;
            let stored = rows(&f).await;
            let row = stored
                .iter()
                .find(|e| e.payload["resource"]["request_id"] == id)
                .unwrap();
            assert_eq!(sampling(row)["seq"], 0);
        }
    });
}

#[test]
fn audit_success_rows_diagnostics_count_decisions() {
    if isolated(
        "audit_success_rows_diagnostics_count_decisions",
        &["counts"],
    )
    .is_none()
    {
        return;
    }
    run(async {
        let f = fixture(AuditSuccessRowsMode::Reduced);
        let before = diagnostics(&f).await;
        assert_eq!(before["audit_success_rows_mode"], "reduced");
        for (id, verb) in ["comm.inbox", "comm.probe", "comm.cursor_get", "whoami"]
            .into_iter()
            .enumerate()
        {
            invoke(&f.registry, verb, args(verb), id as u64).await;
        }
        let after_omitted = diagnostics(&f).await;
        for verb in ["comm.inbox", "comm.probe", "comm.cursor_get", "whoami"] {
            assert_eq!(
                counter(&after_omitted, "audit_success_rows_omitted", verb)
                    - counter(&before, "audit_success_rows_omitted", verb),
                1
            );
        }
        assert_eq!(
            field(&before, "audit_admission_refused_obligations")
                .and_then(Value::as_u64)
                .expect("configured batch admission count"),
            field(&after_omitted, "audit_admission_refused_obligations")
                .and_then(Value::as_u64)
                .expect("configured batch admission count")
        );
        for n in 0..100 {
            invoke(&f.registry, HEARTBEAT, args(HEARTBEAT), 1000 + n).await;
        }
        let stored = rows(&f).await;
        let selected: BTreeSet<_> = stored
            .iter()
            .filter(|e| e.verb == HEARTBEAT)
            .map(|e| sampling(e)["seq"].as_u64().unwrap())
            .collect();
        assert_eq!(selected, SELECTED.into_iter().collect());
        let after = diagnostics(&f).await;
        assert_eq!(
            counter(&after, "audit_success_rows_sampled_out", HEARTBEAT)
                - counter(&before, "audit_success_rows_sampled_out", HEARTBEAT),
            92
        );
        // Cleanup has an independent sequence: failure-shaped success at selected0 wins the sample label.
        for seq in 0..3 {
            let mut note = Note::new("local", "message", format!("expired {seq}")).with_properties(
                json!({"channel_kind":"email","channel_slug":"acceptance","quarantined":true}),
            );
            note.expires_at = Some(1);
            f.runtime
                .backend()
                .notes()
                .unwrap()
                .upsert_note(note)
                .await
                .unwrap();
            if seq == 2 {
                f.sink.fail.store(true, Ordering::SeqCst);
            }
            let result = f
                .registry
                .dispatch_with_disposition(
                    CLEANUP,
                    json!({"channel_kind":"email","channel_slug":"acceptance","as_of_micros":100}),
                    None,
                )
                .await;
            if seq == 2 {
                let error = result.unwrap_err();
                assert_eq!(
                    error.disposition(),
                    khive_runtime::DomainDisposition::Committed
                );
                assert!(matches!(
                    error.source(),
                    RuntimeError::AuditObligation { .. }
                ));
            } else {
                assert_eq!(result.unwrap()["deleted"], 1);
            }
        }
        f.sink.fail.store(false, Ordering::SeqCst);
        let d = diagnostics(&f).await;
        // seq2 is selected too; use the next clean skipped slot for a genuine failed force-only append.
        let mut note = Note::new("local", "message", "expired failed force").with_properties(
            json!({"channel_kind":"email","channel_slug":"acceptance","quarantined":true}),
        );
        note.expires_at = Some(1);
        f.runtime
            .backend()
            .notes()
            .unwrap()
            .upsert_note(note)
            .await
            .unwrap();
        f.sink.fail.store(true, Ordering::SeqCst);
        assert!(f
            .registry
            .dispatch(
                CLEANUP,
                json!({"channel_kind":"email","channel_slug":"acceptance","as_of_micros":100})
            )
            .await
            .is_err());
        f.sink.fail.store(false, Ordering::SeqCst);
        let final_d = diagnostics(&f).await;
        assert_eq!(counter(&d, "audit_success_rows_forced", CLEANUP), 1);
        assert_eq!(
            counter(&final_d, "audit_success_rows_forced", CLEANUP),
            2,
            "failed persistence retains the force decision"
        );
        let all_runtime = KhiveRuntime::memory().unwrap();
        let all_registry = registry(&all_runtime, AuditSuccessRowsMode::All, None);
        for verb in [
            "comm.inbox",
            "comm.probe",
            "comm.cursor_get",
            "whoami",
            HEARTBEAT,
            CLEANUP,
        ] {
            invoke(&all_registry, verb, args(verb), 9000).await;
        }
        let all = KhiveMcpServer::from_registry(all_registry);
        let all_d = envelope(&all, "db_diagnostics", json!({"size_composition":false})).await;
        assert_eq!(all_d["audit_success_rows_mode"], "all");
        for key in [
            "audit_success_rows_omitted",
            "audit_success_rows_sampled_out",
            "audit_success_rows_forced",
        ] {
            assert_eq!(
                field(&all_d, key),
                field(&final_d, key),
                "All adds no decisions and retains accumulated process counts"
            );
        }
    });
}

fn reader_fixture() -> Vec<Event> {
    let mut rows = Vec::new();
    for namespace in ["local", "research"] {
        for actor in ["reader:a", "reader:b"] {
            for verb in [HEARTBEAT, CLEANUP] {
                for object in [
                    None,
                    Some(json!({"every":10,"seq":0,"selected_by":"sample"})),
                    Some(json!({"every":10,"seq":1,"selected_by":"forced"})),
                    Some(Value::Null),
                ] {
                    let mut resource = json!({"request_id":rows.len() as u64,"units":{"db_round_trips":3},"cost_unit":1});
                    if let Some(object) = object {
                        resource["sampling"] = object;
                    }
                    let mut event = Event::new(
                        namespace,
                        verb,
                        EventKind::Audit,
                        SubstrateKind::Event,
                        actor,
                    )
                    .with_duration_us(7)
                    .with_payload(json!({"resource":resource,"marker":rows.len()}));
                    event.created_at = 10 + rows.len() as i64;
                    rows.push(event);
                }
            }
        }
    }
    rows
}
fn by_id(rows: Vec<Event>) -> BTreeMap<Uuid, Event> {
    rows.into_iter().map(|e| (e.id, e)).collect()
}

#[test]
fn sampled_audit_readers_preserve_rows_and_ids() {
    let Some(case) = isolated(
        "sampled_audit_readers_preserve_rows_and_ids",
        &["local", "split"],
    ) else {
        return;
    };
    run(async move {
        let directory =
            std::path::PathBuf::from(std::env::var("KHIVE_SUCCESS_AUDIT_ROOT").unwrap());
        let split = case == "split";
        let socket = directory.join("e");
        let db = directory.join("events.db");
        let daemon = if split {
            let db = db.clone();
            let daemon_socket = socket.clone();
            let task = tokio::spawn(async move {
                khive_runtime::events_split::run_events_daemon(&db, &daemon_socket)
                    .await
                    .unwrap()
            });
            tokio::time::timeout(Duration::from_secs(10), async {
                while !socket.exists() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            Some(task)
        } else {
            None
        };
        let mut config = RuntimeConfig {
            db_path: Some(directory.join("main.db")),
            actor_id: Some("reader:a".into()),
            visible_namespaces: vec![Namespace::parse("research").unwrap()],
            brain_profile: None,
            packs: vec!["kg".into(), "comm".into(), "brain".into()],
            events_split: if split {
                Some(khive_runtime::events_split::EventsSplitConfig {
                    db_path: db,
                    socket_path: Some(socket.clone()),
                })
            } else {
                None
            },
            ..RuntimeConfig::no_embeddings()
        };
        config.brain.fleet_readers = vec!["reader:a".into()];
        let runtime = KhiveRuntime::new_for_test(config).unwrap();
        let registry = registry(&runtime, AuditSuccessRowsMode::All, None);
        let expected = reader_fixture();
        assert_eq!(expected.len(), 32);
        for (index, event) in expected.iter().enumerate() {
            if split && index % 2 == 1 {
                let lane = khive_runtime::events_split::ForwardingEventStore::new(
                    event.namespace.clone(),
                    khive_runtime::events_split::client_for(&socket).unwrap(),
                );
                let result = lane
                    .append_events_idempotent(vec![event.clone()])
                    .await
                    .unwrap();
                assert_eq!(result.rows.len(), 1);
            } else {
                runtime
                    .backend()
                    .events_for_namespace(&event.namespace)
                    .unwrap()
                    .append_event(event.clone())
                    .await
                    .unwrap();
            }
        }
        for namespace in ["local", "research"] {
            let counts=registry.dispatch_with_identity("brain.event_counts",json!({"namespace":namespace,"since":khive_runtime::micros_to_iso(0),"until":khive_runtime::micros_to_iso(100),"all_actors":true}),Some(RequestIdentity{namespace:namespace.into(),actor_id:Some("reader:a".into()),..RequestIdentity::default()})).await.unwrap();
            assert_eq!(
                counts["scope"]["namespace"], namespace,
                "event count scope must follow explicit namespace routing"
            );
            assert_eq!(counts["total"], 16);
            for verb in [HEARTBEAT, CLEANUP] {
                assert_eq!(
                    counts["sampled_success_rows"][verb],
                    json!({"rows":6,"sample_rows":2,"forced_rows":2,"unsampled_rows":2,"every":10})
                );
            }
            let token = runtime
                .authorize(Namespace::parse(namespace).unwrap())
                .unwrap();
            let store = runtime.events(&token).unwrap();
            let own: Vec<_> = expected
                .iter()
                .filter(|e| e.namespace == namespace)
                .cloned()
                .collect();
            let wanted = by_id(own.clone());
            let filter = EventFilter {
                after: Some(0),
                before: Some(100),
                ..Default::default()
            };
            assert_eq!(store.count_events(filter.clone()).await.unwrap(), 16);
            assert_eq!(
                store
                    .count_events_grouped(filter.clone(), EventGroupBy::Verb)
                    .await
                    .unwrap(),
                BTreeMap::from([(HEARTBEAT.into(), 8), (CLEANUP.into(), 8)])
            );
            assert_eq!(
                by_id(
                    store
                        .query_events(
                            filter.clone(),
                            PageRequest {
                                offset: 0,
                                limit: 100
                            }
                        )
                        .await
                        .unwrap()
                        .items
                ),
                wanted
            );
            let mut after = None;
            let mut paged = Vec::new();
            loop {
                let page = store
                    .query_event_page(EventPageQuery {
                        since_us: 0,
                        until_us: 100,
                        kinds: vec![],
                        actors: vec![],
                        exclude_namespaces: vec![],
                        after,
                        max_rows: 3,
                    })
                    .await
                    .unwrap();
                assert!(page.budget_stop.is_none());
                if page.rows.is_empty() {
                    break;
                }
                after = Some(page.rows.last().unwrap().order_key.clone());
                paged.extend(page.rows.into_iter().map(|r| r.event));
            }
            assert_eq!(by_id(paged), wanted);
            for event in &own {
                assert_eq!(
                    store.get_event(event.id).await.unwrap(),
                    Some(event.clone())
                );
                match runtime.resolve(&token, event.id).await.unwrap().unwrap() {
                    Resolved::Event(actual) => assert_eq!(actual, *event),
                    other => panic!("wrong substrate: {other:?}"),
                }
                let got = registry
                    .dispatch("get", json!({"id":event.id}))
                    .await
                    .unwrap();
                assert_eq!(got["id"], event.id.to_string());
                assert_eq!(got["payload"], event.payload);
            }
            let list = registry
                .dispatch_with_identity(
                    "list",
                    json!({"kind":"event","namespace":namespace,"since":0,"until":100,"limit":100}),
                    Some(RequestIdentity {
                        namespace: namespace.into(),
                        actor_id: Some("reader:a".into()),
                        ..RequestIdentity::default()
                    }),
                )
                .await
                .unwrap();
            let items = list
                .as_array()
                .or_else(|| list["items"].as_array())
                .expect("event list rows");
            assert_eq!(items.len(), 16);
            for item in items {
                assert_eq!(
                    item["namespace"], namespace,
                    "explicit routing must return the requested stored namespace"
                );
                let id = Uuid::parse_str(item["id"].as_str().unwrap()).unwrap();
                assert_eq!(item["payload"], wanted[&id].payload);
            }
        }
        let count = registry
            .dispatch(
                "count",
                json!({"kind":"event","since":0,"until":100,"group_by":"verb"}),
            )
            .await
            .unwrap();
        assert_eq!(count, json!({(HEARTBEAT):16,(CLEANUP):16}));
        // Public resolve retains its entity-reference contract. Event UUID resolution above is the runtime API.
        assert!(registry
            .dispatch("resolve", json!({"refs":[expected[0].id],"kind":"event"}))
            .await
            .is_err());
        // Graph query reads the legacy domain table; split audit rows are deliberately outside that substrate union.
        let query=registry.dispatch("query",json!({"query":"MATCH (e:event) WHERE e.created_at < 100 RETURN e.id, e.properties LIMIT 100"})).await.unwrap();
        let query_rows = query["rows"].as_array().unwrap();
        let legacy: Vec<_> = expected
            .iter()
            .enumerate()
            .filter(|(i, _)| !split || i % 2 == 0)
            .map(|(_, e)| e)
            .collect();
        assert_eq!(query_rows.len(), legacy.len());
        for row in query_rows {
            let id = Uuid::parse_str(row["e_id"].as_str().unwrap()).unwrap();
            let event = legacy.iter().find(|e| e.id == id).unwrap();
            let payload = if let Some(text) = row["e_properties"].as_str() {
                serde_json::from_str::<Value>(text).unwrap()
            } else {
                row["e_properties"].clone()
            };
            assert_eq!(payload, event.payload);
        }
        std::env::set_var("KHIVE_AUDIT_SUCCESS_ROWS", "all");
        let server = KhiveMcpServer::new(runtime.clone()).unwrap();
        let listener =
            tokio::net::UnixListener::bind(khive_runtime::daemon::socket_path()).unwrap();
        let connections = Arc::new(AtomicUsize::new(0));
        let observed = connections.clone();
        let serving = server.clone();
        let frames = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                observed.fetch_add(1, Ordering::SeqCst);
                khive_runtime::daemon::serve_connection_for_test(stream, serving.clone()).await;
            }
        });
        // This public MCP path must forward over the native main-daemon frame and then the split EventStore when enabled.
        let (server_transport, client_transport) = tokio::io::duplex(65536);
        let mcp_task = tokio::spawn(async move {
            let service = server.serve(server_transport).await.unwrap();
            service.waiting().await.unwrap();
        });
        let client = tokio::time::timeout(
            Duration::from_secs(15),
            ReaderClient.serve(client_transport),
        )
        .await
        .unwrap()
        .unwrap();
        let response=tokio::time::timeout(Duration::from_secs(15),client.call_tool(CallToolRequestParams::new("request").with_arguments(json!({"ops":json!([{"tool":"brain.event_page","args":{"since":khive_runtime::micros_to_iso(0),"until":khive_runtime::micros_to_iso(100),"namespaces":["local","research"],"all_actors":true,"limit":100}}]).to_string(),"presentation":"verbose","format":"json"}).as_object().unwrap().clone()))).await.unwrap().unwrap();
        let text = response.content[0].as_text().unwrap().text.clone();
        assert_eq!(connections.load(Ordering::SeqCst), 1);
        let body: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(body["results"][0]["ok"], true, "{body}");
        let events = body["results"][0]["result"]["events"].as_array().unwrap();
        assert_eq!(events.len(), 32);
        for event in events {
            let id = Uuid::parse_str(event["id"].as_str().unwrap()).unwrap();
            let original = expected.iter().find(|e| e.id == id).unwrap();
            assert_eq!(event["payload"], original.payload);
        }
        client.cancel().await.unwrap();
        tokio::time::timeout(Duration::from_secs(15), mcp_task)
            .await
            .unwrap()
            .unwrap();
        frames.abort();
        if let Some(daemon) = daemon {
            daemon.abort();
        }
    });
}
