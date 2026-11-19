#[tokio::test]
async fn compose_returns_markdown_for_domain() {
    let f = pack(rt());

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                { "slug": "atom-a", "name": "Atom A", "content": "content of atom a dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }
            ]
        }),
    )
    .await
    .expect("upsert atom");

    f.dispatch(
        "knowledge.upsert_domains",
        json!({
            "domains": [
                {
                    "slug": "test-domain",
                    "name": "Test Domain", "description": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity",
                    "members": ["atom-a"]
                }
            ]
        }),
    )
    .await
    .expect("upsert domain");

    let domain_resp = f
        .dispatch("knowledge.get", json!({ "id": "test-domain" }))
        .await
        .expect("get domain");
    let domain_id = domain_resp["id"].as_str().expect("domain id");

    let resp = f
        .dispatch(
            "knowledge.compose",
            json!({
                "domain_ids": [domain_id],
                "query": "content"
            }),
        )
        .await
        .expect("compose from domain ok");

    let atoms = resp["data"]["atoms"].as_array().expect("atoms");
    assert!(
        !atoms.is_empty(),
        "compose from domain should include member atoms"
    );
}

#[tokio::test]
async fn compose_omits_deleted_domain_members_and_keeps_live_content() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({"atoms": [
            {
                "slug": "compose-surviving-member",
                "name": "Surviving Member",
                "content": "The surviving atom remains available in the composed briefing after another member of the same domain has been deleted, preserving useful and accurate context for subsequent requests."
            },
            {
                "slug": "compose-deleted-member",
                "name": "Deleted Member",
                "content": "This atom is removed after domain membership is stored, leaving a stale member reference in the domain, while the surviving member should still compose normally afterward."
            }
        ]}),
    )
    .await
    .expect("upsert atoms");
    f.dispatch(
        "knowledge.upsert_domains",
        json!({"domains": [{
            "slug": "compose-mixed-domain",
            "name": "Mixed Domain",
            "description": "A domain with one live atom and one member that is later deleted, used to verify composed briefings retain surviving content while reporting stale membership accurately.",
            "members": ["compose-surviving-member", "compose-deleted-member"]
        }]}),
    )
    .await
    .expect("upsert domain");
    f.dispatch(
        "knowledge.delete_atoms",
        json!({"ids": ["compose-deleted-member"]}),
    )
    .await
    .expect("delete member atom");

    let response = f
        .dispatch(
            "knowledge.compose",
            json!({"query": "surviving member", "domain_ids": ["compose-mixed-domain"]}),
        )
        .await
        .expect("stale domain membership must not abort compose");
    assert_eq!(response["status"], "ok", "got: {response}");
    assert_eq!(response["data"]["count"], 1, "got: {response}");
    assert_eq!(
        response["data"]["atoms"][0]["slug"],
        "compose-surviving-member"
    );
    assert!(
        response["data"]["markdown"]
            .as_str()
            .expect("briefing markdown")
            .contains("The surviving atom remains available"),
        "live content must be rendered: {response}"
    );
    assert_eq!(
        response["data"]["omissions"],
        json!(["compose-deleted-member"]),
        "stale domain member reference must be reported: {response}"
    );

    let explicit_missing = f
        .dispatch(
            "knowledge.compose",
            json!({"query": "deleted member", "atom_ids": ["compose-deleted-member"]}),
        )
        .await;
    assert!(
        matches!(
            &explicit_missing,
            Err(khive_runtime::RuntimeError::NotFound(_))
        ),
        "an explicitly requested missing atom must still fail: {explicit_missing:?}"
    );
}

