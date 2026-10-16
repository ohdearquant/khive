// ── knowledge.get + include_sections ─────────────────────────────────────────

#[tokio::test]
async fn get_include_sections_false_returns_no_sections_key() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "s-atom", "name": "SAtom", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert");
    f.dispatch(
        "knowledge.edit",
        json!({ "id": "s-atom", "sections": [{ "section_type": "overview", "content": "This section describes the overview of LoRA and its applications in fine-tuning large language models with low-rank matrix decompositions." }] }),
    )
    .await
    .expect("edit");

    let got = f
        .dispatch("knowledge.get", json!({ "id": "s-atom" }))
        .await
        .expect("get without sections");

    assert_eq!(got["kind"], "atom");
    assert!(
        got.get("sections").is_none(),
        "sections key must not be present by default"
    );
}

#[tokio::test]
async fn get_include_sections_returns_all_sections_ordered() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "sec-atom", "name": "SecAtom", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert");

    f.dispatch(
        "knowledge.edit",
        json!({
            "id": "sec-atom",
            "sections": [
                { "section_type": "overview", "content": "This is the overview section covering the main ideas and introduction to the topic in sufficient detail for embedding purposes." },
                { "section_type": "formalism", "content": "Formal definitions go here including mathematical notation theorems proofs lemmas and corollaries that describe the system formally." },
                { "section_type": "examples", "content": "Concrete examples illustrate the concepts with worked-through scenarios code samples and practical demonstrations of usage patterns." },
            ]
        }),
    )
    .await
    .expect("edit");

    let got = f
        .dispatch(
            "knowledge.get",
            json!({ "id": "sec-atom", "include_sections": true }),
        )
        .await
        .expect("get with sections");

    assert_eq!(got["kind"], "atom");
    let sections = got["sections"].as_array().expect("sections is array");
    assert_eq!(sections.len(), 3, "expected 3 sections, got: {sections:?}");

    let types: Vec<&str> = sections
        .iter()
        .filter_map(|s| s["section_type"].as_str())
        .collect();
    assert!(types.contains(&"overview"), "missing overview: {types:?}");
    assert!(types.contains(&"formalism"), "missing formalism: {types:?}");
    assert!(types.contains(&"examples"), "missing examples: {types:?}");

    for s in sections {
        assert!(
            s["content"].as_str().is_some_and(|c| !c.is_empty()),
            "section content empty"
        );
        assert!(s["section_type"].as_str().is_some(), "section_type missing");
        assert!(s["sort_order"].as_i64().is_some(), "sort_order missing");
    }
}

#[tokio::test]
async fn edit_same_body_persists_changed_section_type() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{
            "slug": "section-type-refresh",
            "name": "Section Type Refresh",
            "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
        }] }),
    )
    .await
    .expect("upsert atom");

    let body = "The same section body can move from an overview to an example when the author changes its role, while its content-addressed identity remains stable.";
    let first = f
        .dispatch(
            "knowledge.edit",
            json!({ "id": "section-type-refresh", "sections": [{
                "section_type": "overview", "heading": "Shared heading", "content": body
            }] }),
        )
        .await
        .expect("create overview section");
    let second = f
        .dispatch(
            "knowledge.edit",
            json!({ "id": "section-type-refresh", "sections": [{
                "section_type": "examples", "heading": "Shared heading", "content": body
            }] }),
        )
        .await
        .expect("change section type without changing body");

    assert_eq!(first["sections"][0]["id"], second["sections"][0]["id"]);
    assert_eq!(second["sections"][0]["section_type"], "examples");
    let stored = f
        .dispatch(
            "knowledge.get",
            json!({ "id": "section-type-refresh", "include_sections": true }),
        )
        .await
        .expect("read section after edit");
    assert_eq!(stored["sections"][0]["section_type"], "examples");
}

#[tokio::test]
async fn get_include_sections_by_uuid() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "uuid-sec-atom", "name": "UuidSecAtom", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert");
    f.dispatch(
        "knowledge.edit",
        json!({ "id": "uuid-sec-atom", "sections": [{ "section_type": "overview", "content": "This section describes the overview of LoRA and its applications in fine-tuning large language models with low-rank matrix decompositions." }] }),
    )
    .await
    .expect("edit");

    let by_slug = f
        .dispatch(
            "knowledge.get",
            json!({ "id": "uuid-sec-atom", "include_sections": true }),
        )
        .await
        .expect("get by slug");
    let atom_uuid = by_slug["id"].as_str().expect("id").to_owned();

    let by_uuid = f
        .dispatch(
            "knowledge.get",
            json!({ "id": atom_uuid, "include_sections": true }),
        )
        .await
        .expect("get by uuid");

    let sections = by_uuid["sections"].as_array().expect("sections array");
    assert_eq!(sections.len(), 1, "expected 1 section by UUID lookup");
}

