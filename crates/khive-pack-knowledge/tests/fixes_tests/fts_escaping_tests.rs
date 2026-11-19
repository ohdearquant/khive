// ── FTS5 MATCH escaping regression ───────────────────────────────────────────

#[tokio::test]
async fn fts_query_special_characters_do_not_crash() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [{
                "slug": "tenant-isolation",
                "name": "Tenant Isolation",
                "content": "multi-tenant isolation and Bob's data separation dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
            }]
        }),
    )
    .await
    .expect("seed atom");

    for query in ["multi-tenant isolation", "Bob's tenant"] {
        let _resp = f
            .dispatch(
                "knowledge.search",
                json!({ "query": query, "rerank": false }),
            )
            .await
            .expect("search should not crash on FTS5 special characters");
    }
}

// Full FTS5 operator regression matrix
#[tokio::test]
async fn fts_operator_matrix_does_not_crash() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [{
                "slug": "fts-matrix-anchor",
                "name": "FTS Matrix Anchor",
                "content": "tenant isolation operator regression matrix anchor dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
            }]
        }),
    )
    .await
    .expect("seed atom");

    // Invariant: no panic + status == "ok". Empty or non-empty results both accepted.
    let cases: &[(&str, &str)] = &[
        // Double-quoted phrases — embedded quotes escaped by quote_fts5_phrase.
        ("double-quoted phrase", "\"tenant isolation\""),
        ("double-quoted embedded", "Bob \"quoted\" tenant"),
        // Boolean operators — treated as user text inside phrase-quoted FTS5 MATCH.
        ("boolean AND", "tenant AND isolation"),
        ("boolean OR", "tenant OR isolation"),
        ("boolean NOT", "tenant NOT isolation"),
        // NEAR — must not reach FTS5 as unsafe operator syntax.
        ("NEAR operator", "tenant NEAR(isolation, 5)"),
        // Wildcard * — must not cause FTS5 syntax errors.
        ("wildcard word", "tenant*"),
        ("wildcard only", "***"),
        // Colon : — must not produce `no such column`.
        ("colon selector", "tenant:isolation"),
        // Caret ^ — must be stripped before MATCH.
        ("caret", "tenant ^ isolation"),
        // Parentheses — must not reach FTS5 as grouping operators.
        ("parentheses", "(tenant isolation)"),
        // Mixed special chars.
        ("mixed special", "(\"+_~!\")"),
        ("mixed colon star caret", "tenant:foo^bar*"),
        // Original regression cases (preserved for history).
        ("hyphenated", "multi-tenant isolation"),
        ("apostrophe", "Bob's tenant"),
    ];

    for (label, query) in cases {
        let resp = f
            .dispatch(
                "knowledge.search",
                json!({ "query": query, "rerank": false }),
            )
            .await
            .unwrap_or_else(|err| {
                panic!("#570 query {label} {query:?} must not crash FTS5: {err}")
            });
        assert!(
            resp["results"].is_array(),
            "#570 query {label} {query:?} must return results array, got: {resp:?}"
        );
    }
}
