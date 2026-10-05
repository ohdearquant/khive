use super::*;
use khive_runtime::{Gate, GateDecision, GateError, GateRequest, RuntimeConfig};
use khive_storage::event::EventFilter;
use khive_storage::types::PageRequest;
use khive_types::{Details, HandlerDef, KhiveError, VerbCategory, Visibility};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
struct DenyCommSendGate;

impl Gate for DenyCommSendGate {
    fn check(&self, request: &GateRequest) -> Result<GateDecision, GateError> {
        if request.verb == "comm.send" {
            Ok(GateDecision::deny(
                "comm.send denied by delivery-failure test",
            ))
        } else {
            Ok(GateDecision::allow())
        }
    }
}

#[derive(Debug)]
struct DenyCreatorCreateGate;

impl Gate for DenyCreatorCreateGate {
    fn check(&self, request: &GateRequest) -> Result<GateDecision, GateError> {
        if request.verb == "create" && request.actor.id == "lambda:schedule-owner" {
            Ok(GateDecision::deny(
                "creator is not authorized to replay create",
            ))
        } else {
            Ok(GateDecision::allow())
        }
    }
}

#[derive(Debug)]
struct DenyAttackerCreateGate;

impl Gate for DenyAttackerCreateGate {
    fn check(&self, request: &GateRequest) -> Result<GateDecision, GateError> {
        if request.verb == "create" && request.actor.id == "lambda:schedule-attacker" {
            Ok(GateDecision::deny(
                "attacker is not authorized to replay create",
            ))
        } else {
            Ok(GateDecision::allow())
        }
    }
}

#[derive(Debug, Default)]
struct CaptureReplayIdentityGate {
    creates: std::sync::Mutex<Vec<(String, String, String)>>,
}

impl Gate for CaptureReplayIdentityGate {
    fn check(&self, request: &GateRequest) -> Result<GateDecision, GateError> {
        if request.verb == "create" {
            self.creates.lock().expect("capture lock").push((
                request.actor.kind.clone(),
                request.actor.id.clone(),
                request.namespace.as_str().to_string(),
            ));
        }
        Ok(GateDecision::allow())
    }
}

#[derive(Default)]
struct AsyncBlockingSideEffectState {
    invocations: std::sync::atomic::AtomicUsize,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

struct ReleaseAsyncBlockingVerbOnDrop(std::sync::Arc<AsyncBlockingSideEffectState>);

impl Drop for ReleaseAsyncBlockingVerbOnDrop {
    fn drop(&mut self) {
        self.0.release.notify_one();
    }
}

struct AsyncBlockingSideEffectPack {
    runtime: KhiveRuntime,
    marker: String,
    state: std::sync::Arc<AsyncBlockingSideEffectState>,
}

impl khive_types::Pack for AsyncBlockingSideEffectPack {
    const NAME: &'static str = "async-blocking-side-effect-test";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
        name: "test.async_blocking_side_effect",
        description: "wait asynchronously, then commit one marker",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    }];
}

#[async_trait::async_trait]
impl khive_runtime::PackRuntime for AsyncBlockingSideEffectPack {
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
        verb: &str,
        _params: Value,
        _registry: &khive_runtime::VerbRegistry,
        token: &khive_runtime::NamespaceToken,
    ) -> std::result::Result<Value, khive_runtime::RuntimeError> {
        debug_assert_eq!(verb, "test.async_blocking_side_effect");
        self.state
            .invocations
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.state.entered.notify_one();
        self.state.release.notified().await;
        let note = self
            .runtime
            .create_note(token, "observation", None, &self.marker, None, None, vec![])
            .await?;
        Ok(json!({"id": note.id}))
    }
}

#[derive(Debug, Default)]
struct FailFirstCreateGate {
    invocations: std::sync::atomic::AtomicUsize,
}

impl Gate for FailFirstCreateGate {
    fn check(&self, request: &GateRequest) -> Result<GateDecision, GateError> {
        if request.verb == "create" {
            let attempt = self
                .invocations
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if attempt == 0 {
                return Ok(GateDecision::deny("first scheduled create fails"));
            }
        }
        Ok(GateDecision::allow())
    }
}

struct AmbiguousSideEffectPack {
    runtime: KhiveRuntime,
    marker: String,
    outbound_id: uuid::Uuid,
    invocations: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

struct OrdinaryHandlerFailurePack {
    invocations: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[derive(Debug)]
struct DenyOrdinaryHandlerFailureGate {
    checks: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Gate for DenyOrdinaryHandlerFailureGate {
    fn check(&self, request: &GateRequest) -> Result<GateDecision, GateError> {
        if request.verb == "test.ordinary_handler_failure" {
            self.checks
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(GateDecision::deny(
                "scheduled action refused before dispatch",
            ))
        } else {
            Ok(GateDecision::allow())
        }
    }
}

impl khive_types::Pack for OrdinaryHandlerFailurePack {
    const NAME: &'static str = "ordinary-handler-failure-test";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
        name: "test.ordinary_handler_failure",
        description: "return an ordinary error after handler admission",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    }];
}

#[async_trait::async_trait]
impl khive_runtime::PackRuntime for OrdinaryHandlerFailurePack {
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
        verb: &str,
        _params: Value,
        _registry: &khive_runtime::VerbRegistry,
        _token: &khive_runtime::NamespaceToken,
    ) -> std::result::Result<Value, khive_runtime::RuntimeError> {
        debug_assert_eq!(verb, "test.ordinary_handler_failure");
        self.invocations
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(khive_runtime::RuntimeError::Internal(
            "handler returned an ordinary error".into(),
        ))
    }
}

impl khive_types::Pack for AmbiguousSideEffectPack {
    const NAME: &'static str = "ambiguous-side-effect-test";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
        name: "test.ambiguous_side_effect",
        description: "commit a marker, then return a side_effects_unknown error",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[],
    }];
}

#[async_trait::async_trait]
impl khive_runtime::PackRuntime for AmbiguousSideEffectPack {
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
        _params: Value,
        _registry: &khive_runtime::VerbRegistry,
        token: &khive_runtime::NamespaceToken,
    ) -> std::result::Result<Value, khive_runtime::RuntimeError> {
        self.invocations
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.runtime
            .create_note(token, "observation", None, &self.marker, None, None, vec![])
            .await?;
        Err(khive_runtime::RuntimeError::Khive(
            KhiveError::conflict(format!(
                "dual_write delivery outcome is uncertain (side_effects_unknown); \
                     call comm.delivered(id=\"{}\") before retrying",
                self.outbound_id
            ))
            .with_details(Details::new_owned([(
                "outbound_id",
                self.outbound_id.to_string(),
            )])),
        ))
    }
}

fn tmp_db() -> (TempDir, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("khive-test.db");
    let path = path.to_str().expect("utf8 path").to_string();
    (dir, path)
}

/// Due, but inside the default missed-event grace window, so callers land
/// on the normal fire/advance path rather than the missed path.
fn due_rfc3339() -> String {
    (Utc::now() - Duration::seconds(5)).to_rfc3339()
}

/// "Now" formatted like the candidate-page query's own bind parameter,
/// for offset-sorting regressions to assert against independently.
fn now_rfc3339_for_ordering_check() -> String {
    Utc::now().to_rfc3339()
}

async fn make_rt(db_path: &str) -> KhiveRuntime {
    make_rt_with_actor(db_path, None).await
}

async fn make_rt_with_actor(db_path: &str, actor_id: Option<&str>) -> KhiveRuntime {
    let cfg = RuntimeConfig {
        db_path: Some(std::path::PathBuf::from(db_path)),
        default_namespace: Namespace::parse("local").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        actor_id: actor_id.map(str::to_string),
        // Pin the pack list explicitly rather than inheriting `KHIVE_PACKS`
        // from the ambient environment: these tests drive schedule.remind
        // / schedule.cancel through the drain path and assert delivery
        // lands in the creator's comm inbox.
        packs: vec!["kg".to_string(), "schedule".to_string(), "comm".to_string()],
        ..Default::default()
    };
    KhiveRuntime::new(cfg).expect("runtime")
}