#[tokio::test]
async fn get_include_sections_namespace_isolation() {
    let f = pack(rt());

    // ADR-007 Rev 2: all storage routes to local. Two DISTINCT slugs each get
    // their own sections; sections from one atom must not leak to the other.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "iso-atom-a", "name": "NSA", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert atom-a");

    f.dispatch(
        "knowledge.edit",
        json!({ "id": "iso-atom-a", "sections": [{ "section_type": "overview", "content": "This section belongs exclusively to atom A and must not be visible when fetching atom B under any circumstances." }] }),
    )
    .await
    .expect("edit atom-a");

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "iso-atom-b", "name": "NSB", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert atom-b");

    let got_b = f
        .dispatch(
            "knowledge.get",
            json!({ "id": "iso-atom-b", "include_sections": true }),
        )
        .await
        .expect("get atom-b");

    let sections_b = got_b["sections"].as_array().expect("sections array");
    assert_eq!(sections_b.len(), 0, "atom-b must not see atom-a sections");
}

// Regression: two sections sharing the same sort_order must come back in a
// stable, deterministic order (id ASC as final tie-breaker).
#[tokio::test]
async fn get_include_sections_ordering_tie_break_is_stable() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "tie-atom", "name": "TieAtom", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert");

    // Insert two sections with the same sort_order (both default to their
    // SectionType ordinal; explicitly override to the same value to guarantee
    // the tie). Each has distinct content so both rows are inserted.
    f.dispatch(
        "knowledge.edit",
        json!({
            "id": "tie-atom",
            "sections": [
                {
                    "section_type": "overview",
                    "content": "First section content for the tie-break test covering overview of the main topic in sufficient detail for the minimum content length requirement to be satisfied.",
                    "sort_order": 5
                },
                {
                    "section_type": "formalism",
                    "content": "Second section content for the tie-break test covering formal definitions and mathematical notation in sufficient detail for the minimum content length requirement.",
                    "sort_order": 5
                },
            ]
        }),
    )
    .await
    .expect("edit");

    // Fetch twice; both calls must return the same order.
    let first = f
        .dispatch(
            "knowledge.get",
            json!({ "id": "tie-atom", "include_sections": true }),
        )
        .await
        .expect("get first");
    let second = f
        .dispatch(
            "knowledge.get",
            json!({ "id": "tie-atom", "include_sections": true }),
        )
        .await
        .expect("get second");

    let s1 = first["sections"].as_array().expect("sections first");
    let s2 = second["sections"].as_array().expect("sections second");

    assert_eq!(s1.len(), 2, "expected 2 sections on first fetch");
    assert_eq!(s2.len(), 2, "expected 2 sections on second fetch");

    // Both rows share sort_order=5; the id-ASC tie-breaker must produce the
    // same sequence across repeated queries.
    let ids_first: Vec<&str> = s1.iter().filter_map(|s| s["id"].as_str()).collect();
    let ids_second: Vec<&str> = s2.iter().filter_map(|s| s["id"].as_str()).collect();
    assert_eq!(
        ids_first, ids_second,
        "section order must be deterministic across repeated fetches (id ASC tie-breaker)"
    );

    // Pin the full ordering contract (sort_order ASC, created_at ASC, id ASC):
    // repeated-read agreement alone can pass on SQLite insertion-order luck
    // even if the id tie-breaker is removed.
    let actual: Vec<(i64, i64, &str)> = s1
        .iter()
        .map(|s| {
            (
                s["sort_order"].as_i64().expect("sort_order"),
                s["created_at"].as_i64().expect("created_at"),
                s["id"].as_str().expect("id"),
            )
        })
        .collect();
    let mut expected = actual.clone();
    expected.sort();
    assert_eq!(
        actual, expected,
        "sections must be sorted by (sort_order, created_at, id)"
    );

    // Both calls must agree on which section type comes first (also validates
    // that the order is NOT random).
    let types_first: Vec<&str> = s1
        .iter()
        .filter_map(|s| s["section_type"].as_str())
        .collect();
    let types_second: Vec<&str> = s2
        .iter()
        .filter_map(|s| s["section_type"].as_str())
        .collect();
    assert_eq!(types_first, types_second, "section_type order must match");
}
