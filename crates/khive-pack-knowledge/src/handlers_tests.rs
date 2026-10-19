use std::collections::HashMap;
use uuid::Uuid;

use khive_brain_core::{FeedbackSignal, SectionPosteriorState, SectionType};
use khive_runtime::{KhiveRuntime, Namespace, PackRuntime, VerbRegistryBuilder};
use serde_json::json;

use crate::knowledge::section_feedback::on_section_feedback;
use crate::KnowledgePack;

/// A runtime shaped like a `[packs.knowledge]` backend assignment: bound
/// to a secondary `knowledge` backend with the shared-graph main backend
/// reachable only through `core()`. Both backends are fresh in-memory
/// databases with migrations applied (same construction as the session
/// pack's ADR-083 §4 regression tests).
fn secondary_backend_runtime() -> KhiveRuntime {
    use std::sync::Arc;

    let make_backend = || {
        let backend = khive_db::StorageBackend::memory().expect("in-memory backend");
        backend.pool().run_migrations().expect("migrations");
        Arc::new(backend)
    };
    let main_backend = make_backend();
    let knowledge_backend = make_backend();

    let mut config = khive_runtime::RuntimeConfig::no_embeddings();
    config.packs = vec!["kg".to_string()];
    config.backend_id = khive_runtime::BackendId::parse("knowledge").expect("valid backend id");

    KhiveRuntime::from_backend(knowledge_backend, config).with_core_backend(main_backend)
}

/// ADR-073 regression: the concept tier (`learn`/`cite`/`topic`) must
/// read and write the core graph. On a secondary-assigned runtime,
/// pre-fix behavior wrote the concept entity into the pack's own backend
/// — self-consistent from inside the pack, but invisible to kg and lost
/// to every core-graph consumer.
#[tokio::test]
async fn concept_tier_routes_through_core_on_secondary_backend() {
    let rt = secondary_backend_runtime();
    let pack = KnowledgePack::new(rt.clone());
    let registry = VerbRegistryBuilder::new()
        .build()
        .expect("empty registry builds");
    let token = rt.authorize(Namespace::local()).expect("authorize local");

    let learned = pack
            .dispatch(
                "knowledge.learn",
                json!({ "name": "core-routing probe", "content": "concept body", "domain": "retrieval" }),
                &registry,
                &token,
            )
            .await
            .expect("learn succeeds");
    let concept_id: Uuid = learned["full_id"]
        .as_str()
        .expect("full_id")
        .parse()
        .expect("uuid");

    // The entity must exist on the core graph...
    let on_core = rt
        .core()
        .get_entities_by_ids(&token, &[concept_id])
        .await
        .expect("core read succeeds");
    assert_eq!(
        on_core.len(),
        1,
        "learn must write the concept entity to the core (main) backend"
    );
    // ...and must NOT have been written into the pack's secondary backend.
    let on_secondary = rt
        .get_entities_by_ids(&token, &[concept_id])
        .await
        .expect("secondary read succeeds");
    assert!(
        on_secondary.is_empty(),
        "concept entity must not land on the knowledge backend"
    );

    // cite: prefix resolution and the edge write share the core seam.
    // The source is a document entity on the core graph (the shape cite
    // links against in production).
    let source = rt
        .core()
        .create_entity_with_embedding_report(
            &token,
            "document",
            None,
            "source paper",
            Some("source body"),
            None,
            vec![],
        )
        .await
        .map(|(entity, _report)| entity)
        .expect("source document entity");
    let source_id = source.id.to_string();
    pack.dispatch(
        "knowledge.cite",
        json!({ "concept_id": concept_id.to_string(), "source_id": source_id }),
        &registry,
        &token,
    )
    .await
    .expect("cite must resolve both core-side entities and write the edge");

    // topic (listing mode) reads through core and must see the concepts.
    let topics = pack
        .dispatch(
            "knowledge.topic",
            json!({ "domain": "retrieval" }),
            &registry,
            &token,
        )
        .await
        .expect("topic succeeds");
    assert_eq!(
        topics["total"].as_u64(),
        Some(1),
        "topic must list the core-side concept: {topics}"
    );
}

include!("handlers_tests/topic_filter_and_feedback_validation_tests.rs");

