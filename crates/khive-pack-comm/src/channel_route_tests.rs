use std::sync::{Arc, Mutex};

use khive_runtime::channel_section::ChannelConfigValidators;
use khive_runtime::channel_send::{ChannelSendFuture, ChannelSendRoute, ChannelSendRoutes};
use khive_runtime::{
    AllowAllGate, KhiveRuntime, Namespace, NamespaceToken, RuntimeConfig, RuntimeError,
    VerbRegistry, VerbRegistryBuilder,
};
use serde_json::{json, Value};

use crate::inbox_signal::InboxSignal;
use crate::CommPack;

#[derive(Clone, Debug, PartialEq)]
struct SeenRequest {
    params: Value,
    namespace: String,
    actor: String,
}

struct TestRoute {
    configured: bool,
    seen: Mutex<Vec<SeenRequest>>,
}

impl TestRoute {
    fn new(configured: bool) -> Arc<Self> {
        Arc::new(Self {
            configured,
            seen: Mutex::new(Vec::new()),
        })
    }
}

impl ChannelSendRoute for TestRoute {
    fn send<'a>(
        &'a self,
        runtime: &'a KhiveRuntime,
        token: &'a NamespaceToken,
        params: Value,
    ) -> ChannelSendFuture<'a> {
        Box::pin(async move {
            self.seen.lock().unwrap().push(SeenRequest {
                params: params.clone(),
                namespace: token.namespace().as_str().to_owned(),
                actor: token.actor().id.clone(),
            });
            if !self.configured {
                return Err(RuntimeError::InvalidInput(
                    "fixture route has no configured destination".into(),
                ));
            }
            // This fixture's complete policy stores a normal local pair, including
            // its real keyed replay guard. The outer dispatch must add no second
            // pair and must return this custom receipt unchanged. A production
            // route supplies its own persistence and transport policy.
            let mut local = params;
            local["to"] = json!("actor:fixture-destination");
            let receipt =
                crate::handlers::handle_send(runtime, &InboxSignal::new(), token, local).await?;
            Ok(json!({"route": "fixture", "receipt": receipt}))
        })
    }
}

fn config(route: &Arc<TestRoute>) -> RuntimeConfig {
    let mut routes = ChannelSendRoutes::default();
    routes.register("fixture", route.clone()).unwrap();
    RuntimeConfig {
        channel_send_routes: routes,
        gate: Arc::new(AllowAllGate),
        default_namespace: Namespace::parse("local").unwrap(),
        actor_id: Some("actor:sender".into()),
        packs: vec!["kg".into(), "comm".into()],
        ..RuntimeConfig::no_embeddings()
    }
}

fn runtime(config: RuntimeConfig) -> KhiveRuntime {
    let backend = khive_db::StorageBackend::memory().unwrap();
    backend.pool().run_migrations().unwrap();
    KhiveRuntime::from_backend(Arc::new(backend), config)
}

fn registry(runtime: &KhiveRuntime) -> Result<VerbRegistry, RuntimeError> {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(CommPack::new(runtime.clone()));
    builder.with_actor_id(Some("actor:sender".into()));
    builder.with_default_namespace("local");
    builder.build()
}

