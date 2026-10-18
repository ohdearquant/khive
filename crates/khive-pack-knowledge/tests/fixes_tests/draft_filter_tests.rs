// ── #78: draft exclusion + include_drafts opt-in ─────────────────────────────

#[tokio::test]
async fn issue78_search_excludes_drafts_by_default() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                {
                    "slug": "reviewed-atom-78",
                    "name": "Reviewed Atom 78",
                    "content": "transformer attention mechanism self-attention multi-head unique zz78 covering concepts techniques algorithms implementations applications use cases and design patterns in detail for production systems"
                },
                {
                    "slug": "draft-atom-78",
                    "name": "Draft Atom 78",
                    "content": "transformer attention mechanism self-attention multi-head unique zz78 covering concepts techniques algorithms implementations applications use cases and design patterns in detail for production systems"
                },
            ]
        }),
    )
    .await
    .expect("upsert");

    f.sql_exec(
        "UPDATE knowledge_atoms SET status='reviewed' WHERE slug=?1",
        vec![SqlValue::Text("reviewed-atom-78".into())],
    )
    .await;
    f.sql_exec(
        "UPDATE knowledge_atoms SET status='draft' WHERE slug=?1",
        vec![SqlValue::Text("draft-atom-78".into())],
    )
    .await;

    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "transformer attention zz78", "rerank": false }),
        )
        .await
        .expect("search ok");
    let results = resp["results"].as_array().expect("results");

    let names: Vec<&str> = results.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(
        names.contains(&"Reviewed Atom 78"),
        "reviewed atom must appear by default: {names:?}"
    );
    assert!(
        !names.contains(&"Draft Atom 78"),
        "draft atom must be excluded by default: {names:?}"
    );
}

#[tokio::test]
async fn issue78_include_drafts_true_returns_draft_atoms() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                {
                    "slug": "rev-atom-78b",
                    "name": "Reviewed Atom 78b",
                    "content": "sparse retrieval bm25 inverted index ranking corpus search unique zz78b covering concepts techniques algorithms implementations applications use cases and design patterns for production systems"
                },
                {
                    "slug": "dft-atom-78b",
                    "name": "Draft Atom 78b",
                    "content": "sparse retrieval bm25 inverted index ranking corpus search unique zz78b covering concepts techniques algorithms implementations applications use cases and design patterns for production systems"
                },
            ]
        }),
    )
    .await
    .expect("upsert");

    f.sql_exec(
        "UPDATE knowledge_atoms SET status='reviewed' WHERE slug=?1",
        vec![SqlValue::Text("rev-atom-78b".into())],
    )
    .await;
    f.sql_exec(
        "UPDATE knowledge_atoms SET status='draft' WHERE slug=?1",
        vec![SqlValue::Text("dft-atom-78b".into())],
    )
    .await;

    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "sparse retrieval bm25 zz78b", "rerank": false, "include_drafts": true }),
        )
        .await
        .expect("search ok");
    let results = resp["results"].as_array().expect("results");

    let names: Vec<&str> = results.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(
        names.contains(&"Draft Atom 78b"),
        "draft atom must appear when include_drafts=true: {names:?}"
    );
    assert!(
        names.contains(&"Reviewed Atom 78b"),
        "reviewed atom must also appear when include_drafts=true: {names:?}"
    );
}

#[tokio::test]
async fn issue78_include_drafts_does_not_surface_deprecated() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                {
                    "slug": "rev-atom-78c",
                    "name": "Reviewed Atom 78c",
                    "content": "vector quantization product quantization compression retrieval unique zz78c covering concepts techniques algorithms implementations applications use cases and design patterns for production systems"
                },
                {
                    "slug": "dep-atom-78c",
                    "name": "Deprecated Atom 78c",
                    "content": "vector quantization product quantization compression retrieval unique zz78c covering concepts techniques algorithms implementations applications use cases and design patterns for production systems"
                },
            ]
        }),
    )
    .await
    .expect("upsert");

    f.sql_exec(
        "UPDATE knowledge_atoms SET status='reviewed' WHERE slug=?1",
        vec![SqlValue::Text("rev-atom-78c".into())],
    )
    .await;
    f.sql_exec(
        "UPDATE knowledge_atoms SET status='deprecated' WHERE slug=?1",
        vec![SqlValue::Text("dep-atom-78c".into())],
    )
    .await;

    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "vector quantization zz78c", "rerank": false, "include_drafts": true }),
        )
        .await
        .expect("search ok");
    let results = resp["results"].as_array().expect("results");

    let names: Vec<&str> = results.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(
        names.contains(&"Reviewed Atom 78c"),
        "reviewed atom must appear: {names:?}"
    );
    assert!(
        !names.contains(&"Deprecated Atom 78c"),
        "deprecated atom must not appear even with include_drafts=true: {names:?}"
    );
}