/// Regression for #346: Tier 3 must read tuned `section_posteriors`, not a
/// fresh default. See crates/khive-pack-knowledge/docs/api/compose-type-weights.md.
#[tokio::test]
async fn resolve_compose_type_weights_reads_tuned_section_posteriors_at_tier3() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let pack = KnowledgePack::new(rt.clone());
    let registry = VerbRegistryBuilder::new()
        .build()
        .expect("empty registry builds");
    let token = rt.authorize(Namespace::local()).expect("authorize local");

    // Skew posteriors: many Useful events on Formalism, many Wrong events on
    // OperationalGuidance.  Force exploration_epoch=0 so deterministic_weights()
    // uses posterior means directly (no Thompson sampling noise).
    {
        let mut states = pack.section_posteriors.lock().unwrap();
        let state = states.entry("local".to_string()).or_default();
        for _ in 0..80 {
            on_section_feedback(
                state,
                &[
                    (SectionType::Formalism, FeedbackSignal::Useful),
                    (SectionType::OperationalGuidance, FeedbackSignal::Wrong),
                ],
            );
        }
        state.exploration_epoch = 0;
    }

    // Tier 1 and 2 are absent (empty registry), so Tier 3 must fire.
    let weights = pack.resolve_compose_type_weights(&registry, &token).await;

    let formalism_tuned = *weights
        .get("formalism")
        .expect("formalism weight must be present");
    let og_tuned = *weights
        .get("operational_guidance")
        .expect("operational_guidance weight must be present");

    // Default priors: OperationalGuidance (α=6,β=1.5) >> Formalism (α=1.5,β=4).
    let default_state = SectionPosteriorState::default();
    let default_w: HashMap<String, f32> = default_state
        .deterministic_weights()
        .into_iter()
        .map(|(st, w)| (st.as_str().to_string(), w as f32))
        .collect();
    let formalism_default = *default_w.get("formalism").unwrap();
    let og_default = *default_w.get("operational_guidance").unwrap();

    assert!(
        formalism_tuned > formalism_default,
        "tuned formalism weight {formalism_tuned:.4} must exceed default {formalism_default:.4} \
             after skewing feedback"
    );
    assert!(
        og_tuned < og_default,
        "tuned og weight {og_tuned:.4} must be below default {og_default:.4} \
             after wrong feedback"
    );
    // After sufficient skewing, formalism must actually dominate operational_guidance —
    // this is the ordering flip that compose's type_weight component now reflects.
    assert!(
        formalism_tuned > og_tuned,
        "after skewing, formalism {formalism_tuned:.4} must outweigh og {og_tuned:.4}"
    );
}

