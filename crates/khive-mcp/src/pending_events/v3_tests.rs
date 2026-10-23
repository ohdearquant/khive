use super::*;
use khive_runtime::{
    Gate, GateDecision, GateError, GateRequest, NamespaceToken, PackRuntime, RuntimeConfig,
    RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_types::{HandlerDef, IdResolutionMode, Pack, ParamDef, VerbCategory, Visibility};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

const NS: &str = "v3-fixture";
const ACTOR: &str = "lambda:v3-fixture";
const DAEMON_ACTOR: &str = "lambda:v3-daemon";
const DAEMON_NS: &str = "daemon-v3";
const ROOT: &str = "KHIVE_D8_MCP_ROOT";
const CASE: &str = "KHIVE_D8_MCP_CASE";
const PHASE: &str = "KHIVE_D8_MCP_PHASE";

fn child(name: &str, root: &Path, phase: Option<&str>) -> Output {
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let locks = root.join("locks");
    std::fs::create_dir_all(&locks).unwrap();
    let writer_mode = std::env::var("KHIVE_WRITE_QUEUE")
        .ok()
        .filter(|mode| mode == "0" || mode == "1");
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args([
        "--exact",
        &format!("pending_events::v3::tests::{name}"),
        "--nocapture",
        "--test-threads=1",
    ]);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("KHIVE_") || key == "LATTICE_MODEL_CACHE" {
            command.env_remove(key);
        }
    }
    command
        .env("HOME", home)
        .env("KHIVE_TEST_HARNESS", "1")
        .env("KHIVE_VOLUME_LOCK_DIR", locks)
        .env("KHIVE_LOCK", root.join("daemon.lock"))
        .env("KHIVE_RECOVERER_LOCK", root.join("recoverer.lock"))
        .env("KHIVE_SOCKET", root.join("s.sock"))
        .env("KHIVE_PID", root.join("s.pid"))
        .env(ROOT, root)
        .env(CASE, name);
    if let Some(mode) = writer_mode {
        command.env("KHIVE_WRITE_QUEUE", mode);
    }
    if let Some(phase) = phase {
        command.env(PHASE, phase);
    }
    command.output().expect("run one actual child test")
}
fn healthy(output: Output) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "child failed: {stdout}\n{stderr}");
    assert!(
        stdout.contains("1 passed"),
        "expected exactly one executed child test: {stdout}\n{stderr}"
    );
}
fn isolate(name: &str) -> Option<PathBuf> {
    if std::env::var(CASE).as_deref() == Ok(name) {
        return Some(PathBuf::from(std::env::var_os(ROOT).unwrap()));
    }
    let root = tempfile::tempdir().unwrap();
    healthy(child(name, root.path(), None));
    None
}

// Only recurrence-window selection is controlled here. The producer's final
// writer clock stays real, so a forward/backward clock step cannot fabricate admission.
tokio::task_local! {
    pub(super) static CAPTURED_HORIZONS: std::collections::HashMap<uuid::Uuid, i64>;
}

