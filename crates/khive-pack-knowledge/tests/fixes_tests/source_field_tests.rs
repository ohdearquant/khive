// ── #1985: upsert UPDATE path must preserve source_uri/source_type when omitted ──

#[tokio::test]
async fn upsert_omitting_source_fields_preserves_existing_values() {
    let f = pack(rt());

    // Initial insert carries source_uri/source_type.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{
            "slug": "preserve-source-atom",
            "name": "Preserve Source",
            "content": "body dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity",
            "source_uri": "https://example.com/doc",
            "source_type": "import"
        }] }),
    )
    .await
    .expect("insert with source fields");

    // Re-upsert the SAME slug WITHOUT source_uri/source_type: both must be
    // preserved, not silently NULLed (COALESCE(?5, source_uri) /
    // COALESCE(?6, source_type) — regression test for #1985).
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{
            "slug": "preserve-source-atom",
            "name": "Preserve Source V2",
            "content": "body v2 dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
        }] }),
    )
    .await
    .expect("re-upsert without source fields");

    let row = f
        .sql_query_one(
            "SELECT source_uri, source_type, created_at FROM knowledge_atoms WHERE slug=?1",
            vec![SqlValue::Text("preserve-source-atom".into())],
        )
        .await
        .expect("atom row");
    assert_eq!(
        row_text(&row, "source_uri").as_deref(),
        Some("https://example.com/doc"),
        "omitting source_uri on re-upsert must preserve the existing value"
    );
    assert_eq!(
        row_text(&row, "source_type").as_deref(),
        Some("import"),
        "omitting source_type on re-upsert must preserve the existing value"
    );
}

#[tokio::test]
async fn upsert_explicit_source_fields_replace_existing_values() {
    let f = pack(rt());

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{
            "slug": "replace-source-atom",
            "name": "Replace Source",
            "content": "body dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity",
            "source_uri": "https://example.com/old",
            "source_type": "import"
        }] }),
    )
    .await
    .expect("insert with source fields");

    // Explicit new values on re-upsert must still replace the existing ones.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{
            "slug": "replace-source-atom",
            "name": "Replace Source V2",
            "content": "body v2 dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity",
            "source_uri": "https://example.com/new",
            "source_type": "manual"
        }] }),
    )
    .await
    .expect("re-upsert with explicit source fields");

    let row = f
        .sql_query_one(
            "SELECT source_uri, source_type FROM knowledge_atoms WHERE slug=?1",
            vec![SqlValue::Text("replace-source-atom".into())],
        )
        .await
        .expect("atom row");
    assert_eq!(
        row_text(&row, "source_uri").as_deref(),
        Some("https://example.com/new"),
        "explicit source_uri on re-upsert must replace the existing value"
    );
    assert_eq!(
        row_text(&row, "source_type").as_deref(),
        Some("manual"),
        "explicit source_type on re-upsert must replace the existing value"
    );
}

// #1999: nullable atom fields distinguish omission from explicit null.

#[tokio::test]
async fn upsert_explicit_nulls_clear_sources_and_reset_finalized_without_demoting_status() {
    let f = pack(rt());

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{
            "slug": "clear-nullable-atom",
            "name": "Clear Nullable Atom",
            "content": "body dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity",
            "source_uri": "https://example.com/original",
            "source_type": "manual",
            "finalized": true
        }] }),
    )
    .await
    .expect("insert attributed finalized atom");

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{
            "slug": "clear-nullable-atom",
            "name": "Clear Nullable Atom V2",
            "content": "body updated dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity",
            "source_uri": null,
            "source_type": null,
            "finalized": null
        }] }),
    )
    .await
    .expect("clear nullable fields");

    let atom = f
        .dispatch("knowledge.get", json!({ "id": "clear-nullable-atom" }))
        .await
        .expect("read cleared atom");
    assert!(
        atom["source_uri"].is_null(),
        "explicit source_uri=null must clear the stored URI: {atom}"
    );
    assert!(
        atom["source_type"].is_null(),
        "explicit source_type=null must clear the stored type: {atom}"
    );
    assert_eq!(
        atom["finalized"],
        json!(false),
        "finalized=null resets the non-null flag to false"
    );
    assert_eq!(
        atom["status"],
        json!("reviewed"),
        "clearing finalization must not demote an independent lifecycle status"
    );

    // A later patch that omits the nullable fields must preserve the explicit clears.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{
            "slug": "clear-nullable-atom",
            "name": "Clear Nullable Atom V3",
            "content": "body updated again dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
        }] }),
    )
    .await
    .expect("re-upsert with nullable fields omitted");
    let atom = f
        .dispatch("knowledge.get", json!({ "id": "clear-nullable-atom" }))
        .await
        .expect("read atom after omitted patch");
    assert!(atom["source_uri"].is_null());
    assert!(atom["source_type"].is_null());
    assert_eq!(atom["finalized"], json!(false));
    assert_eq!(atom["status"], json!("reviewed"));
}

#[tokio::test]
async fn upsert_explicit_nulls_on_insert_use_storage_defaults() {
    let f = pack(rt());

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{
            "slug": "null-default-atom",
            "name": "Null Default Atom",
            "content": "body dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity",
            "source_uri": null,
            "source_type": null,
            "finalized": null
        }] }),
    )
    .await
    .expect("insert atom with explicit nulls");

    let atom = f
        .dispatch("knowledge.get", json!({ "id": "null-default-atom" }))
        .await
        .expect("read inserted atom");
    assert!(atom["source_uri"].is_null());
    assert!(atom["source_type"].is_null());
    assert_eq!(atom["finalized"], json!(false));
    assert_eq!(atom["status"], json!("draft"));
}
