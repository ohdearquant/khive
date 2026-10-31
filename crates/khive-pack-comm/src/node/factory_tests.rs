//! Tests for the comm pack factory's node wiring: what the registered factory builds for a
//! disabled, absent, valid or invalid `[channel.node]` table.

use super::NodeHostState;
use crate::CommPack;
use khive_runtime::comm_transport::PolicyMode;
use khive_runtime::node_config::NodeChannelConfig;
use khive_runtime::{
    AllowAllGate, BackendId, KhiveRuntime, PackFactory, PackRegistration, RuntimeConfig,
    RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::SqlStatement;
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

const NS: &str = "local";
const ACTOR: &str = "actor:node-factory-test";
const NODE_TO: &str = "khive1:realm/agent";

/// The comm factory exactly as the inventory registers it; it is built from a runtime alone.
fn comm_factory() -> &'static dyn PackFactory {
    inventory::iter::<PackRegistration>
        .into_iter()
        .map(|registration| registration.0)
        .find(|factory| factory.name() == "comm")
        .expect("the comm pack factory is registered")
}

/// A runtime over a fresh in-memory database carrying the given node table.
fn runtime_with(table: Option<NodeChannelConfig>) -> KhiveRuntime {
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
            actor_id: Some(ACTOR.to_string()),
            node_channel: table.map(Arc::new),
            ..khive_runtime::RuntimeConfig::no_embeddings()
        },
    )
}

/// A dispatchable registry whose comm pack is built the way the factory builds it
/// (`CommPack::new(runtime)`): the node route, if any, comes from the runtime's table alone.
fn registry_for(runtime: &KhiveRuntime) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(CommPack::new(runtime.clone()));
    builder.with_actor_id(Some(ACTOR.to_string()));
    builder.with_default_namespace(NS);
    builder.build().expect("registry")
}

/// A table that passes validation: every required field present and well formed.
fn valid_enabled_table() -> NodeChannelConfig {
    NodeChannelConfig {
        enabled: true,
        slug: Some("test".to_string()),
        namespace: NS.to_string(),
        actor: Some(ACTOR.to_string()),
        realm: Some("realm.test".to_string()),
        service_url: Some("https://node.example.test".to_string()),
        agent_id: Some(Uuid::from_u128(1).to_string()),
        device_id: Some(Uuid::from_u128(5).to_string()),
        key_epoch: Some(1),
        key_reference: Some("factory-test-key".to_string()),
        key_fingerprint: Some("ab".repeat(32)),
        ..Default::default()
    }
}

async fn count(runtime: &KhiveRuntime, sql: &str) -> i64 {
    let result = runtime
        .sql()
        .reader()
        .await
        .expect("sql reader")
        .query_scalar(SqlStatement {
            sql: sql.into(),
            params: vec![],
            label: Some("node-factory-count".into()),
        })
        .await
        .expect("count query");
    match result {
        Some(khive_storage::SqlValue::Integer(n)) => n,
        other => panic!("unexpected count: {other:?}"),
    }
}

async fn message_count(runtime: &KhiveRuntime) -> i64 {
    count(
        runtime,
        "SELECT count(*) FROM notes WHERE kind = 'message' AND deleted_at IS NULL",
    )
    .await
}

async fn transport_row_count(runtime: &KhiveRuntime) -> i64 {
    count(runtime, "SELECT count(*) FROM comm_sender_transport").await
}

/// Send to a node address and return the text of the refusal.
async fn refused_node_send(registry: &VerbRegistry) -> String {
    let err = registry
        .dispatch(
            "comm.send",
            json!({"to": NODE_TO, "subject": "s", "content": "node hello"}),
        )
        .await
        .expect_err("the node-addressed send must be refused");
    format!("{err:?}")
}