#[derive(Debug, Default)]
struct ReplayGate {
    deny: AtomicBool,
    observed: std::sync::Mutex<Vec<(String, String, String)>>,
    identity_denials: std::sync::Mutex<Vec<(String, String, String)>>,
}
impl Gate for ReplayGate {
    fn check(&self, request: &GateRequest) -> std::result::Result<GateDecision, GateError> {
        if request.verb == "test.v3_marker" {
            if request.actor.kind != "actor"
                || request.actor.id != ACTOR
                || !request.namespace.as_str().starts_with("v3-")
            {
                self.identity_denials.lock().unwrap().push((
                    request.actor.kind.clone(),
                    request.actor.id.clone(),
                    request.namespace.as_str().into(),
                ));
                return Ok(GateDecision::deny(
                    "fixture daemon identity cannot perform the creator effect",
                ));
            }
            self.observed.lock().unwrap().push((
                request.actor.kind.clone(),
                request.actor.id.clone(),
                request.namespace.as_str().into(),
            ));
            if self.deny.load(Ordering::SeqCst) {
                return Ok(GateDecision::deny("fixture replay denied"));
            }
        }
        Ok(GateDecision::allow())
    }
}
#[derive(Debug, Default)]
struct MarkerState {
    block: AtomicBool,
    crash_after_effect: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
struct ReleaseOnDrop(Arc<MarkerState>);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release.notify_one();
    }
}
struct MarkerPack {
    rt: KhiveRuntime,
    state: Arc<MarkerState>,
}
impl Pack for MarkerPack {
    const NAME: &'static str = "v3-marker";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
        name: "test.v3_marker",
        description: "Persist one fixture observation",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[ParamDef {
            name: "label",
            param_type: "string",
            required: true,
            description: "Marker text",
            resolution_mode: IdResolutionMode::NotApplicable,
        }],
    }];
}
#[async_trait::async_trait]
impl PackRuntime for MarkerPack {
    khive_runtime::pack_runtime_metadata!();
    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        _registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> std::result::Result<Value, RuntimeError> {
        assert_eq!(verb, "test.v3_marker");
        if self.state.block.load(Ordering::SeqCst) {
            self.state.entered.notify_one();
            self.state.release.notified().await;
        }
        let label = params["label"]
            .as_str()
            .ok_or_else(|| RuntimeError::InvalidInput("marker label".into()))?;
        let note = self
            .rt
            .create_note(token, "observation", None, label, None, None, vec![])
            .await?;
        if self.state.crash_after_effect.load(Ordering::SeqCst) {
            let root = PathBuf::from(std::env::var_os(ROOT).expect("private crash fixture root"));
            std::fs::write(
                root.join("effect.json"),
                json!({"marker_id":note.id}).to_string(),
            )
            .unwrap();
            std::process::exit(72);
        }
        Ok(json!({"id":note.id,"namespace":note.namespace}))
    }
}
struct Fixture {
    rt: KhiveRuntime,
    server: KhiveMcpServer,
    store: OccurrenceStore,
    gate: Arc<ReplayGate>,
    marker: Arc<MarkerState>,
}
impl Fixture {
    fn open(root: &Path) -> Self {
        let rt = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(root.join("mapped-schedule.db")),
            actor_id: Some(ACTOR.into()),
            default_namespace: Namespace::parse(NS).unwrap(),
            packs: vec!["kg".into(), "schedule".into()],
            ..RuntimeConfig::no_embeddings()
        })
        .unwrap();
        let gate = Arc::new(ReplayGate::default());
        let marker = Arc::new(MarkerState::default());
        let mut builder = VerbRegistryBuilder::new();
        builder
            .with_default_namespace(DAEMON_NS)
            .with_actor_id(Some(DAEMON_ACTOR.into()))
            .with_gate(gate.clone());
        builder.register(khive_pack_kg::KgPack::new(rt.clone()));
        builder.register(khive_pack_schedule::SchedulePack::new(rt.clone()));
        builder.register(MarkerPack {
            rt: rt.clone(),
            state: marker.clone(),
        });
        let registry = builder.build().unwrap();
        registry
            .apply_schema_plans_with_map(&Default::default(), rt.backend())
            .expect("checked production schema install");
        let server =
            KhiveMcpServer::from_registry_with_meta(registry, DAEMON_NS, "v3-private-fixture");
        if let Ok(mode) = std::env::var("KHIVE_WRITE_QUEUE") {
            assert!(mode == "0" || mode == "1", "explicit fixture writer mode");
            assert_eq!(
                rt.backend().pool().write_queue_active(),
                mode == "1",
                "actual file-backed writer mode"
            );
        }
        let store = OccurrenceStore::open(&rt).unwrap();
        Self {
            rt,
            server,
            store,
            gate,
            marker,
        }
    }
    fn token(&self) -> NamespaceToken {
        self.rt.authorize(Namespace::parse(NS).unwrap()).unwrap()
    }
    async fn request(&self, tool: &str, args: Value) -> Value {
        let namespace = args
            .get("namespace")
            .and_then(Value::as_str)
            .unwrap_or(NS)
            .to_owned();
        let ops = serde_json::to_string(&json!([{"tool":tool,"args":args}])).unwrap();
        let raw = self
            .server
            .dispatch_request_replay_as(
                RequestParams {
                    ops,
                    ..Default::default()
                },
                &namespace,
                Some(
                    VerifiedActor::new(ACTOR.to_owned())
                        .expect("fixture's explicit verified creator"),
                ),
            )
            .await
            .unwrap();
        serde_json::from_str(&raw).unwrap()
    }
    async fn declare(&self, label: &str) -> (uuid::Uuid, DateTime<Utc>) {
        self.declare_in(label, None, NS).await
    }
    async fn declare_in(
        &self,
        label: &str,
        repeat: Option<&str>,
        namespace: &str,
    ) -> (uuid::Uuid, DateTime<Utc>) {
        let at = Utc::now() + Duration::seconds(1);
        let mut args = json!({"action":format!("test.v3_marker(label={})",json!(label)),
            "at":at.to_rfc3339(),"receipt_version":3,"namespace":namespace});
        if let Some(repeat) = repeat {
            args["repeat"] = json!(repeat);
        }
        let value = self.request("schedule.schedule", args).await;
        assert_eq!(value["results"][0]["ok"], true, "{value}");
        let id = value["results"][0]["result"]["full_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        (id, at)
    }
    async fn due(at: DateTime<Utc>) {
        if let Ok(wait) = (at + Duration::milliseconds(5) - Utc::now()).to_std() {
            tokio::time::sleep(wait).await;
        }
    }
    async fn counter(&self, label: &str) -> i64 {
        let row=self.rt.sql().reader().await.unwrap().query_row(SqlStatement::new("SELECT COUNT(*) AS count FROM notes WHERE namespace=?1 AND kind='observation' AND content=?2 AND deleted_at IS NULL",
            vec![SqlValue::Text(NS.into()),SqlValue::Text(label.into())])).await.unwrap().unwrap();
        row.i64("count").unwrap()
    }
    async fn stored_rows(&self) -> Value {
        let mut reader = self.rt.sql().reader().await.unwrap();
        let mut all = serde_json::Map::new();
        for table in [
            "scheduled_definitions_v3",
            "scheduled_series_v3",
            "scheduled_occurrences_v3",
            "scheduled_attempts_v3",
            "scheduled_receipts_v3",
            "scheduled_notice_outbox_v3",
            "notes",
            "events",
        ] {
            let rows = reader
                .query_all(SqlStatement::new(
                    format!("SELECT * FROM {table} ORDER BY rowid"),
                    vec![],
                ))
                .await
                .unwrap();
            all.insert(table.into(), serde_json::to_value(rows).unwrap());
        }
        Value::Object(all)
    }
    async fn sql(&self, sql: &str, params: Vec<SqlValue>) {
        self.rt
            .sql()
            .writer()
            .await
            .unwrap()
            .execute(SqlStatement::new(sql, params))
            .await
            .unwrap();
    }
}

