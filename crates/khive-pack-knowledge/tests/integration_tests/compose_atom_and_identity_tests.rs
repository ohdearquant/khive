// ── compose ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn compose_returns_markdown_for_atoms() {
    let f = pack(rt());

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                {
                    "slug": "rag-overview",
                    "name": "RAG Overview",
                    "content": "Retrieval-augmented generation combines retrieval with generation. dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                },
                {
                    "slug": "dense-retrieval",
                    "name": "Dense Retrieval",
                    "content": "Dense retrieval uses vector embeddings to find relevant documents. dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                }
            ]
        }),
    )
    .await
    .expect("upsert atoms");

    let resp = f
        .dispatch(
            "knowledge.compose",
            json!({
                "atom_ids": ["rag-overview", "dense-retrieval"],
                "query": "retrieval augmented generation"
            }),
        )
        .await
        .expect("compose ok");

    let md = resp["data"]["markdown"].as_str().expect("markdown");
    assert!(
        md.contains("Knowledge Briefing"),
        "markdown must have heading"
    );
    let atoms = resp["data"]["atoms"].as_array().expect("atoms array");
    assert_eq!(atoms.len(), 2, "expected 2 atoms in response");
    let count = resp["data"]["count"].as_u64().expect("count");
    assert_eq!(count, 2);
}

/// #3233/#3235: a large first candidate must not stop packing, and the
/// structured atom list/count must describe only what the briefing contains.
#[tokio::test]
async fn compose_skips_large_candidate_and_reports_only_packed_atoms() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                {
                    "slug": "compose-too-large",
                    "name": "Too Large",
                    "content": "oversized ".repeat(400)
                },
                {
                    "slug": "compose-small-a",
                    "name": "Small A",
                    "content": "first small relevant content ".repeat(22)
                },
                {
                    "slug": "compose-small-b",
                    "name": "Small B",
                    "content": "second small relevant content ".repeat(22)
                }
            ]
        }),
    )
    .await
    .expect("seed atoms");

    let response = f
        .dispatch(
            "knowledge.compose",
            json!({
                "atom_ids": ["compose-too-large", "compose-small-a", "compose-small-b"],
                "query": "relevant content",
                "max_tokens": 500,
            }),
        )
        .await
        .expect("compose with bounded output");
    let data = &response["data"];
    let markdown = data["markdown"].as_str().expect("markdown");
    assert!(markdown.len() <= 2_000);
    assert!(!markdown.contains("Too Large"));
    assert!(markdown.contains("Small A"));
    assert!(markdown.contains("Small B"));
    let atoms = data["atoms"].as_array().expect("packed atoms");
    assert_eq!(atoms.len(), 2);
    assert_eq!(data["count"], json!(2));
    assert_eq!(atoms[0]["slug"], json!("compose-small-a"));
    assert_eq!(atoms[1]["slug"], json!("compose-small-b"));
}

/// #1505: the public namespace parameter is an exact compose scope, not a
/// widened visible set. Identical slugs in local and a measurement arm must
/// resolve to the arm's atom only, while an absent parameter preserves the
/// unchanged local default.
#[tokio::test]
async fn compose_namespace_selects_exact_atom_corpus() {
    let f = pack(rt());
    let shared_slug = "namespace-compose-target";

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [{
                "slug": shared_slug,
                "name": "Local Compose Marker",
                "content": "Local corpus marker for namespace composition regression coverage with enough distinct retrieval words to satisfy the atom content validation contract and remain readable in generated markdown output."
            }]
        }),
    )
    .await
    .expect("upsert local atom");
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "namespace": "bench-arm-a",
            "atoms": [{
                "slug": shared_slug,
                "name": "Bench Arm Compose Marker",
                "content": "Bench arm corpus marker for exact namespace composition regression coverage with enough distinct retrieval words to satisfy the atom content validation contract and remain readable in generated markdown output."
            }]
        }),
    )
    .await
    .expect("upsert bench-arm atom");

    let arm = f
        .dispatch(
            "knowledge.compose",
            json!({
                "namespace": "bench-arm-a",
                "atom_ids": [shared_slug],
                "query": "namespace composition marker",
            }),
        )
        .await
        .expect("compose exact bench-arm namespace");
    let arm_atom = &arm["data"]["atoms"][0];
    assert_eq!(arm_atom["name"], json!("Bench Arm Compose Marker"));
    assert!(arm["data"]["markdown"]
        .as_str()
        .expect("arm markdown")
        .contains("Bench arm corpus marker"));
    assert!(
        !arm["data"]["markdown"]
            .as_str()
            .expect("arm markdown")
            .contains("Local corpus marker"),
        "exact arm compose must not blend the same slug from local"
    );

    let local = f
        .dispatch(
            "knowledge.compose",
            json!({
                "atom_ids": [shared_slug],
                "query": "namespace composition marker",
            }),
        )
        .await
        .expect("compose unchanged local default");
    assert_eq!(
        local["data"]["atoms"][0]["name"],
        json!("Local Compose Marker")
    );
}

