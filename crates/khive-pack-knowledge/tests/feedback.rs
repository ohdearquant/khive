//! Integration tests for `knowledge.feedback` 3-tier profile resolution (ADR-035).
//!
//! Tier order (exclusive flow per ADR-035):
//! 1. Explicit brain profile in pack config → route via `brain.feedback`, return early
//! 2. Namespace-bound profile via `brain.resolve(consumer_kind="knowledge_compose")`, matched_binding=true → return early
//! 3. Global section_posteriors → update pack-local prior (only when tiers 1 and 2 do not resolve)

use khive_pack_brain::BrainPack;
use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, VerbRegistryBuilder};
use serde_json::json;

// ── helpers ───────────────────────────────────────────────────────────────────

fn make_rt(brain_profile: Option<String>, with_brain: bool) -> KhiveRuntime {
    let packs: Vec<String> = if with_brain {
        vec!["kg".into(), "knowledge".into(), "brain".into()]
    } else {
        vec!["kg".into(), "knowledge".into()]
    };
    KhiveRuntime::new(RuntimeConfig {
        git_write: Default::default(),
        db_path: None,
        embedding_model: None,
        additional_embedding_models: vec![],
        packs,
        brain_profile,
        ..RuntimeConfig::default()
    })
    .expect("runtime")
}

/// Mirrors `make_rt(None, true)` but with a configured actor. Full literal
/// (no `..RuntimeConfig::default()`) — `Default` resolves `embedding_model`
/// to a real on-disk model, absent on CI runners.
fn make_rt_with_actor(actor: &str) -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        git_write: Default::default(),
        db_path: None,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: std::sync::Arc::new(khive_runtime::AllowAllGate),
        packs: vec!["kg".into(), "knowledge".into(), "brain".into()],
        backend_id: khive_runtime::BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: Some(actor.to_string()),
    })
    .expect("runtime with actor")
}

/// Create a KG concept entity for use as brain.feedback target_id.
///
/// brain.feedback validates that target_id resolves to a real record in the
/// namespace. Knowledge atoms are stored in a separate table outside the KG
/// entity/note graph, so a KG entity (concept) must be used as the target.
async fn make_entity(registry: &khive_runtime::VerbRegistry, ns: &str) -> String {
    let r = registry
        .dispatch(
            "create",
            json!({
                "namespace": ns,
                "kind": "concept",
                "name": "TestConcept",
                "description": "A test concept entity for knowledge feedback tests",
            }),
        )
        .await
        .expect("create entity");
    r["id"].as_str().expect("entity id from create").to_string()
}

// ── Tier-1 tests ──────────────────────────────────────────────────────────────

/// Tier-1: explicit brain_profile in pack config routes exclusively to brain.feedback.
/// When brain pack is not loaded, brain.feedback is absent → the call errors (not falls through).
#[tokio::test]
async fn feedback_tier1_explicit_profile_routes_to_brain() {
    let rt = make_rt(Some("balanced-recall-v1".into()), false);
    let ns = Namespace::parse("local").expect("ns");

    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(KnowledgePack::new(rt.clone()));
    let registry = builder.build().expect("registry");

    let atom_id = make_entity(&registry, ns.as_str()).await;

    // brain.feedback is not registered → explicit profile → error propagates.
    let result = registry
        .dispatch(
            "knowledge.feedback",
            json!({
                "namespace": ns.as_str(),
                "target_id": atom_id,
                "section_signals": {"overview": "useful"},
            }),
        )
        .await;

    assert!(
        result.is_err(),
        "tier-1 with no brain pack must error (verb not found), got: {result:?}"
    );
}