/// PR #816: `knowledge.compose` accepts compact hex prefixes
/// for domains but must normalize them (`hex_prefix_to_uuid_pattern`) before
/// binding the `LIKE` pattern — a >8-char compact prefix could not match the
/// hyphenated `id` column before the fix.
#[tokio::test]
async fn compose_resolves_domain_by_compact_hex_prefix_over_8_chars() {
    let f = pack(rt());

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                { "slug": "atom-a", "name": "Atom A", "content": "content of atom a dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }
            ]
        }),
    )
    .await
    .expect("upsert atom");

    f.dispatch(
        "knowledge.upsert_domains",
        json!({
            "domains": [
                {
                    "slug": "test-domain",
                    "name": "Test Domain", "description": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity",
                    "members": ["atom-a"]
                }
            ]
        }),
    )
    .await
    .expect("upsert domain");

    let domain_resp = f
        .dispatch("knowledge.get", json!({ "id": "test-domain" }))
        .await
        .expect("get domain");
    let domain_id = domain_resp["id"].as_str().expect("domain id").to_string();
    let compact = domain_id.replace('-', "");
    let prefix = &compact[..16];

    let resp = f
        .dispatch(
            "knowledge.compose",
            json!({
                "domain_ids": [prefix],
                "query": "content"
            }),
        )
        .await
        .expect("compose from compact domain prefix must resolve");

    let atoms = resp["data"]["atoms"].as_array().expect("atoms");
    assert!(
        !atoms.is_empty(),
        "compose from compact domain prefix should include member atoms"
    );
}

/// PR #816: same normalization requirement for atom ids.
#[tokio::test]
async fn compose_resolves_atom_by_compact_hex_prefix_over_8_chars() {
    let f = pack(rt());

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                {
                    "slug": "rag-overview",
                    "name": "RAG Overview",
                    "content": "Retrieval-augmented generation combines retrieval with generation. dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                }
            ]
        }),
    )
    .await
    .expect("upsert atom");

    let atom_resp = f
        .dispatch("knowledge.get", json!({ "id": "rag-overview" }))
        .await
        .expect("get atom");
    let atom_id = atom_resp["id"].as_str().expect("atom id").to_string();
    let compact = atom_id.replace('-', "");
    let prefix = &compact[..16];

    let resp = f
        .dispatch(
            "knowledge.compose",
            json!({
                "atom_ids": [prefix],
                "query": "retrieval augmented generation"
            }),
        )
        .await
        .expect("compose from compact atom prefix must resolve");

    let atoms = resp["data"]["atoms"].as_array().expect("atoms");
    assert_eq!(atoms.len(), 1, "expected exactly 1 atom");
    assert_eq!(
        atoms[0]["slug"].as_str(),
        Some("rag-overview"),
        "compact atom prefix must resolve to the correct atom"
    );
}

/// PR #816 (precedence): an all-hex slug must still win over
/// prefix interpretation. Atom B's slug is deliberately set to the same
/// hex string as the first 16 chars of atom A's compact id — a value that
/// is *also* a syntactically valid compact prefix. Because `compose` tries
/// an exact slug match before falling back to prefix scanning, this must
/// resolve atom B (by slug), never atom A (by prefix collision).
#[tokio::test]
async fn compose_atom_all_hex_slug_wins_over_prefix_interpretation() {
    let f = pack(rt());

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                { "slug": "atom-x", "name": "Atom X", "content": "atom x content dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }
            ]
        }),
    )
    .await
    .expect("upsert atom x");

    let atom_x_resp = f
        .dispatch("knowledge.get", json!({ "id": "atom-x" }))
        .await
        .expect("get atom x");
    let atom_x_id = atom_x_resp["id"].as_str().expect("atom x id").to_string();
    let hex_slug = atom_x_id.replace('-', "")[..16].to_string();

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                { "slug": hex_slug.clone(), "name": "Atom Y", "content": "atom y content dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }
            ]
        }),
    )
    .await
    .expect("upsert atom y with all-hex slug");

    let resp = f
        .dispatch(
            "knowledge.compose",
            json!({
                "atom_ids": [hex_slug.clone()],
                "query": "atom content"
            }),
        )
        .await
        .expect("compose by all-hex slug must resolve");

    let atoms = resp["data"]["atoms"].as_array().expect("atoms");
    assert_eq!(atoms.len(), 1, "expected exactly 1 atom");
    assert_eq!(
        atoms[0]["slug"].as_str(),
        Some(hex_slug.as_str()),
        "all-hex slug must resolve to the atom with that slug, not a prefix collision"
    );
    assert_eq!(
        atoms[0]["name"].as_str(),
        Some("Atom Y"),
        "must resolve atom Y by slug, never atom X by prefix interpretation"
    );
}