// Kills (factory): in `pack.rs`, the shared state handed out by the pack
//   old: `Ok(Some(state)) => Some(state.clone()),`
//   new: `Ok(Some(_state)) => None,`
// With the mutant, an enabled valid table yields no host state, so the `Some` expectations below
// fail; the node service would then never receive the state the pack's own route uses.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn node_factory_shares_state_and_refuses_disabled_node_recipients() {
    let factory = comm_factory();
    assert_eq!(factory.name(), "comm");

    // A disabled or absent table: no state, no route, and a node address is refused as
    // disabled before anything is written. It is not treated as an ordinary actor label. A
    // disabled table is not checked field by field, so one with unusable fields is still fine.
    let disabled_with_bad_fields = NodeChannelConfig {
        enabled: false,
        slug: Some("BAD slug".to_string()),
        ..Default::default()
    };
    let mut disabled_text: Option<String> = None;
    for (label, table) in [
        ("absent", None),
        ("disabled", Some(NodeChannelConfig::default())),
        (
            "disabled with unchecked fields",
            Some(disabled_with_bad_fields),
        ),
    ] {
        let runtime = runtime_with(table);
        let pack = factory.create(runtime.clone());
        assert!(pack.host_state().is_none(), "{label}: no node state");
        pack.validate_config()
            .unwrap_or_else(|error| panic!("{label}: validation must pass: {error:?}"));

        let registry = registry_for(&runtime);
        let text = refused_node_send(&registry).await;
        assert!(
            text.contains("the node channel is disabled"),
            "{label}: {text}"
        );
        match &disabled_text {
            Some(first) => assert_eq!(first, &text, "{label}: the same refusal in every case"),
            None => disabled_text = Some(text),
        }
        assert_eq!(message_count(&runtime).await, 0, "{label}: no note written");
        assert_eq!(
            transport_row_count(&runtime).await,
            0,
            "{label}: no transport row written"
        );
    }
    let disabled_text = disabled_text.expect("at least one disabled case ran");

    // An enabled valid table: the factory builds the real state, hands out the same `Arc` on
    // every retrieval, and validation passes.
    let runtime = runtime_with(Some(valid_enabled_table()));
    let pack = factory.create(runtime.clone());
    pack.validate_config()
        .unwrap_or_else(|error| panic!("validation must pass: {error:?}"));
    let first = pack
        .host_state()
        .expect("an enabled valid table yields node state");
    for _ in 0..3 {
        let again = pack.host_state().expect("node state on repeated retrieval");
        assert!(
            Arc::ptr_eq(&first, &again),
            "every retrieval returns the same Arc"
        );
    }
    let Ok(state) = first.downcast::<NodeHostState>() else {
        panic!("the host state is the node host state");
    };
    assert!(state.config().enabled);
    assert_eq!(state.binding().slug, "test");
    assert_eq!(state.binding().namespace, NS);

    // The same table installs the real route: with pair policy set to shadow, a node address
    // is refused by that route (not as disabled) and still writes nothing. The route refuses
    // before any key or network access, so this stays inside the process.
    let shadow = NodeChannelConfig {
        pair_policy_mode: PolicyMode::Shadow,
        ..valid_enabled_table()
    };
    let runtime = runtime_with(Some(shadow));
    let registry = registry_for(&runtime);
    let routed_text = refused_node_send(&registry).await;
    assert!(
        routed_text.contains("pair policy evaluation is unavailable"),
        "the installed route refuses a node address under shadow policy: {routed_text}"
    );
    assert_ne!(
        routed_text, disabled_text,
        "an installed route refuses for its own reason, not as disabled"
    );
    assert_eq!(message_count(&runtime).await, 0, "routed refusal: no note");
    assert_eq!(
        transport_row_count(&runtime).await,
        0,
        "routed refusal: no transport row"
    );

    // An invalid enabled table refuses activation instead of being treated as no route.
    let valid = valid_enabled_table();
    for (label, table) in [
        (
            "missing slug",
            NodeChannelConfig {
                slug: None,
                ..valid.clone()
            },
        ),
        (
            "short fingerprint",
            NodeChannelConfig {
                key_fingerprint: Some("ab".to_string()),
                ..valid.clone()
            },
        ),
        (
            "zero key epoch",
            NodeChannelConfig {
                key_epoch: Some(0),
                ..valid.clone()
            },
        ),
    ] {
        let runtime = runtime_with(Some(table));
        let pack = factory.create(runtime);
        assert!(
            pack.host_state().is_none(),
            "{label}: an invalid table yields no node state"
        );
        match pack
            .validate_config()
            .expect_err("an invalid enabled table must refuse activation")
        {
            RuntimeError::InvalidInput(message) => {
                assert!(message.contains("channel.node"), "{label}: {message}")
            }
            other => panic!("{label}: expected InvalidInput, got {other:?}"),
        }
    }
}
