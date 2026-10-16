// ── upsert_atoms ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn upsert_atoms_creates_new_atoms() {
    let f = pack(rt());
    let resp = f
        .dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [
                    { "slug": "rag", "name": "RAG", "content": "RAG retrieves relevant passages before generating. dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "tags": ["retrieval", "rag"] },
                    { "slug": "lora", "name": "LoRA", "content": "Low-Rank Adaptation of LLMs — covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering", "tags": ["fine-tuning", "adapter"] },
                    { "slug": "flash-attention", "name": "FlashAttention", "content": "Memory-efficient attention — covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering concepts techniques", "tags": ["attention"] },
                ]
            }),
        )
        .await
        .expect("upsert_atoms ok");

    assert_eq!(resp["created"], 3, "expected 3 created");
    assert_eq!(resp["updated"], 0, "expected 0 updated");
    assert_eq!(resp["total"], 3);
}

#[tokio::test]
async fn upsert_atoms_updates_on_second_call() {
    let f = pack(rt());
    // First insert.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "rag", "name": "RAG", "content": "original content dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("first upsert");

    // Second call with same slug — should update.
    let resp = f
        .dispatch(
            "knowledge.upsert_atoms",
            json!({ "atoms": [{ "slug": "rag", "name": "RAG updated", "content": "updated content dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
        )
        .await
        .expect("second upsert");

    assert_eq!(resp["created"], 0, "expected 0 created on second call");
    assert_eq!(resp["updated"], 1, "expected 1 updated");

    // Verify get returns the updated name.
    let got = f
        .dispatch("knowledge.get", json!({ "id": "rag" }))
        .await
        .expect("get ok");
    assert_eq!(got["name"], "RAG updated");
    assert_eq!(got["slug"], "rag");
}

#[tokio::test]
async fn upsert_atoms_rejects_empty_list() {
    let f = pack(rt());
    let err = f
        .dispatch("knowledge.upsert_atoms", json!({ "atoms": [] }))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("must not be empty"), "got: {err}");
}

#[tokio::test]
async fn upsert_atoms_rejects_empty_slug() {
    let f = pack(rt());
    let err = f
        .dispatch(
            "knowledge.upsert_atoms",
            json!({ "atoms": [{ "slug": "  ", "name": "Bad", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("slug"), "got: {err}");
}

#[tokio::test]
async fn upsert_atoms_validation_failure_does_not_commit_valid_prefix() {
    let f = pack(rt());
    let err = f
        .dispatch(
            "knowledge.upsert_atoms",
            json!({ "atoms": [
                {
                    "slug": "valid-prefix",
                    "name": "Valid Prefix",
                    "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                },
                {
                    "slug": "invalid-suffix",
                    "name": "Invalid Suffix",
                    "content": "too short"
                }
            ] }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("20 words"), "got: {err}");

    let persisted = f
        .dispatch("knowledge.get", json!({ "id": "valid-prefix" }))
        .await;
    assert!(
        persisted.is_err(),
        "validation failure must not commit an earlier valid atom: {persisted:?}"
    );
}

#[tokio::test]
async fn upsert_atoms_domain_collision_does_not_commit_valid_prefix() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_domains",
        json!({ "domains": [{
            "slug": "protected-domain",
            "name": "Protected Domain",
            "description": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
        }] }),
    )
    .await
    .expect("seed domain");

    let err = f
        .dispatch(
            "knowledge.upsert_atoms",
            json!({ "atoms": [
                {
                    "slug": "domain-prefix",
                    "name": "Domain Prefix",
                    "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                },
                {
                    "slug": "protected-domain",
                    "name": "Collision",
                    "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                }
            ] }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("domain mirror"), "got: {err}");
    assert!(
        f.dispatch("knowledge.get", json!({ "id": "domain-prefix" }))
            .await
            .is_err(),
        "domain collision must not commit an earlier valid atom"
    );
}

#[tokio::test]
async fn upsert_atoms_tombstone_collision_does_not_commit_valid_prefix() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{
            "slug": "retired-atom",
            "name": "Retired Atom",
            "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
        }] }),
    )
    .await
    .expect("seed atom");
    f.dispatch("knowledge.delete_atoms", json!({ "ids": ["retired-atom"] }))
        .await
        .expect("delete atom");

    let err = f
        .dispatch(
            "knowledge.upsert_atoms",
            json!({ "atoms": [
                {
                    "slug": "tombstone-prefix",
                    "name": "Tombstone Prefix",
                    "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                },
                {
                    "slug": "retired-atom",
                    "name": "Retired Atom Again",
                    "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                }
            ] }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("previously deleted"), "got: {err}");
    assert!(
        f.dispatch("knowledge.get", json!({ "id": "tombstone-prefix" }))
            .await
            .is_err(),
        "tombstone collision must not commit an earlier valid atom"
    );
}

#[tokio::test]
async fn upsert_atoms_rejects_reserved_secret_gate_property_key() {
    let f = pack(rt());
    let err = f
        .dispatch(
            "knowledge.upsert_atoms",
            json!({ "atoms": [{
                "slug": "reserved-key-atom",
                "name": "Reserved key atom",
                "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity",
                "properties": { "khive:secret_gate": "exempted:content-sha256-manifest-v1" }
            }] }),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("khive:secret_gate") && err.to_string().contains("runtime-owned"),
        "got: {err}"
    );

    let list = f
        .dispatch("knowledge.list", json!({}))
        .await
        .expect("list ok");
    assert_eq!(
        list["atoms"].as_array().map(|a| a.len()).unwrap_or(0),
        0,
        "rejected atom must not be persisted"
    );
}