/// Drives one drain pass directly through [`run_pending_events_on`],
/// bypassing [`run_pending_events`]'s TOML-aware config resolution (which
/// depends on process `HOME`/cwd, unisolated here) since these tests
/// target drain semantics, not CLI config resolution.
async fn drain_for_test(db_path: &str) -> Result<DrainSummary> {
    let rt = make_rt(db_path).await;
    let server = KhiveMcpServer::new(rt.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;
    run_pending_events_on(&rt, &server, false).await
}

async fn agenda_ticker_last_tick_at(server: &KhiveMcpServer) -> Option<DateTime<Utc>> {
    let response = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: "schedule.agenda()".to_string(),
            presentation: Some("verbose".to_string()),
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("agenda dispatch");
    let envelope: Value = serde_json::from_str(&response).expect("agenda response JSON");
    assert_eq!(envelope["results"][0]["ok"], true, "{envelope:?}");
    envelope["results"][0]["result"]["ticker"]["last_tick_at"]
        .as_str()
        .map(|timestamp| {
            timestamp
                .parse::<DateTime<Utc>>()
                .expect("last_tick_at is RFC 3339")
        })
}

async fn wait_for_agenda_tick_after(
    server: &KhiveMcpServer,
    after: Option<DateTime<Utc>>,
) -> DateTime<Utc> {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let Some(tick) = agenda_ticker_last_tick_at(server).await {
                if after.as_ref().is_none_or(|prior| tick > *prior) {
                    return tick;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("schedule ticker heartbeat did not advance")
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn quiet_schedule_tick_loop_surfaces_an_advancing_then_stale_heartbeat() {
    let (_file, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let server = KhiveMcpServer::new(rt.clone()).expect("server");
    assert_eq!(agenda_ticker_last_tick_at(&server).await, None);

    let interval = std::time::Duration::from_millis(15);
    let cancellation = CancellationToken::new();
    let health = crate::components::HealthReporter::default();
    let ctx = crate::components::HostContext::new(
        server.clone(),
        cancellation.clone(),
        "schedule-tick",
        health.clone(),
    );
    let task = tokio::spawn(schedule_tick_loop(rt, ctx, interval));
    let first = wait_for_agenda_tick_after(&server, None).await;
    let second = wait_for_agenda_tick_after(&server, Some(first)).await;
    assert!(second > first);
    assert!(
        health
            .status("schedule-tick")
            .and_then(|status| status.last_heartbeat)
            .is_some(),
        "a successful quiet drain must heartbeat through component health"
    );

    cancellation.cancel();
    task.await
        .expect("tick task joins")
        .expect("cooperative cancellation is a clean stop");
    let stopped_at = agenda_ticker_last_tick_at(&server)
        .await
        .expect("the loop recorded at least two ticks before stopping");
    assert!(stopped_at >= second);
    tokio::time::sleep(interval.saturating_mul(2)).await;
    assert_eq!(
        agenda_ticker_last_tick_at(&server).await,
        Some(stopped_at),
        "a stopped loop must leave a stale timestamp, not fabricate liveness"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn schedule_ticker_heartbeat_is_process_local_and_missing_without_a_loop() {
    let (_file, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let server = KhiveMcpServer::new(rt).expect("server");
    assert_eq!(agenda_ticker_last_tick_at(&server).await, None);

    server.record_schedule_ticker_tick();
    assert!(agenda_ticker_last_tick_at(&server).await.is_some());

    let replacement_rt = make_rt(&db_path).await;
    let replacement = KhiveMcpServer::new(replacement_rt).expect("replacement server");
    assert_eq!(
        agenda_ticker_last_tick_at(&replacement).await,
        None,
        "a replacement process must not inherit its predecessor's heartbeat"
    );
}

/// Create a scheduled_event note directly via runtime.create_note, replicating
/// the exact property schema used by handle_schedule / handle_remind in
/// khive-pack-schedule.
async fn create_scheduled_event(
    rt: &KhiveRuntime,
    namespace: &str,
    trigger_at: &str,
    action_dsl: Option<&str>,
    repeat: Option<&str>,
    event_type: &str,
) -> uuid::Uuid {
    let ns = Namespace::parse(namespace).expect("ns");
    let token = rt.authorize(ns).expect("authorize");
    let props = json!({
        "trigger_at": trigger_at,
        "repeat": repeat,
        "status": "pending",
        "event_type": event_type,
        "created_by_actor": token.actor().id.clone(),
        "payload": action_dsl,
        "fired_at": null,
        "cancelled_at": null,
    });

    let content = action_dsl.unwrap_or("test reminder");
    let note = rt
        .create_note(
            &token,
            "scheduled_event",
            None,
            content,
            None,
            Some(props),
            vec![],
        )
        .await
        .expect("create_note");

    // Production schedule verbs append this immutable actor binding
    // before activating a row. Most drain tests create fixtures directly
    // through the runtime to target claim/finalize behavior, so mirror
    // that provenance explicitly. Tests for legacy/forged rows construct
    // their own notes and intentionally omit it.
    let provenance = khive_storage::Event::new(
        namespace,
        khive_pack_schedule::CREATOR_PROVENANCE_VERB,
        EventKind::Audit,
        SubstrateKind::Note,
        format!("{}:{}", token.actor().kind, token.actor().id),
    )
    .with_target(note.id)
    .with_payload(json!({
        "provenance": khive_pack_schedule::CREATOR_PROVENANCE_MARKER_V1,
        "event_type": event_type,
    }));
    rt.events(&token)
        .expect("events")
        .append_event(provenance)
        .await
        .expect("append creator provenance");

    note.id
}

/// Fetch a note's properties from the store.
async fn get_note_props(rt: &KhiveRuntime, id: uuid::Uuid) -> Value {
    let ns = Namespace::parse("local").unwrap();
    let token = rt.authorize(ns).expect("authorize");
    let store = rt.notes(&token).expect("notes");
    let note = store
        .get_note(id)
        .await
        .expect("get_note")
        .expect("note exists");
    note.properties.unwrap_or(json!({}))
}

async fn set_repeat_anchor_for_test(rt: &KhiveRuntime, id: uuid::Uuid, anchor: Value) {
    let mut properties = get_note_props(rt, id).await;
    properties["repeat_anchor"] = anchor;
    let token = rt.authorize(Namespace::local()).expect("authorize");
    assert!(rt
        .notes(&token)
        .expect("notes")
        .update_note_properties(id, Some(properties), Utc::now().timestamp_micros())
        .await
        .expect("set repeat anchor"));
}

async fn get_raw_note_properties(rt: &KhiveRuntime, id: uuid::Uuid) -> String {
    let mut reader = rt.sql().reader().await.expect("open SQL reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT properties FROM notes WHERE id = ?1".to_string(),
            params: vec![SqlValue::Text(id.to_string())],
            label: Some("test_get_raw_note_properties".into()),
        })
        .await
        .expect("query raw note properties");
    match rows.as_slice() {
        [row] => match row.get("properties") {
            Some(SqlValue::Text(value)) => value.clone(),
            other => panic!("unexpected properties column: {other:?}"),
        },
        other => panic!("expected one note row, got {other:?}"),
    }
}

async fn inbound_reminder_messages(rt: &KhiveRuntime, actor: &str) -> Vec<(String, Value)> {
    let mut reader = rt.sql().reader().await.expect("open SQL reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT content, properties FROM notes \
                      WHERE kind = 'message' \
                        AND json_extract(properties, '$.direction') = 'inbound' \
                        AND json_extract(properties, '$.to_actor') = ?1 \
                      ORDER BY created_at ASC, id ASC"
                .to_string(),
            params: vec![SqlValue::Text(actor.to_string())],
            label: Some("test_inbound_reminder_messages".into()),
        })
        .await
        .expect("query reminder messages");
    rows.into_iter()
        .map(|row| {
            let content = match row.get("content") {
                Some(SqlValue::Text(value)) => value.clone(),
                other => panic!("unexpected content column: {other:?}"),
            };
            let properties = match row.get("properties") {
                Some(SqlValue::Text(value)) => {
                    serde_json::from_str(value).expect("message properties JSON")
                }
                other => panic!("unexpected properties column: {other:?}"),
            };
            (content, properties)
        })
        .collect()
}

async fn note_content_count_in_namespace(
    rt: &KhiveRuntime,
    namespace: &str,
    kind: &str,
    content: &str,
) -> usize {
    let token = rt
        .authorize(Namespace::parse(namespace).expect("namespace"))
        .expect("authorize");
    rt.notes(&token)
        .expect("notes")
        .query_notes(
            namespace,
            Some(kind),
            PageRequest {
                limit: 200,
                offset: 0,
            },
        )
        .await
        .expect("query notes")
        .items
        .into_iter()
        .filter(|note| note.content == content)
        .count()
}

async fn note_content_count(rt: &KhiveRuntime, kind: &str, content: &str) -> usize {
    note_content_count_in_namespace(rt, "local", kind, content).await
}

async fn make_repeat_due_again(rt: &KhiveRuntime, id: uuid::Uuid) {
    let mut writer = rt.sql().writer().await.expect("open SQL writer");
    let rows = writer
        .execute(SqlStatement {
            sql: "UPDATE notes \
                      SET properties = json_set(properties, '$.trigger_at', ?1) \
                      WHERE id = ?2"
                .to_string(),
            params: vec![
                SqlValue::Text(due_rfc3339()),
                SqlValue::Text(id.to_string()),
            ],
            label: Some("test_repeat_due_again".into()),
        })
        .await
        .expect("make repeat due again");
    assert_eq!(rows, 1, "repeat fixture row updated");
}

async fn claim_for_test(rt: &KhiveRuntime, id: uuid::Uuid, trigger_at: &str) -> DispatchClaim {
    let parsed = trigger_at
        .parse::<DateTime<Utc>>()
        .expect("trigger timestamp");
    claim_pending_event(
        rt,
        "local",
        id,
        dispatch_occurrence_id(id, parsed),
        trigger_at,
        "anonymous:local",
        DispatchLeaseConfig::from_env(),
    )
    .await
    .expect("claim query")
    .expect("claim must succeed on a fresh pending row")
}

fn short_test_lease() -> DispatchLeaseConfig {
    DispatchLeaseConfig {
        ttl: std::time::Duration::from_millis(300),
        renew_every: std::time::Duration::from_millis(30),
    }
}

#[test]
fn final_disposition_counters_are_branch_local() {
    let mut summary = DrainSummary {
        fired: 7,
        advanced: 11,
        ..DrainSummary::default()
    };
    apply_final_disposition(&mut summary, FinalDisposition::Advanced);
    assert_eq!(summary.fired, 7, "advance must not alter prior fire count");
    assert_eq!(summary.advanced, 12);
    assert_eq!(summary.finalized, 1);

    apply_final_disposition(&mut summary, FinalDisposition::Fired);
    assert_eq!(summary.fired, 8);
    assert_eq!(
        summary.advanced, 12,
        "fire must not alter prior advance count"
    );
    assert_eq!(summary.finalized, 2);
}

async fn expire_dispatch_lease_for_test(rt: &KhiveRuntime, id: uuid::Uuid) {
    let mut writer = rt.sql().writer().await.expect("writer");
    let rows = writer
        .execute(SqlStatement {
            sql: "UPDATE notes SET properties = json_set( \
                        properties, '$.lease_expires_at', ?1) WHERE id = ?2"
                .to_string(),
            params: vec![
                SqlValue::Integer(Utc::now().timestamp_micros() - 1),
                SqlValue::Text(id.to_string()),
            ],
            label: Some("test_expire_dispatch_lease".into()),
        })
        .await
        .expect("expire dispatch lease");
    assert_eq!(rows, 1);
}

async fn overwrite_dispatch_receipt_and_expire_for_test(
    rt: &KhiveRuntime,
    id: uuid::Uuid,
    receipt: &Value,
) {
    let mut writer = rt.sql().writer().await.expect("writer");
    let rows = writer
        .execute(SqlStatement {
            sql: "UPDATE notes SET properties = json_set( \
                        properties, '$.dispatch_receipt', json(?1), \
                        '$.lease_expires_at', ?2) WHERE id = ?3"
                .to_string(),
            params: vec![
                SqlValue::Text(serde_json::to_string(receipt).expect("serialize test receipt")),
                SqlValue::Integer(Utc::now().timestamp_micros() - 1),
                SqlValue::Text(id.to_string()),
            ],
            label: Some("test_overwrite_and_expire_dispatch_receipt".into()),
        })
        .await
        .expect("overwrite and expire dispatch receipt");
    assert_eq!(rows, 1);
}

async fn create_marker_directly(rt: &KhiveRuntime, content: &str) {
    let token = rt.authorize(Namespace::local()).expect("authorize marker");
    rt.create_note(&token, "observation", None, content, None, None, vec![])
        .await
        .expect("create simulated side effect");
}

#[test]
fn reminder_subject_marks_and_truncates_the_content_head() {
    let content = format!("  {}\n tail", "x".repeat(90));
    let subject = reminder_subject(&content);
    assert!(subject.starts_with("[Reminder] "));
    assert!(subject.ends_with('…'));
    assert_eq!(subject.chars().count(), "[Reminder] ".chars().count() + 81);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn fired_reminder_delivers_to_creator_after_daemon_actor_changes() {
    let (_tmp, db_path) = tmp_db();
    let creator = "lambda:reminder-owner";
    let daemon_actor = "lambda:replacement-daemon";
    let id = {
        let creator_rt = make_rt_with_actor(&db_path, Some(creator)).await;
        let creator_server = KhiveMcpServer::new(creator_rt.clone()).expect("creator server");
        let remind_ops = serde_json::to_string(&json!([{
            "tool": "schedule.remind",
            "args": {
                "content": "test reminder",
                "at": "2099-01-01T00:00:00Z"
            }
        }]))
        .expect("serialize reminder op");
        let result = creator_server
            .dispatch_request_local(RequestParams {
                ops: remind_ops,
                ..Default::default()
            })
            .await
            .expect("create reminder through schedule.remind");
        let result: Value = serde_json::from_str(&result).expect("reminder result JSON");
        assert_eq!(result["results"][0]["ok"], true, "{result}");
        let id = result["results"][0]["result"]["full_id"]
            .as_str()
            .expect("reminder full_id")
            .parse()
            .expect("reminder UUID");
        let props = get_note_props(&creator_rt, id).await;
        assert_eq!(props["created_by_actor"], creator, "{props}");
        make_repeat_due_again(&creator_rt, id).await;
        id
    };

    let rt = make_rt_with_actor(&db_path, Some(daemon_actor)).await;
    let server = KhiveMcpServer::new(rt.clone()).expect("replacement daemon server");

    let summary = run_pending_events_on(&rt, &server, false)
        .await
        .expect("drain");

    assert_eq!(summary.fired, 1);
    assert_eq!(summary.failed, 0);
    let messages = inbound_reminder_messages(&rt, creator).await;
    let daemon_messages = inbound_reminder_messages(&rt, daemon_actor).await;
    let local_messages = inbound_reminder_messages(&rt, "local").await;
    assert_eq!(
        messages.len(),
        1,
        "one inbound delivery for the creator; daemon={daemon_messages:?}, local={local_messages:?}"
    );
    assert_eq!(messages[0].0, "test reminder");
    assert_eq!(messages[0].1["direction"], "inbound");
    assert_eq!(messages[0].1["to_actor"], creator);
    assert_eq!(messages[0].1["subject"], "[Reminder] test reminder");
    assert!(daemon_messages.is_empty());
    assert!(local_messages.is_empty());
    let props = get_note_props(&rt, id).await;
    assert_eq!(props["status"], "fired");
    assert!(props["fired_at"].as_str().is_some());
}

async fn assert_unprovenanced_reminder_refused(
    daemon_actor: Option<&str>,
    repeat: Option<&str>,
    trigger_at: &str,
) {
    let (_tmp, db_path) = tmp_db();
    let forged_victim = "lambda:forged-victim";
    let rt = make_rt_with_actor(&db_path, daemon_actor).await;
    let server = KhiveMcpServer::new(rt.clone()).expect("server");
    let token = rt
        .authorize(Namespace::local())
        .expect("authorize reminder fixture");
    let note = rt
        .create_note(
            &token,
            "scheduled_event",
            None,
            "unprovenanced reminder",
            None,
            Some(json!({
                "trigger_at": trigger_at,
                "repeat": repeat,
                "status": "pending",
                "event_type": "remind",
                "created_by_actor": forged_victim,
                "payload": null,
                "fired_at": null,
                "cancelled_at": null,
            })),
            vec![],
        )
        .await
        .expect("create hand-written reminder");
    let action_id = create_scheduled_event(
        &rt,
        "local",
        &due_rfc3339(),
        Some("stats()"),
        None,
        "schedule",
    )
    .await;

    let summary = run_pending_events_on(&rt, &server, false)
        .await
        .expect("drain continues after refusing the reminder");
    assert_eq!(summary.scanned, 2);
    assert_eq!(summary.fired, 1);
    assert_eq!(summary.failed, 1);
    assert_eq!(summary.invoked, 1);
    assert_eq!(summary.advanced, 0);
    assert_eq!(summary.retry_pending, 0);
    assert!(summary.missed.is_empty());
    assert_eq!(get_note_props(&rt, action_id).await["status"], "fired");
    for recipient in [forged_victim, daemon_actor.unwrap_or("local"), "local"] {
        assert!(inbound_reminder_messages(&rt, recipient).await.is_empty());
    }

    let response = server
        .dispatch_request_local(RequestParams {
            ops: format!("get(id=\"{}\")", note.id),
            presentation: Some("verbose".to_string()),
            ..Default::default()
        })
        .await
        .expect("inspect refused reminder through get");
    let response: Value = serde_json::from_str(&response).expect("get response JSON");
    assert_eq!(response["results"][0]["ok"], true, "{response}");
    let props = &response["results"][0]["result"]["properties"];
    assert_eq!(props["status"], "failed", "{response}");
    assert_eq!(props["trigger_at"], trigger_at);
    assert_eq!(props["repeat"], json!(repeat));
    assert!(props["fired_at"].is_null());
    assert!(props["delivery_failed_at"].as_str().is_some());
    let error = props["delivery_error"].as_str().expect("visible refusal");
    assert!(error.contains("missing immutable creator provenance"));
    assert!(error.contains("no recipient selected"));
    assert!(error.contains("schedule.remind"));
    let receipt = &props["dispatch_receipt"];
    assert_eq!(receipt["state"], "not_invoked");
    assert_eq!(receipt["actor"], "anonymous:local");
    assert_eq!(receipt["error"], error);
    assert!(receipt["completed_at"].as_i64().is_some());

    let events = rt
        .events(&token)
        .expect("event store")
        .query_events(
            EventFilter {
                verbs: vec!["schedule.remind.fire".to_string()],
                ..Default::default()
            },
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .expect("query delivery failure events");
    assert!(events.items.is_empty(), "no delivery was attempted");
    let before = get_raw_note_properties(&rt, note.id).await;
    let second = run_pending_events_on(&rt, &server, false)
        .await
        .expect("second drain");
    assert_eq!(second.scanned, 0);
    assert_eq!(second.invoked, 0);
    assert_eq!(get_raw_note_properties(&rt, note.id).await, before);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn unprovenanced_reminder_ignores_forged_actor_property() {
    for daemon_actor in [Some("lambda:daemon-owner"), None] {
        assert_unprovenanced_reminder_refused(daemon_actor, None, &due_rfc3339()).await;
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn unprovenanced_repeating_reminder_is_terminal_and_visible_without_delivery() {
    for daemon_actor in [Some("lambda:daemon-owner"), None] {
        for repeat in ["daily", "weekly"] {
            assert_unprovenanced_reminder_refused(daemon_actor, Some(repeat), &due_rfc3339()).await;
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn unprovenanced_missed_reminder_is_refused_before_rearming() {
    for daemon_actor in [Some("lambda:daemon-owner"), None] {
        assert_unprovenanced_reminder_refused(daemon_actor, Some("daily"), "2000-01-01T00:00:00Z")
            .await;
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn repeating_reminder_delivers_on_consecutive_fires() {
    let (_tmp, db_path) = tmp_db();
    let actor = "lambda:repeat-owner";
    let rt = make_rt_with_actor(&db_path, Some(actor)).await;
    let server = KhiveMcpServer::new(rt.clone()).expect("server");
    let id =
        create_scheduled_event(&rt, "local", &due_rfc3339(), None, Some("daily"), "remind").await;

    let first = run_pending_events_on(&rt, &server, false)
        .await
        .expect("first drain");
    assert_eq!(first.advanced, 1);
    assert_eq!(inbound_reminder_messages(&rt, actor).await.len(), 1);

    make_repeat_due_again(&rt, id).await;
    let second = run_pending_events_on(&rt, &server, false)
        .await
        .expect("second drain");

    assert_eq!(second.advanced, 1);
    assert_eq!(second.failed, 0);
    assert_eq!(
        inbound_reminder_messages(&rt, actor).await.len(),
        2,
        "each fire delivers one inbound message"
    );
}

async fn assert_reminder_delivery_failure_attribution(creator_actor: Option<&str>) {
    let (_tmp, db_path) = tmp_db();
    let namespace = "reminder-failure-tenant";
    let daemon_actor = "lambda:failure-daemon";
    let creator_rt = make_rt_with_actor(&db_path, creator_actor).await;
    let id =
        create_scheduled_event(&creator_rt, namespace, &due_rfc3339(), None, None, "remind").await;
    let expected_actor = creator_actor
        .map(|actor| format!("actor:{actor}"))
        .unwrap_or_else(|| "anonymous:local".to_string());
    let recipient = creator_actor.unwrap_or("local");
    let cfg = RuntimeConfig {
        db_path: Some(std::path::PathBuf::from(&db_path)),
        default_namespace: Namespace::parse("local").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: std::sync::Arc::new(DenyCommSendGate),
        actor_id: Some(daemon_actor.to_string()),
        ..Default::default()
    };
    let rt = KhiveRuntime::new(cfg).expect("runtime");
    let packs = vec!["kg".to_string(), "comm".to_string(), "schedule".to_string()];
    let server = KhiveMcpServer::with_packs(rt.clone(), &packs)
        .expect("server with required reminder delivery pack");
    let action_id = create_scheduled_event(
        &rt,
        namespace,
        &due_rfc3339(),
        Some("stats()"),
        None,
        "schedule",
    )
    .await;
    let mut writer = rt.sql().writer().await.expect("open SQL writer");
    let reordered = writer
        .execute(SqlStatement {
            sql: "UPDATE notes SET created_at = CASE id WHEN ?1 THEN 1 WHEN ?2 THEN 2 END \
                      WHERE id IN (?1, ?2)"
                .to_string(),
            params: vec![
                SqlValue::Text(id.to_string()),
                SqlValue::Text(action_id.to_string()),
            ],
            label: Some("test_reminder_failure_precedes_valid_action".into()),
        })
        .await
        .expect("order reminder before action");
    assert_eq!(reordered, 2);
    drop(writer);

    let summary = run_pending_events_on(&rt, &server, false)
        .await
        .expect("drain continues after failure");

    assert_eq!(summary.scanned, 2);
    assert_eq!(summary.failed, 1);
    assert_eq!(summary.fired, 1);
    assert_eq!(summary.retry_pending, 1);
    assert!(inbound_reminder_messages(&rt, recipient).await.is_empty());
    assert!(inbound_reminder_messages(&rt, daemon_actor)
        .await
        .is_empty());
    let props = get_note_props(&rt, id).await;
    assert_eq!(
        props["status"], "pending",
        "failed one-shot must remain retryable"
    );
    assert!(
        props["delivery_error"]
            .as_str()
            .is_some_and(|error| error.contains("denied by delivery-failure test")),
        "delivery error must be visible on the reminder row: {props:?}"
    );
    assert!(props["delivery_failed_at"].as_str().is_some());
    assert_eq!(props["dispatch_receipt"]["actor"], expected_actor);
    assert_eq!(props["dispatch_receipt"]["state"], "failed");
    let action_props = get_note_props(&rt, action_id).await;
    assert_eq!(action_props["status"], "fired");
    assert!(action_props["fired_at"].as_str().is_some());

    let token = rt
        .authorize(Namespace::parse(namespace).expect("namespace"))
        .expect("authorize");
    let events = rt
        .events(&token)
        .expect("event store")
        .query_events(
            EventFilter {
                verbs: vec!["schedule.remind.fire".to_string()],
                ..Default::default()
            },
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .expect("query reminder failure events");
    assert_eq!(events.items.len(), 1, "one reminder delivery failure event");
    let event = &events.items[0];
    assert_eq!(event.outcome, EventOutcome::Error);
    assert_eq!(event.target_id, Some(id));
    assert_eq!(event.actor, expected_actor);
    assert_eq!(event.namespace, namespace);
    assert_eq!(event.payload["recipient_actor"], recipient);
    assert_eq!(event.payload["scheduled_event_id"], id.to_string());
    assert!(event.payload["error"]
        .as_str()
        .is_some_and(|error| error.contains("denied by delivery-failure test")));
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn reminder_delivery_failure_is_persisted_audited_and_drain_continues() {
    assert_reminder_delivery_failure_attribution(Some("lambda:failure-owner")).await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn reminder_delivery_failure_preserves_verified_anonymous_creator_attribution() {
    assert_reminder_delivery_failure_attribution(None).await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn due_event_is_fired() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;

    // Create a past-due schedule event. Use stats() as the action since it's
    // a valid, registered verb that has no side-effects that need a
    // namespace argument check. `due_rfc3339` is only a few seconds
    // overdue — inside the missed-event grace window — so this exercises
    // the normal fire path, not the ADR-106 missed path.
    let past = due_rfc3339();
    let id = create_scheduled_event(&rt, "local", &past, Some("stats()"), None, "schedule").await;

    let summary = drain_for_test(&db_path).await.expect("drain");

    assert!(summary.scanned >= 1, "must have scanned the due event");
    assert!(
        summary.fired >= 1 || summary.advanced >= 1,
        "must fire or advance"
    );

    let props = get_note_props(&rt, id).await;
    let status = props["status"].as_str().unwrap_or("");
    assert!(
        status == "fired" || status == "pending",
        "status must be fired or pending (repeat), got {status:?}"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn future_event_is_skipped() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;

    let future = "2099-01-01T00:00:00Z";
    let id = create_scheduled_event(&rt, "local", future, Some("stats()"), None, "schedule").await;

    let summary = drain_for_test(&db_path).await.expect("drain");

    // The future event must not be fired. The drain may skip it via the SQL
    // pre-filter (scanned=0, skipped_not_due=0) or via the Rust timestamp
    // check (scanned=1, skipped_not_due=1) — either is correct; the key
    // invariant is that fired=0, advanced=0.
    assert_eq!(summary.fired, 0, "future event must not be fired");
    assert_eq!(summary.advanced, 0, "future event must not be advanced");

    let props = get_note_props(&rt, id).await;
    assert_eq!(
        props["status"].as_str(),
        Some("pending"),
        "future event must remain pending"
    );
}

/// A due event stored with a positive `trigger_at` offset (whose RFC 3339
/// string sorts lexicographically after a UTC "now" string) must still
/// fire — proves the SQL due-ness predicate compares chronologically via
/// `datetime(...)`, not as raw text.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn due_event_with_positive_offset_trigger_at_fires() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;

    // Chronologically 10s overdue (well inside the default 300s grace
    // window), but formatted at +04:00 wall time so the RFC 3339 string
    // sorts AFTER a UTC `now` string as raw text.
    let trigger_instant = Utc::now() - Duration::seconds(10);
    let plus_four = FixedOffset::east_opt(4 * 3600).expect("valid offset");
    let trigger_at = trigger_instant.with_timezone(&plus_four).to_rfc3339();
    assert!(
        trigger_at.as_str() > now_rfc3339_for_ordering_check().as_str(),
        "test setup: {trigger_at:?} must sort AFTER a UTC now-string as raw text \
             for this to exercise the lexicographic-ordering bug"
    );

    let id =
        create_scheduled_event(&rt, "local", &trigger_at, Some("stats()"), None, "schedule").await;

    let summary = drain_for_test(&db_path).await.expect("drain");

    assert!(
        summary.fired >= 1 || summary.advanced >= 1,
        "a due event stored with a positive offset must still fire, got {summary:?}"
    );

    let props = get_note_props(&rt, id).await;
    let status = props["status"].as_str().unwrap_or("");
    assert!(
        status == "fired" || status == "pending",
        "status must be fired or pending (repeat), got {status:?}"
    );
}

/// A future event stored with a negative `trigger_at` offset (whose RFC
/// 3339 string sorts lexicographically before a UTC "now" string) must
/// NOT fire — the mirror case of the positive-offset test above, with the
/// Rust-side `trigger_at > now` re-check as an additional backstop.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn future_event_with_negative_offset_trigger_at_is_not_fired() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;

    // Chronologically 2h in the future, but formatted at -08:00 wall
    // time so the RFC 3339 string sorts BEFORE a UTC `now` string as raw
    // text (a false positive under naive lexicographic comparison).
    let trigger_instant = Utc::now() + Duration::hours(2);
    let minus_eight = FixedOffset::west_opt(8 * 3600).expect("valid offset");
    let trigger_at = trigger_instant.with_timezone(&minus_eight).to_rfc3339();
    assert!(
        trigger_at.as_str() < now_rfc3339_for_ordering_check().as_str(),
        "test setup: {trigger_at:?} must sort BEFORE a UTC now-string as raw text \
             for this to exercise the false-positive path"
    );

    let id =
        create_scheduled_event(&rt, "local", &trigger_at, Some("stats()"), None, "schedule").await;

    let summary = drain_for_test(&db_path).await.expect("drain");

    assert_eq!(
        summary.fired, 0,
        "a chronologically future event must not be fired, got {summary:?}"
    );
    assert_eq!(
        summary.advanced, 0,
        "a chronologically future event must not be advanced, got {summary:?}"
    );

    let props = get_note_props(&rt, id).await;
    assert_eq!(
        props["status"].as_str(),
        Some("pending"),
        "future event must remain pending"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn fired_event_is_idempotent() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;

    let past = due_rfc3339();
    let id = create_scheduled_event(&rt, "local", &past, Some("stats()"), None, "schedule").await;

    // First drain — fires the event.
    let s1 = drain_for_test(&db_path).await.expect("drain 1");
    assert!(s1.scanned >= 1);

    // Second drain — event is now status="fired", not "pending"; must not re-fire.
    let s2 = drain_for_test(&db_path).await.expect("drain 2");
    assert_eq!(s2.scanned, 0, "no pending events on second drain");
    assert_eq!(s2.fired, 0, "no new fires on second drain");

    let props = get_note_props(&rt, id).await;
    let fired_at_1 = props["fired_at"].as_str().unwrap_or("").to_string();
    assert!(
        !fired_at_1.is_empty(),
        "fired_at must be set after first drain"
    );

    // fired_at must not change on the second drain (idempotent).
    let props2 = get_note_props(&rt, id).await;
    assert_eq!(
        props2["fired_at"].as_str().unwrap_or(""),
        fired_at_1.as_str(),
        "fired_at must not change on second drain"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn daily_repeat_advances() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;

    // Use a past (but in-grace) trigger_at with daily repeat.
    let past = due_rfc3339();
    let id = create_scheduled_event(
        &rt,
        "local",
        &past,
        Some("stats()"),
        Some("daily"),
        "schedule",
    )
    .await;

    let summary = drain_for_test(&db_path).await.expect("drain");

    assert!(
        summary.advanced >= 1,
        "daily event must be advanced, not fired"
    );

    let props = get_note_props(&rt, id).await;
    assert_eq!(
        props["status"].as_str(),
        Some("pending"),
        "after advance, status must be pending"
    );
    let new_trigger = props["trigger_at"]
        .as_str()
        .expect("trigger_at must be set");
    let new_ts: DateTime<Utc> = new_trigger.parse().expect("parseable ts");
    let original: DateTime<Utc> = past.parse().unwrap();
    assert_eq!(
        new_ts,
        original + Duration::days(1),
        "daily advance must add 1 day"
    );
}

/// Repeat advancement must preserve the original
/// `trigger_at` timezone offset — not silently re-serialize the advanced
/// occurrence as UTC. A `+04:00` schedule that fires and advances must
/// still carry `+04:00` (and the same local wall-clock hour) on its next
/// occurrence, not drift to a different wall-clock hour under `+00:00`.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn daily_repeat_advance_preserves_original_offset() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;

    // Chronologically a few seconds ago (in-grace), formatted at a
    // non-UTC +04:00 wall-clock offset — the exact shape
    // `khive-pack-schedule` round-trips verbatim from the caller.
    let plus_four = FixedOffset::east_opt(4 * 3600).expect("valid offset");
    let trigger_instant = Utc::now() - Duration::seconds(5);
    let past = trigger_instant.with_timezone(&plus_four).to_rfc3339();

    let id = create_scheduled_event(
        &rt,
        "local",
        &past,
        Some("stats()"),
        Some("daily"),
        "schedule",
    )
    .await;

    let summary = drain_for_test(&db_path).await.expect("drain");
    assert!(
        summary.advanced >= 1,
        "daily event with a non-UTC offset must be advanced, not fired"
    );

    let props = get_note_props(&rt, id).await;
    let new_trigger = props["trigger_at"]
        .as_str()
        .expect("trigger_at must be set");

    // The advanced occurrence must still carry the ORIGINAL +04:00
    // offset, not be silently re-serialized as UTC (+00:00).
    assert!(
        new_trigger.ends_with("+04:00"),
        "advanced trigger_at must preserve the original +04:00 offset, got {new_trigger:?}"
    );

    let new_dt = DateTime::parse_from_rfc3339(new_trigger).expect("parseable advanced ts");
    let original_dt = DateTime::parse_from_rfc3339(&past).expect("parseable original ts");
    assert_eq!(
        *new_dt.offset(),
        plus_four,
        "advanced trigger_at offset must equal the original +04:00 offset"
    );
    assert_eq!(
        new_dt.with_timezone(&Utc),
        original_dt.with_timezone(&Utc) + Duration::days(1),
        "daily advance must add exactly 1 day to the chronological instant"
    );
    // Wall-clock hour must be unchanged (the drift this issue reports):
    // same local time-of-day at the same offset, one day later.
    assert_eq!(
        new_dt.time(),
        original_dt.time(),
        "advanced occurrence must retain the same local wall-clock time"
    );
}

/// The drain must keep accepting the *same* `trigger_at` grammar the
/// write boundary validates with (`khive-pack-schedule`'s
/// `at.parse::<DateTime<Utc>>()`, which is chrono's relaxed RFC 3339
/// form) — not narrow to strict `DateTime::parse_from_rfc3339`. A
/// legacy stored timestamp using a space instead of `T` and an offset
/// without a colon (e.g. `2026-07-14 09:00:00+0400`) must still be
/// recognized as due and advanced, not silently skipped forever as
/// "unparseable".
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn relaxed_legacy_grammar_repeat_advance_preserves_offset() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;

    let plus_four = FixedOffset::east_opt(4 * 3600).expect("valid offset");
    // Whole-second precision: the relaxed `%z` format below drops
    // fractional seconds, so the fixture must match what actually
    // round-trips through it.
    let trigger_instant =
        DateTime::from_timestamp((Utc::now() - Duration::seconds(5)).timestamp(), 0)
            .expect("valid timestamp");
    let past_relaxed = trigger_instant
        .with_timezone(&plus_four)
        .format("%Y-%m-%d %H:%M:%S%z")
        .to_string();
    assert!(
        past_relaxed.contains(' ') && !past_relaxed.contains('T'),
        "fixture must use the relaxed space separator, got {past_relaxed:?}"
    );

    let id = create_scheduled_event(
        &rt,
        "local",
        &past_relaxed,
        Some("stats()"),
        Some("daily"),
        "schedule",
    )
    .await;

    let summary = drain_for_test(&db_path).await.expect("drain");
    assert!(
        summary.advanced >= 1,
        "a relaxed-grammar legacy trigger_at must still be recognized as due and \
             advanced, not skipped as unparseable"
    );
    assert_eq!(
        summary.skipped_not_due, 0,
        "relaxed-grammar trigger_at must not be treated as unparseable"
    );

    let props = get_note_props(&rt, id).await;
    let new_trigger = props["trigger_at"]
        .as_str()
        .expect("trigger_at must be set");
    assert!(
        new_trigger.ends_with("+04:00"),
        "advanced trigger_at must preserve the original +04:00 offset, got {new_trigger:?}"
    );

    let new_dt = DateTime::parse_from_rfc3339(new_trigger).expect("parseable advanced ts");
    assert_eq!(
        new_dt.with_timezone(&Utc),
        trigger_instant + Duration::days(1),
        "daily advance must add exactly 1 day to the chronological instant"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn namespace_isolation() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;

    // Create a due event in namespace "ns-a". The action is stats() which
    // doesn't create notes, so we can't verify write-landing-in-ns-a directly
    // through this drain. Instead we verify the drain scans and fires the event
    // in ns-a without touching the ns-b namespace counts.
    let ns_a = "ns-a";
    let ns_b = "ns-b";
    let past = due_rfc3339();

    let id_a = create_scheduled_event(&rt, ns_a, &past, Some("stats()"), None, "schedule").await;

    // Create a future event in ns-b that must not be fired.
    let _id_b = create_scheduled_event(
        &rt,
        ns_b,
        "2099-01-01T00:00:00Z",
        Some("stats()"),
        None,
        "schedule",
    )
    .await;

    let summary = drain_for_test(&db_path).await.expect("drain");

    // Only the ns-a event should have been processed.
    assert!(summary.scanned >= 1);
    assert!(summary.fired >= 1 || summary.advanced >= 1);

    // ns-a event is fired.
    let token_a = rt.authorize(Namespace::parse(ns_a).unwrap()).expect("auth");
    let store_a = rt.notes(&token_a).expect("notes");
    let note_a = store_a.get_note(id_a).await.expect("get").expect("exists");
    let status_a = note_a
        .properties
        .as_ref()
        .and_then(|p| p.get("status"))
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        status_a == "fired" || status_a == "pending",
        "ns-a event must be fired or advanced, got {status_a:?}"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn concurrent_replay_preserves_each_events_actor_and_namespace() {
    let (_tmp, db_path) = tmp_db();
    let creator_runtime = |actor: &str| {
        KhiveRuntime::new(RuntimeConfig {
            db_path: Some(std::path::PathBuf::from(&db_path)),
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            actor_id: Some(actor.to_string()),
            packs: vec!["kg".to_string(), "schedule".to_string()],
            ..Default::default()
        })
        .expect("creator runtime")
    };
    let actor_a = "lambda:scheduled-a";
    let actor_b = "lambda:scheduled-b";
    let ns_a = "schedule-tenant-a";
    let ns_b = "schedule-tenant-b";
    let rt_a = creator_runtime(actor_a);
    let rt_b = creator_runtime(actor_b);
    create_scheduled_event(
        &rt_a,
        ns_a,
        &due_rfc3339(),
        Some("create(kind=\"observation\", content=\"isolated-a\")"),
        None,
        "schedule",
    )
    .await;
    create_scheduled_event(
        &rt_b,
        ns_b,
        &due_rfc3339(),
        Some("create(kind=\"observation\", content=\"isolated-b\")"),
        None,
        "schedule",
    )
    .await;

    let gate = std::sync::Arc::new(CaptureReplayIdentityGate::default());
    let daemon_rt = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(std::path::PathBuf::from(&db_path)),
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: gate.clone(),
        actor_id: Some("lambda:daemon".to_string()),
        packs: vec!["kg".to_string(), "schedule".to_string()],
        ..Default::default()
    })
    .expect("daemon runtime");
    let server = KhiveMcpServer::new(daemon_rt.clone()).expect("server");

    let (drain_a, drain_b) = tokio::join!(
        run_pending_events_on(&daemon_rt, &server, false),
        run_pending_events_on(&daemon_rt, &server, false),
    );
    let drain_a = drain_a.expect("drain A");
    let drain_b = drain_b.expect("drain B");
    assert_eq!(
        drain_a.failed + drain_b.failed,
        0,
        "{drain_a:?} {drain_b:?}"
    );
    assert_eq!(
        drain_a.fired + drain_b.fired,
        2,
        "both isolated scheduled actions must fire exactly once"
    );

    let mut seen = gate.creates.lock().expect("capture lock").clone();
    seen.sort();
    let mut expected = vec![
        ("actor".to_string(), actor_a.to_string(), ns_a.to_string()),
        ("actor".to_string(), actor_b.to_string(), ns_b.to_string()),
    ];
    expected.sort();
    assert_eq!(
        seen, expected,
        "concurrent replay must not swap actor or namespace identities"
    );
    assert_eq!(
        note_content_count_in_namespace(&daemon_rt, ns_a, "observation", "isolated-a").await,
        1
    );
    assert_eq!(
        note_content_count_in_namespace(&daemon_rt, ns_b, "observation", "isolated-b").await,
        1
    );
    assert_eq!(
        note_content_count_in_namespace(&daemon_rt, ns_a, "observation", "isolated-b").await,
        0,
        "tenant B's action must not land in tenant A"
    );
    assert_eq!(
        note_content_count_in_namespace(&daemon_rt, ns_b, "observation", "isolated-a").await,
        0,
        "tenant A's action must not land in tenant B"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn anonymous_creator_replay_preserves_anonymous_actor_kind() {
    let (_tmp, db_path) = tmp_db();
    let creator_rt = make_rt(&db_path).await;
    create_scheduled_event(
        &creator_rt,
        "local",
        &due_rfc3339(),
        Some("create(kind=\"observation\", content=\"anonymous replay marker\")"),
        None,
        "schedule",
    )
    .await;

    let gate = std::sync::Arc::new(CaptureReplayIdentityGate::default());
    let daemon_rt = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(std::path::PathBuf::from(&db_path)),
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: gate.clone(),
        actor_id: Some("lambda:daemon".to_string()),
        packs: vec!["kg".to_string(), "schedule".to_string()],
        ..Default::default()
    })
    .expect("daemon runtime");
    let server = KhiveMcpServer::new(daemon_rt.clone()).expect("server");

    let summary = run_pending_events_on(&daemon_rt, &server, false)
        .await
        .expect("drain");
    assert_eq!(summary.fired, 1);
    assert_eq!(summary.failed, 0);
    assert_eq!(
        gate.creates.lock().expect("capture lock").as_slice(),
        &[(
            "anonymous".to_string(),
            "local".to_string(),
            "local".to_string(),
        )],
        "verified anonymous provenance must not become authenticated actor:local"
    );
    assert_eq!(
        note_content_count(&daemon_rt, "observation", "anonymous replay marker").await,
        1
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn dispatch_failure_does_not_abort_drain() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;

    // Create a past-due (but in-grace) event with an invalid action DSL
    // (verb not registered).
    let past = due_rfc3339();
    let _id_bad = create_scheduled_event(
        &rt,
        "local",
        &past,
        Some("stats()"), // valid — but let's add a second event with a broken action
        None,
        "schedule",
    )
    .await;
    // Second event with broken action.
    let id_bad2 = create_scheduled_event(
        &rt,
        "local",
        &past,
        Some("this_verb_does_not_exist(foo=\"bar\")"),
        None,
        "schedule",
    )
    .await;

    let summary = drain_for_test(&db_path)
        .await
        .expect("drain must not abort");

    // Both events were scanned. The bad one produced a failure.
    assert!(summary.scanned >= 2, "both events must be scanned");
    assert!(
        summary.failed >= 1 || summary.fired >= 1,
        "at least one event processed (failed or fired)"
    );

    // The drain still ran to completion (no panic / early return).
    let props_bad2 = get_note_props(&rt, id_bad2).await;
    let _ = props_bad2["status"].as_str(); // just verify it's accessible
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn legacy_scheduled_action_without_creator_fails_closed() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt_with_actor(&db_path, Some("lambda:daemon")).await;
    let server = KhiveMcpServer::new(rt.clone()).expect("server");
    let action = "create(kind=\"observation\", content=\"legacy action marker\")";
    let token = rt
        .authorize(Namespace::parse("local").expect("namespace"))
        .expect("authorize");
    let note = rt
        .create_note(
            &token,
            "scheduled_event",
            None,
            action,
            None,
            Some(json!({
                "trigger_at": "2000-01-01T00:00:00Z",
                "repeat": null,
                "status": "pending",
                "event_type": "schedule",
                "payload": action,
                "fired_at": null,
                "cancelled_at": null,
            })),
            vec![],
        )
        .await
        .expect("create legacy scheduled action");

    let summary = run_pending_events_on(&rt, &server, false)
        .await
        .expect("drain");

    assert_eq!(summary.failed, 1, "legacy row must report one failure");
    assert_eq!(summary.fired, 0, "unsafe action must never count as fired");
    assert_eq!(
        note_content_count(&rt, "observation", "legacy action marker").await,
        0,
        "missing attribution must never inherit daemon authority"
    );
    let props = get_note_props(&rt, note.id).await;
    assert_eq!(props["status"], "failed", "{props}");
    assert!(
        props["dispatch_error"]
            .as_str()
            .is_some_and(|error| error.contains("immutable creator provenance")),
        "policy error must explain why replay was refused: {props}"
    );
    assert!(props["dispatch_failed_at"].as_str().is_some(), "{props}");
    assert_eq!(
        props["dispatch_receipt"]["state"],
        DispatchReceiptState::NotInvoked.as_str(),
        "the durable claim receipt must survive provenance refusal: {props}"
    );
    assert_eq!(
        props["dispatch_receipt"]["actor"], "anonymous:local",
        "a refused generic row has no verified creator and must not inherit daemon attribution: {props}"
    );
    assert!(
        props["dispatch_receipt"]["completed_at"].as_i64().is_some(),
        "{props}"
    );
    assert!(
        props["dispatch_receipt"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("immutable creator provenance")),
        "{props}"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn forged_created_by_actor_property_cannot_authorize_replay() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt_with_actor(&db_path, Some("lambda:daemon")).await;
    let server = KhiveMcpServer::new(rt.clone()).expect("server");
    let action = "create(kind=\"observation\", content=\"forged actor marker\")";
    let token = rt
        .authorize(Namespace::parse("local").expect("namespace"))
        .expect("authorize");
    let note = rt
        .create_note(
            &token,
            "scheduled_event",
            None,
            action,
            None,
            Some(json!({
                "trigger_at": due_rfc3339(),
                "repeat": null,
                "status": "pending",
                "event_type": "schedule",
                // Generic note properties are writable by the caller.
                // This claim has no pack-written provenance event.
                "created_by_actor": "lambda:privileged-victim",
                "payload": action,
                "fired_at": null,
                "cancelled_at": null,
            })),
            vec![],
        )
        .await
        .expect("create forged scheduled action");

    let summary = run_pending_events_on(&rt, &server, false)
        .await
        .expect("drain");

    assert_eq!(summary.failed, 1);
    assert_eq!(summary.fired, 0);
    assert_eq!(
        note_content_count(&rt, "observation", "forged actor marker").await,
        0,
        "caller-editable actor metadata must never become replay authority"
    );
    let props = get_note_props(&rt, note.id).await;
    assert_eq!(props["status"], "failed", "{props}");
    assert!(
        props["dispatch_error"]
            .as_str()
            .is_some_and(|error| error.contains("immutable creator provenance")),
        "{props}"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn second_actor_cannot_rewrite_provenanced_schedule_intent() {
    let (_tmp, db_path) = tmp_db();
    let gate = std::sync::Arc::new(DenyAttackerCreateGate);
    let owner = "lambda:schedule-owner";
    let attacker = "lambda:schedule-attacker";
    let original_action =
        "create(kind=\"observation\", content=\"owner-approved schedule intent\")";
    let forged_action =
        "create(kind=\"observation\", content=\"attacker-selected schedule intent\")";

    let owner_rt = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(std::path::PathBuf::from(&db_path)),
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: gate.clone(),
        actor_id: Some(owner.to_string()),
        packs: vec!["kg".to_string(), "schedule".to_string()],
        ..Default::default()
    })
    .expect("owner runtime");
    let note_id = create_scheduled_event(
        &owner_rt,
        "local",
        &due_rfc3339(),
        Some(original_action),
        None,
        "schedule",
    )
    .await;

    // Actor B is allowed to call generic `update`, but is denied the
    // target `create` verb. Before the schedule-managed mutation fence,
    // this patch replaced actor A's payload while retaining A's immutable
    // provenance, so trigger-time Gate evaluation ran as A and allowed it.
    let attacker_rt = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(std::path::PathBuf::from(&db_path)),
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: gate.clone(),
        actor_id: Some(attacker.to_string()),
        packs: vec!["kg".to_string(), "schedule".to_string()],
        ..Default::default()
    })
    .expect("attacker runtime");
    let attacker_server = KhiveMcpServer::new(attacker_rt).expect("attacker server");
    let update_ops = json!([{
        "tool": "update",
        "args": {
            "id": note_id.to_string(),
            "kind": "note",
            "properties": {
                "payload": forged_action,
                "trigger_at": due_rfc3339(),
                "repeat": "daily",
                "status": "pending",
                "event_type": "schedule"
            }
        }
    }])
    .to_string();
    let update_response = attacker_server
        .dispatch_request_local(RequestParams {
            ops: update_ops,
            ..Default::default()
        })
        .await
        .expect("generic update returns an operation envelope");
    let update_response: Value =
        serde_json::from_str(&update_response).expect("update response JSON");
    assert_eq!(
        update_response["results"][0]["ok"], false,
        "{update_response}"
    );
    // Two layered defenses reject this: the KG update handler refuses the
    // `scheduled_event` kind outright, and the runtime curation fence
    // refuses schedule-managed notes. Whichever layer fires first, the
    // rejection must name the scheduled-event trust boundary.
    let update_error = update_response["results"][0]["error"]["message"]
        .as_str()
        .expect("error.message is text");
    assert!(
        update_error.contains("schedule-managed")
            || update_error.contains("scheduled_event notes are not editable"),
        "the generic mutation fence must reject executable schedule changes: \
             {update_response}"
    );

    let unchanged = get_note_props(&owner_rt, note_id).await;
    assert_eq!(unchanged["payload"], original_action, "{unchanged}");
    assert_eq!(unchanged["repeat"], Value::Null, "{unchanged}");

    let daemon_rt = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(std::path::PathBuf::from(&db_path)),
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate,
        actor_id: Some("lambda:daemon".to_string()),
        packs: vec!["kg".to_string(), "schedule".to_string()],
        ..Default::default()
    })
    .expect("daemon runtime");
    let daemon_server = KhiveMcpServer::new(daemon_rt.clone()).expect("daemon server");
    let summary = run_pending_events_on(&daemon_rt, &daemon_server, false)
        .await
        .expect("drain");

    assert_eq!(summary.failed, 0, "{summary:?}");
    assert_eq!(summary.fired, 1, "{summary:?}");
    assert_eq!(
        note_content_count(&daemon_rt, "observation", "owner-approved schedule intent").await,
        1
    );
    assert_eq!(
        note_content_count(
            &daemon_rt,
            "observation",
            "attacker-selected schedule intent"
        )
        .await,
        0,
        "actor B's rejected payload must never replay with actor A's authority"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn scheduled_action_replay_uses_creator_not_daemon_identity() {
    let (_tmp, db_path) = tmp_db();
    let creator_cfg = RuntimeConfig {
        db_path: Some(std::path::PathBuf::from(&db_path)),
        default_namespace: Namespace::parse("local").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: std::sync::Arc::new(DenyCreatorCreateGate),
        actor_id: Some("lambda:schedule-owner".to_string()),
        packs: vec!["kg".to_string(), "schedule".to_string()],
        ..Default::default()
    };
    let creator_rt = KhiveRuntime::new(creator_cfg).expect("creator runtime");
    let action = "create(kind=\"observation\", content=\"identity fence marker\")";
    let note_id = create_scheduled_event(
        &creator_rt,
        "local",
        &due_rfc3339(),
        Some(action),
        None,
        "schedule",
    )
    .await;

    let daemon_cfg = RuntimeConfig {
        db_path: Some(std::path::PathBuf::from(&db_path)),
        default_namespace: Namespace::parse("local").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: std::sync::Arc::new(DenyCreatorCreateGate),
        actor_id: Some("lambda:daemon".to_string()),
        packs: vec!["kg".to_string(), "schedule".to_string()],
        ..Default::default()
    };
    let rt = KhiveRuntime::new(daemon_cfg).expect("daemon runtime");
    let server = KhiveMcpServer::new(rt.clone()).expect("server");

    let summary = run_pending_events_on(&rt, &server, false)
        .await
        .expect("drain");

    assert_eq!(summary.failed, 1, "creator gate denial must be visible");
    assert_eq!(
        note_content_count(&rt, "observation", "identity fence marker").await,
        0,
        "replay as the daemon would bypass the creator's denial"
    );
    let props = get_note_props(&rt, note_id).await;
    assert_eq!(
        props["status"], "pending",
        "failed one-shot remains retryable"
    );
    assert_eq!(summary.retry_pending, 1);
    assert!(
        props["dispatch_error"]
            .as_str()
            .is_some_and(|error| error.contains("creator is not authorized")),
        "dispatch failure must be persisted: {props}"
    );
    assert!(props["dispatch_failed_at"].as_str().is_some(), "{props}");
}

/// A canonical `schedule.schedule` payload that passes write-time
/// validation must dispatch with zero failures at trigger time, proving
/// write-time acceptance and trigger-time replay agree. Runs serially
/// with a raised writer-pool checkout timeout to remove CI scheduler
/// contention as a source of flakiness — see "Writer-pool checkout
/// contention under CI" in `crates/khive-mcp/docs/pending-events.md`.
#[tokio::test]
#[serial_test::serial]
#[serial_test::serial(config_ledger)]
async fn replayable_action_dispatches_without_failure_at_trigger_time() {
    struct RestoreTimeout(Option<String>);
    impl Drop for RestoreTimeout {
        fn drop(&mut self) {
            match self.0.take() {
                Some(v) => std::env::set_var("KHIVE_CHECKOUT_TIMEOUT_SECS", v),
                None => std::env::remove_var("KHIVE_CHECKOUT_TIMEOUT_SECS"),
            }
        }
    }
    let prior_timeout = std::env::var("KHIVE_CHECKOUT_TIMEOUT_SECS").ok();
    let _restore = RestoreTimeout(prior_timeout.clone());
    // #705's coverage margin applies to this test, not every workspace
    // reader (#2367). Preserve a larger explicit pool override as well.
    let replay_floor = std::env::var("KHIVE_TEST_REPLAY_CHECKOUT_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(120)
        .max(120);
    let effective_timeout = prior_timeout
        .as_deref()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|ambient| ambient.max(replay_floor))
        .unwrap_or(replay_floor);
    std::env::set_var("KHIVE_CHECKOUT_TIMEOUT_SECS", effective_timeout.to_string());

    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;

    let past = due_rfc3339();
    let id = create_scheduled_event(
        &rt,
        "local",
        &past,
        Some("schedule.remind(content=\"ping\", at=\"2099-01-01T00:00:00Z\")"),
        None,
        "schedule",
    )
    .await;

    let summary = drain_for_test(&db_path).await.expect("drain");

    assert_eq!(
        summary.failed, 0,
        "a write-time-replayable action must dispatch cleanly at trigger time"
    );
    assert!(
        summary.fired >= 1 || summary.advanced >= 1,
        "the event must be processed"
    );

    let props = get_note_props(&rt, id).await;
    assert_eq!(props["status"].as_str(), Some("fired"));
}

/// A legacy stored action containing a `$prev` reference must be
/// rejected by `dispatch_action` with an error naming the non-literal
/// argument, not silently dropped and dispatched with missing/wrong
/// data — asserted on the specific error text so a downstream handler's
/// unrelated "missing argument" rejection can't mask a reintroduced
/// silent-drop bug.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn dispatch_action_rejects_non_literal_prev_reference() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let server = KhiveMcpServer::new(rt.clone()).expect("server");

    let err = dispatch_action(
        "stats() | get(id=$prev.id)",
        "local",
        Some(VerifiedActor::new("lambda:test").expect("verified actor")),
        &server,
        false,
    )
    .await
    .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("not replayable"),
        "expected the specific non-literal-argument rejection message, got: {msg}"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn replay_defense_rejects_legacy_internal_subhandler_payload() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let server = KhiveMcpServer::new(rt).expect("server");

    let err = dispatch_action(
        "comm.ingest(namespace=\"local\", from=\"email:a@example.com\", \
             to=\"email:b@example.com\", content=\"forged inbound\")",
        "local",
        Some(VerifiedActor::new("lambda:scheduler").expect("verified actor")),
        &server,
        false,
    )
    .await
    .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("internal subhandler") && msg.contains("comm.ingest"),
        "replay must preserve public-surface visibility for legacy/hand-written rows: {msg}"
    );
}

/// Same scenario end-to-end through the drain: confirms the rejection
/// surfaces as a counted failure rather than aborting the drain or being
/// swallowed, and that the drain still completes.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn dispatch_rejects_legacy_prev_reference_instead_of_dropping_it() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;

    let past = due_rfc3339();
    let _id = create_scheduled_event(
        &rt,
        "local",
        &past,
        Some("stats() | get(id=$prev.id)"),
        None,
        "schedule",
    )
    .await;

    let summary = drain_for_test(&db_path)
        .await
        .expect("drain must not abort or panic on a legacy $prev row");

    assert!(
        summary.failed >= 1,
        "a legacy $prev reference must surface as a dispatch failure, not a silent drop"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn legacy_multi_op_action_is_terminally_refused_without_partial_replay() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let server = KhiveMcpServer::new(rt.clone()).expect("server");
    let marker = "legacy-batch-success-must-never-run";
    let action = json!([
        {
            "tool": "create",
            "args": {"kind": "observation", "content": marker}
        },
        {"tool": "this_verb_does_not_exist", "args": {}}
    ])
    .to_string();
    let id = create_scheduled_event(
        &rt,
        "local",
        &due_rfc3339(),
        Some(&action),
        None,
        "schedule",
    )
    .await;

    let first = run_pending_events_on(&rt, &server, false)
        .await
        .expect("first drain");
    assert_eq!(
        first.invoked, 0,
        "a stored batch must be refused pre-invocation"
    );
    assert_eq!(first.failed, 1);
    assert_eq!(note_content_count(&rt, "observation", marker).await, 0);
    let props = get_note_props(&rt, id).await;
    assert_eq!(props["status"], "failed", "{props}");
    assert_eq!(
        props["dispatch_receipt"]["state"],
        DispatchReceiptState::NotInvoked.as_str(),
        "{props}"
    );
    assert!(props["dispatch_receipt"]["error"]
        .as_str()
        .is_some_and(|error| error.contains("multiple operations")));

    let second = run_pending_events_on(&rt, &server, false)
        .await
        .expect("second drain");
    assert_eq!(second.invoked, 0);
    assert_eq!(note_content_count(&rt, "observation", marker).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(config_ledger)]
async fn renewable_lease_prevents_live_overrun_reclaim_and_double_dispatch() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let marker = "renewable-lease-single-invocation";
    let state = std::sync::Arc::new(AsyncBlockingSideEffectState::default());
    let _release_verb_on_unwind = ReleaseAsyncBlockingVerbOnDrop(state.clone());
    let mut builder = khive_runtime::VerbRegistryBuilder::new();
    builder.with_default_namespace("local");
    builder.register(AsyncBlockingSideEffectPack {
        runtime: rt.clone(),
        marker: marker.to_string(),
        state: state.clone(),
    });
    let server = KhiveMcpServer::from_registry(builder.build().expect("test registry"));
    let action = "test.async_blocking_side_effect()";
    let id =
        create_scheduled_event(&rt, "local", &due_rfc3339(), Some(action), None, "schedule").await;
    let lease = short_test_lease();
    let entered = state.entered.notified();
    let drain_rt = rt.clone();
    let drain_server = server.clone();
    let first = tokio::spawn(async move {
        run_pending_events_on_with_lease(&drain_rt, &drain_server, false, lease).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), entered)
        .await
        .expect("dispatch entered async blocking verb");

    let invoking_props = get_note_props(&rt, id).await;
    assert_eq!(invoking_props["status"], "firing");
    let invocation_started_at = invoking_props["dispatch_receipt"]["invocation_started_at"]
        .as_i64()
        .expect("invocation start timestamp");
    let ttl_micros = i64::try_from(lease.ttl.as_micros()).expect("test TTL fits in i64");
    let original_deadline = invocation_started_at
        .checked_add(ttl_micros)
        .expect("test lease deadline fits in i64");
    let proof_horizon = original_deadline
        .checked_add(ttl_micros)
        .expect("multi-TTL proof horizon fits in i64");
    let renewal_margin_micros =
        i64::try_from(lease.renew_every.as_micros()).expect("test renewal interval fits in i64");
    let (live_props, observed_at, live_deadline) =
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let props = get_note_props(&rt, id).await;
                let observed_at = Utc::now().timestamp_micros();
                let future_margin = observed_at
                    .checked_add(renewal_margin_micros)
                    .expect("future-margin timestamp fits in i64");
                let deadline = props["lease_expires_at"].as_i64().unwrap_or(i64::MIN);
                if observed_at > proof_horizon && deadline > future_margin {
                    break (props, observed_at, deadline);
                }
                tokio::time::sleep(lease.renew_every.min(std::time::Duration::from_millis(10)))
                    .await;
            }
        })
        .await
        .expect("live dispatch lease did not remain renewable beyond two lease durations");
    assert_eq!(live_props["status"], "firing");
    assert!(
        observed_at > proof_horizon,
        "proof must observe the dispatch after two original lease durations"
    );
    assert!(
        live_deadline
            > observed_at
                .checked_add(renewal_margin_micros)
                .expect("future-margin timestamp fits in i64"),
        "live dispatch must retain a future lease after the multi-TTL horizon: {live_props}"
    );

    let second = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        run_pending_events_on_with_lease(&rt, &server, false, lease),
    )
    .await
    .expect("competing drain blocked, indicating a duplicate invocation")
    .expect("second drain");
    assert_eq!(second.reclaimed, 0, "live lease must not be reclaimed");
    assert_eq!(second.invoked, 0, "second drain must not invoke the action");

    state.release.notify_one();
    let first = tokio::time::timeout(std::time::Duration::from_secs(2), first)
        .await
        .expect("first drain completes")
        .expect("first drain task joins")
        .expect("first drain succeeds");
    assert_eq!(first.invoked, 1);
    assert_eq!(first.outcomes_persisted, 1);
    assert_eq!(first.finalized, 1);
    assert_eq!(first.fired, 1);
    assert_eq!(
        state.invocations.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the target verb must be entered exactly once"
    );
    assert_eq!(note_content_count(&rt, "observation", marker).await, 1);
    let final_props = get_note_props(&rt, id).await;
    assert_eq!(final_props["dispatch_receipt"]["state"], "succeeded");
    assert!(final_props.get("firing_at").is_none());
    assert!(final_props.get("lease_expires_at").is_none());
}

#[tokio::test]
async fn renewal_between_reclaim_scan_and_finalize_preserves_live_owner() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let trigger = due_rfc3339();
    let id =
        create_scheduled_event(&rt, "local", &trigger, Some("stats()"), None, "schedule").await;
    let claim = claim_for_test(&rt, id, &trigger).await;
    let lease = short_test_lease();
    assert!(mark_dispatch_invoking(&rt, "local", id, &claim, lease)
        .await
        .expect("mark invoking"));
    expire_dispatch_lease_for_test(&rt, id).await;

    // Model a reclaim pass that selected the expired row and retained its
    // stale snapshot, then lost the writer race to the live owner's
    // renewal. Recovery must re-check the deadline in its final CAS.
    let observed_expired_at = Utc::now().timestamp_micros();
    let selected_properties = get_raw_note_properties(&rt, id).await;
    let mut stale_properties: Value =
        serde_json::from_str(&selected_properties).expect("selected properties JSON");
    let mut stale_receipt = stale_properties["dispatch_receipt"].clone();
    let stale_completion = completion_from_receipt(&stale_receipt);
    stale_receipt["state"] = json!("indeterminate");
    stale_receipt["completed_at"] = json!(observed_expired_at);
    stale_receipt["error"] = json!(match &stale_completion {
        DispatchCompletion::Indeterminate(error) => error.as_str(),
        _ => "unexpected stale receipt state",
    });
    let trigger_fixed = trigger
        .parse::<DateTime<FixedOffset>>()
        .expect("fixed-offset trigger");
    let (stale_final, _) = final_properties_after_dispatch(
        std::mem::take(&mut stale_properties),
        stale_receipt,
        &stale_completion,
        trigger_fixed.with_timezone(&Utc),
        *trigger_fixed.offset(),
        &None,
    );

    assert!(renew_dispatch_lease(&rt, "local", id, &claim, lease)
        .await
        .expect("live owner renews"));
    assert!(
        !finalize_expired_firing_event(
            &rt,
            "local",
            id,
            &stale_final,
            Utc::now().timestamp_micros(),
            &claim,
            RecoverySnapshot {
                expired_at: observed_expired_at,
                properties: &selected_properties,
            },
        )
        .await
        .expect("stale recovery finalize"),
        "a renewal newer than the recovery snapshot must fence stale finalization"
    );
    let live = get_note_props(&rt, id).await;
    assert_eq!(live["status"], "firing");
    assert_eq!(live["dispatch_receipt"]["state"], "invoking");
    assert!(live["lease_expires_at"]
        .as_i64()
        .is_some_and(|deadline| deadline > observed_expired_at));

    let receipt =
        persist_dispatch_outcome(&rt, "local", id, &claim, &DispatchCompletion::Succeeded)
            .await
            .expect("persist live outcome")
            .expect("owner still holds receipt");
    let (final_properties, _) = final_properties_after_dispatch(
        live,
        receipt,
        &DispatchCompletion::Succeeded,
        trigger_fixed.with_timezone(&Utc),
        *trigger_fixed.offset(),
        &None,
    );
    let expected_properties = get_raw_note_properties(&rt, id).await;
    assert!(finalize_fired_event(
        &rt,
        "local",
        id,
        &final_properties,
        Utc::now().timestamp_micros(),
        &claim,
        &expected_properties,
    )
    .await
    .expect("live owner finalizes"));
    assert_eq!(get_note_props(&rt, id).await["status"], "fired");
}

#[tokio::test]
async fn durable_success_after_reclaim_scan_fences_stale_recovery_finalize() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let trigger = due_rfc3339();
    let id =
        create_scheduled_event(&rt, "local", &trigger, Some("stats()"), None, "schedule").await;
    let claim = claim_for_test(&rt, id, &trigger).await;
    assert!(
        mark_dispatch_invoking(&rt, "local", id, &claim, short_test_lease())
            .await
            .expect("mark invoking")
    );
    expire_dispatch_lease_for_test(&rt, id).await;

    let selected_properties = get_raw_note_properties(&rt, id).await;
    let mut stale_properties: Value =
        serde_json::from_str(&selected_properties).expect("selected properties JSON");
    let mut stale_receipt = stale_properties["dispatch_receipt"].clone();
    let stale_completion = completion_from_receipt(&stale_receipt);

    let durable_receipt =
        persist_dispatch_outcome(&rt, "local", id, &claim, &DispatchCompletion::Succeeded)
            .await
            .expect("persist success")
            .expect("claim still owned");
    let observed_expired_at = Utc::now().timestamp_micros();
    stale_receipt["state"] = json!(DispatchReceiptState::Indeterminate.as_str());
    stale_receipt["completed_at"] = json!(observed_expired_at);
    stale_receipt["error"] = json!(match &stale_completion {
        DispatchCompletion::Indeterminate(error) => error.as_str(),
        _ => "unexpected selected receipt state",
    });
    stale_receipt["error_payload"] = Value::Null;
    let trigger_fixed = trigger
        .parse::<DateTime<FixedOffset>>()
        .expect("fixed-offset trigger");
    let (stale_final, _) = final_properties_after_dispatch(
        std::mem::take(&mut stale_properties),
        stale_receipt,
        &stale_completion,
        trigger_fixed.with_timezone(&Utc),
        *trigger_fixed.offset(),
        &None,
    );

    assert!(
        !finalize_expired_firing_event(
            &rt,
            "local",
            id,
            &stale_final,
            Utc::now().timestamp_micros(),
            &claim,
            RecoverySnapshot {
                expired_at: observed_expired_at,
                properties: &selected_properties,
            },
        )
        .await
        .expect("stale recovery finalize"),
        "recovery selected an invoking snapshot and must not overwrite a later durable success"
    );
    let current = get_note_props(&rt, id).await;
    assert_eq!(current["status"], "firing", "{current}");
    assert_eq!(current["dispatch_receipt"], durable_receipt, "{current}");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn persisted_success_outcome_resumes_finalization_without_reinvocation() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let server = KhiveMcpServer::new(rt.clone()).expect("server");
    let marker = "success-receipt-crash-marker";
    let trigger = due_rfc3339();
    let action = format!("create(kind=\"observation\", content=\"{marker}\")");
    let id = create_scheduled_event(&rt, "local", &trigger, Some(&action), None, "schedule").await;
    let claim = claim_for_test(&rt, id, &trigger).await;
    assert!(
        mark_dispatch_invoking(&rt, "local", id, &claim, short_test_lease())
            .await
            .expect("mark invoking")
    );
    create_marker_directly(&rt, marker).await;
    assert!(
        persist_dispatch_outcome(&rt, "local", id, &claim, &DispatchCompletion::Succeeded,)
            .await
            .expect("persist outcome")
            .is_some()
    );

    let recovered = run_pending_events_on(&rt, &server, false)
        .await
        .expect("recover finalized outcome");
    assert_eq!(recovered.reclaimed, 1);
    assert_eq!(recovered.invoked, 0);
    assert_eq!(recovered.fired, 1);
    assert_eq!(recovered.finalized, 1);
    assert_eq!(note_content_count(&rt, "observation", marker).await, 1);
    let props = get_note_props(&rt, id).await;
    assert_eq!(props["status"], "fired");
    assert_eq!(props["dispatch_receipt"]["state"], "succeeded");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn expired_row_finalize_failure_does_not_wedge_later_due_work() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let server = KhiveMcpServer::new(rt.clone()).expect("server");
    let poison_trigger = due_rfc3339();
    let poison_id = create_scheduled_event(
        &rt,
        "local",
        &poison_trigger,
        Some("stats()"),
        None,
        "schedule",
    )
    .await;
    let poison_claim = claim_for_test(&rt, poison_id, &poison_trigger).await;
    assert!(
        mark_dispatch_invoking(&rt, "local", poison_id, &poison_claim, short_test_lease())
            .await
            .expect("mark poison invoking")
    );
    assert!(persist_dispatch_outcome(
        &rt,
        "local",
        poison_id,
        &poison_claim,
        &DispatchCompletion::Succeeded,
    )
    .await
    .expect("persist poison success")
    .is_some());

    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    let marker = "due-work-after-poison-expired-row";
    let action = format!("create(kind=\"observation\", content=\"{marker}\")");
    let later_id = create_scheduled_event(
        &rt,
        "local",
        &due_rfc3339(),
        Some(&action),
        None,
        "schedule",
    )
    .await;

    {
        let mut writer = rt.sql().writer().await.expect("writer");
        writer
            .execute(SqlStatement {
                sql: format!(
                    "CREATE TRIGGER test_fail_expired_outcome_finalize \
                         BEFORE UPDATE OF properties ON notes \
                         WHEN OLD.id = '{poison_id}' \
                           AND json_extract(OLD.properties, '$.status') = 'firing' \
                         BEGIN \
                           SELECT RAISE(FAIL, 'injected expired finalization failure'); \
                         END"
                ),
                params: vec![],
                label: Some("test_install_expired_finalize_failure".into()),
            })
            .await
            .expect("install expired finalization failure trigger");
    }

    let summary = run_pending_events_on(&rt, &server, false)
        .await
        .expect("row-local recovery failure is absorbed");
    assert_eq!(summary.failed, 1, "{summary:?}");
    assert_eq!(summary.fired, 1, "later due work must still fire");
    assert_eq!(note_content_count(&rt, "observation", marker).await, 1);
    assert_eq!(get_note_props(&rt, poison_id).await["status"], "firing");
    assert_eq!(get_note_props(&rt, later_id).await["status"], "fired");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn malformed_terminal_receipts_fail_indeterminate_without_replay() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let server = KhiveMcpServer::new(rt.clone()).expect("server");
    let marker = "malformed-terminal-receipt-must-not-dispatch";
    let action = format!("create(kind=\"observation\", content=\"{marker}\")");
    let mut ids = Vec::new();

    for case in 0..5 {
        let trigger = due_rfc3339();
        let id =
            create_scheduled_event(&rt, "local", &trigger, Some(&action), None, "schedule").await;
        claim_for_test(&rt, id, &trigger).await;
        let mut receipt = get_note_props(&rt, id).await["dispatch_receipt"].clone();
        match case {
            0 => {
                receipt["state"] = json!(DispatchReceiptState::Succeeded.as_str());
                receipt["error"] = Value::Null;
                receipt
                    .as_object_mut()
                    .expect("receipt object")
                    .remove("completed_at");
            }
            1 => {
                receipt["state"] = json!(DispatchReceiptState::Succeeded.as_str());
                receipt["completed_at"] = json!("not-a-timestamp");
                receipt["error"] = Value::Null;
            }
            2 => {
                receipt["state"] = json!(DispatchReceiptState::Failed.as_str());
                receipt["error"] = json!("simulated dispatch failure");
                receipt
                    .as_object_mut()
                    .expect("receipt object")
                    .remove("completed_at");
            }
            3 => {
                receipt["state"] = json!(DispatchReceiptState::Failed.as_str());
                receipt["completed_at"] = json!(Utc::now().timestamp_micros());
                receipt
                    .as_object_mut()
                    .expect("receipt object")
                    .remove("error");
            }
            4 => {
                receipt["state"] = json!(DispatchReceiptState::Succeeded.as_str());
                receipt["completed_at"] = json!(Utc::now().timestamp_micros());
                receipt["error"] = Value::Null;
                receipt["occurrence_id"] = json!(uuid::Uuid::new_v4());
            }
            _ => unreachable!(),
        }
        overwrite_dispatch_receipt_and_expire_for_test(&rt, id, &receipt).await;
        ids.push(id);
    }

    let recovered = run_pending_events_on(&rt, &server, false)
        .await
        .expect("malformed receipts are quarantined per row");
    assert_eq!(recovered.reclaimed, 5);
    assert_eq!(recovered.invoked, 0);
    assert_eq!(recovered.outcomes_persisted, 5);
    assert_eq!(recovered.indeterminate, 5);
    assert_eq!(recovered.finalized, 5);
    assert_eq!(recovered.fired, 0);
    assert_eq!(recovered.retry_pending, 0);
    assert_eq!(recovered.failed, 5);
    assert_eq!(note_content_count(&rt, "observation", marker).await, 0);

    for id in ids {
        let props = get_note_props(&rt, id).await;
        assert_eq!(props["status"], "failed", "{props}");
        assert_eq!(
            props["dispatch_receipt"]["state"],
            DispatchReceiptState::Indeterminate.as_str(),
            "{props}"
        );
        assert!(
            props["dispatch_receipt"]["completed_at"].as_i64().is_some(),
            "{props}"
        );
        assert!(
            props["dispatch_receipt"]["error"]
                .as_str()
                .is_some_and(|error| error.contains("refusing automatic replay")),
            "{props}"
        );
        assert!(
            props["dispatch_receipt"]["invalid_receipt"].is_object(),
            "the malformed source receipt must remain available for diagnosis: {props}"
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn expired_invoking_receipt_fails_indeterminate_without_double_dispatch() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let server = KhiveMcpServer::new(rt.clone()).expect("server");
    let marker = "indeterminate-crash-marker";
    let trigger = due_rfc3339();
    let action = format!("create(kind=\"observation\", content=\"{marker}\")");
    let id = create_scheduled_event(&rt, "local", &trigger, Some(&action), None, "schedule").await;
    let claim = claim_for_test(&rt, id, &trigger).await;
    assert!(
        mark_dispatch_invoking(&rt, "local", id, &claim, short_test_lease())
            .await
            .expect("mark invoking")
    );
    create_marker_directly(&rt, marker).await;
    expire_dispatch_lease_for_test(&rt, id).await;

    let recovered = run_pending_events_on(&rt, &server, false)
        .await
        .expect("reconcile ambiguous crash");
    assert_eq!(recovered.reclaimed, 1);
    assert_eq!(recovered.invoked, 0);
    assert_eq!(recovered.outcomes_persisted, 1);
    assert_eq!(recovered.indeterminate, 1);
    assert_eq!(note_content_count(&rt, "observation", marker).await, 1);
    let props = get_note_props(&rt, id).await;
    assert_eq!(props["status"], "failed", "ambiguous outcome fails closed");
    assert_eq!(props["dispatch_receipt"]["state"], "indeterminate");

    let again = run_pending_events_on(&rt, &server, false)
        .await
        .expect("terminal row is not replayed");
    assert_eq!(again.invoked, 0);
    assert_eq!(note_content_count(&rt, "observation", marker).await, 1);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn failed_one_shot_is_retryable_and_succeeds_once_on_later_drain() {
    let (_tmp, db_path) = tmp_db();
    let gate = std::sync::Arc::new(FailFirstCreateGate::default());
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(std::path::PathBuf::from(&db_path)),
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: gate.clone(),
        packs: vec!["kg".to_string(), "schedule".to_string()],
        ..Default::default()
    })
    .expect("runtime");
    let server = KhiveMcpServer::new(rt.clone()).expect("server");
    let marker = "failed-one-shot-recovery-marker";
    let action = format!("create(kind=\"observation\", content=\"{marker}\")");
    let id = create_scheduled_event(
        &rt,
        "local",
        &due_rfc3339(),
        Some(&action),
        None,
        "schedule",
    )
    .await;

    let first = run_pending_events_on(&rt, &server, false)
        .await
        .expect("first drain");
    assert_eq!(first.invoked, 1);
    assert_eq!(first.outcomes_persisted, 1);
    assert_eq!(first.retry_pending, 1);
    assert_eq!(first.fired, 0);
    let first_props = get_note_props(&rt, id).await;
    assert_eq!(first_props["status"], "pending");
    let first_occurrence = first_props["dispatch_receipt"]["occurrence_id"]
        .as_str()
        .expect("occurrence receipt")
        .to_string();
    let first_invocation = first_props["dispatch_receipt"]["invocation_id"]
        .as_str()
        .expect("invocation receipt")
        .to_string();
    assert_eq!(note_content_count(&rt, "observation", marker).await, 0);

    let second = run_pending_events_on(&rt, &server, false)
        .await
        .expect("retry drain");
    assert_eq!(second.invoked, 1);
    assert_eq!(second.outcomes_persisted, 1);
    assert_eq!(second.fired, 1);
    assert_eq!(note_content_count(&rt, "observation", marker).await, 1);
    let second_props = get_note_props(&rt, id).await;
    assert_eq!(second_props["status"], "fired");
    assert_eq!(
        second_props["dispatch_receipt"]["occurrence_id"].as_str(),
        Some(first_occurrence.as_str()),
        "retries share one deterministic occurrence identity"
    );
    assert_ne!(
        second_props["dispatch_receipt"]["invocation_id"].as_str(),
        Some(first_invocation.as_str()),
        "each retry receives a distinct invocation identity"
    );
    assert_eq!(
        gate.invocations.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "one failed invocation and one successful retry"
    );
}

#[test]
fn action_failure_disposition_distinguishes_committed_from_not_committed() {
    for (disposition, uncertain) in [
        ("committed", true),
        ("unknown", true),
        ("not_committed", false),
    ] {
        let entry = json!({
            "ok": false,
            "error": {
                "kind": "internal",
                "message": "post-commit maintenance failed",
                "domain_disposition": disposition,
            },
        });
        let error = action_failures(&[&entry]);
        assert_eq!(error.outcome_uncertain, uncertain, "{disposition}");
        assert_eq!(error.disposition_only_uncertain, uncertain, "{disposition}");
    }

    let entry_only = json!({
        "ok": false,
        "domain_disposition": "committed",
        "error": {"kind": "internal", "message": "obligation failed"},
    });
    let error = action_failures(&[&entry_only]);
    assert!(error.disposition_only_uncertain);
    assert_eq!(
        error.failure.payload.as_ref().unwrap()["entry_domain_disposition"],
        "committed"
    );

    let conflicting = json!({
        "ok": false,
        "domain_disposition": "unknown",
        "error": {
            "kind": "internal",
            "message": "the entry is authoritative too",
            "domain_disposition": "not_committed",
        },
    });
    assert!(action_failures(&[&conflicting]).disposition_only_uncertain);

    let legacy_hold = json!({
        "ok": false,
        "error": {
            "code": "side_effects_unknown",
            "message": "delivery outcome is uncertain",
            "domain_disposition": "not_committed",
        },
    });
    let error = action_failures(&[&legacy_hold]);
    assert!(error.outcome_uncertain);
    assert!(!error.disposition_only_uncertain);
}

#[test]
fn committed_error_without_next_occurrence_finishes_indeterminate() {
    let trigger_at = Utc::now();
    let failure = DispatchFailure::with_payload(
        "post-commit maintenance failed",
        json!({"kind": "internal", "domain_disposition": "committed"}),
    );
    let receipt = json!({
        "state": "failed",
        "completed_at": trigger_at.timestamp_micros(),
        "error_payload": failure.payload.clone(),
    });
    let completion = DispatchCompletion::Failed(failure);
    let (properties, disposition) = final_properties_after_dispatch(
        json!({"event_type": "schedule"}),
        receipt,
        &completion,
        trigger_at,
        FixedOffset::east_opt(0).unwrap(),
        &None,
    );
    assert_eq!(disposition, FinalDisposition::Indeterminate);
    assert_eq!(properties["status"], "failed");
    assert_eq!(properties["dispatch_receipt"]["state"], "indeterminate");
}

#[test]
fn committed_error_with_unadvanceable_repeat_keeps_indeterminate_receipt() {
    let trigger_at = Utc::now();
    let failure = DispatchFailure::with_payload(
        "post-commit maintenance failed",
        json!({"kind": "internal", "domain_disposition": "committed"}),
    );
    let receipt = json!({
        "state": "failed",
        "completed_at": trigger_at.timestamp_micros(),
        "error_payload": failure.payload.clone(),
    });
    let repeat = Some("every:100000000d".to_string());
    let (properties, disposition) = final_properties_after_dispatch(
        json!({
            "event_type": "schedule",
            "repeat": "every:100000000d",
            "trigger_at": trigger_at.to_rfc3339(),
        }),
        receipt,
        &DispatchCompletion::Failed(failure),
        trigger_at,
        FixedOffset::east_opt(0).unwrap(),
        &repeat,
    );
    assert_eq!(disposition, FinalDisposition::RecurrenceFailed);
    assert_eq!(properties["status"], "failed");
    assert_eq!(properties["recurrence_error"], UNADVANCEABLE_REPEAT);
    assert!(properties["recurrence_failed_at"].as_str().is_some());
    assert_eq!(properties["trigger_at"], trigger_at.to_rfc3339());
    assert_eq!(properties["dispatch_receipt"]["state"], "indeterminate");
}

#[test]
fn unknown_error_with_unadvanceable_repeat_keeps_indeterminate_receipt() {
    let trigger_at = Utc::now();
    let failure = DispatchFailure::with_payload(
        "handler outcome is unknown",
        json!({"kind": "internal", "domain_disposition": "unknown"}),
    );
    let receipt = json!({
        "state": "failed",
        "completed_at": trigger_at.timestamp_micros(),
        "error_payload": failure.payload.clone(),
    });
    let repeat = Some("every:100000000d".to_string());
    let (properties, disposition) = final_properties_after_dispatch(
        json!({
            "event_type": "schedule",
            "repeat": "every:100000000d",
            "trigger_at": trigger_at.to_rfc3339(),
        }),
        receipt,
        &DispatchCompletion::Failed(failure),
        trigger_at,
        FixedOffset::east_opt(0).unwrap(),
        &repeat,
    );
    assert_eq!(disposition, FinalDisposition::RecurrenceFailed);
    assert_eq!(properties["status"], "failed");
    assert_eq!(properties["recurrence_error"], UNADVANCEABLE_REPEAT);
    assert_eq!(properties["dispatch_receipt"]["state"], "indeterminate");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn one_shot_handler_error_runs_once_across_two_drains() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let invocations = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut builder = khive_runtime::VerbRegistryBuilder::new();
    builder.with_default_namespace("local");
    builder.register(OrdinaryHandlerFailurePack {
        invocations: invocations.clone(),
    });
    let server = KhiveMcpServer::from_registry(builder.build().expect("test registry"));
    let id = create_scheduled_event(
        &rt,
        "local",
        &due_rfc3339(),
        Some("test.ordinary_handler_failure()"),
        None,
        "schedule",
    )
    .await;

    let first = run_pending_events_on(&rt, &server, false)
        .await
        .expect("first drain");
    assert_eq!(first.invoked, 1);
    assert_eq!(first.indeterminate, 1);
    assert_eq!(first.retry_pending, 0);
    let properties = get_note_props(&rt, id).await;
    assert_eq!(properties["status"], "failed");
    assert_eq!(properties["dispatch_receipt"]["state"], "indeterminate");
    assert_eq!(
        properties["dispatch_receipt"]["error_payload"]["domain_disposition"],
        "unknown"
    );

    let second = run_pending_events_on(&rt, &server, false)
        .await
        .expect("second drain");
    assert_eq!(second.invoked, 0);
    assert_eq!(invocations.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn repeating_handler_error_advances_with_error_recorded() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let invocations = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut builder = khive_runtime::VerbRegistryBuilder::new();
    builder.with_default_namespace("local");
    builder.register(OrdinaryHandlerFailurePack {
        invocations: invocations.clone(),
    });
    let server = KhiveMcpServer::from_registry(builder.build().expect("test registry"));
    let id = create_scheduled_event(
        &rt,
        "local",
        &due_rfc3339(),
        Some("test.ordinary_handler_failure()"),
        Some("daily"),
        "schedule",
    )
    .await;

    let first = run_pending_events_on(&rt, &server, false)
        .await
        .expect("first drain");
    assert_eq!(first.invoked, 1);
    assert_eq!(first.advanced, 1);
    assert_eq!(first.indeterminate, 0);
    let properties = get_note_props(&rt, id).await;
    assert_eq!(properties["status"], "pending");
    assert_eq!(properties["dispatch_receipt"]["state"], "failed");
    assert!(properties["dispatch_error"].as_str().is_some());
    let next = properties["trigger_at"]
        .as_str()
        .unwrap()
        .parse::<DateTime<FixedOffset>>()
        .unwrap();
    assert!(next.with_timezone(&Utc) > Utc::now());

    let second = run_pending_events_on(&rt, &server, false)
        .await
        .expect("second drain");
    assert_eq!(second.invoked, 0);
    assert_eq!(invocations.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ambiguous_side_effect_is_indeterminate_and_never_blindly_retried() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let marker = "ambiguous-outcome-single-side-effect";
    let outbound_id = uuid::Uuid::new_v4();
    let invocations = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut builder = khive_runtime::VerbRegistryBuilder::new();
    builder.with_default_namespace("local");
    builder.register(AmbiguousSideEffectPack {
        runtime: rt.clone(),
        marker: marker.to_string(),
        outbound_id,
        invocations: invocations.clone(),
    });
    let server = KhiveMcpServer::from_registry(builder.build().expect("test registry"));
    let id = create_scheduled_event(
        &rt,
        "local",
        &due_rfc3339(),
        Some("test.ambiguous_side_effect()"),
        None,
        "schedule",
    )
    .await;

    let first = run_pending_events_on(&rt, &server, false)
        .await
        .expect("first drain");
    assert_eq!(first.invoked, 1);
    assert_eq!(first.outcomes_persisted, 1);
    assert_eq!(first.indeterminate, 1);
    assert_eq!(first.retry_pending, 0);
    assert_eq!(first.fired, 0);
    assert_eq!(note_content_count(&rt, "observation", marker).await, 1);
    assert_eq!(invocations.load(std::sync::atomic::Ordering::SeqCst), 1);
    let props = get_note_props(&rt, id).await;
    assert_eq!(props["status"], "failed", "{props}");
    assert_eq!(props["dispatch_receipt"]["state"], "indeterminate");
    assert_eq!(
        props["dispatch_receipt"]["error_payload"]["details"]["outbound_id"],
        outbound_id.to_string(),
        "the durable receipt must retain the comm.delivered correlation id: {props}"
    );

    let second = run_pending_events_on(&rt, &server, false)
        .await
        .expect("second drain");
    assert_eq!(second.invoked, 0);
    assert_eq!(note_content_count(&rt, "observation", marker).await, 1);
    assert_eq!(
        invocations.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "an ambiguous committed side effect must never be retried automatically"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn legacy_unparseable_repeat_row_fails_closed_before_action_invocation() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let server = KhiveMcpServer::new(rt.clone()).expect("server");
    let marker = "legacy-repeat-must-not-dispatch";
    let action = format!("create(kind=\"observation\", content=\"{marker}\")");
    let id = create_scheduled_event(
        &rt,
        "local",
        &due_rfc3339(),
        Some(&action),
        Some("hourly"),
        "schedule",
    )
    .await;

    let summary = run_pending_events_on(&rt, &server, false)
        .await
        .expect("legacy cron reconciliation");
    assert_eq!(summary.invoked, 0);
    assert_eq!(summary.failed, 1);
    assert_eq!(summary.finalized, 1);
    assert_eq!(note_content_count(&rt, "observation", marker).await, 0);
    let props = get_note_props(&rt, id).await;
    assert_eq!(props["status"], "failed");
    assert!(props["dispatch_error"]
        .as_str()
        .is_some_and(|error| error.contains("unsupported repeat")));
    assert_eq!(
        props["dispatch_receipt"]["state"],
        DispatchReceiptState::NotInvoked.as_str(),
        "the durable claim receipt must survive unsupported-repeat refusal: {props}"
    );
    assert!(props["dispatch_receipt"]["occurrence_id"]
        .as_str()
        .is_some());
    assert!(props["dispatch_receipt"]["invocation_id"]
        .as_str()
        .is_some());
    assert!(props["dispatch_receipt"]["completed_at"].as_i64().is_some());
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn empty_payload_finalization_retains_not_invoked_receipt() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let server = KhiveMcpServer::new(rt.clone()).expect("server");
    let id = create_scheduled_event(&rt, "local", &due_rfc3339(), None, None, "schedule").await;

    let summary = run_pending_events_on(&rt, &server, false)
        .await
        .expect("empty payload is finalized per row");
    assert_eq!(summary.invoked, 0);
    assert_eq!(summary.failed, 1);
    assert_eq!(summary.finalized, 1);
    let props = get_note_props(&rt, id).await;
    assert_eq!(props["status"], "failed", "{props}");
    assert_eq!(
        props["dispatch_receipt"]["state"],
        DispatchReceiptState::NotInvoked.as_str(),
        "{props}"
    );
    assert!(props["dispatch_receipt"]["completed_at"].as_i64().is_some());
    assert!(props["dispatch_receipt"]["error"]
        .as_str()
        .is_some_and(|error| error.contains("no executable payload")));
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn unsupported_repeat_finalize_failure_does_not_abort_later_rows() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let server = KhiveMcpServer::new(rt.clone()).expect("server");
    let legacy_id = create_scheduled_event(
        &rt,
        "local",
        &due_rfc3339(),
        Some("stats()"),
        Some("hourly"),
        "schedule",
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    let marker = "row-after-legacy-finalize-failure";
    let action = format!("create(kind=\"observation\", content=\"{marker}\")");
    let later_id = create_scheduled_event(
        &rt,
        "local",
        &due_rfc3339(),
        Some(&action),
        None,
        "schedule",
    )
    .await;

    {
        let mut writer = rt.sql().writer().await.expect("writer");
        writer
            .execute(SqlStatement {
                sql: format!(
                    "CREATE TRIGGER test_fail_unsupported_repeat_finalize \
                         BEFORE UPDATE OF properties ON notes \
                         WHEN OLD.id = '{legacy_id}' \
                           AND json_extract(OLD.properties, '$.status') = 'firing' \
                           AND json_extract(NEW.properties, '$.status') = 'failed' \
                         BEGIN \
                           SELECT RAISE(FAIL, 'injected legacy finalization failure'); \
                         END"
                ),
                params: vec![],
                label: Some("test_install_legacy_finalize_failure".into()),
            })
            .await
            .expect("install finalization failure trigger");
    }

    let summary = run_pending_events_on(&rt, &server, false)
        .await
        .expect("one row-level finalization failure must not abort the drain");
    assert_eq!(summary.scanned, 2);
    assert_eq!(summary.invoked, 1);
    assert_eq!(summary.fired, 1);
    assert_eq!(summary.failed, 1);
    assert_eq!(note_content_count(&rt, "observation", marker).await, 1);
    assert_eq!(get_note_props(&rt, legacy_id).await["status"], "firing");
    assert_eq!(get_note_props(&rt, later_id).await["status"], "fired");
}

/// A `schedule.cancel` arriving after the drain has already CAS-claimed
/// the row for firing must fail — proves a cancel can never be lost to a
/// fire that was already in flight.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn fire_claim_wins_race_against_concurrent_cancel() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let server = KhiveMcpServer::new(rt.clone()).expect("server");

    let past = "2000-01-01T00:00:00Z";
    let id = create_scheduled_event(&rt, "local", past, Some("stats()"), None, "schedule").await;

    // Simulate the drain's claim (pending -> firing), which in the real
    // drain happens right after the page read and before dispatch.
    let claim = claim_for_test(&rt, id, past).await;

    // A `schedule.cancel` arriving after the claim in this race window
    // must now fail instead of clobbering the
    // in-flight fire.
    let cancel_ops = serde_json::to_string(&serde_json::json!([
        { "tool": "schedule.cancel", "args": { "id": id.to_string() } }
    ]))
    .expect("serialize cancel op");
    let cancel_result = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: cancel_ops,
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("dispatch_request_local must not error at the RPC layer");
    let cancel_json: Value = serde_json::from_str(&cancel_result).expect("valid JSON");
    let op_result = &cancel_json["results"][0];
    assert_eq!(
        op_result["ok"], false,
        "cancel of a claimed (firing) event must fail, not silently succeed: {cancel_json}"
    );
    let cancel_err = op_result["error"]["message"]
        .as_str()
        .expect("error.message is text");
    assert!(
        cancel_err.contains("not pending"),
        "cancel must report the event is no longer pending; got: {cancel_err}"
    );

    // Finalize the fire as the drain would, then confirm the terminal
    // state is "fired" — the cancel never got a chance to overwrite it.
    let expected_properties = get_raw_note_properties(&rt, id).await;
    let finalized = finalize_fired_event(
        &rt,
        "local",
        id,
        &serde_json::json!({
            "trigger_at": past,
            "repeat": null,
            "status": "fired",
            "event_type": "schedule",
            "payload": "stats()",
            "fired_at": Utc::now().to_rfc3339(),
            "cancelled_at": null,
        }),
        Utc::now().timestamp_micros(),
        &claim,
        &expected_properties,
    )
    .await
    .expect("finalize query");
    assert!(
        finalized,
        "finalize must succeed on a row still in \"firing\""
    );

    let props = get_note_props(&rt, id).await;
    assert_eq!(
        props["status"].as_str().unwrap_or(""),
        "fired",
        "terminal state must be \"fired\"; cancel must not have won the race"
    );
}

/// Directly set a note's `properties` via raw SQL, bypassing the normal
/// claim/finalize CAS paths. Used to deterministically fabricate a
/// stale-`firing` row (as if a drain claimed it and then crashed before
/// finalizing) without depending on wall-clock sleeps.
async fn force_set_properties(rt: &KhiveRuntime, id: uuid::Uuid, properties: &Value) {
    let props_json = serde_json::to_string(properties).expect("serialize");
    let mut writer = rt.sql().writer().await.expect("writer");
    let rows = writer
        .execute(SqlStatement {
            sql: "UPDATE notes SET properties = ?1 WHERE id = ?2".to_string(),
            params: vec![SqlValue::Text(props_json), SqlValue::Text(id.to_string())],
            label: Some("test_force_set_properties".into()),
        })
        .await
        .expect("force update");
    assert_eq!(rows, 1, "test setup: row must exist");
}

fn assert_reserved_property_refusal(error: &anyhow::Error) {
    match error.downcast_ref::<khive_runtime::RuntimeError>() {
        Some(khive_runtime::RuntimeError::InvalidInput(message)) => {
            assert!(message.contains("khive:secret_gate"), "{message}");
        }
        other => panic!("expected typed reserved-property refusal, got {other:?}: {error}"),
    }
}

async fn plant_reserved_property(rt: &KhiveRuntime, id: uuid::Uuid) -> (Value, String) {
    let mut properties = get_note_props(rt, id).await;
    properties["khive:secret_gate"] = json!("caller-forged");
    force_set_properties(rt, id, &properties).await;
    let raw = get_raw_note_properties(rt, id).await;
    (properties, raw)
}

#[tokio::test]
async fn whole_object_dispatch_writes_refuse_carried_reserved_property() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let trigger = due_rfc3339();
    let lease = short_test_lease();

    let claim_id =
        create_scheduled_event(&rt, "local", &trigger, Some("stats()"), None, "schedule").await;
    let (_, before_claim) = plant_reserved_property(&rt, claim_id).await;
    let claim_error = claim_pending_event(
        &rt,
        "local",
        claim_id,
        dispatch_occurrence_id(claim_id, trigger.parse::<DateTime<Utc>>().unwrap()),
        &trigger,
        "actor:test",
        lease,
    )
    .await
    .expect_err("claim must refuse reserved property");
    assert_reserved_property_refusal(&claim_error);
    assert_eq!(get_raw_note_properties(&rt, claim_id).await, before_claim);

    let invoking_id =
        create_scheduled_event(&rt, "local", &trigger, Some("stats()"), None, "schedule").await;
    let invoking_claim = claim_for_test(&rt, invoking_id, &trigger).await;
    let (_, before_invoking) = plant_reserved_property(&rt, invoking_id).await;
    let invoking_error = mark_dispatch_invoking(&rt, "local", invoking_id, &invoking_claim, lease)
        .await
        .expect_err("invocation marker must refuse reserved property");
    assert_reserved_property_refusal(&invoking_error);
    assert_eq!(
        get_raw_note_properties(&rt, invoking_id).await,
        before_invoking
    );

    let outcome_id =
        create_scheduled_event(&rt, "local", &trigger, Some("stats()"), None, "schedule").await;
    let outcome_claim = claim_for_test(&rt, outcome_id, &trigger).await;
    assert!(
        mark_dispatch_invoking(&rt, "local", outcome_id, &outcome_claim, lease)
            .await
            .expect("mark clean row invoking")
    );
    let (_, before_outcome) = plant_reserved_property(&rt, outcome_id).await;
    let outcome_error = persist_dispatch_outcome(
        &rt,
        "local",
        outcome_id,
        &outcome_claim,
        &DispatchCompletion::Succeeded,
    )
    .await
    .expect_err("outcome persistence must refuse reserved property");
    assert_reserved_property_refusal(&outcome_error);
    assert_eq!(
        get_raw_note_properties(&rt, outcome_id).await,
        before_outcome
    );

    let legacy_id =
        create_scheduled_event(&rt, "local", &trigger, Some("stats()"), None, "schedule").await;
    let stale_firing_at = Utc::now().timestamp_micros() - (LEGACY_STALE_FIRING_TIMEOUT_MICROS * 2);
    let mut legacy = get_note_props(&rt, legacy_id).await;
    legacy["status"] = json!("firing");
    legacy["firing_at"] = json!(stale_firing_at);
    legacy["khive:secret_gate"] = json!("caller-forged");
    force_set_properties(&rt, legacy_id, &legacy).await;
    let before_legacy = get_raw_note_properties(&rt, legacy_id).await;
    let requeue_error =
        requeue_legacy_claim(&rt, "local", legacy_id, stale_firing_at, &before_legacy)
            .await
            .expect_err("legacy requeue must refuse reserved property");
    assert_reserved_property_refusal(&requeue_error);
    assert_eq!(get_raw_note_properties(&rt, legacy_id).await, before_legacy);

    let corrupt_error = finalize_corrupt_receipt(
        &rt,
        "local",
        legacy_id,
        stale_firing_at,
        &legacy,
        Utc::now().timestamp_micros(),
        &before_legacy,
    )
    .await
    .expect_err("corrupt receipt finalization must refuse reserved property");
    assert_reserved_property_refusal(&corrupt_error);
    assert_eq!(get_raw_note_properties(&rt, legacy_id).await, before_legacy);

    let final_id =
        create_scheduled_event(&rt, "local", &trigger, Some("stats()"), None, "schedule").await;
    let final_claim = claim_for_test(&rt, final_id, &trigger).await;
    assert!(
        mark_dispatch_invoking(&rt, "local", final_id, &final_claim, lease)
            .await
            .expect("mark final row invoking")
    );
    let (mut final_properties, before_final) = plant_reserved_property(&rt, final_id).await;
    final_properties["status"] = json!("fired");
    let final_error = finalize_fired_event(
        &rt,
        "local",
        final_id,
        &final_properties,
        Utc::now().timestamp_micros(),
        &final_claim,
        &before_final,
    )
    .await
    .expect_err("firing finalization must refuse reserved property");
    assert_reserved_property_refusal(&final_error);
    assert_eq!(get_raw_note_properties(&rt, final_id).await, before_final);
}

/// A row claimed by a drain that then crashed before finalizing —
/// `status="firing"` with a `firing_at` older than the stale timeout —
/// must be reclaimed back to `pending` and fired on the next pass,
/// instead of being wedged forever.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn stale_firing_row_is_reclaimed_and_fired() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;

    let past = due_rfc3339();
    let id = create_scheduled_event(&rt, "local", &past, Some("stats()"), None, "schedule").await;

    // Simulate a drain claiming the row, then crashing before finalize:
    // status="firing" with a firing_at well past the stale timeout.
    let stale_firing_at = Utc::now().timestamp_micros() - (LEGACY_STALE_FIRING_TIMEOUT_MICROS * 2);
    force_set_properties(
        &rt,
        id,
        &json!({
            "trigger_at": past,
            "repeat": null,
            "status": "firing",
            "event_type": "schedule",
            "created_by_actor": "local",
            "payload": "stats()",
            "fired_at": null,
            "cancelled_at": null,
            "firing_at": stale_firing_at,
        }),
    )
    .await;

    let summary = drain_for_test(&db_path).await.expect("drain");

    assert!(
        summary.reclaimed >= 1,
        "the stale firing row must be reclaimed, got summary={summary:?}"
    );
    assert!(
        summary.fired >= 1 || summary.advanced >= 1,
        "the reclaimed row must be fired (or advanced) in the same pass, \
             got summary={summary:?}"
    );

    let props = get_note_props(&rt, id).await;
    assert_eq!(
        props["status"].as_str(),
        Some("fired"),
        "a reclaimed non-repeating event must end in \"fired\", got {props:?}"
    );
}

/// A row claimed *recently* (fresh `firing_at`, well within the stale
/// timeout) must NOT be reclaimed — a live drain's in-flight claim is
/// never stolen by the reclaim sweep.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn fresh_firing_row_is_not_reclaimed() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;

    let past = "2000-01-01T00:00:00Z";
    let id = create_scheduled_event(&rt, "local", past, Some("stats()"), None, "schedule").await;

    // Fresh claim: firing_at = now, well under the stale threshold.
    let _claim = claim_for_test(&rt, id, past).await;

    let summary = drain_for_test(&db_path).await.expect("drain");

    assert_eq!(
        summary.reclaimed, 0,
        "a fresh firing row must not be reclaimed, got summary={summary:?}"
    );
    assert_eq!(
        summary.fired, 0,
        "a fresh firing row must not be fired by a drain pass that did not claim it"
    );

    let props = get_note_props(&rt, id).await;
    assert_eq!(
        props["status"].as_str(),
        Some("firing"),
        "a fresh firing row must remain firing (owned by the process that claimed it), \
             got {props:?}"
    );
}

/// Finalize must be bound to the owning claim token, not just
/// `status='firing'`: a stale claimant (A) that resumes after a reclaim
/// pass has already let a fresh claimant (B) re-claim the row must have
/// its finalize become a no-op, leaving B's claim untouched.
#[tokio::test]
async fn stale_claimant_cannot_finalize_over_a_fresh_reclaim() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;

    let past = "2000-01-01T00:00:00Z";
    let id = create_scheduled_event(&rt, "local", past, Some("stats()"), None, "schedule").await;

    let a_claim = claim_for_test(&rt, id, past).await;
    let mut writer = rt.sql().writer().await.expect("writer");
    assert_eq!(
        writer
            .execute(SqlStatement {
                sql: "UPDATE notes SET properties = json_set( \
                            properties, '$.lease_expires_at', ?1) WHERE id = ?2"
                    .to_string(),
                params: vec![
                    SqlValue::Integer(Utc::now().timestamp_micros() - 1),
                    SqlValue::Text(id.to_string()),
                ],
                label: Some("test_expire_a_dispatch_lease".into()),
            })
            .await
            .expect("expire A lease"),
        1
    );
    drop(writer);

    // A reclaim pass runs (as a live drain's periodic sweep would),
    // moving the row back to "pending" since A's firing_at is stale.
    let reclaimed = reclaim_stale_firing_events(&rt, Utc::now().timestamp_micros())
        .await
        .expect("reclaim query");
    assert_eq!(reclaimed.rows, 1, "A's stale claim must be reclaimed");
    assert_eq!(reclaimed.retry_pending, 1);
    assert_eq!(
        reclaimed.failed, 0,
        "an expired claimant that never began invocation is retryable, not a failed action"
    );
    let reclaimed_props = get_note_props(&rt, id).await;
    assert_eq!(reclaimed_props["status"], "pending", "{reclaimed_props}");
    assert_eq!(
        reclaimed_props["dispatch_receipt"]["state"],
        DispatchReceiptState::NotInvoked.as_str(),
        "claim expiry before invocation must be recorded truthfully: {reclaimed_props}"
    );
    assert!(
        reclaimed_props["dispatch_receipt"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("before invocation began")),
        "the durable receipt must explain why no invocation occurred: {reclaimed_props}"
    );

    // Drain B re-claims the now-pending row, minting a fresh firing_at
    // token that differs from A's stale one.
    let b_claim = claim_for_test(&rt, id, past).await;
    assert_ne!(
        a_claim.invocation_id, b_claim.invocation_id,
        "B's invocation token must differ from A's stale token"
    );

    // A resumes (unaware it was reclaimed) and attempts to finalize using
    // its own stale claim token. This must be a no-op: it must NOT match
    // B's current firing_at, and must NOT clobber B's live claim.
    let expected_properties_for_a = get_raw_note_properties(&rt, id).await;
    let a_finalize_result = finalize_fired_event(
        &rt,
        "local",
        id,
        &json!({
            "trigger_at": past,
            "repeat": null,
            "status": "fired",
            "event_type": "schedule",
            "payload": "stats()",
            "fired_at": Utc::now().to_rfc3339(),
            "cancelled_at": null,
        }),
        Utc::now().timestamp_micros(),
        &a_claim,
        &expected_properties_for_a,
    )
    .await
    .expect("finalize query must not error");
    assert!(
        !a_finalize_result,
        "A's finalize with a stale claim token must be a no-op, not a successful write"
    );

    // B's claim must be completely intact: still "firing", still stamped
    // with B's own firing_at — A's stale finalize must not have touched it.
    let props_after_a = get_note_props(&rt, id).await;
    assert_eq!(
        props_after_a["status"].as_str(),
        Some("firing"),
        "B's claim must survive A's stale finalize attempt untouched, got {props_after_a:?}"
    );
    assert_eq!(
        props_after_a["firing_at"].as_i64(),
        Some(b_claim.firing_at),
        "B's firing_at token must be unchanged by A's stale finalize attempt"
    );

    // B now finalizes with its own (correct) claim token — this must
    // succeed, proving the fix doesn't wedge legitimate finalization.
    let expected_properties_for_b = get_raw_note_properties(&rt, id).await;
    let b_finalize_result = finalize_fired_event(
        &rt,
        "local",
        id,
        &json!({
            "trigger_at": past,
            "repeat": null,
            "status": "fired",
            "event_type": "schedule",
            "payload": "stats()",
            "fired_at": Utc::now().to_rfc3339(),
            "cancelled_at": null,
        }),
        Utc::now().timestamp_micros(),
        &b_claim,
        &expected_properties_for_b,
    )
    .await
    .expect("finalize query must not error");
    assert!(
        b_finalize_result,
        "B's finalize with its own claim token must succeed"
    );

    let final_props = get_note_props(&rt, id).await;
    assert_eq!(
        final_props["status"].as_str(),
        Some("fired"),
        "terminal state must be \"fired\" via B's own claim, got {final_props:?}"
    );
    assert!(
        final_props.get("firing_at").is_none() || final_props["firing_at"].is_null(),
        "firing_at must be cleared on terminal finalize, got {final_props:?}"
    );
}

/// Regression for the normal-finalization lost-update race (khive #1753).
/// `finalize_fired_event` reads the row's CURRENT properties immediately
/// before finalizing (see the call site above `final_properties_after_dispatch`
/// in the main drain loop) and must refuse the terminal write if a
/// concurrent writer changed properties since that read, even though the
/// claim tokens (`firing_at`/`invocation_id`/lease) are still valid —
/// those predicates alone do not detect an out-of-band property change.
/// This deterministically reproduces "two reads from one revision": the
/// snapshot captured here (`expected_properties`) is used for the stale
/// finalize attempt AFTER a concurrent write has already landed, so the
/// exact-equality predicate must fail. Before threading a real snapshot
/// through, normal finalization always called with `snapshot=None`,
/// which makes `AND (?9 IS NULL OR properties = ?9)` unconditionally
/// true — this test reddens if that call reverts to `None`: the stale
/// finalize would then succeed and the concurrent writer's
/// `concurrent_marker` field would be silently discarded.
#[tokio::test]
async fn normal_finalize_refuses_when_a_concurrent_writer_changed_properties_since_the_read() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;

    let past = "2000-01-01T00:00:00Z";
    let id = create_scheduled_event(&rt, "local", past, Some("stats()"), None, "schedule").await;
    let claim = claim_for_test(&rt, id, past).await;

    // The finalizer's fresh pre-write read (what `current_note_properties_text`
    // returns right before finalizing in production).
    let expected_properties = get_raw_note_properties(&rt, id).await;

    // A concurrent writer mutates the row AFTER that read while leaving
    // every claim predicate (status/firing_at/invocation_id/lease)
    // valid — e.g. an external property patch racing the finalizer.
    let mut writer = rt.sql().writer().await.expect("writer");
    let rows = writer
        .execute(SqlStatement {
            sql: "UPDATE notes SET properties = json_set(properties, \
                      '$.concurrent_marker', 'yes') WHERE id = ?1"
                .to_string(),
            params: vec![SqlValue::Text(id.to_string())],
            label: Some("test_concurrent_property_write".into()),
        })
        .await
        .expect("concurrent write");
    assert_eq!(rows, 1);
    drop(writer);

    let final_props = json!({
        "trigger_at": past,
        "repeat": null,
        "status": "fired",
        "event_type": "schedule",
        "payload": "stats()",
        "fired_at": Utc::now().to_rfc3339(),
        "cancelled_at": null,
    });
    let finalized = finalize_fired_event(
        &rt,
        "local",
        id,
        &final_props,
        Utc::now().timestamp_micros(),
        &claim,
        &expected_properties,
    )
    .await
    .expect("finalize query must not error");
    assert!(
        !finalized,
        "finalize must refuse a terminal write when properties changed since the read it guards on"
    );

    let props_after = get_note_props(&rt, id).await;
    assert_eq!(
        props_after["status"].as_str(),
        Some("firing"),
        "the row must remain firing, not silently finalized over the concurrent writer's \
             change: {props_after:?}"
    );
    assert_eq!(
        props_after["concurrent_marker"].as_str(),
        Some("yes"),
        "the concurrent writer's field must survive the refused finalize: {props_after:?}"
    );

    // A finalize guarded on the CURRENT properties (as the real drain
    // loop does, re-reading right before this call) must still succeed.
    let fresh_properties = get_raw_note_properties(&rt, id).await;
    let finalized_fresh = finalize_fired_event(
        &rt,
        "local",
        id,
        &final_props,
        Utc::now().timestamp_micros(),
        &claim,
        &fresh_properties,
    )
    .await
    .expect("finalize query must not error");
    assert!(
        finalized_fresh,
        "finalize with a fresh snapshot must succeed"
    );
    assert_eq!(
        get_note_props(&rt, id).await["status"].as_str(),
        Some("fired")
    );
}

/// Regression test driven through the PRODUCTION
/// drain entry point (`run_pending_events_on`) rather than calling
/// `final_properties_after_dispatch` directly — this closes a gap a
/// primitive-level test cannot: it would still pass unchanged if the
/// drain loop's call site reverted to building `final_props` from the
/// stale pre-claim `properties` snapshot instead of the freshly read
/// `expected_properties`, since it would never invoke that call site at
/// all. Uses `race_seam::pause_before_finalize_read` (test-only,
/// compiled out of non-test builds) to force the concurrent property
/// write to land deterministically between claim/dispatch and the
/// finalizer's fresh current-properties read — no sleeps, no reliance on
/// scheduler ordering.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn production_drain_preserves_a_property_written_between_claim_and_current_read() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let id = create_scheduled_event(
        &rt,
        "local",
        &due_rfc3339(),
        Some("stats()"),
        None,
        "schedule",
    )
    .await;

    let gate = race_seam::PauseGate {
        at: race_seam::PausePoint::BeforeFinalizeRead,
        reached: std::sync::Arc::new(tokio::sync::Barrier::new(2)),
        release: std::sync::Arc::new(tokio::sync::Barrier::new(2)),
    };

    let drain_task = {
        let rt = rt.clone();
        let gate = gate.clone();
        tokio::spawn(race_seam::PAUSE_GATE.scope(gate, async move {
            let server = KhiveMcpServer::new(rt.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;
            run_pending_events_on(&rt, &server, false).await
        }))
    };

    // Block until the drain task has genuinely parked at the seam — which
    // sits AFTER the candidate-page query and the claim, immediately
    // before the fresh pre-finalize read — THEN write, THEN release it.
    // That placement is what makes the write land strictly between the
    // page-query snapshot and the fresh read: parked any earlier, the
    // write would already be inside the page snapshot and the test would
    // pass whether finalization used the stale page or the fresh read.
    gate.reached.wait().await;

    let mut writer = rt.sql().writer().await.expect("writer");
    let rows = writer
        .execute(SqlStatement {
            sql: "UPDATE notes SET properties = json_set(properties, \
                      '$.custom', 'added-concurrently') WHERE id = ?1"
                .to_string(),
            params: vec![SqlValue::Text(id.to_string())],
            label: Some("test_concurrent_property_add".into()),
        })
        .await
        .expect("concurrent write");
    assert_eq!(rows, 1);
    drop(writer);

    gate.release.wait().await;
    let summary = drain_task
        .await
        .expect("drain task")
        .expect("drain must not error");
    assert_eq!(summary.fired, 1, "the event must have fired: {summary:?}");

    let stored = get_note_props(&rt, id).await;
    assert_eq!(
        stored["custom"].as_str(),
        Some("added-concurrently"),
        "a property written between claim and the finalizer's current-properties read \
             must survive finalization, got {stored:?}"
    );
    assert_eq!(stored["status"].as_str(), Some("fired"));
}

/// Guarding the finalizer's write on the freshly-read properties protects
/// the properties BLOB while still allowing a stale SCHEDULING decision to
/// be computed over it. `repeat` and `trigger_at` are parsed from the
/// pre-claim candidate page; if the finalizer keeps using those, a writer
/// who cancels the repeat in the claim window has their edit retained as
/// the CAS base and then immediately contradicted by a next occurrence
/// scheduled from the value they replaced.
///
/// This fixture makes the two behaviours produce different terminal states
/// rather than different timestamps, so the assertion cannot pass by
/// rounding: the event is seeded `repeat: "daily"`, and the concurrent
/// write clears `repeat` while the drain is parked at the seam. Scheduling
/// from the fresh read yields a terminal `fired`; scheduling from the stale
/// page yields a rescheduled `pending` with an advanced `trigger_at`.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn production_drain_schedules_from_the_fresh_read_not_the_page_snapshot() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let id = create_scheduled_event(
        &rt,
        "local",
        &due_rfc3339(),
        Some("stats()"),
        Some("daily"),
        "schedule",
    )
    .await;

    let gate = race_seam::PauseGate {
        at: race_seam::PausePoint::BeforeFinalizeRead,
        reached: std::sync::Arc::new(tokio::sync::Barrier::new(2)),
        release: std::sync::Arc::new(tokio::sync::Barrier::new(2)),
    };

    let drain_task = {
        let rt = rt.clone();
        let gate = gate.clone();
        tokio::spawn(race_seam::PAUSE_GATE.scope(gate, async move {
            let server = KhiveMcpServer::new(rt.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;
            run_pending_events_on(&rt, &server, false).await
        }))
    };

    gate.reached.wait().await;

    let mut writer = rt.sql().writer().await.expect("writer");
    let rows = writer
        .execute(SqlStatement {
            sql: "UPDATE notes SET properties = json_set(properties, '$.repeat', json('null')) \
                      WHERE id = ?1"
                .to_string(),
            params: vec![SqlValue::Text(id.to_string())],
            label: Some("test_concurrent_repeat_clear".into()),
        })
        .await
        .expect("concurrent write");
    assert_eq!(rows, 1);
    drop(writer);

    gate.release.wait().await;
    let summary = drain_task
        .await
        .expect("drain task")
        .expect("drain must not error");
    assert_eq!(summary.fired, 1, "the event must have fired: {summary:?}");

    let stored = get_note_props(&rt, id).await;
    assert!(
        stored.get("repeat").is_none() || stored["repeat"].is_null(),
        "the concurrent clear of `repeat` must survive finalization, got {stored:?}"
    );
    assert_eq!(
        stored["status"].as_str(),
        Some("fired"),
        "finalization must schedule from the repeat it read fresh (cleared, so terminal), \
             not from the pre-claim page snapshot (\"daily\", which would reschedule to \
             pending): got {stored:?}"
    );
}

/// The claim's `trigger_at` fence, isolated. The receipt's `occurrence_id`
/// is derived from the caller's page snapshot, so a claim that lands on a
/// row whose `trigger_at` has since moved would persist an occurrence id
/// describing an instant the row is no longer scheduled for. Receipt
/// validation rejects exactly that pairing, so such a row can only ever be
/// quarantined as indeterminate; refusing the claim is what keeps it out of
/// the durable record in the first place.
#[tokio::test]
async fn claim_refuses_when_a_concurrent_writer_rescheduled_since_the_page_read() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let snapshot_trigger = "2000-01-01T00:00:00Z";
    let id = create_scheduled_event(
        &rt,
        "local",
        snapshot_trigger,
        Some("stats()"),
        None,
        "schedule",
    )
    .await;

    // Positive control in the same test: the fence admits the claim when
    // the row still carries the snapshot's bytes. Without this arm a
    // refusal below would be consistent with a fence that refuses
    // everything, which proves nothing about the race.
    let admitted = claim_pending_event(
        &rt,
        "local",
        id,
        dispatch_occurrence_id(id, snapshot_trigger.parse::<DateTime<Utc>>().unwrap()),
        snapshot_trigger,
        "actor:test",
        short_test_lease(),
    )
    .await
    .expect("claim query must not error");
    assert!(
        admitted.is_some(),
        "the claim must be admitted when the row still holds the snapshot's trigger_at"
    );

    // Put the row back to pending so the refusal arm is testing the
    // trigger_at predicate and not the status one.
    let rescheduled_trigger = "2000-06-01T00:00:00Z";
    force_set_properties(
        &rt,
        id,
        &json!({
            "trigger_at": rescheduled_trigger,
            "status": "pending",
            "action": "stats()",
            "event_type": "schedule",
        }),
    )
    .await;

    let refused = claim_pending_event(
        &rt,
        "local",
        id,
        dispatch_occurrence_id(id, snapshot_trigger.parse::<DateTime<Utc>>().unwrap()),
        snapshot_trigger,
        "actor:test",
        short_test_lease(),
    )
    .await
    .expect("claim query must not error");
    assert!(
        refused.is_none(),
        "the claim must refuse once the row's trigger_at has moved away from the snapshot"
    );

    let stored = get_note_props(&rt, id).await;
    assert_eq!(
        stored["status"].as_str(),
        Some("pending"),
        "a refused claim must leave the row claimable by the next drain: {stored:?}"
    );
    assert_eq!(
        stored["trigger_at"].as_str(),
        Some(rescheduled_trigger),
        "a refused claim must leave the writer's reschedule intact: {stored:?}"
    );
    assert!(
        stored.get("dispatch_receipt").is_none() || stored["dispatch_receipt"].is_null(),
        "a refused claim must persist no receipt: {stored:?}"
    );
}

/// The same refusal, driven through the production drain rather than the
/// claim primitive, so it would fail if the drain's call site stopped
/// passing the page snapshot's `trigger_at` down. Parks at
/// `PausePoint::BeforeClaim` so the concurrent reschedule lands strictly
/// between the candidate-page query and the claim: that is the window in
/// which the occurrence id is already derived but not yet persisted.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn production_drain_refuses_to_claim_an_event_rescheduled_in_the_claim_window() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let id = create_scheduled_event(
        &rt,
        "local",
        &due_rfc3339(),
        Some("stats()"),
        None,
        "schedule",
    )
    .await;

    let gate = race_seam::PauseGate {
        at: race_seam::PausePoint::BeforeClaim,
        reached: std::sync::Arc::new(tokio::sync::Barrier::new(2)),
        release: std::sync::Arc::new(tokio::sync::Barrier::new(2)),
    };

    let drain_task = {
        let rt = rt.clone();
        let gate = gate.clone();
        tokio::spawn(race_seam::PAUSE_GATE.scope(gate, async move {
            let server = KhiveMcpServer::new(rt.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;
            run_pending_events_on(&rt, &server, false).await
        }))
    };

    gate.reached.wait().await;

    // Still due, so the row stays a drain candidate and the refusal cannot
    // be confused with the event simply not being ready.
    let rescheduled_trigger = "2001-01-01T00:00:00Z";
    let mut writer = rt.sql().writer().await.expect("writer");
    let rows = writer
        .execute(SqlStatement {
            sql: "UPDATE notes SET properties = json_set(properties, '$.trigger_at', ?2) \
                      WHERE id = ?1"
                .to_string(),
            params: vec![
                SqlValue::Text(id.to_string()),
                SqlValue::Text(rescheduled_trigger.to_string()),
            ],
            label: Some("test_concurrent_reschedule".into()),
        })
        .await
        .expect("concurrent write");
    assert_eq!(rows, 1);
    drop(writer);

    gate.release.wait().await;
    let summary = drain_task
        .await
        .expect("drain task")
        .expect("drain must not error");
    assert_eq!(
        summary.fired, 0,
        "an event rescheduled inside the claim window must not fire on this pass: {summary:?}"
    );
    assert_eq!(
        summary.skipped_race, 1,
        "the pass must record the refusal as a lost race, not as a failure or a silent \
             no-candidate pass: {summary:?}"
    );
    assert_eq!(
        summary.failed, 0,
        "a refused claim is not an error: {summary:?}"
    );

    let stored = get_note_props(&rt, id).await;
    assert_eq!(
        stored["status"].as_str(),
        Some("pending"),
        "the refused row must stay pending for the next drain: {stored:?}"
    );
    assert_eq!(
        stored["trigger_at"].as_str(),
        Some(rescheduled_trigger),
        "the writer's reschedule must survive: {stored:?}"
    );
    assert!(
        stored.get("dispatch_receipt").is_none() || stored["dispatch_receipt"].is_null(),
        "no receipt may be persisted for a claim that never succeeded: {stored:?}"
    );
}

/// The post-claim half of the same invariant, and the one that cannot be
/// left to recovery.
///
/// A reschedule landing after the claim but before the finalizer's fresh
/// read is INSIDE that read, so every finalization CAS predicate passes:
/// status, `firing_at`, invocation id, lease, and the exact-properties
/// fence all match. Committing there would write a terminal `fired` row
/// whose `dispatch_receipt.occurrence_id` names the old instant while
/// `trigger_at` names the new one — and the receipt validator that would
/// catch that pairing is only ever reached through the recovery scan, which
/// fences on `status = 'firing'`. A terminal row is past it forever, so the
/// mismatch would never be adjudicated at all.
///
/// So the drain must refuse instead, leaving the row `firing` for recovery.
/// This differs from
/// `production_drain_refuses_to_claim_an_event_rescheduled_in_the_claim_window`
/// only in WHICH seam the write lands at, which is what makes the two
/// windows separately load-bearing.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn production_drain_refuses_to_finalize_an_event_rescheduled_after_the_claim() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let id = create_scheduled_event(
        &rt,
        "local",
        &due_rfc3339(),
        Some("stats()"),
        None,
        "schedule",
    )
    .await;

    let gate = race_seam::PauseGate {
        at: race_seam::PausePoint::BeforeFinalizeRead,
        reached: std::sync::Arc::new(tokio::sync::Barrier::new(2)),
        release: std::sync::Arc::new(tokio::sync::Barrier::new(2)),
    };

    let drain_task = {
        let rt = rt.clone();
        let gate = gate.clone();
        tokio::spawn(race_seam::PAUSE_GATE.scope(gate, async move {
            let server = KhiveMcpServer::new(rt.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;
            run_pending_events_on(&rt, &server, false).await
        }))
    };

    // Parked AFTER claim and dispatch, so the claim's trigger_at fence has
    // already passed and this write cannot be caught by it. Still a valid
    // parseable instant, so the refusal cannot be confused with the
    // unparseable-trigger branch.
    gate.reached.wait().await;
    let rescheduled_trigger = "2002-01-01T00:00:00Z";
    let mut writer = rt.sql().writer().await.expect("writer");
    let rows = writer
        .execute(SqlStatement {
            sql: "UPDATE notes SET properties = json_set(properties, '$.trigger_at', ?2) \
                      WHERE id = ?1"
                .to_string(),
            params: vec![
                SqlValue::Text(id.to_string()),
                SqlValue::Text(rescheduled_trigger.to_string()),
            ],
            label: Some("test_reschedule_after_claim".into()),
        })
        .await
        .expect("concurrent write");
    assert_eq!(rows, 1);
    drop(writer);

    gate.release.wait().await;
    let summary = drain_task
        .await
        .expect("drain task")
        .expect("drain must not error");
    assert_eq!(
        summary.fired, 0,
        "no terminal row may be written for an occurrence the row no longer names: {summary:?}"
    );
    assert_eq!(
        summary.failed, 1,
        "the refusal must be recorded as a failed finalization, which is what leaves the row \
             for recovery: {summary:?}"
    );

    let stored = get_note_props(&rt, id).await;
    assert_eq!(
        stored["status"].as_str(),
        Some("firing"),
        "the row must stay firing so the recovery scan, which fences on status='firing', can \
             still reach it; a terminal row would be past that scan forever: {stored:?}"
    );
    assert_eq!(
        stored["trigger_at"].as_str(),
        Some(rescheduled_trigger),
        "the writer's reschedule must survive: {stored:?}"
    );
    assert!(
        stored.get("dispatch_receipt").is_some(),
        "the claim receipt stays on the row for the validator to adjudicate: {stored:?}"
    );
}

/// `schedule.cancel` on a row that is currently `status="firing"` — even
/// a *stale* one — must still fail cleanly: reclaim only happens as part
/// of a drain pass, so cancel itself never reclaims.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn cancel_on_stale_firing_row_still_fails_cleanly() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let server = KhiveMcpServer::new(rt.clone()).expect("server");

    let past = "2000-01-01T00:00:00Z";
    let id = create_scheduled_event(&rt, "local", past, Some("stats()"), None, "schedule").await;

    let stale_firing_at = Utc::now().timestamp_micros() - (LEGACY_STALE_FIRING_TIMEOUT_MICROS * 2);
    force_set_properties(
        &rt,
        id,
        &json!({
            "trigger_at": past,
            "repeat": null,
            "status": "firing",
            "event_type": "schedule",
            "payload": "stats()",
            "fired_at": null,
            "cancelled_at": null,
            "firing_at": stale_firing_at,
        }),
    )
    .await;

    let cancel_ops = serde_json::to_string(&serde_json::json!([
        { "tool": "schedule.cancel", "args": { "id": id.to_string() } }
    ]))
    .expect("serialize cancel op");
    let cancel_result = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: cancel_ops,
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("dispatch_request_local must not error at the RPC layer");
    let cancel_json: Value = serde_json::from_str(&cancel_result).expect("valid JSON");
    let op_result = &cancel_json["results"][0];
    assert_eq!(
        op_result["ok"], false,
        "cancel of a stale-but-still-firing event must fail, not silently succeed \
             (reclaim happens on drain, not cancel): {cancel_json}"
    );
    let cancel_err = op_result["error"]["message"]
        .as_str()
        .expect("error.message is text");
    assert!(
        cancel_err.contains("not pending"),
        "cancel must report the event is no longer pending; got: {cancel_err}"
    );

    // The row is still "firing" (untouched by the failed cancel attempt).
    let props = get_note_props(&rt, id).await;
    assert_eq!(
        props["status"].as_str().unwrap_or(""),
        "firing",
        "a failed cancel must not alter the row's status"
    );
}

// Unit tests for next_trigger_at

#[test]
fn next_trigger_at_daily() {
    let base: DateTime<Utc> = "2026-06-01T09:00:00Z".parse().unwrap();
    let next = next_trigger_at(&Some("daily".to_string()), base).unwrap();
    assert_eq!(next, base + Duration::days(1));
}

#[test]
fn next_trigger_at_weekly() {
    let base: DateTime<Utc> = "2026-06-01T09:00:00Z".parse().unwrap();
    let next = next_trigger_at(&Some("weekly".to_string()), base).unwrap();
    assert_eq!(next, base + Duration::weeks(1));
}

#[test]
fn next_trigger_at_monthly() {
    let base: DateTime<Utc> = "2026-06-01T09:00:00Z".parse().unwrap();
    let next = next_trigger_at(&Some("monthly".to_string()), base).unwrap();
    // June 1 + 1 month = July 1
    let expected: DateTime<Utc> = "2026-07-01T09:00:00Z".parse().unwrap();
    assert_eq!(next, expected);
}

fn complete_repeating_occurrence(properties: Value, current: &str, repeat: &str) -> Value {
    let trigger_at: DateTime<Utc> = current.parse().expect("fixture trigger");
    let (properties, disposition) = final_properties_after_dispatch(
        properties,
        json!({"completed_at": trigger_at.timestamp_micros()}),
        &DispatchCompletion::Succeeded,
        trigger_at,
        FixedOffset::east_opt(0).expect("UTC offset"),
        &Some(repeat.to_string()),
    );
    assert_eq!(disposition, FinalDisposition::Advanced);
    assert_eq!(properties["status"], "pending");
    properties
}

#[test]
fn monthly_anchor_restores_day_after_nonleap_february_and_april() {
    let anchor = "2027-01-31T09:30:00Z";
    let mut properties = json!({
        "event_type": "schedule",
        "trigger_at": anchor,
        "repeat": "monthly",
        "repeat_anchor": anchor,
    });
    let mut current = anchor;
    for expected in [
        "2027-02-28T09:30:00+00:00",
        "2027-03-31T09:30:00+00:00",
        "2027-04-30T09:30:00+00:00",
    ] {
        properties = complete_repeating_occurrence(properties, current, "monthly");
        assert_eq!(properties["trigger_at"], expected);
        assert_eq!(properties["repeat_anchor"], anchor);
        current = expected;
    }
}

#[test]
fn monthly_anchor_restores_day_after_leap_february() {
    let anchor = "2028-01-31T09:30:00Z";
    let mut properties = json!({
        "event_type": "schedule",
        "trigger_at": anchor,
        "repeat": "monthly",
        "repeat_anchor": anchor,
    });
    let mut current = anchor;
    for expected in ["2028-02-29T09:30:00+00:00", "2028-03-31T09:30:00+00:00"] {
        properties = complete_repeating_occurrence(properties, current, "monthly");
        assert_eq!(properties["trigger_at"], expected);
        assert_eq!(properties["repeat_anchor"], anchor);
        current = expected;
    }
}

#[test]
fn failed_monthly_attempt_rearms_from_anchor_after_february() {
    let anchor = "2027-01-31T09:30:00Z";
    let february: DateTime<Utc> = "2027-02-28T09:30:00Z".parse().unwrap();
    let (properties, disposition) = final_properties_after_dispatch(
        json!({
            "event_type": "schedule",
            "trigger_at": "2027-02-28T09:30:00Z",
            "repeat": "monthly",
            "repeat_anchor": anchor,
        }),
        json!({"completed_at": february.timestamp_micros()}),
        &DispatchCompletion::Failed(DispatchFailure::plain("test failure")),
        february,
        FixedOffset::east_opt(0).expect("UTC offset"),
        &Some("monthly".to_string()),
    );
    assert_eq!(disposition, FinalDisposition::Advanced);
    assert_eq!(properties["trigger_at"], "2027-03-31T09:30:00+00:00");
    assert_eq!(properties["repeat_anchor"], anchor);
}

#[test]
fn legacy_monthly_row_adopts_current_trigger_once_and_stops_drifting() {
    let jan_anchor = "2027-01-31T09:30:00Z";
    let first = complete_repeating_occurrence(
        json!({"event_type": "schedule", "trigger_at": jan_anchor, "repeat": "monthly"}),
        jan_anchor,
        "monthly",
    );
    assert_eq!(first["trigger_at"], "2027-02-28T09:30:00+00:00");
    assert_eq!(first["repeat_anchor"], jan_anchor);
    let second = complete_repeating_occurrence(first, "2027-02-28T09:30:00+00:00", "monthly");
    assert_eq!(second["trigger_at"], "2027-03-31T09:30:00+00:00");
    assert_eq!(second["repeat_anchor"], jan_anchor);

    let clamped_legacy = "2027-02-28T09:30:00Z";
    let advanced = complete_repeating_occurrence(
        json!({
            "event_type": "schedule",
            "trigger_at": clamped_legacy,
            "repeat": "monthly"
        }),
        clamped_legacy,
        "monthly",
    );
    assert_eq!(advanced["trigger_at"], "2027-03-28T09:30:00+00:00");
    assert_eq!(advanced["repeat_anchor"], clamped_legacy);
}

#[test]
fn nonmonthly_forms_advance_without_creating_repeat_anchor() {
    let current = "2026-06-01T09:00:00Z";
    for (repeat, expected) in [
        ("daily", "2026-06-02T09:00:00+00:00"),
        ("weekly", "2026-06-08T09:00:00+00:00"),
        ("every:15m", "2026-06-01T09:15:00+00:00"),
        ("0 9 * * 1", "2026-06-08T09:00:00+00:00"),
    ] {
        let advanced = complete_repeating_occurrence(
            json!({"event_type": "schedule", "trigger_at": current, "repeat": repeat}),
            current,
            repeat,
        );
        assert_eq!(advanced["trigger_at"], expected, "{repeat}");
        assert!(
            advanced.get("repeat_anchor").is_none(),
            "{repeat} must not write a monthly anchor"
        );
    }
}

#[test]
fn next_trigger_at_none_repeat_returns_none() {
    let base: DateTime<Utc> = "2026-06-01T09:00:00Z".parse().unwrap();
    assert!(next_trigger_at(&None, base).is_none());
}

#[test]
fn next_trigger_at_every_adds_the_interval_to_the_previous_trigger() {
    let base: DateTime<Utc> = "2026-06-01T09:00:00Z".parse().unwrap();
    let next = next_trigger_at(&Some("every:15m".to_string()), base).unwrap();
    assert_eq!(next, base + Duration::minutes(15));
    let next = next_trigger_at(&Some("every:2h".to_string()), base).unwrap();
    assert_eq!(next, base + Duration::hours(2));
}

#[test]
fn next_trigger_at_cron_advances_to_the_next_match_in_utc() {
    // 2026-06-01 is a Monday; the next Monday 09:00 is a week later.
    let base: DateTime<Utc> = "2026-06-01T09:00:00Z".parse().unwrap();
    let next = next_trigger_at(&Some("0 9 * * 1".to_string()), base).unwrap();
    let expected: DateTime<Utc> = "2026-06-08T09:00:00Z".parse().unwrap();
    assert_eq!(next, expected);
    let next = next_trigger_at(&Some("*/15 * * * *".to_string()), base).unwrap();
    assert_eq!(next, base + Duration::minutes(15));
}

#[test]
fn next_trigger_at_unparseable_legacy_row_fails_closed() {
    let base: DateTime<Utc> = "2026-06-01T09:00:00Z".parse().unwrap();
    assert!(next_trigger_at(&Some("99 * * * *".to_string()), base).is_none());
    assert!(next_trigger_at(&Some("every:0s".to_string()), base).is_none());
}

// ── ADR-106 missed-event policy ─────────────────────────────────────────

/// Deterministic unit test for `advance_repeat_past_missed`: 14 daily
/// occurrences accumulated while an event was undrained must be skipped
/// in a single advance to the first occurrence strictly after `now` —
/// never a multi-fire catch-up burst.
#[test]
fn advance_repeat_past_missed_skips_all_accumulated_occurrences() {
    let now: DateTime<Utc> = "2026-06-15T09:00:00Z".parse().unwrap();
    let original: DateTime<Utc> = "2026-06-01T09:00:00Z".parse().unwrap();
    let next = advance_repeat_past_missed(&Some("daily".to_string()), original, now).unwrap();
    assert!(next > now, "advanced occurrence must be strictly future");
    assert!(
        next <= now + Duration::days(1),
        "must land on the very next occurrence, not skip further than one interval past now"
    );
    assert_eq!(
        next,
        original + Duration::days(15),
        "must be exactly the first daily occurrence after now (single advance, no burst)"
    );
}

#[test]
fn missed_january_monthly_occurrence_rearms_on_march_anchor_day() {
    let jan_31: DateTime<Utc> = "2027-01-31T09:30:00Z".parse().unwrap();
    let mid_march: DateTime<Utc> = "2027-03-15T12:00:00Z".parse().unwrap();
    let next = advance_repeat_past_missed(&Some("monthly".to_string()), jan_31, mid_march);
    assert_eq!(
        next,
        Some("2027-03-31T09:30:00Z".parse().unwrap()),
        "tick time is only a bound; February's clamp must not become the base"
    );
}

#[test]
fn missed_february_monthly_occurrence_reads_stored_january_anchor() {
    let february: DateTime<Utc> = "2027-02-28T09:30:00Z".parse().unwrap();
    let mid_march: DateTime<Utc> = "2027-03-15T12:00:00Z".parse().unwrap();
    let mut properties = json!({
        "event_type": "schedule",
        "trigger_at": "2027-02-28T09:30:00Z",
        "repeat": "monthly",
        "repeat_anchor": "2027-01-31T09:30:00Z",
    });
    let next = advance_repeat_past_missed_for_event(
        &mut properties,
        &Some("monthly".to_string()),
        february,
        mid_march,
    )
    .expect("stored monthly anchor");
    assert_eq!(next, Some("2027-03-31T09:30:00Z".parse().unwrap()));
    assert_eq!(properties["repeat_anchor"], "2027-01-31T09:30:00Z");
}

#[test]
fn malformed_stored_monthly_anchor_does_not_advance() {
    let current: DateTime<Utc> = "2027-02-28T09:30:00Z".parse().unwrap();
    let mut properties = json!({
        "trigger_at": "2027-02-28T09:30:00Z",
        "repeat_anchor": "not-a-timestamp",
    });
    assert!(
        next_trigger_at_for_event(&mut properties, &Some("monthly".to_string()), current,).is_err()
    );
    assert_eq!(properties["trigger_at"], "2027-02-28T09:30:00Z");
    assert_eq!(properties["repeat_anchor"], "not-a-timestamp");
}

#[test]
fn monthly_anchor_rejects_nonstring_and_later_than_trigger() {
    let current: DateTime<Utc> = "2027-02-28T09:30:00Z".parse().unwrap();
    for anchor in [json!(42), json!("2027-03-01T09:30:00Z")] {
        let mut properties = json!({
            "trigger_at": "2027-02-28T09:30:00Z",
            "repeat": "monthly",
            "repeat_anchor": anchor,
        });
        assert_eq!(
            next_trigger_at_for_event(&mut properties, &Some("monthly".to_string()), current),
            Err(INVALID_MONTHLY_ANCHOR)
        );
        assert_eq!(properties["repeat_anchor"], anchor);
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn stored_unadvanceable_interval_fails_after_success_without_becoming_one_shot() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let server = KhiveMcpServer::new(rt.clone()).expect("server");
    let trigger = due_rfc3339();
    let id = create_scheduled_event(
        &rt,
        "local",
        &trigger,
        Some("stats()"),
        Some("every:100000000d"),
        "schedule",
    )
    .await;

    let summary = run_pending_events_on(&rt, &server, false)
        .await
        .expect("drain unadvanceable stored repeat");
    assert_eq!(summary.invoked, 1, "{summary:?}");
    assert_eq!(summary.finalized, 1, "{summary:?}");
    assert_eq!(summary.failed, 1, "{summary:?}");
    assert_eq!(summary.indeterminate, 0, "{summary:?}");
    assert_eq!(summary.fired, 0, "repeat must not become a one-shot");
    assert_eq!(
        summary.retry_pending, 0,
        "repeat must not retry at the same trigger"
    );
    let properties = get_note_props(&rt, id).await;
    assert_eq!(properties["status"], "failed", "{properties}");
    assert_eq!(properties["trigger_at"], trigger);
    assert_eq!(properties["recurrence_error"], UNADVANCEABLE_REPEAT);
    assert_eq!(properties["dispatch_receipt"]["state"], "succeeded");
    assert!(properties["fired_at"].as_str().is_some());

    let second = run_pending_events_on(&rt, &server, false)
        .await
        .expect("terminal row remains inert");
    assert_eq!(second.invoked, 0, "{second:?}");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn successful_action_with_invalid_monthly_anchor_is_a_known_failed_row() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let server = KhiveMcpServer::new(rt.clone()).expect("server");
    let id = create_scheduled_event(
        &rt,
        "local",
        &due_rfc3339(),
        Some("stats()"),
        Some("monthly"),
        "schedule",
    )
    .await;
    set_repeat_anchor_for_test(&rt, id, json!("not-a-timestamp")).await;

    let summary = run_pending_events_on(&rt, &server, false)
        .await
        .expect("drain invalid monthly anchor");
    assert_eq!(summary.invoked, 1, "{summary:?}");
    assert_eq!(summary.finalized, 1, "{summary:?}");
    assert_eq!(summary.failed, 1, "{summary:?}");
    assert_eq!(summary.indeterminate, 0, "{summary:?}");
    let properties = get_note_props(&rt, id).await;
    assert_eq!(properties["status"], "failed", "{properties}");
    assert_eq!(properties["dispatch_receipt"]["state"], "succeeded");
    assert_eq!(properties["recurrence_error"], INVALID_MONTHLY_ANCHOR);
    assert!(properties.get("dispatch_error").is_none());
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn failed_action_keeps_its_error_when_monthly_anchor_is_invalid() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let invocations = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let handler_invocations = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut builder = khive_runtime::VerbRegistryBuilder::new();
    builder.with_default_namespace("local");
    builder.with_gate(std::sync::Arc::new(DenyOrdinaryHandlerFailureGate {
        checks: invocations.clone(),
    }));
    builder.register(OrdinaryHandlerFailurePack {
        invocations: handler_invocations.clone(),
    });
    let server = KhiveMcpServer::from_registry(builder.build().expect("test registry"));
    let id = create_scheduled_event(
        &rt,
        "local",
        &due_rfc3339(),
        Some("test.ordinary_handler_failure()"),
        Some("monthly"),
        "schedule",
    )
    .await;
    set_repeat_anchor_for_test(&rt, id, json!("not-a-timestamp")).await;

    let summary = run_pending_events_on(&rt, &server, false)
        .await
        .expect("drain invalid monthly anchor");
    assert_eq!(summary.invoked, 1, "{summary:?}");
    assert_eq!(summary.finalized, 1, "{summary:?}");
    assert_eq!(
        summary.failed, 1,
        "action and calendar failure count one row"
    );
    assert_eq!(
        summary.indeterminate, 0,
        "known action outcome is not indeterminate"
    );
    let properties = get_note_props(&rt, id).await;
    assert_eq!(properties["status"], "failed", "{properties}");
    assert_eq!(properties["dispatch_receipt"]["state"], "failed");
    assert_eq!(
        properties["dispatch_receipt"]["error_payload"]["domain_disposition"],
        "not_committed"
    );
    assert_eq!(properties["recurrence_error"], INVALID_MONTHLY_ANCHOR);
    assert_eq!(
        properties["dispatch_error"], properties["dispatch_receipt"]["error"],
        "the action's error must not be replaced by the anchor error"
    );
    assert_ne!(properties["dispatch_error"], properties["recurrence_error"]);

    let second = run_pending_events_on(&rt, &server, false)
        .await
        .expect("terminal row remains inert");
    assert_eq!(second.invoked, 0, "{second:?}");
    assert_eq!(invocations.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        handler_invocations.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn recovered_known_success_with_invalid_anchor_counts_failed_not_indeterminate() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let server = KhiveMcpServer::new(rt.clone()).expect("server");
    let trigger = due_rfc3339();
    let id = create_scheduled_event(
        &rt,
        "local",
        &trigger,
        Some("stats()"),
        Some("monthly"),
        "schedule",
    )
    .await;
    set_repeat_anchor_for_test(&rt, id, json!(42)).await;
    let claim = claim_for_test(&rt, id, &trigger).await;
    assert!(
        mark_dispatch_invoking(&rt, "local", id, &claim, short_test_lease())
            .await
            .expect("mark invoking")
    );
    let receipt =
        persist_dispatch_outcome(&rt, "local", id, &claim, &DispatchCompletion::Succeeded)
            .await
            .expect("persist outcome")
            .expect("claim still owned");
    expire_dispatch_lease_for_test(&rt, id).await;

    let summary = run_pending_events_on(&rt, &server, false)
        .await
        .expect("recover invalid monthly anchor");
    assert_eq!(summary.reclaimed, 1, "{summary:?}");
    assert_eq!(summary.invoked, 0, "{summary:?}");
    assert_eq!(summary.finalized, 1, "{summary:?}");
    assert_eq!(summary.failed, 1, "{summary:?}");
    assert_eq!(summary.indeterminate, 0, "{summary:?}");
    let properties = get_note_props(&rt, id).await;
    assert_eq!(properties["status"], "failed", "{properties}");
    assert_eq!(properties["recurrence_error"], INVALID_MONTHLY_ANCHOR);
    assert_eq!(properties["dispatch_receipt"], receipt);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn missed_invalid_anchor_counts_once_and_does_not_enter_missed_list() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let server = KhiveMcpServer::new(rt.clone()).expect("server");
    let id = create_scheduled_event(
        &rt,
        "local",
        "2000-01-01T00:00:00Z",
        Some("stats()"),
        Some("monthly"),
        "schedule",
    )
    .await;
    set_repeat_anchor_for_test(&rt, id, json!("not-a-timestamp")).await;

    let summary = run_pending_events_on(&rt, &server, false)
        .await
        .expect("drain missed invalid anchor");
    assert_eq!(summary.invoked, 0, "{summary:?}");
    assert_eq!(summary.finalized, 1, "{summary:?}");
    assert_eq!(summary.failed, 1, "{summary:?}");
    assert!(summary.missed.is_empty(), "failed row is not a missed row");
    let properties = get_note_props(&rt, id).await;
    assert_eq!(properties["status"], "failed", "{properties}");
    assert_eq!(properties["recurrence_error"], INVALID_MONTHLY_ANCHOR);
    assert_eq!(properties["dispatch_receipt"]["state"], "missed");
}

#[test]
fn advance_repeat_past_missed_interval_lands_on_the_first_future_occurrence() {
    let original: DateTime<Utc> = "2026-06-01T09:00:00Z".parse().unwrap();
    let now: DateTime<Utc> = "2026-06-15T09:07:00Z".parse().unwrap();
    let next = advance_repeat_past_missed(&Some("every:15m".to_string()), original, now).unwrap();
    let expected: DateTime<Utc> = "2026-06-15T09:15:00Z".parse().unwrap();
    assert_eq!(
        next, expected,
        "phase-locked to the original trigger, strictly after now"
    );
    let on_the_dot: DateTime<Utc> = "2026-06-15T09:15:00Z".parse().unwrap();
    let next =
        advance_repeat_past_missed(&Some("every:15m".to_string()), original, on_the_dot).unwrap();
    assert_eq!(next, on_the_dot + Duration::minutes(15));
}

#[test]
fn advance_repeat_past_missed_cron_asks_the_pattern_from_now() {
    let original: DateTime<Utc> = "2026-06-01T09:00:00Z".parse().unwrap();
    let now: DateTime<Utc> = "2026-06-17T10:00:00Z".parse().unwrap();
    let next = advance_repeat_past_missed(&Some("0 9 * * 1".to_string()), original, now).unwrap();
    let expected: DateTime<Utc> = "2026-06-22T09:00:00Z".parse().unwrap();
    assert_eq!(next, expected);
}

/// No `repeat` never advances, so the caller marks a stale one-shot missed.
#[test]
fn advance_repeat_past_missed_no_repeat_returns_none() {
    let now: DateTime<Utc> = "2026-06-15T09:00:00Z".parse().unwrap();
    let original: DateTime<Utc> = "2026-06-01T09:00:00Z".parse().unwrap();
    assert!(advance_repeat_past_missed(&None, original, now).is_none());
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn missed_reminder_receipt_retains_creator_not_daemon_actor() {
    let (_tmp, db_path) = tmp_db();
    let creator_rt = make_rt_with_actor(&db_path, Some("lambda:reminder-owner")).await;
    let id = create_scheduled_event(
        &creator_rt,
        "local",
        "2000-01-01T00:00:00Z",
        None,
        None,
        "remind",
    )
    .await;

    let daemon_rt = make_rt_with_actor(&db_path, Some("lambda:scheduler-daemon")).await;
    let server = KhiveMcpServer::new(daemon_rt.clone()).expect("daemon server");
    let summary = run_pending_events_on(&daemon_rt, &server, false)
        .await
        .expect("missed reminder drain");
    assert_eq!(summary.invoked, 0);
    assert_eq!(summary.missed, vec![id]);

    let props = get_note_props(&daemon_rt, id).await;
    assert_eq!(props["status"], "missed", "{props}");
    assert_eq!(
        props["dispatch_receipt"]["actor"], "actor:lambda:reminder-owner",
        "a grace-policy receipt is still creator-attributed: {props}"
    );
    assert!(
        inbound_reminder_messages(&daemon_rt, "lambda:reminder-owner")
            .await
            .is_empty(),
        "a missed reminder must not dispatch"
    );
    assert!(
        inbound_reminder_messages(&daemon_rt, "lambda:scheduler-daemon")
            .await
            .is_empty(),
        "the daemon actor must neither receive nor own the missed reminder"
    );
}

/// 9 non-repeating events overdue well beyond the default grace window
/// (the first-boot-against-a-large-backlog scenario) must ALL be marked
/// `"missed"` and NONE dispatched — asserted by the absence of the
/// side-effecting action's write, not just zeroed summary counters.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn nine_overdue_events_beyond_grace_are_missed_with_zero_dispatch() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;

    let past = "2000-01-01T00:00:00Z";
    let marker = "nine-overdue-zero-dispatch-marker";
    let action_dsl = format!("create(kind=\"observation\", content=\"{marker}\")");
    let mut ids = Vec::new();
    for _ in 0..9 {
        let id = create_scheduled_event(
            &rt,
            "local",
            past,
            Some(action_dsl.as_str()),
            None,
            "schedule",
        )
        .await;
        ids.push(id);
    }

    let summary = drain_for_test(&db_path).await.expect("drain");

    assert_eq!(summary.scanned, 9, "all 9 overdue rows must be scanned");
    assert_eq!(summary.fired, 0, "zero dispatches: nothing may be fired");
    assert_eq!(
        summary.advanced, 0,
        "zero dispatches: nothing may be advanced"
    );
    assert_eq!(summary.failed, 0, "the missed path is not a failure");
    assert_eq!(
        summary.missed.len(),
        9,
        "all 9 overdue rows must be marked missed, got summary={summary:?}"
    );
    for id in &ids {
        assert!(
            summary.missed.contains(id),
            "missed list must name every overdue id"
        );
    }

    for id in ids {
        let props = get_note_props(&rt, id).await;
        assert_eq!(
            props["status"].as_str(),
            Some("missed"),
            "note {id} must end in status=missed, got {props:?}"
        );
        assert!(
            props["missed_at"].as_i64().is_some(),
            "note {id} must have missed_at stamped, got {props:?}"
        );
        assert!(
            props["fired_at"].is_null(),
            "note {id} must never have fired_at set (never dispatched), got {props:?}"
        );
        assert_eq!(
            props["dispatch_receipt"]["state"],
            DispatchReceiptState::Missed.as_str(),
            "the durable claim receipt must survive missed finalization: {props}"
        );
        assert!(props["dispatch_receipt"]["completed_at"].as_i64().is_some());
        assert!(props["dispatch_receipt"]["error"].is_null());
    }

    // Strongest evidence: the side-effecting action's own output record
    // must be entirely absent — not merely "summary says zero fired".
    let ns = Namespace::parse("local").unwrap();
    let token = rt.authorize(ns).expect("authorize");
    let store = rt.notes(&token).expect("notes");
    let page = store
        .query_notes(
            "local",
            Some("observation"),
            PageRequest {
                limit: 50,
                offset: 0,
            },
        )
        .await
        .expect("query observation notes");
    let marker_hits: Vec<_> = page.items.iter().filter(|n| n.content == marker).collect();
    assert!(
        marker_hits.is_empty(),
        "the missed action must never dispatch: found {} marker note(s): {marker_hits:?}",
        marker_hits.len()
    );
}

/// An event overdue by less than the grace window must still fire
/// normally — the missed policy only applies beyond the grace threshold.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn overdue_within_grace_still_fires() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;

    // 60s overdue is comfortably inside the 300s default grace window.
    let trigger_at = (Utc::now() - Duration::seconds(60)).to_rfc3339();
    let id =
        create_scheduled_event(&rt, "local", &trigger_at, Some("stats()"), None, "schedule").await;

    let summary = drain_for_test(&db_path).await.expect("drain");

    assert!(
        summary.missed.is_empty(),
        "an event within grace must never be marked missed, got summary={summary:?}"
    );
    assert!(
        summary.fired >= 1 || summary.advanced >= 1,
        "an event within grace must be dispatched normally, got summary={summary:?}"
    );

    let props = get_note_props(&rt, id).await;
    assert_eq!(
        props["status"].as_str(),
        Some("fired"),
        "non-repeating in-grace event must end fired, got {props:?}"
    );
    assert!(
        props["fired_at"].as_str().is_some(),
        "in-grace event must have fired_at set, got {props:?}"
    );
}

/// End-to-end (drain-level) confirmation that a missed *repeating* event
/// is re-armed at a future occurrence instead of ending terminally
/// missed — complements the deterministic
/// `advance_repeat_past_missed_skips_all_accumulated_occurrences` unit
/// test above with the full claim/finalize wiring.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn missed_repeat_is_rearmed_at_next_future_occurrence() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;

    // 10 days overdue with a daily repeat: 10 accumulated occurrences,
    // all missed, must collapse into exactly one future re-arm.
    let original_trigger: DateTime<Utc> = Utc::now() - Duration::days(10);
    let id = create_scheduled_event(
        &rt,
        "local",
        &original_trigger.to_rfc3339(),
        Some("stats()"),
        Some("daily"),
        "schedule",
    )
    .await;

    let summary = drain_for_test(&db_path).await.expect("drain");

    assert_eq!(summary.fired, 0, "a missed repeat must not fire");
    assert_eq!(
        summary.advanced, 0,
        "a missed repeat's re-arm is counted as missed, not advanced"
    );
    assert_eq!(
        summary.missed.len(),
        1,
        "exactly one missed occurrence recorded"
    );
    assert!(summary.missed.contains(&id));

    let props = get_note_props(&rt, id).await;
    assert_eq!(
        props["status"].as_str(),
        Some("pending"),
        "a missed repeat must be re-armed to pending, not left terminal, got {props:?}"
    );
    assert!(
        props["missed_at"].as_i64().is_some(),
        "missed_at must be stamped even though the row is re-armed, got {props:?}"
    );
    let new_trigger: DateTime<Utc> = props["trigger_at"]
        .as_str()
        .expect("trigger_at must be set")
        .parse()
        .expect("parseable trigger_at");
    let now = Utc::now();
    assert!(
        new_trigger > now,
        "re-armed trigger_at must be strictly in the future, got {new_trigger} (now={now})"
    );
    assert!(
        new_trigger <= now + Duration::days(1),
        "re-armed trigger_at must be the very next occurrence, not skip further \
             (no catch-up burst), got {new_trigger} (now={now})"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn missed_monthly_drain_records_miss_and_rearms_on_anchor_calendar() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let anchor_text = "2025-01-31T09:30:00Z";
    let anchor: DateTime<Utc> = anchor_text.parse().expect("anchor timestamp");
    let id = create_scheduled_event(
        &rt,
        "local",
        anchor_text,
        Some("stats()"),
        Some("monthly"),
        "schedule",
    )
    .await;

    let before = Utc::now();
    let summary = drain_for_test(&db_path)
        .await
        .expect("missed monthly drain");
    let after = Utc::now();
    assert_eq!(summary.invoked, 0, "missed occurrence must not dispatch");
    assert!(
        summary.missed.contains(&id),
        "original occurrence recorded as missed"
    );

    let props = get_note_props(&rt, id).await;
    assert_eq!(props["status"], "pending", "monthly row re-armed");
    assert!(
        props["missed_at"].as_i64().is_some(),
        "missed instant recorded"
    );
    assert_eq!(props["dispatch_receipt"]["state"], "missed");
    assert_eq!(
        props["dispatch_receipt"]["occurrence_id"],
        dispatch_occurrence_id(id, anchor).to_string(),
        "receipt must identify the original missed occurrence"
    );
    assert_eq!(props["repeat_anchor"], anchor_text, "legacy anchor adopted");

    let next: DateTime<Utc> = props["trigger_at"]
        .as_str()
        .expect("next trigger")
        .parse()
        .expect("parse next trigger");
    let first_anchored_after = |bound| {
        (1..=1200)
            .find_map(|months| {
                anchor
                    .checked_add_months(chrono::Months::new(months))
                    .filter(|candidate| *candidate > bound)
            })
            .expect("next monthly occurrence within a century")
    };
    assert!(
        next == first_anchored_after(before) || next == first_anchored_after(after),
        "re-armed occurrence must be the first anchored date after the drain tick; got {next}"
    );
}

/// Issue #792 (missed-path variant): a missed repeat's re-arm must also
/// preserve the original `trigger_at` offset, not just the normal
/// fire-and-advance path — both call through the same
/// `next_trigger_at`-derived arithmetic and must both render at the
/// caller's original offset.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn missed_repeat_rearm_preserves_original_offset() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;

    // 10 days overdue with a daily repeat, formatted at a non-UTC
    // +04:00 wall-clock offset.
    let plus_four = FixedOffset::east_opt(4 * 3600).expect("valid offset");
    let original_trigger = (Utc::now() - Duration::days(10)).with_timezone(&plus_four);
    let id = create_scheduled_event(
        &rt,
        "local",
        &original_trigger.to_rfc3339(),
        Some("stats()"),
        Some("daily"),
        "schedule",
    )
    .await;

    let summary = drain_for_test(&db_path).await.expect("drain");
    assert_eq!(
        summary.missed.len(),
        1,
        "exactly one missed occurrence recorded"
    );

    let props = get_note_props(&rt, id).await;
    let new_trigger = props["trigger_at"]
        .as_str()
        .expect("trigger_at must be set");
    assert!(
        new_trigger.ends_with("+04:00"),
        "re-armed trigger_at must preserve the original +04:00 offset, got {new_trigger:?}"
    );
    let new_dt = DateTime::parse_from_rfc3339(new_trigger).expect("parseable re-armed ts");
    assert_eq!(
        *new_dt.offset(),
        plus_four,
        "re-armed trigger_at offset must equal the original +04:00 offset"
    );
    assert_eq!(
        new_dt.time(),
        original_trigger.time(),
        "re-armed occurrence must retain the same local wall-clock time"
    );
}

/// A backlog larger than the drain's internal page size (200) must be
/// fully processed in ONE drain pass, not silently truncated at the page
/// boundary — 201 rows exercises the exact boundary.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn backlog_larger_than_page_size_is_fully_drained_in_one_pass() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;

    const OVERDUE_ROW_COUNT: usize = 201; // PAGE_SIZE (200) + 1
    let past = "2000-01-01T00:00:00Z"; // far beyond the missed-event grace window
    let mut ids = Vec::with_capacity(OVERDUE_ROW_COUNT);
    for _ in 0..OVERDUE_ROW_COUNT {
        let id =
            create_scheduled_event(&rt, "local", past, Some("stats()"), None, "schedule").await;
        ids.push(id);
    }

    let summary = drain_for_test(&db_path).await.expect("drain");

    assert_eq!(
        summary.scanned, OVERDUE_ROW_COUNT as u64,
        "every overdue row across both pages must be scanned in one pass, got \
             summary={summary:?}"
    );
    assert_eq!(
        summary.missed.len(),
        OVERDUE_ROW_COUNT,
        "every overdue row across both pages must be marked missed in one pass \
             (the page-boundary row must not be skipped), got summary={summary:?}"
    );
    for id in &ids {
        assert!(
            summary.missed.contains(id),
            "missed list must name every row, including ones beyond the first page"
        );
    }
    for id in ids {
        let props = get_note_props(&rt, id).await;
        assert_eq!(
            props["status"].as_str(),
            Some("missed"),
            "note {id} must end in status=missed (not left pending past the page \
                 boundary), got {props:?}"
        );
    }
}

/// Two concurrent drain passes over the same store must never double-fire
/// a row: the `pending -> firing` CAS claim makes exactly one of the two
/// concurrent callers win each row. Each action is a genuinely
/// side-effecting write (not a read-only op) so the test can assert
/// exactly ONE marker note per event exists, rather than trusting summary
/// counters alone to catch a double-dispatch-one-finalize regression.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn concurrent_drains_fire_each_row_exactly_once() {
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;

    const ROW_COUNT: usize = 20;
    let past = due_rfc3339(); // in-grace: exercises the normal fire path, not missed
    let mut ids = Vec::with_capacity(ROW_COUNT);
    let mut markers = Vec::with_capacity(ROW_COUNT);
    for i in 0..ROW_COUNT {
        let marker = format!("concurrent-drain-marker-{i}");
        let action_dsl = format!("create(kind=\"observation\", content=\"{marker}\")");
        let id = create_scheduled_event(
            &rt,
            "local",
            &past,
            Some(action_dsl.as_str()),
            None,
            "schedule",
        )
        .await;
        ids.push(id);
        markers.push(marker);
    }

    let db_path_a = db_path.clone();
    let db_path_b = db_path.clone();
    let (summary_a, summary_b) = tokio::join!(
        async move { drain_for_test(&db_path_a).await },
        async move { drain_for_test(&db_path_b).await },
    );
    let summary_a = summary_a.expect("drain A");
    let summary_b = summary_b.expect("drain B");

    let total_dispatched =
        summary_a.fired + summary_a.advanced + summary_b.fired + summary_b.advanced;
    assert_eq!(
        total_dispatched, ROW_COUNT as u64,
        "every row must be dispatched exactly once across both concurrent drains, \
             got a={summary_a:?} b={summary_b:?}"
    );
    assert_eq!(
        summary_a.failed + summary_b.failed,
        0,
        "the CAS claim must make the losing drain skip cleanly (skipped_race), \
             never fail: a={summary_a:?} b={summary_b:?}"
    );

    for id in &ids {
        let props = get_note_props(&rt, *id).await;
        assert_eq!(
            props["status"].as_str(),
            Some("fired"),
            "note {id} must end fired exactly once, got {props:?}"
        );
    }

    // Strongest evidence: exactly one marker note per row. A
    // double-dispatch-one-finalize bug would leave the CAS-tracked
    // `status`/summary counters looking clean while still writing the
    // action's side effect twice for the row that raced — this is the
    // only assertion that would catch it.
    let ns = Namespace::parse("local").unwrap();
    let token = rt.authorize(ns).expect("authorize");
    let store = rt.notes(&token).expect("notes");
    let page = store
        .query_notes(
            "local",
            Some("observation"),
            PageRequest {
                limit: (ROW_COUNT as u32) + 10,
                offset: 0,
            },
        )
        .await
        .expect("query observation notes");
    for marker in &markers {
        let hits: Vec<_> = page.items.iter().filter(|n| &n.content == marker).collect();
        assert_eq!(
            hits.len(),
            1,
            "marker {marker:?} must appear exactly once (double-dispatch check), \
                 found {}: {hits:?}",
            hits.len()
        );
    }
}

// `run_pending_events`'s wrapper seam must not misread a default
// namespace as an explicit actor override. These tests exercise the real
// config-discovery path (process cwd / `HOME`); the helpers below mirror
// `serve.rs`'s own equivalents, kept local since they are test-only.

/// RAII guard: redirects process cwd and `HOME` to isolated locations so
/// the real machine's global `~/.khive/config.toml` never leaks into a
/// test. Restores both on drop, even on panic/unwind.
struct SeatEnv {
    original_cwd: std::path::PathBuf,
    original_home: Option<std::ffi::OsString>,
    _isolated_home: tempfile::TempDir,
}

impl SeatEnv {
    fn enter(project_root: &std::path::Path) -> Self {
        let original_cwd = std::env::current_dir().expect("read cwd");
        let original_home = std::env::var_os("HOME");
        let isolated_home = tempfile::tempdir().expect("isolated HOME tempdir");
        std::env::set_current_dir(project_root).expect("chdir into seat project root");
        std::env::set_var("HOME", isolated_home.path());
        Self {
            original_cwd,
            original_home,
            _isolated_home: isolated_home,
        }
    }
}

impl Drop for SeatEnv {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original_cwd);
        match &self.original_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
    }
}