/// Tier-1 with brain loaded: explicit profile is credited, not the namespace-bound one.
#[tokio::test]
async fn feedback_tier1_explicit_wins_over_bound_profile() {
    let rt = make_rt(Some("balanced-recall-v1".into()), true);
    let ns = Namespace::parse("local").expect("ns");

    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(KnowledgePack::new(rt.clone()));
    builder.register(BrainPack::new(rt.clone()));
    let registry = builder.build().expect("registry");
    rt.install_edge_rules(registry.all_edge_rules());

    let atom_id = make_entity(&registry, ns.as_str()).await;

    // Create and bind a secondary profile as the "tier-2" candidate.
    registry
        .dispatch(
            "brain.create_profile",
            json!({"namespace": ns.as_str(), "name": "alt-profile", "consumer_kind": "knowledge_compose"}),
        )
        .await
        .expect("create alt profile");
    registry
        .dispatch(
            "brain.activate",
            json!({"namespace": ns.as_str(), "profile_id": "alt-profile"}),
        )
        .await
        .expect("activate alt profile");
    registry
        .dispatch(
            "brain.bind",
            json!({"namespace": ns.as_str(), "profile_id": "alt-profile", "consumer_kind": "knowledge_compose"}),
        )
        .await
        .expect("bind alt profile");

    // Tier-1 must win: brain.feedback returns {"emitted": true, ...}.
    let r = registry
        .dispatch(
            "knowledge.feedback",
            json!({
                "namespace": ns.as_str(),
                "target_id": atom_id,
                "section_signals": {"overview": "useful"},
            }),
        )
        .await
        .expect("feedback ok");

    assert_eq!(
        r["emitted"], true,
        "tier-1 explicit profile must route to brain pack: {r:?}"
    );
    // knowledge.feedback tier-1 response includes both 'emitted' and 'brain_profile'.
    assert_eq!(
        r.get("brain_profile").and_then(|v| v.as_str()),
        Some("balanced-recall-v1"),
        "knowledge.feedback tier-1 must include the explicit brain_profile in response: {r:?}"
    );

    // alt-profile must have 0 events (tier-1 bypassed it).
    let alt = registry
        .dispatch(
            "brain.profile",
            json!({"namespace": ns.as_str(), "profile_id": "alt-profile"}),
        )
        .await
        .expect("brain.profile alt");
    assert_eq!(
        alt["total_events"].as_u64().unwrap_or(0),
        0,
        "alt-profile must NOT receive events when tier-1 is active"
    );
}

// ── Tier-2 tests ──────────────────────────────────────────────────────────────

/// Tier-2: namespace-bound profile (explicit binding) receives feedback when
/// no explicit brain_profile is configured.
#[tokio::test]
async fn feedback_tier2_namespace_bound_profile_credited() {
    let rt = make_rt(None, true);
    let ns = Namespace::parse("local").expect("ns");

    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(KnowledgePack::new(rt.clone()));
    builder.register(BrainPack::new(rt.clone()));
    let registry = builder.build().expect("registry");
    rt.install_edge_rules(registry.all_edge_rules());

    let atom_id = make_entity(&registry, ns.as_str()).await;

    // Create a secondary profile and bind it explicitly for consumer_kind="knowledge_compose".
    registry
        .dispatch(
            "brain.create_profile",
            json!({"namespace": ns.as_str(), "name": "ns-bound-compose", "consumer_kind": "knowledge_compose"}),
        )
        .await
        .expect("create ns-bound profile");
    registry
        .dispatch(
            "brain.activate",
            json!({"namespace": ns.as_str(), "profile_id": "ns-bound-compose"}),
        )
        .await
        .expect("activate ns-bound profile");
    registry
        .dispatch(
            "brain.bind",
            json!({"namespace": ns.as_str(), "profile_id": "ns-bound-compose", "consumer_kind": "knowledge_compose"}),
        )
        .await
        .expect("bind ns-bound profile");

    // Confirm brain.resolve returns the bound profile with matched_binding=true.
    let resolve = registry
        .dispatch(
            "brain.resolve",
            json!({"namespace": ns.as_str(), "consumer_kind": "knowledge_compose"}),
        )
        .await
        .expect("brain.resolve");
    assert_eq!(
        resolve["resolved_profile_id"], "ns-bound-compose",
        "brain.resolve must return the bound profile"
    );
    assert_eq!(
        resolve["matched_binding"], true,
        "must be matched_binding=true for an explicit binding"
    );

    // Send feedback — tier-2 must route to ns-bound-compose.
    let r = registry
        .dispatch(
            "knowledge.feedback",
            json!({
                "namespace": ns.as_str(),
                "target_id": atom_id,
                "section_signals": {"overview": "useful"},
            }),
        )
        .await
        .expect("feedback ok");
    assert_eq!(
        r["emitted"], true,
        "tier-2 feedback must route to brain pack: {r:?}"
    );

    // ns-bound-compose must have total_events == 1.
    let prof = registry
        .dispatch(
            "brain.profile",
            json!({"namespace": ns.as_str(), "profile_id": "ns-bound-compose"}),
        )
        .await
        .expect("brain.profile");
    assert_eq!(
        prof["total_events"].as_u64().unwrap_or(0),
        1,
        "ns-bound-compose must receive the feedback event"
    );
}

