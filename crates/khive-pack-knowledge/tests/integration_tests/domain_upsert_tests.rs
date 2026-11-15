// ── upsert_domains ────────────────────────────────────────────────────────────

#[tokio::test]
async fn upsert_domains_creates_and_updates() {
    let f = pack(rt());
    let resp = f
        .dispatch(
            "knowledge.upsert_domains",
            json!({
                "domains": [
                    { "slug": "retrieval", "name": "Retrieval", "description": "Retrieval techniques — covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering concepts techniques", "members": ["rag", "dense-retrieval"] }
                ]
            }),
        )
        .await
        .expect("upsert_domains ok");

    assert_eq!(resp["created"], 1);
    assert_eq!(resp["updated"], 0);

    // Second call — update.
    let resp2 = f
        .dispatch(
            "knowledge.upsert_domains",
            json!({
                "domains": [
                    { "slug": "retrieval", "name": "Retrieval updated", "description": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "members": ["rag", "dense-retrieval", "bm25"] }
                ]
            }),
        )
        .await
        .expect("second upsert_domains ok");

    assert_eq!(resp2["created"], 0);
    assert_eq!(resp2["updated"], 1);

    // get by slug returns updated name.
    let got = f
        .dispatch("knowledge.get", json!({ "id": "retrieval" }))
        .await
        .expect("get domain ok");
    assert_eq!(got["name"], "Retrieval updated");
    assert_eq!(got["kind"], "domain");
    let members = got["members"].as_array().expect("members array");
    assert_eq!(members.len(), 3);
}

#[tokio::test]
async fn upsert_domains_rejects_empty_list() {
    let f = pack(rt());
    let err = f
        .dispatch("knowledge.upsert_domains", json!({ "domains": [] }))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("must not be empty"), "got: {err}");
}

#[tokio::test]
async fn upsert_domains_rejects_atom_slug_collision_without_partial_domain() {
    let f = pack(rt());

    // Seed a normal atom that owns the slug the domain upsert will collide on.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{
            "slug": "shared-slug",
            "name": "Original Atom Name",
            "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity",
            "tags": ["distinctive-tag"],
        }] }),
    )
    .await
    .expect("seed atom");

    let err = f
        .dispatch(
            "knowledge.upsert_domains",
            json!({ "domains": [{
                "slug": "shared-slug",
                "name": "Colliding Domain",
                "description": "covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering concepts techniques through practical retrieval storage evaluation examples",
            }] }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err.refusal_source(), RuntimeError::InvalidInput(_)),
        "expected InvalidInput, got: {err:?}"
    );

    // No domain row must exist after the rejected upsert (no partial commit).
    let domains = f
        .dispatch("knowledge.list", json!({ "type": "domain" }))
        .await
        .expect("list domains ok");
    let results = domains["results"].as_array().expect("results array");
    assert!(
        results
            .iter()
            .all(|d| d["slug"].as_str() != Some("shared-slug")),
        "no domain with slug 'shared-slug' should exist after rejected collision: {results:?}"
    );

    // Retry — still rejected, and the original atom is untouched.
    let err2 = f
        .dispatch(
            "knowledge.upsert_domains",
            json!({ "domains": [{
                "slug": "shared-slug",
                "name": "Colliding Domain Retry",
                "description": "covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering concepts techniques through practical retrieval storage evaluation examples",
            }] }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err2.refusal_source(), RuntimeError::InvalidInput(_)),
        "expected InvalidInput on retry, got: {err2:?}"
    );

    let atom = f
        .dispatch("knowledge.get", json!({ "id": "shared-slug" }))
        .await
        .expect("original atom still resolvable");
    assert_eq!(atom["kind"], "atom");
    assert_eq!(atom["name"], "Original Atom Name");
    assert_eq!(
        atom["content"],
        "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
    );
    let tags = atom["tags"].as_array().expect("tags array");
    assert!(tags.iter().any(|t| t == "distinctive-tag"));
}

#[tokio::test]
async fn upsert_atoms_rejects_domain_mirror_slug_collision_without_mutation() {
    let f = pack(rt());

    f.dispatch(
        "knowledge.upsert_domains",
        json!({ "domains": [{
            "slug": "retrieval",
            "name": "Retrieval",
            "description": "Dense and sparse retrieval techniques — covering concepts techniques algorithms implementations applications use cases and design patterns in detail —",
        }] }),
    )
    .await
    .expect("upsert domain");

    // A plain upsert_atoms call targeting the domain's mirror slug must be
    // rejected, not silently strip type:domain from the mirror's tags.
    let err = f
        .dispatch(
            "knowledge.upsert_atoms",
            json!({ "atoms": [{
                "slug": "retrieval",
                "name": "Retrieval Atom Overwrite Attempt",
                "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity",
            }] }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err.refusal_source(), RuntimeError::InvalidInput(_)),
        "expected InvalidInput, got: {err:?}"
    );

    // The domain mirror must be untouched: still classified as a domain by
    // direct lookup and by search, with its original name intact.
    let got = f
        .dispatch("knowledge.get", json!({ "id": "retrieval" }))
        .await
        .expect("get must still resolve the domain");
    assert_eq!(got["kind"], "domain");
    assert_eq!(got["name"], "Retrieval");

    let search = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "retrieval", "type": "domain", "rerank": false }),
        )
        .await
        .expect("search ok");
    let results = search["results"].as_array().expect("results array");
    assert!(
        results.iter().any(|r| r["slug"] == "retrieval"),
        "search must still find the domain, not a demoted atom: {results:?}"
    );
}
