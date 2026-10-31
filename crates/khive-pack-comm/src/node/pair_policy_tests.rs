//! Tests for the pair policy gate of the node send route.

use super::{NodeHostState, RuntimeNodeSendRoute};
use crate::node_route::{NodeSealRequest, NodeSendRefusal, NodeSendRoute};
use khive_channel_node::encoding::{CanonicalUuid, NodeAddress, Realm};
use khive_runtime::comm_transport::PolicyMode;
use khive_runtime::node_config::NodeChannelConfig;
use khive_runtime::{AllowAllGate, BackendId, KhiveRuntime, RuntimeConfig};
use khive_storage::{SqlStatement, SqlValue};
use std::sync::Arc;
use uuid::Uuid;

const NS: &str = "local";
const CONFIGURED_ACTOR: &str = "actor:node-pair-policy";
const OTHER_ACTOR: &str = "actor:someone-else";

fn node_config(mode: PolicyMode) -> NodeChannelConfig {
    NodeChannelConfig {
        enabled: true,
        slug: Some("test".to_string()),
        namespace: NS.to_string(),
        actor: Some(CONFIGURED_ACTOR.to_string()),
        realm: Some("realm.test".to_string()),
        service_url: Some("https://node.example.test".to_string()),
        agent_id: Some(Uuid::from_u128(1).to_string()),
        device_id: Some(Uuid::from_u128(5).to_string()),
        key_epoch: Some(1),
        key_reference: Some("test-credential".to_string()),
        key_fingerprint: Some("ab".repeat(32)),
        pair_policy_mode: mode,
        ..Default::default()
    }
}

fn runtime_with_node(mode: PolicyMode) -> KhiveRuntime {
    let backend = khive_db::StorageBackend::memory().expect("in-memory backend");
    backend.pool().run_migrations().expect("migrations");
    KhiveRuntime::from_backend(
        Arc::new(backend),
        RuntimeConfig {
            wal_ceiling_bytes: 0,
            wal_ceiling_configured_bytes: 0,
            wal_ceiling_source: Default::default(),
            wal_ceiling_env_raw: None,
            web: Default::default(),
            telemetry: Default::default(),
            mounts: Vec::new(),
            brain: Default::default(),
            git_write: Default::default(),
            exec: Default::default(),
            display_timezone: khive_runtime::config::resolve_default_display_timezone(),
            events_split: None,
            db_path: None,
            blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
            default_namespace: khive_runtime::Namespace::parse(NS).unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string(), "comm".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: Some(CONFIGURED_ACTOR.to_string()),
            node_channel: Some(Arc::new(node_config(mode))),
            ..khive_runtime::RuntimeConfig::no_embeddings()
        },
    )
}

fn recipient_address() -> String {
    NodeAddress::new(
        Realm::parse("realm.test").expect("realm"),
        CanonicalUuid::from_uuid(Uuid::from_u128(2)),
    )
    .to_string()
}

fn request<'a>(address: &'a str, from_actor: &'a str) -> NodeSealRequest<'a> {
    NodeSealRequest {
        address,
        logical_message_id: Uuid::from_u128(0xabc),
        from_actor,
        content: "hello node",
        subject: Some("subject"),
        thread_id: None,
        namespace: NS,
        sent_at: "2026-01-01T00:00:00Z",
    }
}

async fn count(runtime: &KhiveRuntime, sql: &str, label: &str) -> i64 {
    let result = runtime
        .sql()
        .reader()
        .await
        .unwrap()
        .query_scalar(SqlStatement {
            sql: sql.into(),
            params: vec![],
            label: Some(label.into()),
        })
        .await
        .unwrap();
    match result {
        Some(SqlValue::Integer(n)) => n,
        other => panic!("unexpected count: {other:?}"),
    }
}

async fn transport_row_count(runtime: &KhiveRuntime) -> i64 {
    count(
        runtime,
        "SELECT count(*) FROM comm_sender_transport",
        "node-pair-policy-transport-count",
    )
    .await
}

async fn note_row_count(runtime: &KhiveRuntime) -> i64 {
    count(
        runtime,
        "SELECT count(*) FROM notes",
        "node-pair-policy-note-count",
    )
    .await
}

fn route_for(runtime: &KhiveRuntime) -> RuntimeNodeSendRoute {
    let state = NodeHostState::from_runtime(runtime.clone())
        .expect("the fixture node config is valid")
        .expect("the fixture node config is enabled");
    RuntimeNodeSendRoute::new(state)
}

// Kills (pair policy): in `node/route.rs`
//   old: `PolicyMode::Shadow | PolicyMode::Enforce`
//   new: `PolicyMode::Enforce`
// With that edit the Shadow case is no longer refused with `PairPolicyUnavailable`: the
// request falls through to the sender binding check, so the Shadow assertions fail.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn node_pair_policy_shadow_and_enforce_refuse_before_anything_is_written() {
    let address = recipient_address();

    for mode in [PolicyMode::Shadow, PolicyMode::Enforce] {
        let runtime = runtime_with_node(mode);
        let route = route_for(&runtime);
        let notes_before = note_row_count(&runtime).await;
        let transports_before = transport_row_count(&runtime).await;

        // The same refusal comes back whether or not the sender matches the configured
        // binding, so the policy check runs before every other check.
        for (from_actor, what) in [
            (CONFIGURED_ACTOR, "configured actor"),
            (OTHER_ACTOR, "other actor"),
        ] {
            let result = route.seal(request(&address, from_actor)).await;
            assert_eq!(
                result.err(),
                Some(NodeSendRefusal::PairPolicyUnavailable),
                "{mode:?}, {what}: expected a pair policy refusal"
            );
        }

        assert_eq!(
            note_row_count(&runtime).await,
            notes_before,
            "{mode:?}: a refused seal writes no note"
        );
        assert_eq!(
            transport_row_count(&runtime).await,
            transports_before,
            "{mode:?}: a refused seal writes no transport row"
        );
    }
}

// Kills (pair policy, over-refusal): in `node/route.rs`
//   old: `PolicyMode::Shadow | PolicyMode::Enforce`
//   new: `PolicyMode::Off | PolicyMode::Shadow | PolicyMode::Enforce`
// With that edit `off` is refused with `PairPolicyUnavailable` too, so the assertions below fail.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn node_pair_policy_off_lets_the_request_past_the_policy_check() {
    let address = recipient_address();
    let runtime = runtime_with_node(PolicyMode::Off);
    let route = route_for(&runtime);
    let notes_before = note_row_count(&runtime).await;
    let transports_before = transport_row_count(&runtime).await;

    // The request names an actor other than the configured one, so sealing stops at the sender
    // binding check. That refusal differs from the policy refusal, which shows the policy check
    // let the request through.
    let result = route.seal(request(&address, OTHER_ACTOR)).await;
    let refusal = result.err();
    assert_ne!(
        refusal,
        Some(NodeSendRefusal::PairPolicyUnavailable),
        "off: the policy check must not refuse"
    );
    assert_eq!(refusal, Some(NodeSendRefusal::SenderBindingMismatch));

    assert_eq!(note_row_count(&runtime).await, notes_before);
    assert_eq!(transport_row_count(&runtime).await, transports_before);
}