/// Tier-2, actor-bound: `knowledge.feedback`'s own call site must thread the
/// caller's actor identity, not just `resolve_compose_type_weights` (the read
/// side). Binds `actor-bound-compose` by `actor="leo"` only, leaving namespace
/// as the wildcard `"*"` — a namespace-only resolution can never reach it.
/// Mutation: reverting `handle_feedback`'s call site back to `actor=None`
/// must make this test fail (fall through to tier-3, `emitted` absent/false).
#[tokio::test]
async fn feedback_tier2_actor_bound_profile_credited() {
    let rt = make_rt_with_actor("leo");
    let ns = Namespace::parse("local").expect("ns");

    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(KnowledgePack::new(rt.clone()));
    builder.register(BrainPack::new(rt.clone()));
    // `VerbRegistry` mints its own per-dispatch tokens from its own
    // construction-baked actor id (independent of `RuntimeConfig::actor_id`) —
    // bake the same actor here so `registry.dispatch` calls carry it too.
    builder.with_actor_id(Some("leo".to_string()));
    let registry = builder.build().expect("registry");
    rt.install_edge_rules(registry.all_edge_rules());

    let atom_id = make_entity(&registry, ns.as_str()).await;

    registry
        .dispatch(
            "brain.create_profile",
            json!({"namespace": ns.as_str(), "name": "actor-bound-compose", "consumer_kind": "knowledge_compose"}),
        )
        .await
        .expect("create actor-bound profile");
    registry
        .dispatch(
            "brain.activate",
            json!({"namespace": ns.as_str(), "profile_id": "actor-bound-compose"}),
        )
        .await
        .expect("activate actor-bound profile");
    // Bind by actor only — namespace defaults to the "*" wildcard.
    registry
        .dispatch(
            "brain.bind",
            json!({"actor": "leo", "profile_id": "actor-bound-compose", "consumer_kind": "knowledge_compose"}),
        )
        .await
        .expect("bind actor-bound profile to actor=leo");

    let r = registry
        .dispatch(
            "knowledge.feedback",
            json!({
                "namespace": ns.as_str(),
                "target_id": atom_id,
                "section_signals": {"overview": "useful"},
            }),
        )
        .await
        .expect("feedback ok");
    assert_eq!(
        r["emitted"], true,
        "tier-2 actor-bound feedback must route to brain pack: {r:?}"
    );
    assert_eq!(
        r.get("brain_profile").and_then(|v| v.as_str()),
        Some("actor-bound-compose"),
        "knowledge.feedback must credit the actor-bound profile: {r:?}"
    );

    let prof = registry
        .dispatch(
            "brain.profile",
            json!({"namespace": ns.as_str(), "profile_id": "actor-bound-compose"}),
        )
        .await
        .expect("brain.profile");
    assert_eq!(
        prof["total_events"].as_u64().unwrap_or(0),
        1,
        "actor-bound-compose must receive the feedback event"
    );
}

// ── Tier-3 tests ──────────────────────────────────────────────────────────────