#[test]
fn exec_domain_outcomes_preserve_timeout_nullable_status_and_original_evidence() {
    let receipt = json!({"id":uuid::Uuid::new_v4(),"timed_out":false,"denied":false,"success":true,"exit_code":0,"exit_signal":null,
        "started_at":"2026-01-01T00:00:00Z","finished_at":"2026-01-01T00:00:01Z","process_cleanup":{"status":"uncertified"},"stdout_ref":"retained"});
    let envelope = |domain: Value| json!({"results":[{"ok":true,"result":domain}]});
    let healthy = classify_envelope(envelope(receipt.clone()), true);
    assert_eq!(healthy.outcome, TerminalOutcome::Succeeded);
    assert_eq!(healthy.exit_code, Some(0));
    assert!(healthy.exec_receipt_id.is_some());
    let mut timeout = receipt.clone();
    timeout["timed_out"] = json!(true);
    let value = envelope(timeout);
    let observed = classify_envelope(value.clone(), true);
    assert_eq!(observed.outcome, TerminalOutcome::TimedOut);
    assert_eq!(observed.evidence, value);
    let mut failed = receipt.clone();
    failed["success"] = json!(false);
    failed["exit_code"] = json!(2);
    assert_eq!(
        classify_envelope(envelope(failed), true).outcome,
        TerminalOutcome::Failed
    );
    let mut denied = receipt.clone();
    denied["denied"] = json!(true);
    denied["success"] = json!(false);
    denied["exit_code"] = Value::Null;
    assert_eq!(
        classify_envelope(envelope(denied), true).outcome,
        TerminalOutcome::Refused
    );
    let mut malformed = receipt.clone();
    malformed["exit_code"] = json!("0");
    assert_eq!(
        classify_envelope(envelope(malformed), true).outcome,
        TerminalOutcome::Indeterminate
    );
    let mut unknown = receipt;
    unknown["success"] = json!("unknown");
    assert_eq!(
        classify_envelope(envelope(unknown), true).outcome,
        TerminalOutcome::Indeterminate
    );
    assert_eq!(
        classify_envelope(json!({"results":[{"ok":true}]}), true).outcome,
        TerminalOutcome::Indeterminate
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn actual_competing_drainers_replay_one_effect_as_original_creator() {
    let Some(root) = isolate("actual_competing_drainers_replay_one_effect_as_original_creator")
    else {
        return;
    };
    let f = Fixture::open(&root);
    let daemon = f
        .server
        .dispatch_request_local(RequestParams {
            ops: serde_json::to_string(
                &json!([{"tool":"test.v3_marker","args":{"label":"daemon-probe"}}]),
            )
            .unwrap(),
            ..Default::default()
        })
        .await
        .unwrap();
    let daemon: Value = serde_json::from_str(&daemon).unwrap();
    assert_eq!(
        daemon["results"][0]["ok"], false,
        "the baked daemon identity is actually denied"
    );
    assert_eq!(f.counter("daemon-probe").await, 0);
    assert!(f
        .gate
        .identity_denials
        .lock()
        .unwrap()
        .iter()
        .any(|(_, actor, ns)| actor == DAEMON_ACTOR && ns == DAEMON_NS));
    let (id, at) = f.declare("one-real-effect").await;
    Fixture::due(at).await;
    let (a, b) = tokio::join!(
        super::super::run_pending_events_on(&f.rt, &f.server, false),
        super::super::run_pending_events_on(&f.rt, &f.server, false)
    );
    a.unwrap();
    b.unwrap();
    assert_eq!(f.counter("one-real-effect").await, 1);
    let inventory = f
        .store
        .occurrences(&f.token(), id, None, None, 200)
        .await
        .unwrap();
    assert_eq!(inventory.items.len(), 1);
    assert_eq!(
        inventory.items[0].terminal.as_ref().unwrap()["outcome"],
        "succeeded"
    );
    assert_eq!(
        f.store
            .pending_notices(&f.token(), None, 200)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    assert!(!f.gate.observed.lock().unwrap().is_empty());
    assert!(f
        .gate
        .observed
        .lock()
        .unwrap()
        .iter()
        .all(|(kind, actor, ns)| kind == "actor" && actor == ACTOR && ns == NS));
    super::super::run_pending_events_on(&f.rt, &f.server, false)
        .await
        .unwrap();
    assert_eq!(f.counter("one-real-effect").await, 1);
}

#[tokio::test]
async fn fresh_live_gate_denial_has_a_receipt_and_cannot_inherit_daemon_identity() {
    let Some(root) =
        isolate("fresh_live_gate_denial_has_a_receipt_and_cannot_inherit_daemon_identity")
    else {
        return;
    };
    let f = Fixture::open(&root);
    let (id, at) = f.declare("denied-effect").await;
    f.gate.deny.store(true, Ordering::SeqCst);
    Fixture::due(at).await;
    super::super::run_pending_events_on(&f.rt, &f.server, false)
        .await
        .unwrap();
    assert_eq!(f.counter("denied-effect").await, 0);
    let page = f
        .store
        .occurrences(&f.token(), id, None, None, 200)
        .await
        .unwrap();
    assert_eq!(
        page.items[0].terminal.as_ref().unwrap()["outcome"],
        "refused"
    );
    assert_eq!(
        f.store
            .pending_notices(&f.token(), None, 200)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    f.gate.deny.store(false, Ordering::SeqCst);
    let (_, at) = f.declare("allowed-effect").await;
    Fixture::due(at).await;
    super::super::run_pending_events_on(&f.rt, &f.server, false)
        .await
        .unwrap();
    assert_eq!(f.counter("allowed-effect").await, 1);
}

#[tokio::test]
async fn terminal_write_failure_after_real_effect_recovers_uncertainty_without_replay() {
    let Some(root) =
        isolate("terminal_write_failure_after_real_effect_recovers_uncertainty_without_replay")
    else {
        return;
    };
    let f = Fixture::open(&root);
    let (id, at) = f.declare("effect-before-storage-failure").await;
    Fixture::due(at).await;
    f.sql("CREATE TRIGGER injected_outbox BEFORE INSERT ON scheduled_notice_outbox_v3 BEGIN SELECT RAISE(FAIL,'injected outbox after effect'); END",vec![]).await;
    let error = super::super::run_pending_events_on(&f.rt, &f.server, false)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("injected outbox after effect"),
        "{error}"
    );
    assert_eq!(f.counter("effect-before-storage-failure").await, 1);
    let item = f
        .store
        .occurrences(&f.token(), id, None, None, 200)
        .await
        .unwrap()
        .items
        .remove(0);
    assert!(item.terminal.is_none());
    assert!(f
        .store
        .pending_notices(&f.token(), None, 200)
        .await
        .unwrap()
        .items
        .is_empty());
    f.sql("DROP TRIGGER injected_outbox", vec![]).await;
    f.sql(
        "UPDATE scheduled_attempts_v3 SET lease_expires_at_us=0 WHERE occurrence_id=?1",
        vec![SqlValue::Text(item.id.to_string())],
    )
    .await;
    let recovery = super::super::run_pending_events_on(&f.rt, &f.server, false)
        .await
        .unwrap();
    assert_eq!(
        recovery.reclaimed, 1,
        "one actual expired v3 attempt and no legacy claims"
    );
    assert_eq!(f.counter("effect-before-storage-failure").await, 1);
    let page = f
        .store
        .occurrences(&f.token(), id, None, None, 200)
        .await
        .unwrap();
    assert_eq!(
        page.items[0].terminal.as_ref().unwrap()["outcome"],
        "indeterminate"
    );
    assert_eq!(
        f.store
            .pending_notices(&f.token(), None, 200)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancellation_wins_after_intent_and_late_actual_response_only_reconciles() {
    let Some(root) =
        isolate("cancellation_wins_after_intent_and_late_actual_response_only_reconciles")
    else {
        return;
    };
    let f = Fixture::open(&root);
    let (id, at) = f.declare("cancelled-in-flight").await;
    Fixture::due(at).await;
    f.marker.block.store(true, Ordering::SeqCst);
    let _release = ReleaseOnDrop(f.marker.clone());
    let entered = f.marker.entered.notified();
    let rt = f.rt.clone();
    let server = f.server.clone();
    let running =
        tokio::spawn(async move { super::super::run_pending_events_on(&rt, &server, false).await });
    tokio::time::timeout(std::time::Duration::from_secs(10), entered)
        .await
        .expect("real handler entered after intent");
    let before =
        f.rt.notes(&f.token())
            .unwrap()
            .get_note(id)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(
        before.properties.as_ref().unwrap()["status"],
        "firing",
        "projection publishes the real in-flight lifecycle"
    );
    let subsequent = super::super::run_pending_events_on(&f.rt, &f.server, false)
        .await
        .unwrap();
    assert_eq!(
        subsequent.reclaimed, 0,
        "legacy reclaim must leave the actual bound v3 invocation alone"
    );
    assert_eq!(
        f.rt.notes(&f.token())
            .unwrap()
            .get_note(id)
            .await
            .unwrap()
            .unwrap(),
        before
    );
    let cut = f.store.cancel_v3(&f.token(), id).await.unwrap().unwrap();
    assert!(cut.changed);
    f.marker.release.notify_one();
    running.await.unwrap().unwrap();
    assert_eq!(f.counter("cancelled-in-flight").await, 1);
    let item = f
        .store
        .occurrences(&f.token(), id, None, None, 200)
        .await
        .unwrap()
        .items
        .remove(0);
    let receipts = f
        .store
        .receipt(&f.token(), item.id, None, 200)
        .await
        .unwrap();
    assert_eq!(receipts.items.len(), 2);
    assert!(receipts
        .items
        .iter()
        .any(|r| r["record_kind"] == "terminal" && r["outcome"] == "indeterminate"));
    assert!(receipts
        .items
        .iter()
        .any(|r| r["record_kind"] == "reconciliation" && r["outcome"] == "succeeded"));
    assert_eq!(
        f.store
            .pending_notices(&f.token(), None, 200)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
}

#[tokio::test]
async fn forged_v3_marker_never_activates_or_falls_through_legacy_execution() {
    let Some(root) = isolate("forged_v3_marker_never_activates_or_falls_through_legacy_execution")
    else {
        return;
    };
    let f = Fixture::open(&root);
    let token = f.token();
    let note=f.rt.create_note(&token,"scheduled_event",None,"forged",None,Some(json!({"receipt_version":3,"status":"pending",
        "trigger_at":"2000-01-01T00:00:00Z","event_type":"schedule","payload":"test.v3_marker(label=\"forged-effect\")","created_by_actor":ACTOR})),vec![]).await.unwrap();
    super::super::run_pending_events_on(&f.rt, &f.server, false)
        .await
        .unwrap();
    assert_eq!(f.counter("forged-effect").await, 0);
    assert!(f.store.series(note.id).await.unwrap().is_none());
    let (_, at) = f.declare("genuine-effect").await;
    Fixture::due(at).await;
    super::super::run_pending_events_on(&f.rt, &f.server, false)
        .await
        .unwrap();
    assert_eq!(f.counter("genuine-effect").await, 1);
}

#[tokio::test]
async fn partial_auxiliary_schema_refuses_before_any_generic_effect() {
    let Some(root) = isolate("partial_auxiliary_schema_refuses_before_any_generic_effect") else {
        return;
    };
    let f = Fixture::open(&root);
    f.sql("DROP TABLE scheduled_notice_outbox_v3", vec![]).await;
    let error = super::super::run_pending_events_on(&f.rt, &f.server, false)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("partially installed"), "{error}");
}

#[tokio::test]
async fn accounting_pressure_reports_transient_failure_and_resumes_same_identity() {
    let Some(root) =
        isolate("accounting_pressure_reports_transient_failure_and_resumes_same_identity")
    else {
        return;
    };
    let f = Fixture::open(&root);
    let (_, healthy_at) = f.declare("accounting-healthy-control").await;
    Fixture::due(healthy_at).await;
    super::super::run_pending_events_on(&f.rt, &f.server, false)
        .await
        .unwrap();
    assert_eq!(f.counter("accounting-healthy-control").await, 1);

    let (id, at) = f.declare("accounting-faulted-effect").await;
    Fixture::due(at).await;
    let before = f.store.series(id).await.unwrap().unwrap();
    let expected = khive_pack_schedule::occurrence::OccurrenceKey {
        store_id: f.store.store_id(),
        schedule_id: id,
        definition_revision: before.definition_revision,
        scheduled_for: before.next.unwrap(),
    }
    .id();
    let horizon = at.timestamp_micros() + 1;
    let horizons = [(id, horizon)]
        .into_iter()
        .collect::<std::collections::HashMap<_, _>>();
    f.sql("CREATE TRIGGER injected_accounting BEFORE INSERT ON scheduled_occurrences_v3 BEGIN SELECT RAISE(FAIL,'injected materialization pressure'); END", vec![]).await;
    let rows = f.stored_rows().await;
    let error = CAPTURED_HORIZONS
        .scope(
            horizons.clone(),
            super::super::run_pending_events_on(&f.rt, &f.server, false),
        )
        .await
        .expect_err("real occurrence materialization must fail");
    assert!(
        error.to_string().contains("accounting_blocked"),
        "{error:#}"
    );
    assert!(
        error
            .to_string()
            .contains("injected materialization pressure"),
        "{error:#}"
    );
    assert!(error.chain().skip(1).any(|cause| cause
        .to_string()
        .contains("injected materialization pressure")));
    assert_eq!(f.counter("accounting-faulted-effect").await, 0);
    assert_eq!(f.counter("accounting-healthy-control").await, 1);
    assert_eq!(f.store.series(id).await.unwrap().unwrap(), before);
    assert_eq!(
        f.stored_rows().await,
        rows,
        "rollback includes note bytes/version and every ledger row"
    );

    f.sql("DROP TRIGGER injected_accounting", vec![]).await;
    CAPTURED_HORIZONS
        .scope(
            horizons,
            super::super::run_pending_events_on(&f.rt, &f.server, false),
        )
        .await
        .unwrap();
    assert_eq!(f.counter("accounting-faulted-effect").await, 1);
    let items = f
        .store
        .occurrences(&f.token(), id, None, None, 200)
        .await
        .unwrap()
        .items;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].id, expected);
    assert_eq!(items[0].selection_at_us, horizon);
    assert_eq!(items[0].terminal.as_ref().unwrap()["outcome"], "succeeded");
    assert_eq!(
        f.store
            .pending_notices(&f.token(), None, 200)
            .await
            .unwrap()
            .items
            .iter()
            .filter(|notice| notice.occurrence_id == expected)
            .count(),
        1
    );
}

#[tokio::test]
async fn cold_ready_identity_claims_once_without_live_note_iteration() {
    let name = "cold_ready_identity_claims_once_without_live_note_iteration";
    if std::env::var(CASE).as_deref() != Ok(name) {
        let root = tempfile::tempdir().unwrap();
        let crashed = child(name, root.path(), Some("crash"));
        assert_eq!(crashed.status.code(), Some(71), "{:?}", crashed);
        assert!(root.path().join("ready.json").exists());
        healthy(child(name, root.path(), Some("resume")));
        healthy(child(name, root.path(), Some("again")));
        return;
    }
    let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
    let f = Fixture::open(&root);
    let phase = std::env::var(PHASE).unwrap();
    if phase == "crash" {
        let (id, at) = f.declare("cold-real-effect").await;
        Fixture::due(at).await;
        let snapshot = f.store.series(id).await.unwrap().unwrap();
        let definition = f.store.definition(&snapshot).await.unwrap();
        let page = f
            .store
            .prepare_page(
                &snapshot,
                &definition,
                f.store.clock().await.unwrap(),
                300,
                200,
            )
            .unwrap();
        let committed = f.store.commit_page(page).await.unwrap().unwrap();
        let selected = committed.selected_occurrence_id.unwrap();
        assert!(f.store.series(id).await.unwrap().unwrap().next.is_none());
        let row =
            f.rt.sql()
                .reader()
                .await
                .unwrap()
                .query_row(SqlStatement::new(
                    "SELECT COUNT(*) AS count FROM scheduled_attempts_v3",
                    vec![],
                ))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(row.i64("count").unwrap(), 0);
        assert_eq!(f.counter("cold-real-effect").await, 0);
        // Lower-level display-status mutation isolates infrastructure discovery;
        // ordinary KG updates of scheduled_event remain refused.
        let notes = f.rt.notes(&f.token()).unwrap();
        let mut props = notes
            .get_note(id)
            .await
            .unwrap()
            .unwrap()
            .properties
            .unwrap();
        props["status"] = json!("display-only");
        assert!(notes
            .update_note_properties(id, Some(props), Utc::now().timestamp_micros())
            .await
            .unwrap());
        assert!(super::super::discover_pending_namespaces(&f.rt, Utc::now())
            .await
            .unwrap()
            .is_empty());
        std::fs::write(
            root.join("ready.json"),
            json!({"id":id,"selected":selected,"store":f.store.store_id()}).to_string(),
        )
        .unwrap();
        std::process::exit(71);
    }
    let state: Value =
        serde_json::from_slice(&std::fs::read(root.join("ready.json")).unwrap()).unwrap();
    let id = state["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        f.store.store_id().to_string(),
        state["store"].as_str().unwrap()
    );
    if phase == "resume" {
        assert!(super::super::discover_pending_namespaces(&f.rt, Utc::now())
            .await
            .unwrap()
            .is_empty());
    }
    super::super::run_pending_events_on(&f.rt, &f.server, false)
        .await
        .unwrap();
    assert_eq!(f.counter("cold-real-effect").await, 1);
    let inventory = f
        .store
        .occurrences(&f.token(), id, None, None, 200)
        .await
        .unwrap();
    assert_eq!(inventory.items.len(), 1);
    assert_eq!(
        inventory.items[0].id.to_string(),
        state["selected"].as_str().unwrap()
    );
    assert_eq!(
        inventory.items[0].terminal.as_ref().unwrap()["outcome"],
        "succeeded"
    );
    assert_eq!(
        f.store
            .pending_notices(&f.token(), None, 200)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
}

#[tokio::test]
async fn two_namespaces_share_exactly_one_200_new_occurrence_budget() {
    let Some(root) = isolate("two_namespaces_share_exactly_one_200_new_occurrence_budget") else {
        return;
    };
    let f = Fixture::open(&root);
    let a = f.declare_in("budget-a", Some("every:1s"), NS).await;
    let b = f.declare_in("budget-b", Some("every:1s"), "v3-other").await;
    Fixture::due(a.1.max(b.1)).await;
    let horizons = [
        (a.0, a.1.timestamp_micros() + 600_000_000),
        (b.0, b.1.timestamp_micros() + 600_000_000),
    ]
    .into_iter()
    .collect();
    CAPTURED_HORIZONS
        .scope(
            horizons,
            super::super::run_pending_events_on(&f.rt, &f.server, false),
        )
        .await
        .unwrap();
    let mut reader = f.rt.sql().reader().await.unwrap();
    let row = reader
        .query_row(SqlStatement::new(
            "SELECT COUNT(*) AS count FROM scheduled_occurrences_v3",
            vec![],
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.i64("count").unwrap(),
        200,
        "one shared pass budget, not 200 per namespace/series"
    );
    let before = f.store.series(a.0).await.unwrap().unwrap();
    assert!(before.next.is_some());
    assert_eq!(f.counter("budget-a").await, 0);
}

#[tokio::test]
async fn exhausted_200_budget_still_finds_ready_candidate_beyond_discovery_page() {
    let Some(root) =
        isolate("exhausted_200_budget_still_finds_ready_candidate_beyond_discovery_page")
    else {
        return;
    };
    let f = Fixture::open(&root);
    let mut declarations = Vec::new();
    for index in 0..201 {
        declarations.push(f.declare(&format!("ready-budget-{index}")).await);
    }
    Fixture::due(declarations.iter().map(|(_, at)| *at).max().unwrap()).await;
    declarations.sort_by_key(|(id, _)| *id);
    let (ready, _) = declarations.pop().unwrap();
    let snapshot = f.store.series(ready).await.unwrap().unwrap();
    let definition = f.store.definition(&snapshot).await.unwrap();
    let prepared = f
        .store
        .prepare_page(
            &snapshot,
            &definition,
            f.store.clock().await.unwrap(),
            300,
            200,
        )
        .unwrap();
    let selected = f
        .store
        .commit_page(prepared)
        .await
        .unwrap()
        .unwrap()
        .selected_occurrence_id
        .unwrap();
    let label = match definition.payload {
        ScheduledPayload::Action { action } => {
            khive_request::parse_request(&action).unwrap().ops[0].args["label"].clone()
        }
        _ => panic!("action fixture"),
    };
    let khive_request::ArgValue::Value(label) = label else {
        panic!("literal fixture label")
    };
    let label = label.as_str().unwrap().to_owned();
    let horizons = declarations
        .iter()
        .map(|(id, at)| (*id, at.timestamp_micros() + 600_000_000))
        .collect();
    CAPTURED_HORIZONS
        .scope(
            horizons,
            super::super::run_pending_events_on(&f.rt, &f.server, false),
        )
        .await
        .unwrap();
    let row =
        f.rt.sql()
            .reader()
            .await
            .unwrap()
            .query_row(SqlStatement::new(
                "SELECT COUNT(*) AS count FROM scheduled_occurrences_v3",
                vec![],
            ))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(
        row.i64("count").unwrap(),
        201,
        "200 new plus the original ready identity"
    );
    assert_eq!(
        f.counter(&label).await,
        1,
        "zero-new sweep must reach the ready candidate beyond first 200 series"
    );
    let page = f
        .store
        .receipt(&f.token(), selected, None, 200)
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0]["outcome"], "succeeded");
    let mut after = None;
    let mut notices = 0;
    loop {
        let page = f
            .store
            .pending_notices(&f.token(), after, 200)
            .await
            .unwrap();
        notices += page.items.len();
        after = page.items.last().map(|v| (v.created_at_us, v.id));
        if !page.has_more {
            break;
        }
    }
    assert_eq!(notices, 201);
}

#[tokio::test]
async fn ordinary_kg_update_still_refuses_schedule_intent_and_storage_tamper_is_inert() {
    let Some(root) =
        isolate("ordinary_kg_update_still_refuses_schedule_intent_and_storage_tamper_is_inert")
    else {
        return;
    };
    let f = Fixture::open(&root);
    let (id, at) = f.declare("immutable-ledger-intent").await;
    let refusal = f
        .request(
            "update",
            json!({"id":id,"properties":{"receipt_version":null},"namespace":NS}),
        )
        .await;
    assert_eq!(
        refusal["results"][0]["ok"], false,
        "ordinary managed-note mutation remains refused"
    );
    Fixture::due(at).await;
    super::super::run_pending_events_on(&f.rt, &f.server, false)
        .await
        .unwrap();
    assert_eq!(f.counter("immutable-ledger-intent").await, 1);
    // This lower-level storage mutation is only a robustness oracle. It is not
    // an available public KG update route or a claim of an ordinary CRUD exploit.
    let store = f.rt.notes(&f.token()).unwrap();
    let mut properties = store
        .get_note(id)
        .await
        .unwrap()
        .unwrap()
        .properties
        .unwrap();
    properties
        .as_object_mut()
        .unwrap()
        .remove("receipt_version");
    assert!(store
        .update_note_properties(id, Some(properties.clone()), Utc::now().timestamp_micros())
        .await
        .unwrap());
    super::super::run_pending_events_on(&f.rt, &f.server, false)
        .await
        .unwrap();
    assert_eq!(f.counter("immutable-ledger-intent").await, 1);
    properties["status"] = json!("firing");
    properties["lease_expires_at"] = json!(0);
    properties["firing_at"] = json!(0);
    assert!(store
        .update_note_properties(id, Some(properties), Utc::now().timestamp_micros())
        .await
        .unwrap());
    let summary = super::super::run_pending_events_on(&f.rt, &f.server, false)
        .await
        .unwrap();
    assert_eq!(summary.reclaimed, 0);
    assert_eq!(f.counter("immutable-ledger-intent").await, 1);
    assert_eq!(
        f.store
            .pending_notices(&f.token(), None, 200)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_overlap_accounts_200_refused_instants_without_a_second_effect() {
    let Some(root) = isolate("live_overlap_accounts_200_refused_instants_without_a_second_effect")
    else {
        return;
    };
    let f = Fixture::open(&root);
    let (id, at) = f
        .declare_in("one-overlapping-effect", Some("every:1s"), NS)
        .await;
    Fixture::due(at).await;
    f.marker.block.store(true, Ordering::SeqCst);
    let _release = ReleaseOnDrop(f.marker.clone());
    let entered = f.marker.entered.notified();
    let rt = f.rt.clone();
    let server = f.server.clone();
    let running =
        tokio::spawn(async move { super::super::run_pending_events_on(&rt, &server, false).await });
    tokio::time::timeout(std::time::Duration::from_secs(10), entered)
        .await
        .unwrap();
    Fixture::due(at + Duration::seconds(1)).await;
    let horizons = [(id, at.timestamp_micros() + 300_000_000)]
        .into_iter()
        .collect();
    CAPTURED_HORIZONS
        .scope(
            horizons,
            super::super::run_pending_events_on(&f.rt, &f.server, false),
        )
        .await
        .unwrap();
    let first_page = f
        .store
        .occurrences(&f.token(), id, None, None, 200)
        .await
        .unwrap();
    assert!(first_page.has_more);
    assert!(first_page
        .items
        .iter()
        .skip(1)
        .all(|o| o.terminal.as_ref().unwrap()["reason_code"] == "overlap"));
    let after = first_page.items.last().map(|o| (o.key.scheduled_for, o.id));
    let last = f
        .store
        .occurrences(&f.token(), id, None, after, 200)
        .await
        .unwrap();
    assert_eq!(last.items.len(), 1);
    f.marker.release.notify_one();
    running.await.unwrap().unwrap();
    assert_eq!(f.counter("one-overlapping-effect").await, 1);
}

#[tokio::test]
async fn legacy_v1_v2_receipts_and_ids_remain_exact_across_cold_reopen() {
    let name = "legacy_v1_v2_receipts_and_ids_remain_exact_across_cold_reopen";
    if std::env::var(CASE).as_deref() != Ok(name) {
        let root = tempfile::tempdir().unwrap();
        healthy(child(name, root.path(), Some("create")));
        healthy(child(name, root.path(), Some("reopen")));
        return;
    }
    let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
    let f = Fixture::open(&root);
    if std::env::var(PHASE).as_deref() == Ok("create") {
        let mut legacy = Vec::new();
        let mut latest = Utc::now();
        for (version, label) in [(1, "legacy-v1"), (2, "legacy-v2")] {
            let at = Utc::now() + Duration::seconds(1);
            latest = latest.max(at);
            let mut args = json!({"action":format!("test.v3_marker(label={})",json!(label)),"at":at.to_rfc3339(),"namespace":NS});
            if version == 2 {
                args["misfire_policy"] = json!("catch_up_once");
                args["max_lateness"] = json!(60);
            }
            let result = f.request("schedule.schedule", args).await;
            assert_eq!(result["results"][0]["ok"], true, "{result}");
            let id = result["results"][0]["result"]["full_id"]
                .as_str()
                .unwrap()
                .parse::<uuid::Uuid>()
                .unwrap();
            legacy.push((id, at, version));
        }
        let (v3, at) = f.declare("explicit-v3").await;
        Fixture::due(latest.max(at)).await;
        super::super::run_pending_events_on(&f.rt, &f.server, false)
            .await
            .unwrap();
        let mut saved = Vec::new();
        for (id, at, version) in legacy {
            let note =
                f.rt.notes(&f.token())
                    .unwrap()
                    .get_note(id)
                    .await
                    .unwrap()
                    .unwrap();
            let properties = note.properties.unwrap();
            assert_eq!(properties["dispatch_receipt"]["version"], version);
            assert_eq!(
                properties["dispatch_receipt"]["occurrence_id"],
                super::super::dispatch_occurrence_id(id, at).to_string()
            );
            assert!(f
                .store
                .occurrences(&f.token(), id, None, None, 200)
                .await
                .unwrap()
                .items
                .is_empty());
            saved.push(json!({"id":id,"properties":properties}));
        }
        assert_eq!(
            f.store
                .occurrences(&f.token(), v3, None, None, 200)
                .await
                .unwrap()
                .items
                .len(),
            1
        );
        std::fs::write(
            root.join("legacy.json"),
            serde_json::to_vec(&saved).unwrap(),
        )
        .unwrap();
    } else {
        let saved: Vec<Value> =
            serde_json::from_slice(&std::fs::read(root.join("legacy.json")).unwrap()).unwrap();
        for entry in saved {
            let id = entry["id"].as_str().unwrap().parse().unwrap();
            let note =
                f.rt.notes(&f.token())
                    .unwrap()
                    .get_note(id)
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(note.properties.as_ref().unwrap(), &entry["properties"]);
            assert!(f
                .store
                .occurrences(&f.token(), id, None, None, 200)
                .await
                .unwrap()
                .items
                .is_empty());
        }
        super::super::run_pending_events_on(&f.rt, &f.server, false)
            .await
            .unwrap();
        for label in ["legacy-v1", "legacy-v2", "explicit-v3"] {
            assert_eq!(f.counter(label).await, 1);
        }
    }
}

#[tokio::test]
async fn cold_crash_after_real_effect_before_terminal_never_replays() {
    let name = "cold_crash_after_real_effect_before_terminal_never_replays";
    if std::env::var(CASE).as_deref() != Ok(name) {
        let root = tempfile::tempdir().unwrap();
        let crashed = child(name, root.path(), Some("effect"));
        assert_eq!(crashed.status.code(), Some(72), "{:?}", crashed);
        assert!(root.path().join("effect.json").exists());
        healthy(child(name, root.path(), Some("recover")));
        healthy(child(name, root.path(), Some("after-cancel")));
        return;
    }
    let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
    let f = Fixture::open(&root);
    let phase = std::env::var(PHASE).unwrap();
    if phase == "effect" {
        let (id, at) = f.declare("effect-survives-crash").await;
        std::fs::write(root.join("launch.json"), json!({"id":id}).to_string()).unwrap();
        Fixture::due(at).await;
        f.marker.crash_after_effect.store(true, Ordering::SeqCst);
        super::super::run_pending_events_on(&f.rt, &f.server, false)
            .await
            .unwrap();
        panic!("fixture must exit after its real effect");
    }
    let state: Value =
        serde_json::from_slice(&std::fs::read(root.join("launch.json")).unwrap()).unwrap();
    let id = state["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(f.counter("effect-survives-crash").await, 1);
    if phase == "recover" {
        let inventory = f
            .store
            .occurrences(&f.token(), id, None, None, 200)
            .await
            .unwrap();
        assert_eq!(inventory.items.len(), 1);
        assert!(inventory.items[0].terminal.is_none());
        f.sql(
            "UPDATE scheduled_attempts_v3 SET lease_expires_at_us=0 WHERE occurrence_id=?1",
            vec![SqlValue::Text(inventory.items[0].id.to_string())],
        )
        .await;
        let summary = super::super::run_pending_events_on(&f.rt, &f.server, false)
            .await
            .unwrap();
        assert_eq!(summary.reclaimed, 1);
        assert_eq!(summary.invoked, 0);
        f.store.cancel_v3(&f.token(), id).await.unwrap().unwrap();
        f.sql(
            "DELETE FROM notes WHERE id=?1",
            vec![SqlValue::Text(id.to_string())],
        )
        .await;
    } else {
        let work = f
            .store
            .series_work_page(WorkPurpose::UnresolvedEvidence, None, 200, 0)
            .await
            .unwrap();
        assert_eq!(work.items.len(), 1);
        assert_eq!(work.items[0].series.schedule_id, id);
        super::super::run_pending_events_on(&f.rt, &f.server, false)
            .await
            .unwrap();
    }
    assert_eq!(f.counter("effect-survives-crash").await, 1);
    let inventory = f
        .store
        .occurrences(&f.token(), id, None, None, 200)
        .await
        .unwrap();
    assert_eq!(
        inventory.items[0].terminal.as_ref().unwrap()["outcome"],
        "indeterminate"
    );
    assert_eq!(
        f.store
            .pending_notices(&f.token(), None, 200)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
}

#[tokio::test]
async fn cold_preintent_claim_reclaims_same_invocation_and_runs_once() {
    let name = "cold_preintent_claim_reclaims_same_invocation_and_runs_once";
    if std::env::var(CASE).as_deref() != Ok(name) {
        let root = tempfile::tempdir().unwrap();
        let crashed = child(name, root.path(), Some("claim"));
        assert_eq!(crashed.status.code(), Some(73), "{:?}", crashed);
        assert!(root.path().join("claim.json").exists());
        healthy(child(name, root.path(), Some("resume")));
        return;
    }
    let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
    let f = Fixture::open(&root);
    if std::env::var(PHASE).as_deref() == Ok("claim") {
        let (id, at) = f.declare("preintent-safe-reclaim").await;
        Fixture::due(at).await;
        let snapshot = f.store.series(id).await.unwrap().unwrap();
        let definition = f.store.definition(&snapshot).await.unwrap();
        let page = f
            .store
            .prepare_page(
                &snapshot,
                &definition,
                f.store.clock().await.unwrap(),
                300,
                200,
            )
            .unwrap();
        let selected = f
            .store
            .commit_page(page)
            .await
            .unwrap()
            .unwrap()
            .selected_occurrence_id
            .unwrap();
        let claim = f
            .store
            .claim_candidate(id, selected)
            .await
            .unwrap()
            .unwrap();
        std::fs::write(
            root.join("claim.json"),
            json!({"id":id,"occurrence":selected,"invocation":claim.invocation_id()}).to_string(),
        )
        .unwrap();
        assert_eq!(f.counter("preintent-safe-reclaim").await, 0);
        std::process::exit(73);
    }
    let state: Value =
        serde_json::from_slice(&std::fs::read(root.join("claim.json")).unwrap()).unwrap();
    let id = state["id"].as_str().unwrap().parse().unwrap();
    f.sql(
        "UPDATE scheduled_attempts_v3 SET lease_expires_at_us=0 WHERE occurrence_id=?1",
        vec![SqlValue::Text(state["occurrence"].as_str().unwrap().into())],
    )
    .await;
    super::super::run_pending_events_on(&f.rt, &f.server, false)
        .await
        .unwrap();
    assert_eq!(f.counter("preintent-safe-reclaim").await, 1);
    let inventory = f
        .store
        .occurrences(&f.token(), id, None, None, 200)
        .await
        .unwrap();
    assert_eq!(inventory.items.len(), 1);
    assert_eq!(
        inventory.items[0].id.to_string(),
        state["occurrence"].as_str().unwrap()
    );
    assert_eq!(
        inventory.items[0].terminal.as_ref().unwrap()["invocation_id"],
        state["invocation"]
    );
    assert_eq!(
        f.store
            .pending_notices(&f.token(), None, 200)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
}

#[tokio::test]
async fn public_agenda_uses_current_v3_trigger_state_and_mixed_precise_continuation() {
    let Some(root) =
        isolate("public_agenda_uses_current_v3_trigger_state_and_mixed_precise_continuation")
    else {
        return;
    };
    let f = Fixture::open(&root);
    let (repeating, at) = f.declare_in("agenda-repeat", Some("every:1d"), NS).await;
    let (finished, done_at) = f.declare("agenda-finished").await;
    Fixture::due(at.max(done_at)).await;
    super::super::run_pending_events_on(&f.rt, &f.server, false)
        .await
        .unwrap();
    assert_eq!(f.counter("agenda-repeat").await, 1);
    assert_eq!(f.counter("agenda-finished").await, 1);
    let next = f
        .store
        .series(repeating)
        .await
        .unwrap()
        .unwrap()
        .next
        .unwrap()
        .datetime()
        .unwrap();
    let (cancelled, _) = f.declare("agenda-cancelled").await;
    f.store
        .cancel_v3(&f.token(), cancelled)
        .await
        .unwrap()
        .unwrap();
    let offset = next
        .with_timezone(&chrono::FixedOffset::east_opt(7200).unwrap())
        .to_rfc3339();
    let later = next + Duration::nanoseconds(1);
    let mut expected = vec![(next, next.to_rfc3339(), repeating.to_string())];
    for (label, instant, text) in [
        ("agenda-legacy-offset", next, offset),
        ("agenda-legacy-fraction", later, later.to_rfc3339()),
    ] {
        let value=f.request("schedule.schedule",json!({"action":format!("test.v3_marker(label={})",json!(label)),"at":text,"namespace":NS})).await;
        assert_eq!(value["results"][0]["ok"], true, "{value}");
        expected.push((
            instant,
            text,
            value["results"][0]["result"]["full_id"]
                .as_str()
                .unwrap()
                .to_owned(),
        ));
    }
    expected.sort();
    let expected = expected
        .into_iter()
        .map(|(_, _, id)| id)
        .collect::<Vec<_>>();
    let mut args =
        json!({"from":next.to_rfc3339(),"to":later.to_rfc3339(),"limit":1,"namespace":NS});
    let mut actual = Vec::new();
    for _ in 0..=expected.len() {
        let result = f.request("schedule.agenda", args.clone()).await;
        assert_eq!(result["results"][0]["ok"], true, "{result}");
        let page = &result["results"][0]["result"];
        let events = page["events"].as_array().unwrap();
        if events.is_empty() {
            break;
        }
        assert_eq!(events.len(), 1);
        let row = &events[0];
        actual.push(row["full_id"].as_str().unwrap().to_owned());
        assert_eq!(page["next"]["after"], row["properties"]["trigger_at"]);
        assert_eq!(page["next"]["after_id"], row["full_id"]);
        args["after"] = page["next"]["after"].clone();
        args["after_id"] = page["next"]["after_id"].clone();
    }
    assert_eq!(
        actual, expected,
        "authoritative next trigger participates before window/order/LIMIT"
    );
    let result = f
        .request("schedule.agenda", json!({"namespace":NS,"limit":200}))
        .await;
    let events = result["results"][0]["result"]["events"].as_array().unwrap();
    assert!(!events
        .iter()
        .any(|event| event["full_id"] == finished.to_string()
            || event["full_id"] == cancelled.to_string()));
    let note =
        f.rt.notes(&f.token())
            .unwrap()
            .get_note(repeating)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(
        note.properties.as_ref().unwrap()["trigger_at"],
        next.to_rfc3339()
    );
}