/// #1505: Tier-3 feedback is isolated by exact namespace. Local/live
/// tuning must not alter the fallback weights used by a measurement arm
/// that has recorded no feedback of its own.
#[tokio::test]
async fn tier3_section_posteriors_do_not_cross_namespaces() {
    // Mirror `KhiveRuntime::memory()` plus an actor: `..Default::default()`
    // resolves `db_path` to the real store anchor, which the harness refuses.
    let rt = KhiveRuntime::new(khive_runtime::RuntimeConfig {
        db_path: None,
        packs: vec!["kg".to_string()],
        brain_profile: None,
        actor_id: Some("section-isolation-test".into()),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime");
    let pack = KnowledgePack::new(rt.clone());
    let registry = VerbRegistryBuilder::new()
        .build()
        .expect("empty registry builds");
    let local_token = rt.authorize(Namespace::local()).expect("local token");
    let arm_token = rt
        .authorize(Namespace::parse("bench-arm-a").expect("arm namespace"))
        .expect("arm token");

    for _ in 0..80 {
        pack.dispatch(
            "knowledge.feedback",
            json!({
                "section_signals": {
                    "formalism": "useful",
                    "operational_guidance": "wrong"
                }
            }),
            &registry,
            &local_token,
        )
        .await
        .expect("local Tier-3 feedback");
    }

    let local = pack
        .resolve_compose_type_weights(&registry, &local_token)
        .await;
    let arm = pack
        .resolve_compose_type_weights(&registry, &arm_token)
        .await;
    let defaults: HashMap<String, f32> = SectionPosteriorState::default()
        .deterministic_weights()
        .into_iter()
        .map(|(st, w)| (st.as_str().to_string(), w as f32))
        .collect();

    assert!(
        local["formalism"] > local["operational_guidance"],
        "local feedback must tune the local fallback"
    );
    assert_eq!(
        arm, defaults,
        "an untouched measurement arm must retain fresh Tier-3 defaults"
    );
}

/// Regression for #346 Tier-2: must read weights from a namespace-bound
/// brain profile (`brain.bind`), not fall through to Tier 3/default. See
/// crates/khive-pack-knowledge/docs/api/compose-type-weights.md.
#[tokio::test]
async fn resolve_compose_type_weights_reads_bound_profile_weights_at_tier2() {
    use khive_pack_brain::BrainPack;
    use khive_pack_kg::KgPack;

    // Build the production-like registry slice: brain requires kg, and the
    // knowledge pack declares the `knowledge_compose` consumer kind.
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(BrainPack::new(rt.clone()));
    builder.register(KnowledgePack::new(rt.clone()));
    let registry = builder.build().expect("kg+brain+knowledge registry builds");
    let token = rt.authorize(Namespace::local()).expect("authorize local");

    // Create a profile with inverted seed_priors:
    //   formalism:           Beta(8, 1) → mean ≈ 0.889
    //     (default prior:               α=1.5, β=4.0 → mean ≈ 0.273)
    //   operational_guidance: Beta(1, 8) → mean ≈ 0.111
    //     (default prior:               α=6.0, β=1.5 → mean ≈ 0.8)
    // ESS = alpha + beta = 9 ≤ DEFAULT_ESS_CAP (100.0) ✓
    registry
        .dispatch(
            "brain.create_profile",
            json!({
                "name": "compose-tuned-v1",
                "consumer_kind": "knowledge_compose",
                "seed_priors": {
                    "section_posteriors": {
                        "formalism": {"alpha": 8.0, "beta": 1.0},
                        "operational_guidance": {"alpha": 1.0, "beta": 8.0}
                    }
                }
            }),
        )
        .await
        .expect("create_profile with skewed seed_priors");

    // Activate so the profile leaves the Inactive state (matches proper usage).
    registry
        .dispatch("brain.activate", json!({"profile_id": "compose-tuned-v1"}))
        .await
        .expect("activate compose-tuned-v1");

    // Bind for namespace="local", consumer_kind="knowledge_compose".
    // `resolve_consumer_profile` dispatches:
    //   brain.resolve(namespace="local", consumer_kind="knowledge_compose")
    // which must find this binding with matched_binding=true.
    registry
        .dispatch(
            "brain.bind",
            json!({
                "profile_id": "compose-tuned-v1",
                "namespace": "local",
                "consumer_kind": "knowledge_compose"
            }),
        )
        .await
        .expect("bind compose-tuned-v1 for namespace=local");

    // KnowledgePack with brain_profile=None → Tier 1 is skipped.
    let pack = KnowledgePack::new(rt.clone());

    // Tier 2 fires: brain.resolve returns matched_binding=true,
    // load_profile_type_weights dispatches brain.profile and extracts the
    // precomputed `weight` field from section_posteriors.
    // brain.profile always calls derive_deterministic_weights() — no Thompson
    // sampling, no epoch dependency — so results are deterministic from seed_priors.
    let weights = pack.resolve_compose_type_weights(&registry, &token).await;

    let formalism_w = *weights
        .get("formalism")
        .expect("formalism weight must be present");
    let og_w = *weights
        .get("operational_guidance")
        .expect("operational_guidance weight must be present");

    // seed_priors: formalism Beta(8,1) mean≈0.889 >> og Beta(1,8) mean≈0.111.
    // Default priors have the opposite ordering: og α=6,β=1.5 >> formalism α=1.5,β=4.
    // If load_profile_type_weights broke or Tier 2 fell through to Tier 3/default,
    // og would dominate — this assertion FAILS (genuine RED→GREEN guard).
    assert!(
        formalism_w > og_w,
        "Tier-2 bound-profile: formalism {formalism_w:.4} must exceed og {og_w:.4}; \
             ordering reflects seed_priors (formalism β(8,1) >> og β(1,8)), \
             not default priors where og α=6,β=1.5 dominates"
    );
}

/// #1505: after resolving a binding in a non-default namespace, the
/// follow-up `brain.profile` read must stay in that namespace too. Before
/// the fix, the second cross-pack dispatch silently fell back to local,
/// missed the arm profile, and compose used Tier-3 default weights.
#[tokio::test]
async fn resolve_compose_type_weights_keeps_nonlocal_namespace_for_profile_read() {
    use khive_pack_brain::BrainPack;
    use khive_pack_kg::KgPack;

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(BrainPack::new(rt.clone()));
    builder.register(KnowledgePack::new(rt.clone()));
    let registry = builder.build().expect("kg+brain+knowledge registry builds");
    let arm_ns = "bench-arm-a";

    registry
        .dispatch(
            "brain.create_profile",
            json!({
                "namespace": arm_ns,
                "name": "compose-bench-arm-v1",
                "consumer_kind": "knowledge_compose",
                "seed_priors": {
                    "section_posteriors": {
                        "formalism": {"alpha": 8.0, "beta": 1.0},
                        "operational_guidance": {"alpha": 1.0, "beta": 8.0}
                    }
                }
            }),
        )
        .await
        .expect("create arm profile");
    registry
        .dispatch(
            "brain.activate",
            json!({
                "namespace": arm_ns,
                "profile_id": "compose-bench-arm-v1",
            }),
        )
        .await
        .expect("activate arm profile");
    registry
        .dispatch(
            "brain.bind",
            json!({
                "namespace": arm_ns,
                "profile_id": "compose-bench-arm-v1",
                "consumer_kind": "knowledge_compose",
            }),
        )
        .await
        .expect("bind arm profile");

    let pack = KnowledgePack::new(rt.clone());
    let token = rt
        .authorize(Namespace::parse(arm_ns).expect("arm namespace"))
        .expect("arm token");
    let weights = pack.resolve_compose_type_weights(&registry, &token).await;
    let formalism = weights["formalism"];
    let operational = weights["operational_guidance"];
    assert!(
        formalism > operational,
        "nonlocal Tier-2 profile must supply its inverted weights; got \
             formalism={formalism:.4}, operational_guidance={operational:.4}"
    );
}

/// #700: `resolve_compose_type_weights` must thread the caller's actor identity
/// into `resolve_consumer_profile` so an actor-scoped (namespace-wildcard)
/// `knowledge_compose` binding resolves at Tier 2 — not just namespace-scoped
/// bindings, which the sibling `..._at_tier2` test above already covers.
///
/// Binds `compose-tuned-actor-v1` by `actor="leo"` only, leaving namespace as
/// the wildcard `"*"`. Before #700 (call sites passed `actor=None`), this
/// binding could never match — `resolve_compose_type_weights` would fall
/// through to Tier 3 and return the untuned default weights, where
/// `operational_guidance` dominates `formalism`. With the caller's actor
/// threaded through, Tier 2 must resolve this binding and return the
/// seed-tuned weights instead.
#[tokio::test]
async fn resolve_compose_type_weights_reads_bound_profile_weights_via_actor_binding_at_tier2() {
    use khive_pack_brain::BrainPack;
    use khive_pack_kg::KgPack;

    // Mirror `KhiveRuntime::memory()` exactly, plus a configured actor.
    // `..RuntimeConfig::default()` is a CI trap here: `Default` resolves
    // `embedding_model` to a real on-disk model, which is absent on CI
    // runners and fails entity creation with `ModelInitialization`.
    let rt = KhiveRuntime::new(khive_runtime::RuntimeConfig {
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: Default::default(),
        wal_ceiling_env_raw: None,
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: khive_runtime::config::resolve_default_display_timezone(),
        events_split: None,
        db_path: None,
        blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: std::sync::Arc::new(khive_runtime::AllowAllGate),
        packs: vec!["kg".to_string()],
        backend_id: khive_runtime::BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: Some("leo".to_string()),
        exec: Default::default(),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime with actor");

    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(BrainPack::new(rt.clone()));
    builder.register(KnowledgePack::new(rt.clone()));
    let registry = builder.build().expect("kg+brain+knowledge registry builds");

    let token = rt.authorize(Namespace::local()).expect("authorize local");
    assert_eq!(
        token.actor().id,
        "leo",
        "test setup: token must carry the configured actor"
    );

    // Same inverted seed_priors as the namespace-bound sibling test:
    //   formalism:            Beta(8, 1) → mean ≈ 0.889
    //   operational_guidance: Beta(1, 8) → mean ≈ 0.111
    registry
        .dispatch(
            "brain.create_profile",
            json!({
                "name": "compose-tuned-actor-v1",
                "consumer_kind": "knowledge_compose",
                "seed_priors": {
                    "section_posteriors": {
                        "formalism": {"alpha": 8.0, "beta": 1.0},
                        "operational_guidance": {"alpha": 1.0, "beta": 8.0}
                    }
                }
            }),
        )
        .await
        .expect("create_profile with skewed seed_priors");

    registry
        .dispatch(
            "brain.activate",
            json!({"profile_id": "compose-tuned-actor-v1"}),
        )
        .await
        .expect("activate compose-tuned-actor-v1");

    // Bind by actor only — namespace left as the "*" wildcard — so a
    // namespace-only resolution (pre-#700 behavior) can never reach this
    // binding; only threading the caller's actor through can.
    registry
        .dispatch(
            "brain.bind",
            json!({
                "actor": "leo",
                "profile_id": "compose-tuned-actor-v1",
                "consumer_kind": "knowledge_compose"
            }),
        )
        .await
        .expect("bind compose-tuned-actor-v1 for actor=leo");

    // KnowledgePack with brain_profile=None → Tier 1 is skipped.
    let pack = KnowledgePack::new(rt.clone());
    let weights = pack.resolve_compose_type_weights(&registry, &token).await;

    let formalism_w = *weights
        .get("formalism")
        .expect("formalism weight must be present");
    let og_w = *weights
        .get("operational_guidance")
        .expect("operational_guidance weight must be present");

    assert!(
        formalism_w > og_w,
        "Tier-2 actor-bound profile: formalism {formalism_w:.4} must exceed \
             og {og_w:.4}; ordering reflects seed_priors (formalism β(8,1) >> \
             og β(1,8)) resolved via the actor-scoped binding, not the default \
             priors where og α=6,β=1.5 dominates"
    );
}

/// Systemic-fix regression: an ANONYMOUS caller must not match an explicit
/// `actor="local"` binding.
///
/// `ActorRef::anonymous()` carries `id: "local"` (`khive-gate/src/actor.rs`).
/// Before the `binding_id()` fix, `resolve_compose_type_weights` threaded
/// `token.actor().id` unconditionally, so an anonymous token's `"local"`
/// id could match an explicit `actor="local"` binding that a pre-actor-aware
/// `None` could never have matched. This binds `compose-tuned-anon-v1` by
/// `actor="local"` and asserts an anonymous-token caller falls through to
/// Tier 3 default weights instead (where `operational_guidance` dominates
/// `formalism`), not the bound profile's inverted seed_priors.
#[tokio::test]
async fn resolve_compose_type_weights_anonymous_caller_does_not_match_explicit_actor_local_binding()
{
    use khive_pack_brain::BrainPack;
    use khive_pack_kg::KgPack;

    // Default RuntimeConfig — no actor_id configured — mints an anonymous
    // token (id="local") via `rt.authorize`, matching an unauthenticated caller.
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(BrainPack::new(rt.clone()));
    builder.register(KnowledgePack::new(rt.clone()));
    let registry = builder.build().expect("kg+brain+knowledge registry builds");

    let token = rt.authorize(Namespace::local()).expect("authorize local");
    assert!(
        token.actor().is_anonymous(),
        "test setup: token must carry the anonymous actor"
    );
    assert_eq!(
        token.actor().id,
        "local",
        "test setup: anonymous actor id must be \"local\" to exercise the collision"
    );

    registry
        .dispatch(
            "brain.create_profile",
            json!({
                "name": "compose-tuned-anon-v1",
                "consumer_kind": "knowledge_compose",
                "seed_priors": {
                    "section_posteriors": {
                        "formalism": {"alpha": 8.0, "beta": 1.0},
                        "operational_guidance": {"alpha": 1.0, "beta": 8.0}
                    }
                }
            }),
        )
        .await
        .expect("create_profile with skewed seed_priors");

    registry
        .dispatch(
            "brain.activate",
            json!({"profile_id": "compose-tuned-anon-v1"}),
        )
        .await
        .expect("activate compose-tuned-anon-v1");

    // Bind explicitly to actor="local" — the exact id anonymous tokens carry.
    registry
        .dispatch(
            "brain.bind",
            json!({
                "actor": "local",
                "profile_id": "compose-tuned-anon-v1",
                "consumer_kind": "knowledge_compose"
            }),
        )
        .await
        .expect("bind compose-tuned-anon-v1 for actor=local");

    let pack = KnowledgePack::new(rt.clone());
    let weights = pack.resolve_compose_type_weights(&registry, &token).await;

    let formalism_w = *weights
        .get("formalism")
        .expect("formalism weight must be present");
    let og_w = *weights
        .get("operational_guidance")
        .expect("operational_guidance weight must be present");

    assert!(
        og_w > formalism_w,
        "anonymous caller must NOT match the actor=\"local\" binding: og {og_w:.4} \
             must exceed formalism {formalism_w:.4} (default-prior ordering, Tier 3), \
             not the bound profile's inverted seed_priors (formalism β(8,1) >> og β(1,8))"
    );
}
