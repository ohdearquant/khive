// ── knowledge.get ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn get_returns_atom_by_slug() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "lora", "name": "LoRA", "content": "Low-Rank Adaptation — covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering concepts techniques" }] }),
    )
    .await
    .expect("upsert");

    let got = f
        .dispatch("knowledge.get", json!({ "id": "lora" }))
        .await
        .expect("get ok");

    assert_eq!(got["slug"], "lora");
    assert_eq!(got["name"], "LoRA");
    assert_eq!(got["kind"], "atom");
}

#[tokio::test]
async fn get_returns_not_found_for_unknown_slug() {
    let f = pack(rt());
    let err = f
        .dispatch("knowledge.get", json!({ "id": "nonexistent-slug-xyz" }))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("not found") || err.to_string().contains("NotFound"),
        "expected not-found error, got: {err}"
    );
}

#[tokio::test]
async fn get_by_domain_uuid_returns_canonical_domain_not_mirror_atom() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_domains",
        json!({
            "domains": [
                { "slug": "uuid-domain", "name": "UUID Domain", "description": "Retrieval techniques — covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering concepts techniques", "members": ["rag"] }
            ]
        }),
    )
    .await
    .expect("upsert_domains ok");

    // Resolve the domain's UUID via the slug path (already correct).
    let by_slug = f
        .dispatch("knowledge.get", json!({ "id": "uuid-domain" }))
        .await
        .expect("get by slug ok");
    assert_eq!(by_slug["kind"], "domain");
    let uuid = by_slug["id"].as_str().expect("id string").to_string();

    // The UUID path must agree with the slug path: canonical domain, not the mirror atom.
    let by_uuid = f
        .dispatch("knowledge.get", json!({ "id": uuid }))
        .await
        .expect("get by uuid ok");
    assert_eq!(
        by_uuid["kind"], "domain",
        "UUID lookup must return the canonical domain, got: {by_uuid}"
    );
    let members = by_uuid["members"]
        .as_array()
        .expect("members must be present and an array");
    assert_eq!(members.len(), 1);
    assert_eq!(members[0], "rag");
}

#[tokio::test]
async fn get_resolves_atom_by_compact_prefix_longer_than_eight_chars() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{
            "slug": "prefix-atom",
            "name": "Prefix Atom",
            "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
        }] }),
    )
    .await
    .expect("upsert atom");

    let by_slug = f
        .dispatch("knowledge.get", json!({ "id": "prefix-atom" }))
        .await
        .expect("get atom by slug");
    let full_id = by_slug["id"].as_str().expect("full atom id");
    let compact: String = full_id.chars().filter(|ch| *ch != '-').take(12).collect();

    let by_prefix = f
        .dispatch("knowledge.get", json!({ "id": compact }))
        .await
        .expect("get atom by unique compact prefix");
    assert_eq!(by_prefix["id"], full_id);
    assert_eq!(by_prefix["kind"], "atom");

    let compact_uuid = full_id.replace('-', "");
    let by_compact_uuid = f
        .dispatch("knowledge.get", json!({ "id": compact_uuid }))
        .await
        .expect("32-character compact UUID is a complete identifier");
    assert_eq!(by_compact_uuid["id"], full_id);
}

#[tokio::test]
async fn get_exact_all_hex_slug_wins_over_uuid_prefix_collision() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{
            "slug": "hex-prefix-source",
            "name": "Hex Prefix Source",
            "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
        }] }),
    )
    .await
    .expect("upsert prefix source");

    let source = f
        .dispatch("knowledge.get", json!({ "id": "hex-prefix-source" }))
        .await
        .expect("get prefix source");
    let source_id = source["id"].as_str().expect("source id").to_string();
    let hex_slug = source_id.replace('-', "")[..16].to_string();

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{
            "slug": hex_slug.clone(),
            "name": "Exact Hex Slug",
            "content": "exact hexadecimal slug retrieval must precede compact identifier prefix interpretation across knowledge corpus reads and preserve deterministic registered slug addressing"
        }] }),
    )
    .await
    .expect("upsert exact all-hex slug");

    let by_slug = f
        .dispatch("knowledge.get", json!({ "id": hex_slug.clone() }))
        .await
        .expect("exact all-hex slug must win over UUID prefix collision");
    assert_eq!(by_slug["slug"], hex_slug);
    assert_eq!(by_slug["name"], "Exact Hex Slug");
    assert_ne!(
        by_slug["id"], source_id,
        "prefix interpretation must not return the colliding source record"
    );
}

