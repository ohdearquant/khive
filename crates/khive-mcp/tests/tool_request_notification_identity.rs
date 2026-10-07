//! Approval notifications retain the caller of the real tool request.

use std::sync::{Arc, Mutex};

use khive_pack_comm::CommPack;
use khive_pack_kg::KgPack;
use khive_pack_tool::ToolPack;
use khive_runtime::{
    Gate, GateDecision, GateError, GateRequest, KhiveRuntime, Namespace, RequestIdentity,
    RuntimeConfig, VerbRegistry, VerbRegistryBuilder, WalCeilingSource,
};
use khive_storage::Note;
use serde_json::{json, Value};

const CALLER: &str = "lambda:request-caller";
const BAKED: &str = "lambda:registry-default";
const CALLER_NS: &str = "request-scope";
const BAKED_NS: &str = "registry-scope";
const BENEFICIARY: &str = "agent:beneficiary";
const RECIPIENT: &str = "lambda:approver";

#[derive(Debug, Default)]
struct NotificationGate {
    deny_caller_notification: bool,
    notifications: Mutex<Vec<GateRequest>>,
}

impl Gate for NotificationGate {
    fn check(&self, request: &GateRequest) -> Result<GateDecision, GateError> {
        if request.verb == "comm.send" {
            self.notifications.lock().unwrap().push(request.clone());
            if self.deny_caller_notification && request.actor.id == CALLER {
                return Ok(GateDecision::deny("fixture declines this notification"));
            }
        }
        Ok(GateDecision::allow())
    }
}

struct Fixture {
    runtime: KhiveRuntime,
    registry: VerbRegistry,
    gate: Arc<NotificationGate>,
}

impl Fixture {
    fn new(deny_caller_notification: bool) -> Self {
        let config = RuntimeConfig {
            db_path: None,
            embedding_model: None,
            additional_embedding_models: Vec::new(),
            wal_ceiling_bytes: 0,
            wal_ceiling_configured_bytes: 0,
            wal_ceiling_source: WalCeilingSource::Default,
            wal_ceiling_env_raw: None,
            disk_guard_environment: Default::default(),
            disk_guard_config: None,
            volume_lock_dir: None,
            visibility_receipts: None,
            credentials: Vec::new(),
            actor_id: None,
            brain_profile: None,
            brain: Default::default(),
            events_split: None,
            mounts: Vec::new(),
            blob: Default::default(),
            packs: vec!["kg".into(), "tool".into(), "comm".into()],
            ..RuntimeConfig::no_embeddings()
        };
        assert!(config.db_path.is_none());
        assert!(config.embedding_model.is_none());
        assert!(config.additional_embedding_models.is_empty());
        let runtime = KhiveRuntime::new(config).expect("private memory runtime");
        assert!(!runtime.backend().is_file_backed());
        assert!(runtime.backend_data_dir().is_none());
        assert!(runtime.backend_ann_root().is_none());
        assert!(runtime.registered_embedding_model_names().is_empty());
        let gate = Arc::new(NotificationGate {
            deny_caller_notification,
            ..Default::default()
        });
        let mut builder = VerbRegistryBuilder::new();
        builder.with_actor_id(Some(BAKED.into()));
        builder.with_default_namespace(BAKED_NS);
        builder.with_gate(gate.clone());
        builder.register(KgPack::new(runtime.clone()));
        builder.register(ToolPack::new(runtime.clone()));
        builder.register(CommPack::new(runtime.clone()));
        let registry = builder.build().expect("real KG, tool, and comm registry");
        registry
            .apply_schema_plans_with_map(&Default::default(), runtime.backend())
            .expect("private pack schema");
        runtime.install_edge_rules(registry.all_edge_rules());
        Self {
            runtime,
            registry,
            gate,
        }
    }

    async fn messages(&self, namespace: &str) -> Vec<Note> {
        let token = self
            .runtime
            .authorize(Namespace::parse(namespace).unwrap())
            .unwrap();
        self.runtime
            .list_notes(&token, Some("message"), 20, 0)
            .await
            .unwrap()
    }

