//! Real reminder creation, drain, and default-inbox routing regressions.

use chrono::{Duration, Utc};
use khive_runtime::{
    Gate, GateDecision, GateError, GateRequest, KhiveRuntime, Namespace, RequestIdentity,
    RuntimeConfig,
};
use khive_storage::types::{SqlStatement, SqlValue};
use serde_json::{json, Value};
use tempfile::TempDir;

use super::run_pending_events_on;
use crate::server::KhiveMcpServer;
use crate::tools::request::RequestParams;

const CREATOR: &str = "lambda:reminder-owner";
const DAEMON: &str = "lambda:replacement-daemon";
const DAEMON_NAMESPACE: &str = "lambda:replacement-daemon";
const EVENT_NAMESPACE: &str = "lambda:reminder-event";
const CREATOR_HOME_NAMESPACE: &str = "lambda:reminder-home";

#[derive(Debug, Default)]
struct AllowReminderNamespacesGate {
    deliveries: std::sync::Mutex<Vec<(String, String, String)>>,
}

impl Gate for AllowReminderNamespacesGate {
    fn check(&self, request: &GateRequest) -> Result<GateDecision, GateError> {
        if request.verb == "comm.send" {
            self.deliveries.lock().expect("delivery requests").push((
                request.actor.kind.clone(),
                request.actor.id.clone(),
                request.namespace.to_string(),
            ));
        }
        Ok(GateDecision::allow())
    }
}

fn fixture() -> (
    TempDir,
    String,
    KhiveRuntime,
    KhiveMcpServer,
    std::sync::Arc<AllowReminderNamespacesGate>,
) {
    let temp = tempfile::tempdir().expect("private reminder fixture");
    let db_path = temp.path().join("khive-test.db");
    let db_path = db_path.to_str().expect("utf8 database path").to_string();
    let gate = std::sync::Arc::new(AllowReminderNamespacesGate::default());
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(std::path::PathBuf::from(&db_path)),
        default_namespace: Namespace::parse(DAEMON_NAMESPACE).expect("daemon namespace"),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: gate.clone(),
        actor_id: Some(DAEMON.to_string()),
        packs: vec!["kg".to_string(), "schedule".to_string(), "comm".to_string()],
        ..Default::default()
    })
    .expect("reminder runtime");
    let server = KhiveMcpServer::new(rt.clone()).expect("reminder server");
    (temp, db_path, rt, server, gate)
}

fn creator_identity() -> RequestIdentity {
    RequestIdentity {
        namespace: "local".to_string(),
        actor_id: Some(CREATOR.to_string()),
        process_ref: khive_runtime::process_ref_from_env(),
        visible_namespaces: Vec::new(),
        owned_namespaces: Vec::new(),
        request_id: None,
    }
}

fn creator_identity_in(namespace: &str) -> RequestIdentity {
    RequestIdentity {
        namespace: namespace.to_string(),
        ..creator_identity()
    }
}

async fn dispatch_as(server: &KhiveMcpServer, identity: RequestIdentity, ops: Value) -> Value {
    let response = server
        .dispatch_request_inner(
            RequestParams {
                ops: serde_json::to_string(&ops).expect("serialize creator request"),
                ..Default::default()
            },
            true,
            Some(identity),
            crate::server::DispatchOrigin::Daemon,
        )
        .await
        .expect("dispatch creator request");
    serde_json::from_str(&response).expect("creator request result JSON")
}

async fn create_reminder(
    server: &KhiveMcpServer,
    namespace: Option<&str>,
    content: &str,
) -> uuid::Uuid {
    create_reminder_as(server, creator_identity(), namespace, content).await
}

async fn create_reminder_as(
    server: &KhiveMcpServer,
    identity: RequestIdentity,
    namespace: Option<&str>,
    content: &str,
) -> uuid::Uuid {
    let mut args = json!({
        "content": content,
        "at": "2099-01-01T00:00:00Z",
    });
    if let Some(namespace) = namespace {
        args["namespace"] = json!(namespace);
    }
    let result = dispatch_as(
        server,
        identity,
        json!([{"tool": "schedule.remind", "args": args}]),
    )
    .await;
    assert_eq!(result["results"][0]["ok"], true, "{result}");
    result["results"][0]["result"]["full_id"]
        .as_str()
        .expect("reminder full_id")
        .parse()
        .expect("reminder UUID")
}

async fn make_reminder_due(rt: &KhiveRuntime, id: uuid::Uuid) {
    let mut writer = rt.sql().writer().await.expect("open fixture SQL writer");
    let rows = writer
        .execute(SqlStatement {
            sql: "UPDATE notes \
                  SET properties = json_set(properties, '$.trigger_at', ?1) \
                  WHERE id = ?2"
                .to_string(),
            params: vec![
                SqlValue::Text((Utc::now() - Duration::seconds(5)).to_rfc3339()),
                SqlValue::Text(id.to_string()),
            ],
            label: Some("test_reminder_namespace_due".into()),
        })
        .await
        .expect("make private reminder due");
    assert_eq!(rows, 1, "one private reminder made due");
}

