#[tokio::test]
async fn compose_rejects_missing_ids() {
    let f = pack(rt());
    let err = f
        .dispatch("knowledge.compose", json!({ "query": "test" }))
        .await
        .expect_err("compose with no ids should fail");
    assert!(
        matches!(err, khive_runtime::RuntimeError::InvalidInput(_)),
        "expected InvalidInput, got: {err:?}"
    );
}

#[tokio::test]
async fn compose_rejects_empty_query() {
    let f = pack(rt());
    let err = f
        .dispatch(
            "knowledge.compose",
            json!({ "atom_ids": ["some-atom"], "query": "" }),
        )
        .await
        .expect_err("empty query should fail");
    assert!(
        matches!(err, khive_runtime::RuntimeError::InvalidInput(_)),
        "expected InvalidInput, got: {err:?}"
    );
}

#[tokio::test]
async fn suggest_returns_empty_when_no_domains_present() {
    let f = pack(rt());
    // Empty corpus: no domains upserted. suggest should succeed with an empty results array.
    let resp = f
        .dispatch(
            "knowledge.suggest",
            json!({ "query": "anything related to general knowledge retrieval methods" }),
        )
        .await
        .expect("suggest on empty corpus must not crash");
    let results = resp["results"].as_array().expect("results array");
    assert!(
        results.is_empty(),
        "no domains in corpus → empty results, got: {results:?}"
    );
}

#[tokio::test]
async fn suggest_honors_limit_param() {
    let f = pack(rt());

    f.dispatch(
        "knowledge.upsert_domains",
        json!({
            "domains": [
                { "slug": "domain-one", "name": "Domain One", "description": "first domain about retrieval — covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering" },
                { "slug": "domain-two", "name": "Domain Two", "description": "second domain about search — covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering" },
                { "slug": "domain-three", "name": "Domain Three", "description": "third domain about indexing — covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering" },
            ]
        }),
    )
    .await
    .expect("upsert domains");

    let resp = f
        .dispatch(
            "knowledge.suggest",
            json!({ "query": "domain retrieval search indexing methods and techniques", "limit": 1 }),
        )
        .await
        .expect("suggest with limit=1");

    let results = resp["results"].as_array().expect("results array");
    // All 3 seeded domains match the FTS phrase "domain"; suggest truncates to
    // exactly `limit` via hits.truncate(limit) before returning.
    assert_eq!(
        results.len(),
        1,
        "limit=1 with 3 matching domains must return exactly 1 result, got: {}",
        results.len()
    );
}

#[tokio::test]
async fn compose_accepts_mix_of_domain_ids_and_atom_ids() {
    let f = pack(rt());

    // Atom directly referenced by atom_ids.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                { "slug": "direct-atom", "name": "Direct Atom", "content": "directly specified atom content dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" },
                { "slug": "member-atom", "name": "Member Atom", "content": "member atom from domain dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" },
            ]
        }),
    )
    .await
    .expect("upsert atoms");

    // Domain whose member provides member-atom.
    f.dispatch(
        "knowledge.upsert_domains",
        json!({
            "domains": [
                { "slug": "mix-domain", "name": "Mix Domain", "description": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "members": ["member-atom"] }
            ]
        }),
    )
    .await
    .expect("upsert domain");

    let domain_resp = f
        .dispatch("knowledge.get", json!({ "id": "mix-domain" }))
        .await
        .expect("get domain");
    let domain_id = domain_resp["id"].as_str().expect("domain id");

    let resp = f
        .dispatch(
            "knowledge.compose",
            json!({
                "domain_ids": [domain_id],
                "atom_ids": ["direct-atom"],
                "query": "content"
            }),
        )
        .await
        .expect("compose with mix of domain_ids and atom_ids");

    let atoms = resp["data"]["atoms"].as_array().expect("atoms array");
    assert_eq!(
        atoms.len(),
        2,
        "compose with 1 domain member + 1 direct atom should yield 2 atoms (deduped), got: {atoms:?}"
    );
    let count = resp["data"]["count"].as_u64().expect("count");
    assert_eq!(count, 2);
}

// ── compose slim output (explain flag) ───────────────────────────────────────

#[tokio::test]
async fn compose_default_omits_sections_and_score_annotations() {
    let f = pack(rt());

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                {
                    "slug": "slim-atom-a",
                    "name": "Slim Atom A",
                    "content": "retrieval augmented generation dense sparse corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                }
            ]
        }),
    )
    .await
    .expect("upsert atom");

    let resp = f
        .dispatch(
            "knowledge.compose",
            json!({
                "atom_ids": ["slim-atom-a"],
                "query": "retrieval augmented generation"
            }),
        )
        .await
        .expect("compose default ok");

    let data = &resp["data"];
    assert!(
        data.get("sections").is_none(),
        "sections must be absent in default mode"
    );
    assert!(
        data.get("section_count").is_none(),
        "section_count must be absent in default mode"
    );

    let md = data["markdown"].as_str().expect("markdown");
    assert!(
        !md.contains("(score:"),
        "markdown must not contain (score: in default mode"
    );
    assert!(
        !md.contains("Score:"),
        "markdown must not contain Score: in default mode"
    );

    let atoms = data["atoms"].as_array().expect("atoms array");
    assert!(!atoms.is_empty(), "atoms must be present");
    let score_val = atoms[0]["score"].as_f64().expect("score is a number");
    let rendered = format!("{}", atoms[0]["score"]);
    let decimal_len = rendered
        .find('.')
        .map(|dot| rendered.len() - dot - 1)
        .unwrap_or(0);
    assert!(
        decimal_len <= 4,
        "atom score must serialize with at most 4 decimal places, got: {rendered}"
    );
    let _ = score_val;
}

#[tokio::test]
async fn compose_explain_true_atom_path_includes_score_in_markdown() {
    // This test uses a no-embedder runtime (rt()). Without an embedder,
    // embed_query() returns None, so section_results is always empty and
    // compose falls through to the atom-path markdown branch. The sole
    // assertion here is that explain=true causes "Score:" to appear in the
    // atom-path output. The section path (sections[] + breakdown + section_count
    // + "(score:") is exercised by the embedder-backed test in fixes.rs:
    // compose_explain_sections::compose_explain_true_section_path_is_exercised.
    let f = pack(rt());

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                {
                    "slug": "explain-atom-b",
                    "name": "Explain Atom B",
                    "content": "retrieval augmented generation combines dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                }
            ]
        }),
    )
    .await
    .expect("upsert atom");

    let resp = f
        .dispatch(
            "knowledge.compose",
            json!({
                "atom_ids": ["explain-atom-b"],
                "query": "retrieval augmented generation dense sparse",
                "explain": true
            }),
        )
        .await
        .expect("compose explain ok");

    let data = &resp["data"];
    let md = data["markdown"].as_str().expect("markdown");

    // Without an embedder, sections are never emitted — the atom-path branch
    // runs and renders "Score: X.XXXX" per atom when explain=true.
    assert!(
        data.get("sections").is_none(),
        "no-embedder runtime must not emit sections key"
    );
    assert!(
        md.contains("Score:"),
        "atom-path markdown must contain 'Score:' when explain=true, got: {md}"
    );
}
