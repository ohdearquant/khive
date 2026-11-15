// ── #441: soft-deleted slugs cannot be upserted again ────────────────────────

/// Soft-deleting an atom's slug and then upserting the same slug again must
/// return a clean lifecycle error, never a raw SQLite unique-constraint error.
#[tokio::test]
async fn upsert_atoms_rejects_reuse_of_soft_deleted_slug_cleanly() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "draft-guide", "name": "Draft Guide", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("seed atom");

    f.dispatch("knowledge.delete_atoms", json!({ "ids": ["draft-guide"] }))
        .await
        .expect("soft delete atom");

    let err = f
        .dispatch(
            "knowledge.upsert_atoms",
            json!({ "atoms": [{ "slug": "draft-guide", "name": "Draft Guide Reborn", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err.refusal_source(), RuntimeError::InvalidInput(_)),
        "expected InvalidInput, got: {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("deleted") || msg.contains("previously deleted"),
        "error should explain the slug was previously deleted: {msg}"
    );
    let lower = msg.to_lowercase();
    assert!(
        !lower.contains("unique") && !lower.contains("constraint") && !lower.contains("sqlite"),
        "no raw SQLite unique-constraint wording may leak to the caller: {msg}"
    );
}

/// Soft-deleting a domain (via the generic delete verb, which tombstones both
/// the canonical domain row and its mirror atom) and then upserting the same
/// slug again must return a clean lifecycle error, never a raw SQLite error.
#[tokio::test]
async fn upsert_domains_rejects_reuse_of_soft_deleted_slug_cleanly() {
    let f = pack_via_registry(rt());
    f.dispatch(
        "knowledge.upsert_domains",
        json!({ "domains": [{ "slug": "draft-domain", "name": "Draft Domain", "description": "Draft domain techniques — covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering concepts techniques" }] }),
    )
    .await
    .expect("seed domain");

    let by_slug = f
        .dispatch("knowledge.get", json!({ "id": "draft-domain" }))
        .await
        .expect("get domain before delete");
    let uuid = by_slug["id"].as_str().expect("id string").to_string();

    // Soft-delete via the generic delete verb, not knowledge.delete_atoms.
    f.dispatch("delete", json!({ "id": uuid }))
        .await
        .expect("generic soft delete domain");

    let err = f
        .dispatch(
            "knowledge.upsert_domains",
            json!({ "domains": [{ "slug": "draft-domain", "name": "Draft Domain Reborn", "description": "Draft domain techniques — covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering concepts techniques" }] }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err.refusal_source(), RuntimeError::InvalidInput(_)),
        "expected InvalidInput, got: {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("deleted") || msg.contains("previously deleted"),
        "error should explain the slug was previously deleted: {msg}"
    );
    let lower = msg.to_lowercase();
    assert!(
        !lower.contains("unique")
            && !lower.contains("constraint")
            && !lower.contains("sqlite")
            && !lower.contains("knowledge_domains")
            && !lower.contains("knowledge_atoms"),
        "no raw SQLite unique-constraint wording may leak to the caller: {msg}"
    );

    // The rejected reuse must not resurrect the tombstoned domain.
    let not_found = f
        .dispatch("knowledge.get", json!({ "id": "draft-domain" }))
        .await;
    assert!(
        matches!(not_found, Err(RuntimeError::NotFound(_))),
        "expected NotFound after rejected reuse, got: {not_found:?}"
    );
}