    async fn assert_pending(&self, result: &Value, namespace: &str, actor: &str) {
        assert_eq!(result["ok"], true, "{result}");
        assert_eq!(result["status"], "requested", "{result}");
        assert_eq!(result["actor"], actor, "{result}");
        let rows = self
            .registry
            .dispatch(
                "tool.requests",
                json!({
                    "namespace": namespace, "status": "requested",
                }),
            )
            .await
            .unwrap();
        assert_eq!(rows["count"], 1, "{rows}");
        assert_eq!(rows["requests"][0]["id"], result["request_id"]);
        assert_eq!(rows["requests"][0]["actor"], actor);
        for other in [CALLER_NS, BAKED_NS, "local"] {
            if other != namespace {
                let empty = self
                    .registry
                    .dispatch("tool.requests", json!({"namespace": other}))
                    .await
                    .unwrap();
                assert_eq!(empty["count"], 0, "{empty}");
            }
        }
    }

    fn assert_notification_gate(&self, actor: &str, namespace: &str) {
        let requests = self.gate.notifications.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].actor.id, actor);
        assert_eq!(requests[0].namespace.as_str(), namespace);
        assert_eq!(requests[0].args["namespace"], namespace);
    }
}

fn identity(process_ref: Option<&str>) -> RequestIdentity {
    RequestIdentity {
        namespace: CALLER_NS.into(),
        actor_id: Some(CALLER.into()),
        visible_namespaces: vec!["caller-visible".into()],
        process_ref: process_ref.map(str::to_owned),
        request_id: Some(731),
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn approval_notification_uses_request_caller_and_origin() {
    for process_ref in [Some("request-process"), None] {
        let f = Fixture::new(false);
        let result = f
            .registry
            .dispatch_with_identity(
                "tool.request",
                json!({
                    "namespace": CALLER_NS, "tool": "unregistered-notification-tool",
                    "actor": BENEFICIARY, "notify": RECIPIENT,
                    "reason": "fixture approval", "scope": "fixture scope",
                }),
                Some(identity(process_ref)),
            )
            .await
            .unwrap();
        assert_eq!(result["notified"], true, "{result}");
        f.assert_pending(&result, CALLER_NS, BENEFICIARY).await;
        f.assert_notification_gate(CALLER, CALLER_NS);
        let messages = f.messages(CALLER_NS).await;
        assert_eq!(messages.len(), 2);
        let mut directions = Vec::new();
        for message in &messages {
            assert_eq!(message.namespace, CALLER_NS);
            assert!(message.content.contains(BENEFICIARY));
            let props = message.properties.as_ref().unwrap();
            assert_eq!(props["from_actor"], CALLER);
            assert_eq!(props["to_actor"], RECIPIENT);
            assert_eq!(
                props.get("sent_by_process").and_then(Value::as_str),
                process_ref
            );
            directions.push(props["direction"].as_str().unwrap());
        }
        directions.sort_unstable();
        assert_eq!(directions, ["inbound", "outbound"]);
        assert!(f.messages("local").await.is_empty());
        assert!(f.messages(BAKED_NS).await.is_empty());
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn refused_notification_keeps_the_real_request_pending() {
    for (deny, recipient) in [(true, RECIPIENT), (false, "invalid\nrecipient")] {
        let f = Fixture::new(deny);
        let result = f
            .registry
            .dispatch_with_identity(
                "tool.request",
                json!({
                    "namespace": CALLER_NS, "tool": "unregistered-notification-tool",
                    "notify": recipient,
                }),
                Some(identity(None)),
            )
            .await
            .unwrap();
        assert_eq!(result["notified"], false, "{result}");
        f.assert_pending(&result, CALLER_NS, CALLER).await;
        f.assert_notification_gate(CALLER, CALLER_NS);
        for namespace in [CALLER_NS, BAKED_NS, "local"] {
            assert!(f.messages(namespace).await.is_empty());
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn default_dispatch_and_omitted_notification_keep_their_behavior() {
    for notify in [false, true] {
        let f = Fixture::new(false);
        let mut params = json!({"tool": "unregistered-notification-tool"});
        if notify {
            params["notify"] = json!(RECIPIENT);
        }
        let result = f.registry.dispatch("tool.request", params).await.unwrap();
        assert_eq!(result["notified"], notify, "{result}");
        f.assert_pending(&result, "local", BAKED).await;
        let messages = f.messages("local").await;
        assert_eq!(messages.len(), if notify { 2 } else { 0 });
        for message in messages {
            assert_eq!(message.properties.as_ref().unwrap()["from_actor"], BAKED);
        }
        assert!(f.messages(BAKED_NS).await.is_empty());
        if notify {
            f.assert_notification_gate(BAKED, "local");
        } else {
            assert!(f.gate.notifications.lock().unwrap().is_empty());
        }
    }
}