#[tokio::test]
async fn issue78_explicit_status_filter_overrides_include_drafts() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                {
                    "slug": "rev-atom-78d",
                    "name": "Reviewed Atom 78d",
                    "content": "graph neural network node embedding link prediction unique zz78d covering concepts techniques algorithms implementations applications use cases and design patterns for production systems"
                },
                {
                    "slug": "dft-atom-78d",
                    "name": "Draft Atom 78d",
                    "content": "graph neural network node embedding link prediction unique zz78d covering concepts techniques algorithms implementations applications use cases and design patterns for production systems"
                },
            ]
        }),
    )
    .await
    .expect("upsert");

    f.sql_exec(
        "UPDATE knowledge_atoms SET status='reviewed' WHERE slug=?1",
        vec![SqlValue::Text("rev-atom-78d".into())],
    )
    .await;
    f.sql_exec(
        "UPDATE knowledge_atoms SET status='draft' WHERE slug=?1",
        vec![SqlValue::Text("dft-atom-78d".into())],
    )
    .await;

    // Explicit status="draft" returns only draft atoms regardless of include_drafts.
    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "graph neural network zz78d", "rerank": false, "status": "draft" }),
        )
        .await
        .expect("search ok");
    let results = resp["results"].as_array().expect("results");
    let names: Vec<&str> = results.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(
        names.contains(&"Draft Atom 78d"),
        "explicit status=draft must return draft atoms: {names:?}"
    );
    assert!(
        !names.contains(&"Reviewed Atom 78d"),
        "explicit status=draft must not return reviewed atoms: {names:?}"
    );
}

// ── issue78: suggest excludes draft domain atoms by default ──────────────────

#[tokio::test]
async fn issue78_suggest_excludes_draft_domain_atoms_by_default() {
    let f = pack(rt());

    // Seed a reviewed domain atom and a draft domain atom.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                {
                    "slug": "suggest-domain-rev",
                    "name": "Suggest Domain Reviewed",
                    "content": "machine learning transformer architecture attention mechanism neural network deep learning optimization gradient descent backpropagation unique zz78s reviewed domain for suggest test",
                    "tags": ["type:domain"]
                },
                {
                    "slug": "suggest-domain-dft",
                    "name": "Suggest Domain Draft",
                    "content": "machine learning transformer architecture attention mechanism neural network deep learning optimization gradient descent backpropagation unique zz78s draft domain for suggest test",
                    "tags": ["type:domain"]
                },
            ]
        }),
    )
    .await
    .expect("seed domain atoms");

    f.sql_exec(
        "UPDATE knowledge_atoms SET status='reviewed' WHERE slug=?1",
        vec![SqlValue::Text("suggest-domain-rev".into())],
    )
    .await;
    f.sql_exec(
        "UPDATE knowledge_atoms SET status='draft' WHERE slug=?1",
        vec![SqlValue::Text("suggest-domain-dft".into())],
    )
    .await;

    let resp = f
        .dispatch(
            "knowledge.suggest",
            json!({
                "query": "machine learning transformer architecture attention mechanism gradient"
            }),
        )
        .await
        .expect("suggest ok");

    let results = resp["results"].as_array().expect("results");
    let names: Vec<&str> = results.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(
        !names.contains(&"Suggest Domain Draft"),
        "suggest must exclude draft domain atoms by default: {names:?}"
    );
}

#[tokio::test]
async fn search_rerank_false_is_explicit_opt_out() {
    let f = pack(rt_with_default_embedder());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                { "slug": "optout-a", "name": "Opt Out Alpha", "content": "opt out rerank test dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" },
            ]
        }),
    )
    .await
    .expect("seed atom");

    // Explicit rerank=false must succeed and return valid results.
    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "opt out rerank false unique wwwxxx", "rerank": false }),
        )
        .await
        .expect("rerank=false search ok");
    let results = resp["results"].as_array().expect("results");
    for r in results {
        let score = r["score"].as_f64().expect("score");
        assert!(
            (0.0..=1.0).contains(&score),
            "score {score} out of [0,1] with rerank=false"
        );
    }
}

#[tokio::test]
async fn search_default_rerank_decompose_guard_avoids_fts_no_such_column() {
    let f = pack(rt_with_default_embedder());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                { "slug": "decompose-guard", "name": "Decompose Guard", "content": "multi-concept tenant isolation decompose guard dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" },
            ]
        }),
    )
    .await
    .expect("seed atom");

    // Query with operator-like text, default rerank (omitted), decompose=true.
    // Must not produce a 'no such column' FTS error.
    let resp = f
        .dispatch(
            "knowledge.search",
            json!({
                "query": "multi-concept tenant:isolation decompose guard",
                "decompose": true,
            }),
        )
        .await
        .expect("default rerank + decompose must not crash");
    assert!(
        resp["results"].is_array(),
        "expected results array, got: {resp:?}"
    );
}
