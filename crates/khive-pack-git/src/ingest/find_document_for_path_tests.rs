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