/// Write a project-local `.khive/config.toml` declaring `[actor] id`.
fn write_project_actor_config(project_root: &std::path::Path, actor_id: &str) {
    std::fs::create_dir_all(project_root.join(".khive")).expect("mkdir .khive");
    std::fs::write(
        project_root.join(".khive/config.toml"),
        format!("[actor]\nid = \"{actor_id}\"\n"),
    )
    .expect("write project actor config");
}

/// Regression: a `DatabaseOverrideConflict` raised by the builder must
/// leave `run_pending_events_with_config` as the top-level error so
/// `kkernel exec`'s refusal-envelope downcast recognizes it.
#[tokio::test]
#[serial_test::serial]
#[serial_test::serial(config_ledger)]
async fn run_pending_events_keeps_db_override_conflict_top_level() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    std::env::remove_var("KHIVE_DB");
    std::env::remove_var("KHIVE_PACKS");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");

    let seat_dir = tempfile::tempdir().expect("seat tempdir");
    let _seat_env = SeatEnv::enter(seat_dir.path());
    let config_dir = tempfile::tempdir().expect("config tempdir");
    let config_path = config_dir.path().join("backends.toml");
    std::fs::write(
        &config_path,
        "[[backends]]\nname = \"main\"\n\n[[backends]]\nname = \"sessions\"\n",
    )
    .expect("write multi-backend config");

    let error = run_pending_events_with_config(
        Some("/tmp/definitely-not-the-main.db"),
        Some(&config_path),
        "local",
        false,
    )
    .await
    .expect_err("a divergent concrete --db override must be refused");

    assert!(
        error
            .downcast_ref::<crate::serve::DatabaseOverrideConflict>()
            .is_some(),
        "the conflict must remain the top-level error for the refusal envelope: {error:?}"
    );
    assert!(
        crate::serve::db_override_refusal_envelope(&error).is_some(),
        "the documented JSON refusal envelope must be derivable: {error:?}"
    );
}