/// Tier-3: when no explicit profile is configured and no explicit namespace binding
/// exists, feedback updates the pack-local section_posteriors directly — even
/// when balanced-recall-v1 is Active (system-default fallback, not a binding match).
///
/// This is a regression test: before the fix, tier-3 fired
/// unconditionally (before tiers 1/2 were checked), and tier-2 used consumer_kind
/// "knowledge.search" which never matched recall bindings.
#[tokio::test]
async fn feedback_tier3_global_fallback_no_explicit_binding() {
    // No explicit brain_profile, brain pack loaded but NO explicit binding.
    let rt = make_rt(None, true);
    let ns = Namespace::parse("local").expect("ns");

    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(KnowledgePack::new(rt.clone()));
    builder.register(BrainPack::new(rt.clone()));
    let registry = builder.build().expect("registry");
    rt.install_edge_rules(registry.all_edge_rules());

    let atom_id = make_entity(&registry, ns.as_str()).await;

    // Confirm brain.resolve reports no explicit binding for consumer_kind=
    // "knowledge_compose". Unlike "recall" (which always has the seeded
    // balanced-recall-v1 system default to fall back to), no default profile is
    // registered for "knowledge_compose" yet, so brain.resolve legitimately
    // errors here rather than returning matched_binding=false. Both outcomes mean
    // "no tier-2 hit" to `khive_brain_core::resolve_consumer_profile` (ADR-058
    // amendment, #542), which folds an Err the same as matched_binding=false —
    // hence nothing to assert in the Err arm below.
    if let Ok(resolve) = registry
        .dispatch(
            "brain.resolve",
            json!({"namespace": ns.as_str(), "consumer_kind": "knowledge_compose"}),
        )
        .await
    {
        assert_eq!(
            resolve["matched_binding"], false,
            "no explicit binding: matched_binding must be false (system default)"
        );
    }

    // Tier-3 must fire: section_posteriors updated, ok=true, no emitted key.
    let r = registry
        .dispatch(
            "knowledge.feedback",
            json!({
                "namespace": ns.as_str(),
                "target_id": atom_id,
                "section_signals": {"overview": "useful"},
            }),
        )
        .await
        .expect("tier-3 feedback must not error");

    assert_eq!(r["ok"], true, "tier-3 must return ok=true: {r:?}");
    assert!(
        r.get("total_events").is_some(),
        "tier-3 must include total_events from section_posteriors: {r:?}"
    );
    assert!(
        r.get("emitted").is_none(),
        "tier-3 must not route to brain.feedback (no emitted key): {r:?}"
    );
}

/// Tier-3 without brain pack: feedback always falls through to global prior.
#[tokio::test]
async fn feedback_tier3_no_brain_pack() {
    let rt = make_rt(None, false);
    let ns = Namespace::parse("local").expect("ns");

    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(KnowledgePack::new(rt.clone()));
    let registry = builder.build().expect("registry");
    rt.install_edge_rules(registry.all_edge_rules());

    let atom_id = make_entity(&registry, ns.as_str()).await;

    let r = registry
        .dispatch(
            "knowledge.feedback",
            json!({
                "namespace": ns.as_str(),
                "target_id": atom_id,
                "section_signals": {"overview": "not_useful"},
            }),
        )
        .await
        .expect("tier-3 feedback must not error");

    assert_eq!(r["ok"], true, "tier-3 must return ok=true: {r:?}");
    assert!(
        r.get("total_events").is_some(),
        "tier-3 must include total_events: {r:?}"
    );
}

/// Tier-3 fires even without a target_id (section_posteriors still updated).
#[tokio::test]
async fn feedback_tier3_no_target_id() {
    let rt = make_rt(None, false);
    let ns = Namespace::parse("local").expect("ns");

    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(KnowledgePack::new(rt.clone()));
    let registry = builder.build().expect("registry");

    // No target_id supplied — tier-3 (section_posteriors) must still apply.
    let r = registry
        .dispatch(
            "knowledge.feedback",
            json!({
                "namespace": ns.as_str(),
                "section_signals": {"overview": "wrong"},
            }),
        )
        .await
        .expect("feedback without target_id must not error");

    assert_eq!(r["ok"], true, "ok=true even without target_id: {r:?}");
    assert!(
        r.get("total_events").is_some(),
        "total_events must be present: {r:?}"
    );
}