/// ADR-096/#1505: nested brain.resolve/profile calls must carry the outer
/// request's principal and exact namespace, not the registry's baked daemon
/// identity. The gate denies either nested read under any other actor.
#[tokio::test]
async fn compose_nested_profile_reads_preserve_request_identity() {
    use khive_pack_brain::BrainPack;

    let rt = rt();
    let gate = Arc::new(NestedProfileIdentityGate::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(BrainPack::new(rt.clone()));
    builder.register(KnowledgePack::new(rt.clone()));
    builder.with_gate(gate.clone());
    builder.with_actor_id(Some("daemon".to_string()));
    let registry = builder.build().expect("registry");
    let arm_ns = "bench-arm-a";

    registry
        .dispatch(
            "brain.create_profile",
            json!({
                "namespace": arm_ns,
                "name": "requester-compose-v1",
                "consumer_kind": "knowledge_compose",
            }),
        )
        .await
        .expect("create arm profile");
    registry
        .dispatch(
            "brain.activate",
            json!({
                "namespace": arm_ns,
                "profile_id": "requester-compose-v1",
            }),
        )
        .await
        .expect("activate arm profile");
    registry
        .dispatch(
            "brain.bind",
            json!({
                "namespace": arm_ns,
                "profile_id": "requester-compose-v1",
                "consumer_kind": "knowledge_compose",
            }),
        )
        .await
        .expect("bind arm profile");
    registry
        .dispatch(
            "knowledge.upsert_atoms",
            json!({
                "namespace": arm_ns,
                "atoms": [{
                    "slug": "requester-compose-atom",
                    "name": "Requester Compose Atom",
                    "content": "Per-request identity propagation for nested profile reads with enough retrieval corpus words to satisfy validation and produce a deterministic briefing.",
                }]
            }),
        )
        .await
        .expect("upsert arm atom");
    gate.requests.lock().expect("gate request lock").clear();

    registry
        .dispatch_with_identity(
            "knowledge.compose",
            json!({
                "namespace": arm_ns,
                "atom_ids": ["requester-compose-atom"],
                "query": "per request identity propagation",
            }),
            Some(RequestIdentity {
                namespace: "local".to_string(),
                actor_id: Some("requester".to_string()),
                visible_namespaces: vec!["local".to_string()],
                process_ref: None,
                request_id: Some(1505),
            }),
        )
        .await
        .expect("compose with request-scoped nested profile reads");

    let requests = gate.requests.lock().expect("gate request lock");
    let nested: Vec<_> = requests
        .iter()
        .filter(|(verb, _, _)| matches!(verb.as_str(), "brain.resolve" | "brain.profile"))
        .collect();
    assert_eq!(
        nested.len(),
        2,
        "bound compose must perform both nested profile reads: {requests:?}"
    );
    assert!(
        nested
            .iter()
            .all(|(_, actor, namespace)| actor == "requester" && namespace == arm_ns),
        "nested Gate checks must preserve requester + exact arm identity: {nested:?}"
    );
}