/// Sibling regression for the provenance half of the same seam: build
/// failures that are NOT the typed conflict keep the generic
/// "pending-events: build server" context (an invalid explicit config
/// surfaces as `config error: ...` underneath).
#[tokio::test]
#[serial_test::serial]
#[serial_test::serial(config_ledger)]
async fn run_pending_events_wraps_non_conflict_build_errors_with_context() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    std::env::remove_var("KHIVE_DB");
    std::env::remove_var("KHIVE_PACKS");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");

    let seat_dir = tempfile::tempdir().expect("seat tempdir");
    let _seat_env = SeatEnv::enter(seat_dir.path());
    let config_dir = tempfile::tempdir().expect("config tempdir");
    let config_path = config_dir.path().join("broken.toml");
    std::fs::write(&config_path, "this is not [valid toml\n").expect("write malformed config");

    let error = run_pending_events_with_config(None, Some(&config_path), "local", false)
        .await
        .expect_err("an invalid explicit config must fail the build");

    assert!(
        error
            .downcast_ref::<crate::serve::DatabaseOverrideConflict>()
            .is_none(),
        "not a database-override conflict: {error:?}"
    );
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("pending-events: build server"),
        "non-conflict build failures keep the generic provenance: {rendered}"
    );
    assert!(
        rendered.contains("config error"),
        "the underlying config failure must remain in the chain: {rendered}"
    );
}

