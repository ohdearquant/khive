// ── ADR-007 Rev 6: episodic actor-namespace routing ─────────────────────────────────

/// Build a registry whose dispatch tokens carry the given actor_id.
///
/// This is the test seam for ADR-007 Rev 6: `VerbRegistryBuilder::with_actor_id`
/// configures the actor that `dispatch()` uses when minting the NamespaceToken
/// passed to each handler. `mint_authorized` is `pub(crate)` to khive-runtime, so
/// tests in other crates must go through this builder path.
fn make_registry_with_actor(rt: KhiveRuntime, actor_id: &str) -> khive_runtime::VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some(actor_id.to_string()));
    builder.register(KgPack::new(rt.clone()));
    builder.register(MemoryPack::new(
        receipt_credentials::with_receipt_credentials(rt),
    ));
    builder.build().expect("registry with actor builds")
}

/// ADR-007 Rev 6: episodic memory → stamped with actor namespace.
///
/// A registry configured with actor_id="alice" writes an episodic memory.
/// The note's stored namespace must be "alice".
#[tokio::test]
async fn test_episodic_remember_stamps_actor_namespace() {
    let rt = make_runtime();
    let registry = make_registry_with_actor(rt.clone(), "alice");

    let result = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "alice episodic event for namespace routing test",
                "memory_type": "episodic",
            }),
        )
        .await
        .expect("memory.remember must succeed");

    let note_id = result["id"].as_str().expect("response has id").to_owned();

    // Fetch the note by ID (by-ID get is namespace-agnostic per PR-A1).
    let got = registry
        .dispatch("get", json!({ "id": note_id }))
        .await
        .expect("get by UUID must succeed");

    let stored_ns = got["namespace"].as_str().expect("note has namespace field");
    assert_eq!(
        stored_ns, "alice",
        "ADR-007 Rev 6: episodic memory with actor_id=alice must be stamped in namespace 'alice'; \
         got: {stored_ns:?}"
    );
}

/// Semantic memory without `namespace=` lands in the caller's actor namespace.
///
/// With actor_id="alice", a semantic memory is stored in "alice", the same as an
/// episodic one, and carries no `attribution` flag.
#[tokio::test]
async fn test_semantic_remember_stamps_actor_namespace() {
    let rt = make_runtime();
    let registry = make_registry_with_actor(rt.clone(), "alice");

    let result = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "alice semantic fact for namespace routing test",
                "memory_type": "semantic",
            }),
        )
        .await
        .expect("memory.remember must succeed");

    let note_id = result["id"].as_str().expect("response has id").to_owned();

    let got = registry
        .dispatch("get", json!({ "id": note_id }))
        .await
        .expect("get by UUID must succeed");

    let stored_ns = got["namespace"].as_str().expect("note has namespace field");
    assert_eq!(
        stored_ns, "alice",
        "a semantic memory without namespace= must land in the caller's actor namespace; \
         got: {stored_ns:?}"
    );
    assert!(
        result.get("attribution").is_none(),
        "an attributed write must not carry an attribution key: {result}"
    );
    assert!(
        got["properties"].get("attribution").is_none(),
        "an attributed write must not store an attribution key: {got}"
    );
}

/// An explicit `namespace="local"` still wins over the actor default for a semantic write.
#[tokio::test]
async fn test_semantic_remember_explicit_local_namespace_wins() {
    let rt = make_runtime();
    let registry = make_registry_with_actor(rt.clone(), "alice");

    let result = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "alice semantic fact written to the shared namespace on purpose",
                "memory_type": "semantic",
                "namespace": "local",
            }),
        )
        .await
        .expect("memory.remember must succeed");
    let note_id = result["id"].as_str().expect("response has id").to_owned();

    let got = registry
        .dispatch("get", json!({ "id": note_id }))
        .await
        .expect("get by UUID must succeed");
    assert_eq!(got["namespace"], "local");
    assert!(result.get("attribution").is_none());
}

/// An anonymous caller has no actor namespace: both memory types stay in `local` and are
/// flagged as unresolved in the response and in the stored properties.
#[tokio::test]
async fn test_anonymous_remember_is_flagged_unresolved() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    for memory_type in ["semantic", "episodic"] {
        let result = registry
            .dispatch(
                "memory.remember",
                json!({
                    "content": format!("anonymous {memory_type} memory without a namespace"),
                    "memory_type": memory_type,
                }),
            )
            .await
            .expect("memory.remember must succeed");
        assert_eq!(
            result["attribution"], "unresolved",
            "{memory_type}: {result}"
        );
        let note_id = result["id"].as_str().expect("response has id").to_owned();

        let got = registry
            .dispatch("get", json!({ "id": note_id }))
            .await
            .expect("get by UUID must succeed");
        assert_eq!(got["namespace"], "local", "{memory_type}: {got}");
        assert_eq!(
            got["properties"]["attribution"], "unresolved",
            "{memory_type}: {got}"
        );
    }
}

