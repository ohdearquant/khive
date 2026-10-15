// ── W1 + D1: is_domain hydration ──────────────────────────────────────────────

#[tokio::test]
async fn w1_atom_with_type_domain_tag_returns_kind_domain_in_search() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [{
                "slug": "retrieval-domain",
                "name": "Retrieval Domain",
                "tags": ["type:domain", "retrieval"],
                "content": "retrieval domain techniques xyzabc organization dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity",
                "finalized": true
            }]
        }),
    )
    .await
    .expect("upsert");

    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "retrieval domain techniques xyzabc", "rerank": false }),
        )
        .await
        .expect("search ok");
    let results = resp["results"].as_array().expect("results");
    let hit = results
        .iter()
        .find(|r| r["name"].as_str() == Some("Retrieval Domain"))
        .expect("Retrieval Domain should appear in results");
    assert_eq!(
        hit["kind"].as_str().unwrap_or(""),
        "domain",
        "atom with type:domain tag must have kind=domain in search results"
    );
}

#[tokio::test]
async fn d1_upserted_domain_returns_kind_domain_in_domain_search() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_domains",
        json!({
            "domains": [{
                "slug": "ml-techniques",
                "name": "ML Techniques",
                "description": "machine learning techniques domain organization — covering concepts techniques algorithms implementations applications use cases and design patterns in detail —"
            }]
        }),
    )
    .await
    .expect("upsert domain");

    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "machine learning techniques domain", "type": "domain", "rerank": false }),
        )
        .await
        .expect("search ok");
    let results = resp["results"].as_array().expect("results");
    assert!(
        !results.is_empty(),
        "domain search should return the upserted domain"
    );
    for r in results {
        assert_eq!(
            r["kind"].as_str().unwrap_or(""),
            "domain",
            "all results in type=domain search must have kind=domain: {r}"
        );
    }
}

// ── W8: content-addressed section upsert (dedup by content_hash) ──────────────

#[tokio::test]
async fn w8_reimport_identical_section_content_is_idempotent() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "edit-atom", "name": "Edit Atom", "content": "original dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert");

    let content = "Overview content long enough to satisfy the 80-character minimum section length requirement. dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index";

    // Create the section via edit, then mark it verified out-of-band.
    f.dispatch(
        "knowledge.edit",
        json!({ "id": "edit-atom", "sections": [{ "section_type": "overview", "content": content }] }),
    )
    .await
    .expect("edit ok");
    f.sql_exec(
        "UPDATE knowledge_sections SET status='verified' WHERE section_type='overview'",
        vec![],
    )
    .await;

    // Re-edit with byte-identical content: idempotent, no new row, status preserved.
    f.dispatch(
        "knowledge.edit",
        json!({ "id": "edit-atom", "sections": [{ "section_type": "overview", "content": content }] }),
    )
    .await
    .expect("edit ok");

    let count = f
        .sql_query_one(
            "SELECT COUNT(*) AS n FROM knowledge_sections WHERE section_type='overview'",
            vec![],
        )
        .await
        .expect("count row");
    assert_eq!(
        row_i64(&count, "n"),
        Some(1),
        "identical content must not create a sibling row"
    );

    let status = f
        .sql_query_one(
            "SELECT status FROM knowledge_sections WHERE section_type='overview'",
            vec![],
        )
        .await
        .expect("status row");
    assert_eq!(
        row_text(&status, "status").as_deref(),
        Some("verified"),
        "re-importing identical content must not downgrade verification"
    );
}

#[tokio::test]
async fn w8_edit_distinct_content_same_type_creates_sibling() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "edit-atom2", "name": "Edit Atom 2", "content": "original dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert");

    let first = "First overview block long enough to satisfy the 80-character minimum section length requirement. dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector";
    let second = "Second overview block, distinct content, also long enough to satisfy the 80-character minimum. examples formalism boundary conditions operational guidance failure modes expert lens references other";

    f.dispatch(
        "knowledge.edit",
        json!({ "id": "edit-atom2", "sections": [{ "section_type": "overview", "content": first }] }),
    )
    .await
    .expect("edit ok");
    f.sql_exec(
        "UPDATE knowledge_sections SET status='verified' WHERE section_type='overview'",
        vec![],
    )
    .await;

    // Distinct content under the same section_type must insert a sibling row,
    // not overwrite the existing (verified) one.
    f.dispatch(
        "knowledge.edit",
        json!({ "id": "edit-atom2", "sections": [{ "section_type": "overview", "content": second }] }),
    )
    .await
    .expect("edit ok");

    let total = f
        .sql_query_one(
            "SELECT COUNT(*) AS n FROM knowledge_sections WHERE section_type='overview'",
            vec![],
        )
        .await
        .expect("count row");
    assert_eq!(
        row_i64(&total, "n"),
        Some(2),
        "distinct same-type content must coexist as sibling rows"
    );

    let verified = f
        .sql_query_one(
            "SELECT COUNT(*) AS n FROM knowledge_sections WHERE section_type='overview' AND status='verified'",
            vec![],
        )
        .await
        .expect("verified count row");
    assert_eq!(
        row_i64(&verified, "n"),
        Some(1),
        "inserting a sibling must not disturb an existing verified section"
    );
}