/// An explicit `--config` naming a MISSING file must fail loud, not run
/// the drain with defaults; the error surfaces wrapped in the generic
/// build context, not as a `DatabaseOverrideConflict`.
#[tokio::test]
#[serial_test::serial]
#[serial_test::serial(config_ledger)]
async fn run_pending_events_fails_loud_for_missing_explicit_config() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    std::env::remove_var("KHIVE_DB");
    std::env::remove_var("KHIVE_PACKS");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");

    let seat_dir = tempfile::tempdir().expect("seat tempdir");
    let _seat_env = SeatEnv::enter(seat_dir.path());
    let config_dir = tempfile::tempdir().expect("config tempdir");
    let missing_config = config_dir.path().join("does-not-exist.toml");

    let error = run_pending_events_with_config(None, Some(&missing_config), "local", false)
        .await
        .expect_err("a missing explicit config must fail loud, not run with defaults");

    assert!(
        error
            .downcast_ref::<crate::serve::DatabaseOverrideConflict>()
            .is_none(),
        "not a database-override conflict: {error:?}"
    );
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("pending-events: build server"),
        "non-conflict build failures keep the generic provenance: {rendered}"
    );
    assert!(
        rendered.contains("does not exist"),
        "the underlying missing-file failure must name the selected path: {rendered}"
    );
    assert!(
        rendered.contains("does-not-exist.toml"),
        "the error must name the missing file the operator selected: {rendered}"
    );
}