async fn message_count(runtime: &KhiveRuntime) -> u64 {
    let token = runtime
        .authorize(Namespace::parse("local").unwrap())
        .unwrap();
    runtime
        .notes(&token)
        .unwrap()
        .count_notes("local", Some("message"))
        .await
        .unwrap()
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn registered_send_route_owns_persistence_replay_and_exact_result() {
    let route = TestRoute::new(true);
    let runtime = runtime(config(&route));
    let registry = registry(&runtime).unwrap();
    let params = json!({
        "to": "fixture:recipient", "content": "  preserve body spacing  ",
        "subject": null, "tags": ["fixture"], "idempotency_key": "route-operation"
    });
    let first = registry.dispatch("comm.send", params.clone()).await;
    assert!(
        first.is_ok(),
        "registered route must handle the actual send: {first:?}"
    );
    let first = first.unwrap();
    assert_eq!(
        first["route"], "fixture",
        "custom route result must be returned unchanged"
    );
    assert_eq!(first["receipt"]["replayed"], false);
    assert_eq!(
        message_count(&runtime).await,
        2,
        "only the route-owned pair may be stored"
    );
    let replay = registry.dispatch("comm.send", params.clone()).await;
    assert!(
        replay.is_ok(),
        "route-owned replay must succeed: {replay:?}"
    );
    let replay = replay.unwrap();
    assert_eq!(replay["receipt"]["replayed"], true);
    for field in ["full_id", "recipient_id", "thread_id", "sent_at"] {
        assert_eq!(replay["receipt"][field], first["receipt"][field]);
    }
    assert_eq!(
        message_count(&runtime).await,
        2,
        "outer dispatch must not append after replay"
    );
    assert_eq!(
        *route.seen.lock().unwrap(),
        vec![
            SeenRequest {
                params: params.clone(),
                namespace: "local".into(),
                actor: "actor:sender".into()
            };
            2
        ],
        "route must receive original values and the dispatch-authorized identity"
    );
    let token = runtime
        .authorize(Namespace::parse("local").unwrap())
        .unwrap();
    let id = first["receipt"]["full_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let stored = runtime
        .notes(&token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.content, "  preserve body spacing  ");
    let mut conflict = params;
    conflict["content"] = json!("different body");
    let result = registry.dispatch("comm.send", conflict).await;
    assert!(
        result.is_err(),
        "route-owned keyed conflict must not fall through"
    );
    assert_eq!(message_count(&runtime).await, 2);
    assert_eq!(
        runtime.notes(&token).unwrap().get_note(id).await.unwrap(),
        Some(stored),
        "route-owned conflict must preserve the complete stored note"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn route_refusal_is_preserved_without_legacy_write() {
    let route = TestRoute::new(false);
    let runtime = runtime(config(&route));
    let result = registry(&runtime)
        .unwrap()
        .dispatch(
            "comm.send",
            json!({"to": "fixture:recipient", "content": "body"}),
        )
        .await;
    assert!(
        matches!(&result, Err(RuntimeError::InvalidInput(message)) if message == "fixture route has no configured destination"),
        "registered route refusal must survive without fallback: {result:?}"
    );
    assert_eq!(route.seen.lock().unwrap().len(), 1);
    assert_eq!(
        message_count(&runtime).await,
        0,
        "route refusal must write no legacy pair"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn common_validation_and_padded_address_refuse_before_route() {
    let route = TestRoute::new(true);
    let runtime = runtime(config(&route));
    let registry = registry(&runtime).unwrap();
    for params in [
        json!({"to": "fixture:recipient", "content": " "}),
        json!({"to": "fixture:recipient", "content": "body", "unknown": true}),
        json!({"to": "fixture:recipient", "content": "body", "thread_id": "not-a-uuid"}),
        json!({"to": "fixture:recipient", "content": "body", "idempotency_key": "invalid\u{0000}key"}),
        json!({"to": "fixture:recipient", "content": "body", "idempotency_key": "k".repeat(513)}),
        json!({"to": "fixture:recipient\n", "content": "body"}),
        json!({"to": " fixture:recipient", "content": "body"}),
        json!({"to": "fixture:recipient ", "content": "body"}),
    ] {
        let result = registry.dispatch("comm.send", params.clone()).await;
        assert!(
            result.is_err(),
            "invalid send must refuse before route: {params}: {result:?}"
        );
        assert!(
            route.seen.lock().unwrap().is_empty(),
            "invalid send must not invoke custom route"
        );
        assert_eq!(message_count(&runtime).await, 0);
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn component_config_failure_prevents_pack_activation_and_send() {
    let route = TestRoute::new(true);
    let mut config = config(&route);
    let mut validators = ChannelConfigValidators::default();
    validators
        .register(
            "fixture",
            Arc::new(|_| {
                Err(RuntimeError::InvalidInput(
                    "fixture configuration rejected".into(),
                ))
            }),
        )
        .unwrap();
    config.channel_config_validators = validators;
    let runtime = runtime(config);
    let result = registry(&runtime);
    assert!(
        matches!(&result, Err(RuntimeError::InvalidInput(message)) if message == "fixture configuration rejected"),
        "component configuration must be validated before pack activation"
    );
    assert!(route.seen.lock().unwrap().is_empty());
    assert_eq!(message_count(&runtime).await, 0);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn unregistered_actor_and_legacy_node_paths_do_not_call_custom_route() {
    let route = TestRoute::new(true);
    let runtime = runtime(config(&route));
    let registry = registry(&runtime).unwrap();
    let local = registry
        .dispatch(
            "comm.send",
            json!({"to": "actor:recipient", "content": "body"}),
        )
        .await;
    assert!(
        local.is_ok(),
        "ordinary actor send must retain the legacy path: {local:?}"
    );
    assert!(local.unwrap().get("full_id").is_some());
    assert_eq!(message_count(&runtime).await, 2);
    let node = registry
        .dispatch(
            "comm.send",
            json!({"to": "khive1:realm/agent", "content": "body"}),
        )
        .await;
    assert!(
        node.is_err(),
        "legacy node send without its own route must remain refused"
    );
    assert_eq!(message_count(&runtime).await, 2);
    assert!(
        route.seen.lock().unwrap().is_empty(),
        "custom route must not intercept legacy addresses"
    );
}

#[test]
fn route_registration_rejects_duplicates_reserved_kinds_and_preserves_exact_matching() {
    let first = TestRoute::new(true);
    let second = TestRoute::new(false);
    let mut routes = ChannelSendRoutes::default();
    let first_route: Arc<dyn ChannelSendRoute> = first;
    routes.register("fixture", first_route.clone()).unwrap();
    let duplicate = routes.register("fixture", second.clone());
    assert!(
        duplicate.is_err(),
        "duplicate route registration must fail closed"
    );
    assert!(Arc::ptr_eq(
        &routes.resolve("fixture:recipient").unwrap().unwrap(),
        &first_route
    ));
    for kind in [
        "khive1",
        "KHIVE1",
        "local",
        "LOCAL",
        "email",
        "EMAIL",
        "telegram",
        "TELEGRAM",
        "actor",
        "ACTOR",
        "lambda",
        "LAMBDA",
        "agent",
        "AGENT",
        "",
        " fixture",
        "fixture:",
        "fixture\n",
    ] {
        assert!(
            routes.register(kind, second.clone()).is_err(),
            "reserved or malformed kind must refuse: {kind:?}"
        );
    }
    assert!(routes.resolve("Fixture:recipient").unwrap().is_none());
    assert!(routes.resolve("actor:recipient").unwrap().is_none());
    assert!(routes.resolve("fixture:recipient ").is_err());
    assert!(routes.resolve(" fixture:recipient").is_err());
    let clone = routes.clone();
    routes.register("other", second).unwrap();
    assert!(clone.resolve("other:recipient").unwrap().is_none());
    assert_eq!(
        format!("{clone:?}"),
        "ChannelSendRoutes { kinds: [\"fixture\"] }"
    );
}
