use super::*;
use khive_runtime::Namespace;

async fn create_document(rt: &KhiveRuntime, token: &NamespaceToken, source_uri: &str) -> Uuid {
    rt.create_entity_with_embedding_report(
        token,
        "document",
        None,
        source_uri,
        None,
        Some(json!({ "source_uri": source_uri })),
        vec![],
    )
    .await
    .map(|(row, _report)| row)
    .unwrap()
    .id
}

#[tokio::test]
async fn path_with_like_wildcards_resolves_only_itself() {
    let rt = KhiveRuntime::memory().unwrap();
    let token = rt.authorize(Namespace::local()).unwrap();

    // Neither document's `source_uri` matches `path` exactly, so
    // resolution must fall through to the suffix-`LIKE` scan. Under the
    // pre-fix unescaped pattern, `%` matches zero-or-more chars and `_`
    // matches exactly one char, so this decoy (`100` + "" + "Q" +
    // `done.rs`) would incorrectly satisfy `LIKE '%src/100%_done.rs'`.
    // With `%`/`_` escaped, the pattern requires the literal substring
    // `100%_done.rs` and the decoy no longer matches.
    let path = "src/100%_done.rs";
    let decoy_source_uri = "prefix/src/100Qdone.rs";
    create_document(&rt, &token, decoy_source_uri).await;

    let resolved = find_document_for_path(&rt, &token, path).await.unwrap();
    assert_eq!(
        resolved, None,
        "a % or _ in the path must be matched literally, not as a LIKE wildcard"
    );
}

#[tokio::test]
async fn exact_match_wins_over_wildcard_broadened_candidate() {
    let rt = KhiveRuntime::memory().unwrap();
    let token = rt.authorize(Namespace::local()).unwrap();

    let path = "crates/khive-pack-git/src/ingest.rs";
    let broadened_suffix_path = "other/crates/khive-pack-git/src/ingest.rs";
    // Created first so an unordered `LIMIT 1` scan without exact-match
    // priority would be free to return it instead of the exact match.
    create_document(&rt, &token, broadened_suffix_path).await;
    let exact_id = create_document(&rt, &token, path).await;

    let resolved = find_document_for_path(&rt, &token, path).await.unwrap();
    assert_eq!(
        resolved,
        Some(exact_id),
        "an exact source_uri match must always win over a suffix-LIKE candidate"
    );
}

/// PR #816: TOCTOU regression — single-query snapshot must still rank
/// the exact match first regardless of insertion order.
#[tokio::test]
async fn single_query_snapshot_prefers_exact_over_broadened() {
    let rt = KhiveRuntime::memory().unwrap();
    let token = rt.authorize(Namespace::local()).unwrap();

    let path = "crates/khive-pack-git/src/toctou.rs";
    let broadened_suffix_path = "other/crates/khive-pack-git/src/toctou.rs";
    let exact_id = create_document(&rt, &token, path).await;
    create_document(&rt, &token, broadened_suffix_path).await;

    let resolved = find_document_for_path(&rt, &token, path).await.unwrap();
    assert_eq!(
        resolved,
        Some(exact_id),
        "a single query covering both exact and broadened candidates \
             must still rank the exact match first, regardless of insertion order"
    );
}

async fn create_named_document(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    name: &str,
    properties: Option<Value>,
) -> Uuid {
    rt.create_entity_with_embedding_report(token, "document", None, name, None, properties, vec![])
        .await
        .map(|(row, _report)| row)
        .unwrap()
        .id
}

#[tokio::test]
async fn basename_fallback_matches_only_documents_without_source_uri() {
    let rt = KhiveRuntime::memory().unwrap();
    let token = rt.authorize(Namespace::local()).unwrap();
    let first_path = "crates/a/README.md";
    let second_path = "crates/b/README.md";
    let first = create_named_document(
        &rt,
        &token,
        "README.md",
        Some(json!({ "source_uri": first_path })),
    )
    .await;
    let second = create_named_document(
        &rt,
        &token,
        "README.md",
        Some(json!({ "source_uri": second_path })),
    )
    .await;

    // Query both rows: one exact target necessarily has the larger ID, so
    // basename ties cannot pass merely because of random UUID ordering.
    for (path, expected) in [(first_path, first), (second_path, second)] {
        assert_eq!(
            find_document_for_path(&rt, &token, path).await.unwrap(),
            Some(expected),
            "a different source_uri must not compete through its basename"
        );
    }
    assert_eq!(
        find_document_for_path(&rt, &token, "elsewhere/README.md")
            .await
            .unwrap(),
        None,
        "basename must not match rows that carry a different source_uri"
    );

    let legacy = create_named_document(&rt, &token, "README.md", None).await;
    assert_eq!(
        find_document_for_path(&rt, &token, "elsewhere/README.md")
            .await
            .unwrap(),
        Some(legacy),
        "a document without source_uri remains a basename fallback"
    );
    let null_uri =
        create_named_document(&rt, &token, "NULL.md", Some(json!({ "source_uri": null }))).await;
    assert_eq!(
        find_document_for_path(&rt, &token, "elsewhere/NULL.md")
            .await
            .unwrap(),
        Some(null_uri),
        "JSON-null source_uri remains an absent URI for basename fallback"
    );
    create_named_document(&rt, &token, "EMPTY.md", Some(json!({ "source_uri": "" }))).await;
    assert_eq!(
        find_document_for_path(&rt, &token, "elsewhere/EMPTY.md")
            .await
            .unwrap(),
        None,
        "an empty source_uri is present and must not become a basename fallback"
    );
}

#[tokio::test]
async fn suffix_match_outranks_basename_fallback() {
    let rt = KhiveRuntime::memory().unwrap();
    let token = rt.authorize(Namespace::local()).unwrap();
    create_named_document(&rt, &token, "README.md", None).await;
    let suffix = create_named_document(
        &rt,
        &token,
        "README.md",
        Some(json!({ "source_uri": "workspace/docs/README.md" })),
    )
    .await;
    assert_eq!(
        find_document_for_path(&rt, &token, "docs/README.md")
            .await
            .unwrap(),
        Some(suffix),
        "a suffix source_uri match must outrank a basename fallback"
    );
}

#[tokio::test]
async fn suffix_match_requires_a_path_component_boundary() {
    for (path, wrong_uri, aligned_uri) in [
        ("a.md", "x/ba.md", "x/a.md"),
        (
            "docs/README.md",
            "workspace/otherdocs/README.md",
            "workspace/docs/README.md",
        ),
    ] {
        let rt = KhiveRuntime::memory().unwrap();
        let token = rt.authorize(Namespace::local()).unwrap();
        let wrong = create_document(&rt, &token, wrong_uri).await;
        assert_eq!(
            find_document_for_path(&rt, &token, wrong_uri)
                .await
                .unwrap(),
            Some(wrong),
            "the seeded wrong-suffix document must resolve by its own exact URI"
        );
        assert_eq!(
            find_document_for_path(&rt, &token, path).await.unwrap(),
            None,
            "a suffix match must not start inside a path component: {path} in {wrong_uri}"
        );

        let aligned = create_document(&rt, &token, aligned_uri).await;
        assert_eq!(
            find_document_for_path(&rt, &token, path).await.unwrap(),
            Some(aligned),
            "a slash-aligned source URI suffix must remain eligible"
        );
        let exact = create_document(&rt, &token, path).await;
        assert_eq!(
            find_document_for_path(&rt, &token, path).await.unwrap(),
            Some(exact),
            "an exact source URI must still outrank the aligned suffix"
        );
    }
}