/// The wrapper seam (`build_server_with_explicit_namespace`, called by
/// `run_pending_events` with `namespace_explicit: true, actor_explicit:
/// false`) must let a `"local"`-resolved default namespace fall through
/// to the project-configured actor — never clear it the way a genuine
/// `--actor`/`--namespace` CLI override would (`build_server`'s own,
/// correctly-narrower semantic). Regression for PR #782:
/// before this fix, `run_pending_events` called
/// `build_server` directly with a synthesized `namespace: Some("local")`,
/// which `resolve_cli_namespace` reported as `explicit = true` and
/// `build_server` then fed into BOTH `namespace_explicit` AND
/// `actor_explicit`, tripping the "genuinely explicit actor tier
/// requesting anonymous" branch in `resolve_runtime_config` and silently
/// discarding the configured `[actor] id`.
#[tokio::test]
#[serial_test::serial]
#[serial_test::serial(config_ledger)]
async fn wrapper_seam_falls_through_to_project_actor_instead_of_clearing_it() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    std::env::remove_var("KHIVE_ACTOR");
    std::env::remove_var("KHIVE_DB");
    std::env::remove_var("KHIVE_PACKS");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");

    let seat_dir = tempfile::tempdir().expect("seat tempdir");
    write_project_actor_config(seat_dir.path(), "lambda:pending-events-tenant");
    let _seat_env = SeatEnv::enter(seat_dir.path());

    let args = crate::args::Args {
        db: Some(":memory:".to_string()),
        actor: None,
        namespace: None,
        no_embed: false,
        pack: Vec::new(),
        config: None,
        daemon: false,
        lifetime: None,
        idle_timeout_secs: None,
        transport: None,
        bind: None,
        brain_profile: None,
        resumed_generation: None,
    };
    let ns = Namespace::parse("local").expect("local namespace");

    // The seam `run_pending_events` actually calls: namespace is a real
    // default (`namespace_explicit: true`) but NOT an actor override
    // (`actor_explicit: false`).
    let (_server, schedule_rt) =
        crate::serve::build_server_with_explicit_namespace(&args, ns, true, false)
            .await
            .expect("build_server_with_explicit_namespace must succeed");
    let rt = schedule_rt.expect("\"schedule\" pack is in the default pack set");
    assert_eq!(
        rt.config().actor_id.as_deref(),
        Some("lambda:pending-events-tenant"),
        "a default namespace resolving to \"local\" must fall through to the \
             project-configured [actor] id, not clear it as if it were an explicit \
             --actor/--namespace override"
    );
}