/// An attributed caller writes both memory types to its actor namespace and the row
/// carries no `attribution` key, in the response or in the stored properties.
#[tokio::test]
async fn test_attributed_remember_carries_no_attribution_key() {
    let rt = make_runtime();
    let registry = make_registry_with_actor(rt.clone(), "alice");

    for memory_type in ["semantic", "episodic"] {
        let result = registry
            .dispatch(
                "memory.remember",
                json!({
                    "content": format!("alice {memory_type} memory with an attributed author"),
                    "memory_type": memory_type,
                }),
            )
            .await
            .expect("memory.remember must succeed");
        assert!(
            result.get("attribution").is_none(),
            "{memory_type}: {result}"
        );
        let note_id = result["id"].as_str().expect("response has id").to_owned();

        let got = registry
            .dispatch("get", json!({ "id": note_id }))
            .await
            .expect("get by UUID must succeed");
        assert_eq!(got["namespace"], "alice", "{memory_type}: {got}");
        assert!(
            got["properties"].get("attribution").is_none(),
            "{memory_type}: {got}"
        );
    }
}

/// ADR-007 Rev 6: explicit namespace= override takes precedence over routing rules.
///
/// Passing `namespace="override-ns"` with an episodic memory for actor "alice"
/// must stamp the note in "override-ns", not "alice".
///
/// Note: override correctness is provided at the dispatch layer — the Rule-3 escape
/// in pack.rs (~line 886) mints the override namespace into the token BEFORE the
/// handler runs. This test confirms the end-to-end override behavior as seen by
/// callers, not the handler's own override branch in isolation.
#[tokio::test]
async fn test_remember_namespace_override_takes_precedence() {
    let rt = make_runtime();
    let registry = make_registry_with_actor(rt.clone(), "alice");

    let result = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "explicit override namespace test",
                "memory_type": "episodic",
                "namespace": "override-ns",
            }),
        )
        .await
        .expect("memory.remember with explicit namespace must succeed");

    let note_id = result["id"].as_str().expect("response has id").to_owned();

    let got = registry
        .dispatch("get", json!({ "id": note_id }))
        .await
        .expect("get by UUID must succeed");

    let stored_ns = got["namespace"].as_str().expect("note has namespace field");
    assert_eq!(
        stored_ns, "override-ns",
        "ADR-007 Rev 6: explicit namespace= must override actor routing; \
         got: {stored_ns:?}"
    );
}

/// ADR-007 Rev 6: anonymous actor (actor_id="local") episodic → namespace "local".
///
/// When no actor is configured (`ActorRef::anonymous()` has id="local"), episodic
/// memories must land in "local" — backward-compatible with pre-Rev-6 behavior.
#[tokio::test]
async fn test_episodic_anonymous_actor_uses_local() {
    // make_registry uses the default builder (no actor_id) → ActorRef::anonymous() → id="local".
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    let result = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "anonymous episodic memory backward-compat test",
                "memory_type": "episodic",
            }),
        )
        .await
        .expect("memory.remember must succeed");

    let note_id = result["id"].as_str().expect("response has id").to_owned();

    let got = registry
        .dispatch("get", json!({ "id": note_id }))
        .await
        .expect("get by UUID must succeed");

    let stored_ns = got["namespace"].as_str().expect("note has namespace field");
    assert_eq!(
        stored_ns, "local",
        "ADR-007 Rev 6: anonymous actor (id='local') episodic memory must land in 'local' \
         (backward-compat no-op); got: {stored_ns:?}"
    );
}