#[tokio::test]
async fn get_exact_all_hex_slug_wins_when_uuid_prefix_matches_nothing() {
    const HEX_SLUG: &str = "fffffffffffffffffffffffffffffffff";
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{
            "slug": HEX_SLUG,
            "name": "Overlong Hex Slug",
            "content": "overlong hexadecimal slug lookup remains exact and addressable even though no canonical UUID can match a prefix longer than thirty two hexadecimal characters"
        }] }),
    )
    .await
    .expect("upsert overlong all-hex slug");

    let by_slug = f
        .dispatch("knowledge.get", json!({ "id": HEX_SLUG }))
        .await
        .expect("exact all-hex slug must resolve before a guaranteed prefix miss");
    assert_eq!(by_slug["slug"], HEX_SLUG);
    assert_eq!(by_slug["name"], "Overlong Hex Slug");
}

#[tokio::test]
async fn get_domain_prefix_deduplicates_same_uuid_mirror_atom() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_domains",
        json!({ "domains": [{
            "slug": "prefix-domain",
            "name": "Prefix Domain",
            "description": "Retrieval concepts techniques algorithms implementations applications use cases and design patterns in sufficient detail for this deterministic domain prefix regression fixture"
        }] }),
    )
    .await
    .expect("upsert domain");

    let by_slug = f
        .dispatch("knowledge.get", json!({ "id": "prefix-domain" }))
        .await
        .expect("get domain by slug");
    let full_id = by_slug["id"].as_str().expect("full domain id");
    let prefix = &full_id[..8];

    let by_prefix = f
        .dispatch("knowledge.get", json!({ "id": prefix }))
        .await
        .expect("domain and mirror atom must count as one prefix match");
    assert_eq!(by_prefix["id"], full_id);
    assert_eq!(by_prefix["kind"], "domain");
}

#[tokio::test]
async fn get_rejects_ambiguous_prefix_across_distinct_knowledge_records() {
    let runtime = rt();
    let f = pack(runtime.clone());
    let mut writer = runtime.sql().writer().await.expect("knowledge writer");
    writer
        .execute_batch(vec![
            SqlStatement {
                sql: "INSERT INTO knowledge_atoms \
                      (id, namespace, slug, name, content, created_at, updated_at) \
                      VALUES (?1, 'local', 'ambiguous-a', 'Ambiguous A', 'content a', 1, 1)"
                    .into(),
                params: vec![SqlValue::Text(
                    "deadbeef-0000-4000-8000-000000000001".into(),
                )],
                label: Some("test.knowledge_get.ambiguous_a".into()),
            },
            SqlStatement {
                sql: "INSERT INTO knowledge_atoms \
                      (id, namespace, slug, name, content, created_at, updated_at) \
                      VALUES (?1, 'local', 'ambiguous-b', 'Ambiguous B', 'content b', 2, 2)"
                    .into(),
                params: vec![SqlValue::Text(
                    "deadbeef-0000-4000-8000-000000000002".into(),
                )],
                label: Some("test.knowledge_get.ambiguous_b".into()),
            },
        ])
        .await
        .expect("seed colliding prefixes");
    drop(writer);

    let err = f
        .dispatch("knowledge.get", json!({ "id": "deadbeef" }))
        .await
        .expect_err("distinct UUIDs sharing a prefix must be ambiguous");
    let RuntimeError::AmbiguousPrefix { prefix, matches } = err else {
        panic!("expected AmbiguousPrefix, got {err:?}");
    };
    assert_eq!(prefix, "deadbeef");
    assert_eq!(matches.len(), 2);
}