/// Positive control for the failure mode the fix above closes: routing
/// the same inputs through `build_server` (the genuine CLI-flag seam,
/// unchanged by this fix) DOES clear the actor, because there a
/// present namespace value really does mean "the operator typed
/// --namespace". This documents why `run_pending_events` must not reuse
/// that entry point for a synthesized, non-CLI-parsed namespace default.
#[tokio::test]
#[serial_test::serial]
#[serial_test::serial(config_ledger)]
async fn build_server_cli_seam_clears_actor_for_explicit_local_namespace() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    std::env::remove_var("KHIVE_ACTOR");
    std::env::remove_var("KHIVE_DB");
    std::env::remove_var("KHIVE_PACKS");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");

    let seat_dir = tempfile::tempdir().expect("seat tempdir");
    write_project_actor_config(seat_dir.path(), "lambda:pending-events-tenant");
    let _seat_env = SeatEnv::enter(seat_dir.path());

    let args = crate::args::Args {
        db: Some(":memory:".to_string()),
        actor: None,
        namespace: Some("local".to_string()),
        no_embed: false,
        pack: Vec::new(),
        config: None,
        daemon: false,
        lifetime: None,
        idle_timeout_secs: None,
        transport: None,
        bind: None,
        brain_profile: None,
        resumed_generation: None,
    };

    let (_server, schedule_rt) = crate::serve::build_server(&args)
        .await
        .expect("build_server must succeed");
    let rt = schedule_rt.expect("\"schedule\" pack is in the default pack set");
    assert_eq!(
        rt.config().actor_id,
        None,
        "build_server's genuine CLI-flag seam must still treat a present --namespace \
             value as an explicit actor override and clear the actor for \"local\" — this \
             is correct CLI behavior, unaffected by the wrapper-seam fix"
    );
}