/// ADR-007 Rev 6 (R1): cross-actor isolation — episodic write lands in actor ns AND is
/// invisible to a principal whose visible set excludes that actor namespace.
///
/// This is the discriminating test for the Rev-6 carve-out:
///   (a) Alice's episodic memory writes to the "alice" namespace and is recallable by alice.
///   (b) A `{local}`-only principal (anonymous make_registry) CANNOT recall alice's memory.
///
/// The vector leg is seeded with a ConstVecProvider but — because all stored
/// memories share the same constant embedding vector — it cannot discriminate
/// between alice's memory and any other.  The genuine cross-actor gate is the FTS
/// namespace fanout; the vector leg is structurally present but not the discriminating
/// signal (noted honestly here per the brief).  The FTS isolation IS the RED→GREEN
/// discriminator validated by the red-run procedure.
///
/// Note: recall as alice requires alice's registry to have `with_visible_namespaces([alice_ns])`
/// because pack.rs dispatch (default path) fans reads over {local} ∪ visible_namespaces only.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn adr007_rev6_episodic_cross_actor_isolation() {
    const MODEL_A: &str = "rev6-isolation-enc";
    const DIMS: usize = 4;

    // Build a runtime. Both alice and the anonymous observer share the same in-memory DB,
    // which is the only way to verify cross-actor isolation within a single test.
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: vec![],
        visible_namespaces: vec![Namespace::parse("alice").unwrap()],
        ..RuntimeConfig::default()
    })
    .expect("in-memory runtime");
    // Register a ConstVecProvider so the vector leg is structurally live.
    // NOTE: ConstVecProvider returns the same constant vector for every input, so the
    // vector leg cannot discriminate between principals — the cross-actor isolation
    // enforced here is via the FTS namespace fanout.  The vector leg is present but
    // not a meaningful signal for this test.
    rt.register_embedder(ConstVecProvider::new(MODEL_A, DIMS, 0.9));

    // Alice's registry: actor_id="alice" + alice in visible_namespaces so recall fans out there.
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("alice".to_string()));
    builder.with_visible_namespaces(vec![Namespace::parse("alice").unwrap()]);
    builder.register(KgPack::new(rt.clone()));
    builder.register(MemoryPack::new(
        receipt_credentials::with_receipt_credentials(rt.clone()),
    ));
    let alice_registry = builder.build().expect("alice registry builds");

    // Anonymous (bob-equivalent) registry: no actor_id, no alice in visible set →
    // only {local} is in scope for reads.
    let anon_registry = make_registry(rt.clone());

    // Step (a): alice writes an episodic memory (no explicit namespace= arg).
    let result = alice_registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "alice private episodic cross-actor isolation marker",
                "memory_type": "episodic",
            }),
        )
        .await
        .expect("memory.remember as alice must succeed");
    let note_id = result["id"].as_str().expect("note id present").to_owned();
    assert!(!note_id.is_empty());

    // Confirm the write landed in the "alice" namespace (not "local").
    let got = alice_registry
        .dispatch("get", json!({ "id": note_id }))
        .await
        .expect("get by UUID must succeed");
    let stored_ns = got["namespace"].as_str().expect("note has namespace field");
    assert_eq!(
        stored_ns, "alice",
        "ADR-007 Rev 6: episodic memory must land in actor namespace 'alice'; got: {stored_ns:?}"
    );

    // Step (a-recall): alice can recall her own episodic memory.
    let alice_recall = alice_registry
        .dispatch(
            "memory.recall",
            json!({ "query": "alice private episodic cross-actor isolation marker" }),
        )
        .await
        .expect("memory.recall as alice must succeed");
    let alice_hits = alice_recall.as_array().expect("recall returns array");
    let alice_ids: Vec<&str> = alice_hits
        .iter()
        .map(|h| h["id"].as_str().unwrap_or(""))
        .collect();
    assert!(
        alice_ids.contains(&note_id.as_str()),
        "ADR-007 Rev 6: alice must recall her own episodic memory; got: {alice_ids:?}"
    );

    // Step (b): the anonymous (local-only) principal MUST NOT recall alice's episodic memory.
    // This is the cross-actor isolation guarantee — the "alice" namespace is outside the
    // anonymous visible set {local}, so the memory must be absent from recall results.
    let anon_recall = anon_registry
        .dispatch(
            "memory.recall",
            json!({ "query": "alice private episodic cross-actor isolation marker" }),
        )
        .await
        .expect("memory.recall as anonymous must succeed");
    let anon_hits = anon_recall.as_array().expect("recall returns array");
    let anon_ids: Vec<&str> = anon_hits
        .iter()
        .map(|h| h["id"].as_str().unwrap_or(""))
        .collect();
    assert!(
        !anon_ids.contains(&note_id.as_str()),
        "ADR-007 Rev 6: anonymous principal (visible={{local}}) must NOT see alice's episodic \
         memory (in namespace 'alice'); isolation FAILED — got: {anon_ids:?}"
    );
}
