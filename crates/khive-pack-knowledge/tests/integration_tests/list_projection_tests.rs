// ── knowledge.list ────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_atoms_returns_all_atoms() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                { "slug": "a1", "name": "Alpha", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" },
                { "slug": "a2", "name": "Beta", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" },
                { "slug": "a3", "name": "Gamma", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" },
            ]
        }),
    )
    .await
    .expect("upsert");

    let resp = f
        .dispatch("knowledge.list", json!({ "type": "atom" }))
        .await
        .expect("list ok");

    let results = resp["results"].as_array().expect("results array");
    assert_eq!(results.len(), 3);
    assert_eq!(resp["total"], 3);
}

#[tokio::test]
async fn list_domains_returns_only_domains() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "a1", "name": "Alpha", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert atom");
    f.dispatch(
        "knowledge.upsert_domains",
        json!({ "domains": [{ "slug": "d1", "name": "Domain1", "description": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert domain");

    let resp = f
        .dispatch("knowledge.list", json!({ "type": "domain" }))
        .await
        .expect("list domains ok");

    let results = resp["results"].as_array().expect("results array");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["kind"], "domain");
}

#[tokio::test]
async fn list_respects_limit_and_offset() {
    let f = pack(rt());
    for i in 0..10 {
        f.dispatch(
            "knowledge.upsert_atoms",
            json!({ "atoms": [{ "slug": format!("a{i}"), "name": format!("Atom{i}"), "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
        )
        .await
        .expect("upsert");
    }

    let page1 = f
        .dispatch("knowledge.list", json!({ "limit": 3, "offset": 0 }))
        .await
        .expect("page1 ok");
    let page2 = f
        .dispatch("knowledge.list", json!({ "limit": 3, "offset": 3 }))
        .await
        .expect("page2 ok");

    let r1 = page1["results"].as_array().expect("r1");
    let r2 = page2["results"].as_array().expect("r2");
    assert_eq!(r1.len(), 3, "page1 should have 3 items");
    assert_eq!(r2.len(), 3, "page2 should have 3 items");
    assert_eq!(page1["total"], 10);
    // IDs on page1 and page2 should not overlap.
    let ids1: std::collections::HashSet<&str> =
        r1.iter().filter_map(|v| v["id"].as_str()).collect();
    let ids2: std::collections::HashSet<&str> =
        r2.iter().filter_map(|v| v["id"].as_str()).collect();
    assert!(
        ids1.is_disjoint(&ids2),
        "page1 and page2 ids must not overlap"
    );
}

#[tokio::test]
async fn list_fields_projects_atom_and_domain_keys_only() {
    let f = pack(rt());
    let large_content = format!(
        "{} {}",
        "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity",
        "payload".repeat(32_000)
    );
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{
            "slug": "projected-atom",
            "name": "Projected Atom",
            "content": large_content,
        }] }),
    )
    .await
    .expect("upsert projected atom");
    f.dispatch(
        "knowledge.upsert_domains",
        json!({ "domains": [{
            "slug": "projected-domain",
            "name": "Projected Domain",
            "description": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity",
        }] }),
    )
    .await
    .expect("upsert projected domain");

    for kind in ["atom", "domain"] {
        let response = f
            .dispatch(
                "knowledge.list",
                json!({
                    "type": kind,
                    "fields": ["id", "slug"],
                    "after": "",
                    "limit": 10,
                }),
            )
            .await
            .expect("projected cursor list");
        let results = response["results"].as_array().expect("results array");
        assert_eq!(results.len(), 1, "one {kind} result");
        let object = results[0].as_object().expect("projected object");
        let keys: std::collections::HashSet<&str> = object.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            std::collections::HashSet::from(["id", "slug"]),
            "projection must return exactly the requested fields"
        );
        assert_eq!(response["order"], "created_at_asc_id_asc");
        assert!(response["next_after"].is_null());
        assert!(
            response.get("total").is_none(),
            "a cursor page never counts the namespace"
        );
        assert!(
            serde_json::to_vec(&response)
                .expect("serialize response")
                .len()
                < 1_024,
            "key-only response must not carry the large atom content"
        );
    }
}

#[tokio::test]
async fn list_projection_and_cursor_inputs_are_strict() {
    let f = pack(rt());

    let empty_fields = f
        .dispatch("knowledge.list", json!({ "fields": [] }))
        .await
        .expect_err("empty fields must fail")
        .to_string();
    assert!(empty_fields.contains("fields") && empty_fields.contains("non-empty"));

    let unknown_field = f
        .dispatch(
            "knowledge.list",
            json!({ "type": "atom", "fields": ["slug", "description"] }),
        )
        .await
        .expect_err("atom-only projection vocabulary must be enforced")
        .to_string();
    assert!(unknown_field.contains("description") && unknown_field.contains("atom"));

    let mixed_modes = f
        .dispatch("knowledge.list", json!({ "after": "", "offset": 0 }))
        .await
        .expect_err("offset and cursor modes must be exclusive")
        .to_string();
    assert!(mixed_modes.contains("mutually exclusive"));

    let prefix_cursor = f
        .dispatch("knowledge.list", json!({ "after": "deadbeef" }))
        .await
        .expect_err("cursor prefixes must fail")
        .to_string();
    assert!(prefix_cursor.contains("full UUID"));

    let missing_cursor = f
        .dispatch(
            "knowledge.list",
            json!({ "after": "ffffffff-ffff-4fff-bfff-ffffffffffff" }),
        )
        .await
        .expect_err("missing cursor rows must fail")
        .to_string();
    assert!(missing_cursor.contains("cursor") && missing_cursor.contains("not found"));
}
