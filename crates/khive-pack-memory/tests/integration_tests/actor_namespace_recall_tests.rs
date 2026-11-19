/// (a) ADR-007 Rev 4: [actor] id=lambda:khive ⇒ default recall surfaces a lambda:khive note via
/// BOTH the FTS leg (5a) and the vector leg (5b, lexically-disjoint query).
///
/// A `KhiveRuntime` with `visible_namespaces = ["lambda:khive"]` and a registered
/// `ConstVecProvider` (so the vector leg is live) writes a note into `lambda:khive`
/// via the Rule-3 explicit-namespace escape, then asserts:
///   5a — default recall with a matching query returns the note (FTS + vector both reachable).
///   5b — default recall with a lexically-disjoint query (no shared trigrams) still returns the
///        note, isolating the vector leg via the `authorize(ns)` fanout.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn adr007_rev4_default_recall_surfaces_actor_ns_via_both_legs() {
    const MODEL_A: &str = "custom-enc-a";
    const DIMS: usize = 4;

    // Build a runtime with visible_namespaces = ["lambda:khive"] and a registered
    // ConstVecProvider so the vector leg is live (no lattice model needed).
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: vec![],
        visible_namespaces: vec![Namespace::parse("lambda:khive").unwrap()],
        ..RuntimeConfig::default()
    })
    .expect("in-memory runtime");
    rt.register_embedder(ConstVecProvider::new(MODEL_A, DIMS, 0.9));

    // Build the registry with with_visible_namespaces, mirroring tests (b)/(d).
    let mut builder = VerbRegistryBuilder::new();
    builder.with_visible_namespaces(rt.config().visible_namespaces.clone());
    builder.register(KgPack::new(rt.clone()));
    builder.register(MemoryPack::new(
        receipt_credentials::with_receipt_credentials(rt.clone()),
    ));
    let registry = builder.build().expect("registry builds");

    // Step 3: write the note into lambda:khive via the Rule-3 explicit-namespace escape.
    let result = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "alpha beta gamma delta",
                "salience": 0.8,
                "namespace": "lambda:khive"
            }),
        )
        .await
        .expect("memory.remember with explicit namespace must succeed");
    let note_id = result["id"].as_str().expect("note id present").to_owned();
    assert!(!note_id.is_empty());

    // Step 5a: default recall with a query that shares trigrams with the content.
    // Both FTS and vector legs can surface the note here.
    let recall_5a = registry
        .dispatch("memory.recall", json!({ "query": "alpha beta" }))
        .await
        .expect("memory.recall (5a) must succeed");
    let hits_5a = recall_5a.as_array().expect("recall returns array");
    let ids_5a: Vec<&str> = hits_5a
        .iter()
        .map(|h| h["id"].as_str().unwrap_or(""))
        .collect();
    assert!(
        ids_5a.contains(&note_id.as_str()),
        "5a: default recall must surface the lambda:khive note (both legs); got: {ids_5a:?}"
    );

    // Step 5b: default recall with a lexically-disjoint query — no shared letters with
    // "alpha beta gamma delta" (content uses a,l,p,h,b,e,t,g,m,d; query uses j,x,w,v,z,r,f,k)
    // so zero shared trigrams. FTS cannot match; only the vector leg can surface the note
    // via the constant-vector ANN index fanned out through authorize("lambda:khive").
    // This isolates the vector-leg fanout as the sole return path.
    let recall_5b = registry
        .dispatch("memory.recall", json!({ "query": "jxwvz qrpfk" }))
        .await
        .expect("memory.recall (5b) must succeed");
    let hits_5b = recall_5b.as_array().expect("recall returns array");
    let ids_5b: Vec<&str> = hits_5b
        .iter()
        .map(|h| h["id"].as_str().unwrap_or(""))
        .collect();
    assert!(
        ids_5b.contains(&note_id.as_str()),
        "5b: vector-leg fanout via authorize(lambda:khive) must surface the note \
         even with a lexically-disjoint query (no FTS match possible); got: {ids_5b:?}"
    );
}

/// (a2) ADR-007 Rev 4: FTS-leg isolation — with the vector leg disabled (no embedder),
/// default recall surfaces a lambda:khive note via the FTS fanout alone.
///
/// Builds a `KhiveRuntime` with `embedding_model: None` and registers NO embedder,
/// so the vector leg is dead. Asserts `registered_embedding_model_names()` is empty
/// (self-documenting premise). Dispatches `memory.remember` with
/// `namespace="lambda:khive"` (explicit-param write into that namespace), then
/// `memory.recall` with a lexically matching query and no `namespace=` param.
/// The note id must appear — with the vector leg dead, the ONLY path that can
/// surface it is the FTS fanout (`with_namespace` selecting `fts_notes_lambda:khive`).
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn adr007_rev4_default_recall_surfaces_actor_ns_via_fts_leg() {
    // Build a runtime with visible_namespaces = ["lambda:khive"] and NO embedder,
    // so the vector leg is dead.
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: vec![],
        visible_namespaces: vec![Namespace::parse("lambda:khive").unwrap()],
        ..RuntimeConfig::default()
    })
    .expect("in-memory runtime");

    // Assert the vector leg is dead — no embedder registered.
    // This makes the test premise self-documenting: if an embedder were registered,
    // the vector leg could carry the result without FTS, and the test would prove nothing.
    assert!(
        rt.registered_embedding_model_names().is_empty(),
        "FTS-leg isolation premise: no embedder must be registered (vector leg must be dead)"
    );

    // Build the registry with with_visible_namespaces mirroring tests (b)/(d).
    let mut builder = VerbRegistryBuilder::new();
    builder.with_visible_namespaces(rt.config().visible_namespaces.clone());
    builder.register(KgPack::new(rt.clone()));
    builder.register(MemoryPack::new(
        receipt_credentials::with_receipt_credentials(rt.clone()),
    ));
    let registry = builder.build().expect("registry builds");

    // Write the note into lambda:khive via the Rule-3 explicit-namespace escape.
    let result = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "alpha beta gamma delta",
                "salience": 0.8,
                "namespace": "lambda:khive"
            }),
        )
        .await
        .expect("memory.remember with explicit namespace must succeed");
    let note_id = result["id"].as_str().expect("note id present").to_owned();
    assert!(!note_id.is_empty());

    // Default recall with a lexically matching query and no namespace= param.
    // With the vector leg dead, the ONLY path that can surface the note is the
    // FTS fanout (with_namespace selecting fts_notes_lambda:khive). This isolates
    // the FTS leg as the sole return path.
    let recall = registry
        .dispatch("memory.recall", json!({ "query": "alpha beta" }))
        .await
        .expect("memory.recall must succeed");
    let hits = recall.as_array().expect("recall returns array");
    let ids: Vec<&str> = hits
        .iter()
        .map(|h| h["id"].as_str().unwrap_or(""))
        .collect();
    assert!(
        ids.contains(&note_id.as_str()),
        "FTS-leg: default recall must surface the lambda:khive note via FTS fanout alone \
         (vector leg is dead — no embedder registered); got: {ids:?}"
    );
}
