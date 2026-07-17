//! Regression tests for #741: `brain.resolve` defaults `actor` from the
//! caller's dispatch identity, so the introspection verb agrees with the
//! serve path's binding resolution (#708).

use std::sync::Arc;

use khive_pack_brain::BrainPack;
use khive_runtime::{
    BackendId, KhiveRuntime, Namespace, PackRuntime, RuntimeConfig, VerbRegistryBuilder,
};
use serde_json::json;

fn runtime_with_actor(actor_id: Option<&str>) -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        git_write: Default::default(),
        db_path: None,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(khive_runtime::AllowAllGate),
        packs: vec!["kg".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: actor_id.map(str::to_owned),
    })
    .expect("runtime")
}

async fn bind_seat_profile(brain: &BrainPack, rt: &KhiveRuntime, actor: &str) {
    let registry = VerbRegistryBuilder::new().build().expect("registry");
    let token = rt.authorize(Namespace::local()).expect("token");
    brain
        .dispatch(
            "brain.create_profile",
            json!({ "name": "seat-recall-test", "consumer_kind": "recall" }),
            &registry,
            &token,
        )
        .await
        .expect("create profile");
    brain
        .dispatch(
            "brain.bind",
            json!({
                "profile_id": "seat-recall-test",
                "actor": actor,
                "consumer_kind": "recall",
                "priority": 10
            }),
            &registry,
            &token,
        )
        .await
        .expect("bind profile");
}

/// A bound actor's no-arg `brain.resolve(consumer_kind="recall")` through the
/// dispatch path must return its bound profile with `matched_binding: true`.
/// Before #741 the omitted `actor` param resolved as the wildcard caller and
/// this returned the balanced fallback with `matched_binding: false`.
#[tokio::test]
async fn resolve_defaults_actor_from_dispatch_identity() {
    let rt = runtime_with_actor(Some("lambda:test-seat"));
    let brain = Arc::new(BrainPack::new(rt.clone()));
    bind_seat_profile(&brain, &rt, "lambda:test-seat").await;

    let registry = VerbRegistryBuilder::new().build().expect("registry");
    let token = rt.authorize(Namespace::local()).expect("token");
    let out = brain
        .dispatch(
            "brain.resolve",
            json!({ "consumer_kind": "recall" }),
            &registry,
            &token,
        )
        .await
        .expect("resolve");

    assert_eq!(
        out.get("matched_binding").and_then(|v| v.as_bool()),
        Some(true),
        "bound caller must match its binding without an explicit actor param; got {out}"
    );
    assert_eq!(
        out.get("resolved_profile_id").and_then(|v| v.as_str()),
        Some("seat-recall-test"),
        "must resolve the bound profile; got {out}"
    );
}

/// An explicit `actor` param wins over the caller's dispatch identity, so
/// evaluation tooling can query other identities.
#[tokio::test]
async fn explicit_actor_param_overrides_dispatch_identity() {
    let rt = runtime_with_actor(Some("lambda:other-seat"));
    let brain = Arc::new(BrainPack::new(rt.clone()));
    bind_seat_profile(&brain, &rt, "lambda:test-seat").await;

    let registry = VerbRegistryBuilder::new().build().expect("registry");
    let token = rt.authorize(Namespace::local()).expect("token");
    let out = brain
        .dispatch(
            "brain.resolve",
            json!({ "consumer_kind": "recall", "actor": "lambda:test-seat" }),
            &registry,
            &token,
        )
        .await
        .expect("resolve");

    assert_eq!(
        out.get("resolved_profile_id").and_then(|v| v.as_str()),
        Some("seat-recall-test"),
        "explicit actor param must be honored verbatim; got {out}"
    );
    assert_eq!(
        out.get("matched_binding").and_then(|v| v.as_bool()),
        Some(true)
    );
}

/// The anonymous caller never matches an explicit binding (#708 invariant).
/// The binding deliberately uses `actor="local"` — the anonymous `ActorRef`
/// carries raw id `"local"`, so this test fails if `handle_resolve` ever
/// regresses from `binding_id()` (anonymous → `None`) to the raw actor id,
/// which would let an unauthenticated caller match an explicit `local` row.
#[tokio::test]
async fn anonymous_caller_does_not_match_explicit_binding() {
    let rt = runtime_with_actor(None);
    let brain = Arc::new(BrainPack::new(rt.clone()));
    bind_seat_profile(&brain, &rt, "local").await;

    let registry = VerbRegistryBuilder::new().build().expect("registry");
    let token = rt.authorize(Namespace::local()).expect("token");
    let out = brain
        .dispatch(
            "brain.resolve",
            json!({ "consumer_kind": "recall" }),
            &registry,
            &token,
        )
        .await
        .expect("resolve");

    assert_eq!(
        out.get("matched_binding").and_then(|v| v.as_bool()),
        Some(false),
        "anonymous caller must not match an explicit actor binding; got {out}"
    );
    assert_ne!(
        out.get("resolved_profile_id").and_then(|v| v.as_str()),
        Some("seat-recall-test"),
        "anonymous caller must not resolve the actor-bound profile; got {out}"
    );
}
