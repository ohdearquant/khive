use crate::{KhiveRuntime, Namespace, NamespaceToken};
use khive_storage::Event;
use khive_types::{EventKind, SubstrateKind};
use serde_json::{json, Value};

/// Build a runtime whose minted tokens carry a configured actor identity
/// (rather than `ActorRef::anonymous()`), so actor-scoped bindings can match.
pub(super) fn runtime_with_actor(actor_id: &str) -> KhiveRuntime {
    // Mirror KhiveRuntime::memory() exactly (in-memory db, NO embedding model)
    // plus a configured actor. `..RuntimeConfig::default()` is a CI trap: the
    // Default impl resolves embedding_model to a real on-disk model, which is
    // absent on CI runners and fails entity creation with ModelInitialization.
    KhiveRuntime::new(crate::RuntimeConfig {
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: Default::default(),
        wal_ceiling_env_raw: None,
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        git_write: Default::default(),
        brain: Default::default(),
        exec: Default::default(),
        display_timezone: crate::config::resolve_default_display_timezone(),
        events_split: None,
        db_path: None,
        blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: std::sync::Arc::new(crate::AllowAllGate),
        packs: vec!["kg".to_string()],
        backend_id: crate::BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: Some(actor_id.to_string()),
        ..crate::RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime with actor")
}

/// Append a synthetic event through the raw backend fixture seam, bypassing
/// verb dispatch, so analytics tests can control actor, `created_at`, and
/// payload precisely instead of relying on wall-clock timing. Production
/// pack code must use `KhiveRuntime::events`, whose token-scoped decorator
/// intentionally seals actor and namespace attribution.
pub(super) async fn seed_event(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    verb: &str,
    kind: EventKind,
    actor: &str,
    created_at: i64,
    payload: Value,
) {
    let mut event = Event::new(
        token.namespace().as_str(),
        verb,
        kind,
        SubstrateKind::Note,
        actor,
    );
    event.created_at = created_at;
    event.payload = payload;
    if kind == EventKind::SearchExecuted && event.payload.get("result_kind").is_none() {
        event.payload["result_kind"] = json!("note");
    }
    rt.backend()
        .events_for_namespace(token.namespace().as_str())
        .expect("trusted fixture event store")
        .append_event(event)
        .await
        .expect("seed event append");
}

pub(super) fn cross_sum(value: &Value) -> u64 {
    value
        .as_object()
        .unwrap()
        .values()
        .flat_map(|actors| actors.as_object().unwrap().values())
        .map(|count| count.as_u64().unwrap())
        .sum()
}