/// `run_pending_events` (the actual `kkernel exec --pending-events`
/// entry point, not the lower-level `drain_for_test` helper) must
/// succeed under strict actor mode when a project `[actor] id` is
/// configured — proving the wrapper's server construction no longer
/// spuriously trips `enforce_strict_actor_mode` the way routing through
/// `build_server`'s actor-clearing path would have.
#[tokio::test]
#[serial_test::serial]
#[serial_test::serial(config_ledger)]
async fn wrapper_succeeds_under_strict_actor_mode_with_configured_project_actor() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    std::env::remove_var("KHIVE_ACTOR");
    std::env::remove_var("KHIVE_DB");
    std::env::remove_var("KHIVE_PACKS");
    let prev_strict = std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR").ok();
    std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", "1");

    let seat_dir = tempfile::tempdir().expect("seat tempdir");
    write_project_actor_config(seat_dir.path(), "lambda:pending-events-tenant");
    let _seat_env = SeatEnv::enter(seat_dir.path());

    let result = run_pending_events(Some(":memory:"), "local", false).await;

    match prev_strict {
        Some(v) => std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", v),
        None => std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
    }

    result.expect(
        "run_pending_events must succeed under strict actor mode when a project \
             [actor] id is configured — the same config a live `kkernel mcp --daemon` \
             boot in this project would resolve",
    );
}

// ── scheduled-error-scan: falsifiability arms ───────────────────────────
//
// A handler failure whose message matches a real credential pattern must
// never reach a stored `scheduled_event` record, the emitted delivery-
// failure log, or the durable `schedule.remind.fire` audit event in raw
// form -- but on the drain's two direct-SQL receipt writes (this is the
// DRAIN path: the dispatch already happened and the lease is already
// held), the real outcome must still land durably, masked rather than
// refused, so the row leaves `"firing"` instead of sitting there for
// expired-lease recovery to reclassify it as `"indeterminate"`.
// `AKIAIOSFODNN7EXAMPLE` is AWS's own published example access-key // gitleaks:allow
// id (docs.aws.amazon.com), obviously synthetic and recognized by the
// `aws-access-key-id` detector regardless of surrounding prose.

#[derive(Debug)]
struct DenyCommSendWithCredentialGate;

impl Gate for DenyCommSendWithCredentialGate {
    fn check(&self, request: &GateRequest) -> Result<GateDecision, GateError> {
        if request.verb == "comm.send" {
            Ok(GateDecision::deny(
                "comm.send denied: credential AKIAIOSFODNN7EXAMPLE in body", // gitleaks:allow
            ))
        } else {
            Ok(GateDecision::allow())
        }
    }
}

/// Captures `tracing` output for masking assertions. Mirrors
/// `khive-mcp/src/server.rs`'s `SearchCapturedLog` test helper.
#[derive(Clone, Default)]
struct CapturedDrainLog(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for CapturedDrainLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("captured drain log mutex poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedDrainLog {
    type Writer = CapturedDrainLog;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl CapturedDrainLog {
    fn contents(&self) -> String {
        String::from_utf8(
            self.0
                .lock()
                .expect("captured drain log mutex poisoned")
                .clone(),
        )
        .expect("captured drain logs are UTF-8")
    }
}

/// The masker itself: a credential appearing as a JSON object KEY (not
/// just a value) must be masked too -- `secret_gate::check_json`'s own
/// scanner checks keys (`scan_json_value` calls `check(k)` on every
/// key), so an unmasked key is exactly what the post-mask assert at each
/// call site is guaranteed to catch. Two distinct keys that mask to the
/// same string must both survive, disambiguated, rather than the later
/// one silently overwriting the earlier.
#[test]
fn mask_json_content_masks_object_keys_and_disambiguates_collisions() {
    const CREDENTIAL: &str = "AKIAIOSFODNN7EXAMPLE"; // gitleaks:allow
    const OTHER_CREDENTIAL: &str = "ASIAFAKEKEY00000000000"; // gitleaks:allow

    let keyed = json!({ CREDENTIAL: "ordinary value" });
    let masked = mask_json_content(&keyed);
    let masked_text = masked.to_string();
    assert!(
        !masked_text.contains(CREDENTIAL),
        "a credential used as an object key must never survive unmasked: {masked_text}"
    );
    assert!(
        masked_text.contains("***MASKED***"),
        "a masked key must carry the masked form: {masked_text}"
    );

    // Both fixtures are whole-string credential matches -- the
    // aws-access-key-id detector's AKIA and ASIA prefixes, each with
    // nothing left over once the marker is substituted in -- so they
    // mask down to the identical base string: the collision this
    // function must disambiguate rather than let the second entry
    // silently replace the first. Assert that precondition explicitly,
    // so a detector change that breaks it fails here with a clear
    // message rather than inside the assertions below.
    let masked_base = khive_runtime::secret_gate::bounded_masked_log_text(CREDENTIAL);
    assert_eq!(
        khive_runtime::secret_gate::bounded_masked_log_text(OTHER_CREDENTIAL),
        masked_base,
        "fixture precondition: both credentials must mask to the identical \
             base string for this test to exercise the collision path"
    );

    let colliding = {
        let mut map = serde_json::Map::new();
        map.insert(CREDENTIAL.to_string(), json!("first"));
        map.insert(OTHER_CREDENTIAL.to_string(), json!("second"));
        Value::Object(map)
    };
    let masked = mask_json_content(&colliding);
    let object = masked.as_object().expect("masked value is an object");
    assert_eq!(
        object.len(),
        2,
        "colliding masked keys must both survive, not overwrite one another: {object:?}"
    );
    assert_eq!(object.get(masked_base.as_str()), Some(&json!("first")));
    assert_eq!(
        object.get(format!("{masked_base}#2").as_str()),
        Some(&json!("second")),
        "the second colliding key must be disambiguated with a #2 suffix: {object:?}"
    );
}

/// A handler failure message matching a real credential pattern must
/// never reach the durable `scheduled_event` receipt in raw form via the
/// first direct-SQL write that bypasses the ordinary write-time content
/// scan (`persist_dispatch_outcome`) -- but unlike the outright refusal
/// this test used to assert, the write and the real outcome it records
/// must still land: the dispatch already happened and the lease is
/// already held by the time this runs, so a `?`-propagated refusal here
/// would not stop anything from occurring, only leave
/// `dispatch_receipt.state` stuck on `"invoking"` forever, which is
/// exactly the state expired-lease recovery treats as unresolved and
/// finalizes as `"indeterminate"` -- discarding the real, known
/// `"failed"` outcome.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn dispatch_outcome_write_masks_credential_shaped_failure_content() {
    const CREDENTIAL: &str = "AKIAIOSFODNN7EXAMPLE"; // gitleaks:allow
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let trigger = due_rfc3339();
    let id =
        create_scheduled_event(&rt, "local", &trigger, Some("stats()"), None, "schedule").await;
    let claim = claim_for_test(&rt, id, &trigger).await;
    assert!(
        mark_dispatch_invoking(&rt, "local", id, &claim, short_test_lease())
            .await
            .expect("mark invoking")
    );

    let credential_failure = DispatchCompletion::Failed(DispatchFailure::plain(format!(
        "action produced 1 failure(s): tool rejected credential {CREDENTIAL}"
    )));
    let receipt = persist_dispatch_outcome(&rt, "local", id, &claim, &credential_failure)
        .await
        .expect("a credential-shaped failure must be masked, never refused")
        .expect("claim still owned");

    // Assertion 1: the masked form, not the raw credential, is what is
    // returned AND what is durably stored.
    let receipt_text = receipt.to_string();
    assert!(
        !receipt_text.contains(CREDENTIAL),
        "the returned receipt must never carry the raw credential: {receipt_text}"
    );
    assert!(
        receipt_text.contains("***MASKED***"),
        "the returned receipt must carry the masked form: {receipt_text}"
    );
    let raw_props = get_raw_note_properties(&rt, id).await;
    assert!(
        !raw_props.contains(CREDENTIAL),
        "the stored scheduled-event record must never carry the raw credential: {raw_props}"
    );
    assert!(
        raw_props.contains("***MASKED***"),
        "the stored scheduled-event record must carry the masked form: {raw_props}"
    );

    // Assertion 2: the REAL outcome is persisted and durable. The
    // receipt names the true terminal state -- this dispatch failed --
    // rather than being left on the pre-outcome "invoking" a refusal
    // would leave behind.
    assert_eq!(receipt["state"], "failed");
    let live = get_note_props(&rt, id).await;
    assert_eq!(live["dispatch_receipt"]["state"], "failed");

    // Negative control, on a fresh row: ordinary failure text with no
    // detector match persists exactly as written -- the masker must not
    // over-mask benign content. This cannot reuse the claim above: that
    // write already advanced `dispatch_receipt.state` past `"invoking"`,
    // which is the CAS this function's UPDATE is keyed on.
    let ordinary_id =
        create_scheduled_event(&rt, "local", &trigger, Some("stats()"), None, "schedule").await;
    let ordinary_claim = claim_for_test(&rt, ordinary_id, &trigger).await;
    assert!(mark_dispatch_invoking(
        &rt,
        "local",
        ordinary_id,
        &ordinary_claim,
        short_test_lease()
    )
    .await
    .expect("mark invoking"));
    let ordinary_failure = DispatchCompletion::Failed(DispatchFailure::plain(
        "action produced 1 failure(s): tool timed out after 30s".to_string(),
    ));
    let ordinary_receipt = persist_dispatch_outcome(
        &rt,
        "local",
        ordinary_id,
        &ordinary_claim,
        &ordinary_failure,
    )
    .await
    .expect("ordinary failure content is not blocked")
    .expect("claim still owned");
    assert_eq!(
        ordinary_receipt["error"],
        "action produced 1 failure(s): tool timed out after 30s"
    );
}

/// Same falsifiability arm as the test above, for the second direct-SQL
/// write: the terminal finalize write shared by `finalize_fired_event`
/// and expired-lease recovery. Refusing here (this test's prior
/// behaviour) would leave the row's `status` stuck on `"firing"`
/// forever -- the dispatch and its outcome already happened, so the row
/// would sit past its lease deadline until expired-lease recovery swept
/// it up and finalized it as `"indeterminate"`, discarding the real,
/// known `"failed"` outcome this write is trying to record.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn finalize_fired_event_masks_credential_shaped_final_properties() {
    const CREDENTIAL: &str = "AKIAIOSFODNN7EXAMPLE"; // gitleaks:allow
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let trigger = due_rfc3339();
    let id =
        create_scheduled_event(&rt, "local", &trigger, Some("stats()"), None, "schedule").await;
    let claim = claim_for_test(&rt, id, &trigger).await;
    assert!(
        mark_dispatch_invoking(&rt, "local", id, &claim, short_test_lease())
            .await
            .expect("mark invoking")
    );
    let expected_properties = get_raw_note_properties(&rt, id).await;
    let mut final_properties: Value =
        serde_json::from_str(&expected_properties).expect("properties JSON");
    final_properties["dispatch_error"] = json!(format!("tool rejected credential {CREDENTIAL}"));
    final_properties["dispatch_receipt"]["state"] = json!("failed");
    final_properties["status"] = json!("pending");

    let finalized = finalize_fired_event(
        &rt,
        "local",
        id,
        &final_properties,
        Utc::now().timestamp_micros(),
        &claim,
        &expected_properties,
    )
    .await
    .expect("a credential-shaped dispatch_error must be masked, never refused");
    assert!(
        finalized,
        "the terminal write must land against its CAS guard"
    );

    // Assertion 1: the masked form, not the raw credential, is what
    // lands in storage.
    let raw_props = get_raw_note_properties(&rt, id).await;
    assert!(
        !raw_props.contains(CREDENTIAL),
        "the stored terminal record must never carry the raw credential: {raw_props}"
    );
    assert!(
        raw_props.contains("***MASKED***"),
        "the stored terminal record must carry the masked form: {raw_props}"
    );

    // Assertion 2: the REAL outcome is persisted and durable. The row
    // left `"firing"` for the status this write named, and
    // `dispatch_receipt.state` is the true terminal state rather than
    // being left for expired-lease recovery to reclassify.
    let live = get_note_props(&rt, id).await;
    assert_eq!(live["status"], "pending");
    assert_ne!(live["status"], "firing");
    assert_eq!(live["dispatch_receipt"]["state"], "failed");
}

/// The expired-lease recovery path through `finalize_firing_event`
/// (`finalize_expired_firing_event`) is the same masking seam as
/// `finalize_fired_event` above, reached by a different caller: a
/// process that claimed and invoked a dispatch crashed or lost its
/// lease before writing a durable outcome, and a later recovery pass
/// finalizes the row from its own reconstructed receipt. That receipt
/// carries handler-supplied failure content exactly as the fresh-
/// dispatch path does, so it goes through the same mask-then-assert
/// seam rather than a refusal that would leave the row `"firing"` for a
/// second recovery pass to find again.
#[tokio::test]
async fn expired_lease_recovery_masks_credential_shaped_final_properties() {
    const CREDENTIAL: &str = "AKIAIOSFODNN7EXAMPLE"; // gitleaks:allow
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let trigger = due_rfc3339();
    let id =
        create_scheduled_event(&rt, "local", &trigger, Some("stats()"), None, "schedule").await;
    let claim = claim_for_test(&rt, id, &trigger).await;
    assert!(
        mark_dispatch_invoking(&rt, "local", id, &claim, short_test_lease())
            .await
            .expect("mark invoking")
    );
    expire_dispatch_lease_for_test(&rt, id).await;

    let selected_properties = get_raw_note_properties(&rt, id).await;
    let mut recovered_properties: Value =
        serde_json::from_str(&selected_properties).expect("selected properties JSON");
    recovered_properties["dispatch_receipt"]["state"] = json!("indeterminate");
    recovered_properties["dispatch_receipt"]["error"] = json!(format!(
        "dispatch lease expired holding credential {CREDENTIAL}"
    ));
    recovered_properties["dispatch_error"] = json!(format!(
        "dispatch lease expired holding credential {CREDENTIAL}"
    ));
    recovered_properties["status"] = json!("failed");
    let observed_expired_at = Utc::now().timestamp_micros();

    let finalized = finalize_expired_firing_event(
        &rt,
        "local",
        id,
        &recovered_properties,
        Utc::now().timestamp_micros(),
        &claim,
        RecoverySnapshot {
            expired_at: observed_expired_at,
            properties: &selected_properties,
        },
    )
    .await
    .expect("a credential-shaped recovered receipt must be masked, never refused");
    assert!(
        finalized,
        "the recovery finalize must land against its CAS guard"
    );

    // Assertion 1: the masked form, not the raw credential, is what
    // lands in storage.
    let raw_props = get_raw_note_properties(&rt, id).await;
    assert!(
        !raw_props.contains(CREDENTIAL),
        "the stored recovery record must never carry the raw credential: {raw_props}"
    );
    assert!(
        raw_props.contains("***MASKED***"),
        "the stored recovery record must carry the masked form: {raw_props}"
    );

    // Assertion 2: the REAL outcome is persisted and durable, and the
    // row leaves `"firing"` so a later recovery pass does not sweep it
    // up again.
    let live = get_note_props(&rt, id).await;
    assert_eq!(live["dispatch_receipt"]["state"], "indeterminate");
    assert_ne!(
        live["status"], "firing",
        "the row must leave \"firing\" so recovery does not re-examine it again"
    );
}

/// End-to-end: a `comm.send` gate denial whose reason happens to match a
/// real credential pattern must reach neither the emitted `tracing` log
/// nor the stored `scheduled_event` record in raw form -- but the real
/// outcome (the delivery failed) must still land durably, and the row
/// must leave `"firing"` rather than sit there for expired-lease
/// recovery to reclassify. The log carries the masked form
/// (`bounded_masked_log_text`, the same helper the gate-failure log path
/// in `khive-runtime/src/pack.rs` already uses); the record write is
/// masked, not refused, by the mechanism proven directly in
/// `dispatch_outcome_write_masks_credential_shaped_failure_content` and
/// `finalize_fired_event_masks_credential_shaped_final_properties`.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn reminder_delivery_failure_log_and_record_mask_credential_shaped_denial_reason() {
    const CREDENTIAL: &str = "AKIAIOSFODNN7EXAMPLE"; // gitleaks:allow
    let (_tmp, db_path) = tmp_db();
    let cfg = RuntimeConfig {
        db_path: Some(std::path::PathBuf::from(&db_path)),
        default_namespace: Namespace::parse("local").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: std::sync::Arc::new(DenyCommSendWithCredentialGate),
        actor_id: Some("lambda:credential-daemon".to_string()),
        ..Default::default()
    };
    let rt = KhiveRuntime::new(cfg).expect("runtime");
    let packs = vec!["kg".to_string(), "comm".to_string(), "schedule".to_string()];
    let server = KhiveMcpServer::with_packs(rt.clone(), &packs)
        .expect("server with required reminder delivery pack");
    let id = create_scheduled_event(&rt, "local", &due_rfc3339(), None, None, "remind").await;

    let captured = CapturedDrainLog::default();
    // Capture only THIS module's log output, and do it at the subscriber
    // rather than by filtering captured text.
    //
    // The buffer would otherwise also hold `khive_runtime`'s `gate.check`
    // audit line, which serializes the whole `AuditEvent` -- `deny_reason`
    // included -- at INFO with no masking. That is a real sink of the same
    // shape, and it is deliberately not this change's subject: it belongs
    // to the gate-audit path in `khive-runtime`, it fires for every verb
    // rather than for scheduled dispatch, and masking it is a decision
    // about audit fidelity that a scheduled-dispatch fix has no business
    // making quietly.
    //
    // Filtering the captured text by target name does NOT work here, and
    // the way it fails is the reason this uses an `EnvFilter`: the audit
    // line's own JSON payload carries `"gate_impl":
    // "khive_mcp::pending_events::tests::DenyCommSendWithCredentialGate"`,
    // so a substring match on this module's path keeps the very line it
    // was written to exclude. A target filter has to be applied where
    // targets are structured data, not after they have been flattened into
    // a message body that can quote them.
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_ansi(false)
        .without_time()
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            "khive_mcp::pending_events=trace",
        ))
        .finish();
    let tracing_guard = tracing::subscriber::set_default(subscriber);
    let summary = run_pending_events_on(&rt, &server, false)
        .await
        .expect("drain continues after masking a credential-shaped failure");
    drop(tracing_guard);

    let log_text = captured.contents();
    // Non-vacuity control: a filter that admitted nothing would make every
    // masking assertion below pass for the wrong reason.
    assert!(
        log_text.contains("khive_mcp::pending_events"),
        "no pending-events log line was captured, so the masking assertions \
             below would pass vacuously: {log_text:?}"
    );
    assert!(
        !log_text.contains("gate.check"),
        "the subscriber filter must exclude the gate-audit line; this test \
             asserts about this module's own sink only: {log_text}"
    );
    assert_eq!(summary.scanned, 1);
    assert_eq!(summary.invoked, 1);
    assert_eq!(
        summary.outcomes_persisted, 1,
        "the masked outcome must be durably persisted, not dropped"
    );
    assert_eq!(
        summary.finalized, 1,
        "finalization must complete instead of being aborted by a refusal"
    );
    assert!(
        !log_text.contains(CREDENTIAL),
        "the delivery-failure log must never carry the raw credential: {log_text}"
    );
    assert!(
        log_text.contains("***MASKED***"),
        "the delivery-failure log must carry the masked form: {log_text}"
    );

    let raw_props = get_raw_note_properties(&rt, id).await;
    assert!(
        !raw_props.contains(CREDENTIAL),
        "the stored scheduled-event record must never carry the raw credential: \
             {raw_props}"
    );
    assert!(
        raw_props.contains("***MASKED***"),
        "the stored scheduled-event record must carry the masked form: {raw_props}"
    );
    let props: Value = serde_json::from_str(&raw_props).expect("properties JSON");
    assert_eq!(
        props["dispatch_receipt"]["state"], "failed",
        "the real outcome must be the durable terminal state, not left on \"invoking\""
    );
    assert_eq!(
        props["status"], "pending",
        "a one-shot failure must return the row to \"pending\" rather than \
             strand it on \"firing\""
    );
}

/// The `schedule.remind.fire` audit event is a durable stored record too
/// (append-only, per this module's own doc comment); its `error` payload
/// must carry the masked form for credential-shaped content and the
/// unmasked original for ordinary text.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn reminder_delivery_failure_event_masks_credential_shaped_error() {
    const CREDENTIAL: &str = "AKIAIOSFODNN7EXAMPLE"; // gitleaks:allow
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let packs = vec!["kg".to_string(), "comm".to_string(), "schedule".to_string()];
    let server = KhiveMcpServer::with_packs(rt.clone(), &packs)
        .expect("server with required reminder delivery pack");
    let id = create_scheduled_event(&rt, "local", &due_rfc3339(), None, None, "remind").await;

    append_reminder_delivery_failure_event(
        &server,
        "local",
        id,
        "lambda:credential-daemon",
        "local",
        &format!("comm.send denied: credential {CREDENTIAL} in body"),
    )
    .await;
    append_reminder_delivery_failure_event(
        &server,
        "local",
        id,
        "lambda:credential-daemon",
        "local",
        "comm.send denied by ordinary policy check",
    )
    .await;

    let token = rt
        .authorize(Namespace::parse("local").expect("namespace"))
        .expect("authorize");
    let events = rt
        .events(&token)
        .expect("event store")
        .query_events(
            EventFilter {
                verbs: vec!["schedule.remind.fire".to_string()],
                ..Default::default()
            },
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .expect("query reminder failure events");
    assert_eq!(
        events.items.len(),
        2,
        "both delivery-failure events must be recorded"
    );

    let credential_event = events
        .items
        .iter()
        .find(|event| {
            event.payload["error"]
                .as_str()
                .is_some_and(|error| error.contains("***MASKED***"))
        })
        .expect("the credential-shaped event must have a masked payload");
    assert!(
        !credential_event.payload["error"]
            .as_str()
            .unwrap_or_default()
            .contains(CREDENTIAL),
        "the stored event must never carry the raw credential: {:?}",
        credential_event.payload
    );

    assert!(
        events
            .items
            .iter()
            .any(|event| event.payload["error"] == "comm.send denied by ordinary policy check"),
        "ordinary error text must survive unmasked and unchanged"
    );
}

/// The two message-construction sites that embed the entire stored
/// action DSL (`dispatch_action`'s parse-error and non-literal-argument
/// branches) mask it before it ever becomes a `DispatchFailure` message
/// -- the mechanism that lets a legitimately-sanitized message pass the
/// write-time gate above rather than being refused outright.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn dispatch_action_masks_credential_shaped_stored_action_string_in_parse_failure() {
    const CREDENTIAL: &str = "AKIAIOSFODNN7EXAMPLE"; // gitleaks:allow
    let (_tmp, db_path) = tmp_db();
    let rt = make_rt(&db_path).await;
    let server = KhiveMcpServer::new(rt.clone()).expect("server");

    let malformed_dsl = format!("stats({CREDENTIAL}");
    let err = dispatch_action(
        &malformed_dsl,
        "local",
        Some(VerifiedActor::new("lambda:test").expect("verified actor")),
        &server,
        false,
    )
    .await
    .unwrap_err();
    let msg = err.to_string();
    assert!(
        !msg.contains(CREDENTIAL),
        "the parse-failure message must never carry the raw stored credential: {msg}"
    );
    assert!(
        msg.contains("***MASKED***"),
        "the parse-failure message must carry the masked stored action string: {msg}"
    );

    // Negative control: ordinary malformed DSL with no detector match is
    // echoed verbatim, so the fix does not degrade an operator's ability
    // to see what failed to parse.
    let ordinary_malformed_dsl = "stats(ordinarytoken";
    let err = dispatch_action(
        ordinary_malformed_dsl,
        "local",
        Some(VerifiedActor::new("lambda:test").expect("verified actor")),
        &server,
        false,
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string().contains(ordinary_malformed_dsl),
        "ordinary DSL text must survive unmasked: {err}"
    );
}