async fn note_properties(rt: &KhiveRuntime, id: uuid::Uuid, namespace: &str) -> Value {
    let token = rt
        .authorize(Namespace::parse(namespace).expect("event namespace"))
        .expect("authorize fixture note read");
    let note = rt
        .notes(&token)
        .expect("fixture note store")
        .get_note(id)
        .await
        .expect("read reminder note")
        .expect("reminder exists");
    note.properties.unwrap_or(json!({}))
}

async fn drain_reminder(
    rt: &KhiveRuntime,
    server: &KhiveMcpServer,
    id: uuid::Uuid,
    event_namespace: &str,
) {
    let summary = run_pending_events_on(rt, server, false)
        .await
        .expect("drain private reminder");
    assert_eq!(summary.fired, 1, "one reminder fires");
    assert_eq!(summary.failed, 0, "reminder delivery succeeds");
    let props = note_properties(rt, id, event_namespace).await;
    assert_eq!(props["status"], "fired", "{props}");
    assert_eq!(props["dispatch_receipt"]["state"], "succeeded", "{props}");
}

async fn assert_default_inbox_reminder(server: &KhiveMcpServer, content: &str) {
    assert_default_inbox_reminder_as(server, creator_identity(), content).await;
}

async fn assert_default_inbox_reminder_as(
    server: &KhiveMcpServer,
    identity: RequestIdentity,
    content: &str,
) {
    // Omit namespace: this must exercise the creator's ordinary inbox read.
    let response = dispatch_as(
        server,
        identity,
        json!([{"tool": "comm.inbox", "args": {"status": "all"}}]),
    )
    .await;
    assert_eq!(response["results"][0]["ok"], true, "{response}");
    let inbox = &response["results"][0]["result"];
    let messages = inbox["messages"].as_array();
    let matching = messages
        .into_iter()
        .flatten()
        .filter(|message| message["content"] == content)
        .count();
    assert_eq!(
        matching, 1,
        "reminder_reaches_creator_default_inbox: exactly one matching reminder; {inbox}"
    );
    let messages = messages.expect("matching reminder requires the public inbox messages array");
    assert_eq!(inbox["count"], 1, "{inbox}");
    assert_eq!(messages.len(), 1, "{inbox}");
}

fn assert_delivery_uses_creator(gate: &AllowReminderNamespacesGate) {
    let deliveries = gate.deliveries.lock().expect("delivery requests");
    assert!(
        !deliveries.is_empty(),
        "delivery must pass through the gate"
    );
    assert!(
        deliveries
            .iter()
            .all(|(kind, id, _)| kind == "actor" && id == CREATOR),
        "reminder delivery must retain its verified creator: {deliveries:?}"
    );
}

fn assert_delivery_gate_namespace_is_local(gate: &AllowReminderNamespacesGate) {
    let deliveries = gate.deliveries.lock().expect("delivery requests");
    assert!(
        !deliveries.is_empty()
            && deliveries
                .iter()
                .all(|(_, _, namespace)| namespace == "local"),
        "reminder_delivery_gate_namespace_is_local: actual comm.send gate requests must match the default mailbox namespace; {deliveries:?}"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn reminder_with_frame_default_and_explicit_namespace_reaches_creator_default_inbox() {
    let (_temp, _db_path, rt, server, gate) = fixture();
    let content = "explicit event namespace still reaches the creator default inbox";
    let id = create_reminder(&server, Some(EVENT_NAMESPACE), content).await;
    make_reminder_due(&rt, id).await;
    gate.deliveries.lock().expect("delivery requests").clear();

    drain_reminder(&rt, &server, id, EVENT_NAMESPACE).await;
    assert_default_inbox_reminder(&server, content).await;
    assert_delivery_uses_creator(&gate);
    assert_delivery_gate_namespace_is_local(&gate);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn reminder_without_explicit_namespace_keeps_default_inbox_delivery() {
    let (_temp, _db_path, rt, server, gate) = fixture();
    let content = "default namespace reminder still reaches the creator default inbox";
    let id = create_reminder(&server, None, content).await;
    make_reminder_due(&rt, id).await;
    gate.deliveries.lock().expect("delivery requests").clear();

    drain_reminder(&rt, &server, id, "local").await;
    assert_default_inbox_reminder(&server, content).await;
    assert_delivery_uses_creator(&gate);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn reminder_from_creator_with_non_local_identity_namespace_reaches_default_inbox() {
    let (_temp, _db_path, rt, server, gate) = fixture();
    let creator = creator_identity_in(CREATOR_HOME_NAMESPACE);
    let content = "creator with a non-local identity namespace still reads the reminder";
    let id = create_reminder_as(&server, creator.clone(), None, content).await;
    make_reminder_due(&rt, id).await;
    gate.deliveries.lock().expect("delivery requests").clear();

    drain_reminder(&rt, &server, id, "local").await;
    assert_default_inbox_reminder_as(&server, creator, content).await;
    assert_delivery_uses_creator(&gate);
}
