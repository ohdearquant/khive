// ── review #527: status stays consistent with finalized through the UPDATE path ──

#[tokio::test]
async fn upsert_finalizing_existing_atom_promotes_draft_to_reviewed() {
    let f = pack(rt());

    // First insert: a non-finalized atom defaults to status='draft'.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "lifecycle-atom", "name": "Lifecycle", "content": "body dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("insert draft");
    let row = f
        .sql_query_one(
            "SELECT status FROM knowledge_atoms WHERE slug=?1",
            vec![SqlValue::Text("lifecycle-atom".into())],
        )
        .await
        .expect("atom row");
    assert_eq!(
        row_text(&row, "status").as_deref(),
        Some("draft"),
        "fresh non-finalized atom is draft"
    );

    // Re-upsert the SAME slug with finalized=true: the UPDATE path must promote
    // status to 'reviewed', mirroring the V22 finalized=1 => reviewed backfill.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "lifecycle-atom", "name": "Lifecycle", "content": "body dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "finalized": true }] }),
    )
    .await
    .expect("finalize upsert");
    let row = f
        .sql_query_one(
            "SELECT status FROM knowledge_atoms WHERE slug=?1",
            vec![SqlValue::Text("lifecycle-atom".into())],
        )
        .await
        .expect("atom row");
    assert_eq!(
        row_text(&row, "status").as_deref(),
        Some("reviewed"),
        "finalizing via upsert must promote draft -> reviewed"
    );
}

// The upsert CASE expression only promotes draft → reviewed when finalizing.
// Any non-draft status (reviewed, deprecated, or any future value) must be left
// untouched — re-finalizing must not overwrite an already-reviewed or deprecated atom.
#[tokio::test]
async fn upsert_finalizing_does_not_demote_non_draft_status() {
    let f = pack(rt());

    // Insert a finalized atom (starts as reviewed).
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "non-draft-atom", "name": "V", "content": "b dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "finalized": true }] }),
    )
    .await
    .expect("insert");

    // Manually set to deprecated — a valid non-draft status in the closed taxonomy.
    f.sql_exec(
        "UPDATE knowledge_atoms SET status='deprecated' WHERE slug=?1",
        vec![SqlValue::Text("non-draft-atom".into())],
    )
    .await;

    // Re-upsert with finalized=true: CASE only fires on draft; deprecated must remain.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "non-draft-atom", "name": "V2", "content": "b2 dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "finalized": true }] }),
    )
    .await
    .expect("re-upsert");

    let row = f
        .sql_query_one(
            "SELECT status FROM knowledge_atoms WHERE slug=?1",
            vec![SqlValue::Text("non-draft-atom".into())],
        )
        .await
        .expect("row");
    assert_eq!(
        row_text(&row, "status").as_deref(),
        Some("deprecated"),
        "re-finalizing must not overwrite a non-draft status (deprecated in this case)"
    );
}

// ── #1980: upsert UPDATE path must preserve finalized when the caller omits it ──

#[tokio::test]
async fn upsert_omitting_finalized_preserves_existing_flag() {
    let f = pack(rt());

    // Initial insert: finalized=true.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "preserve-atom", "name": "Preserve", "content": "body dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "finalized": true }] }),
    )
    .await
    .expect("insert finalized");

    // Re-upsert the SAME slug WITHOUT the finalized field: the flag must be preserved,
    // not silently reset to false (COALESCE(?7, finalized) — regression test for #1980).
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "preserve-atom", "name": "Preserve V2", "content": "body v2 dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("re-upsert without finalized");

    let row = f
        .sql_query_one(
            "SELECT finalized FROM knowledge_atoms WHERE slug=?1",
            vec![SqlValue::Text("preserve-atom".into())],
        )
        .await
        .expect("atom row");
    assert_eq!(
        row_i64(&row, "finalized"),
        Some(1),
        "omitting finalized on re-upsert must preserve the existing true flag"
    );
}

#[tokio::test]
async fn upsert_explicit_finalized_false_clears_flag() {
    let f = pack(rt());

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "demote-atom", "name": "Demote", "content": "body dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "finalized": true }] }),
    )
    .await
    .expect("insert finalized");

    // Explicit finalized=false is a deliberate demote and must still work.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "demote-atom", "name": "Demote V2", "content": "body v2 dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "finalized": false }] }),
    )
    .await
    .expect("re-upsert with explicit false");

    let row = f
        .sql_query_one(
            "SELECT finalized FROM knowledge_atoms WHERE slug=?1",
            vec![SqlValue::Text("demote-atom".into())],
        )
        .await
        .expect("atom row");
    assert_eq!(
        row_i64(&row, "finalized"),
        Some(0),
        "explicit finalized=false on re-upsert must clear the flag"
    );
}

#[tokio::test]
async fn upsert_explicit_finalized_true_on_draft_sets_flag_and_promotes_status() {
    let f = pack(rt());

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "promote-atom", "name": "Promote", "content": "body dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("insert draft");

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "promote-atom", "name": "Promote V2", "content": "body v2 dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "finalized": true }] }),
    )
    .await
    .expect("re-upsert with explicit true");

    let row = f
        .sql_query_one(
            "SELECT finalized, status FROM knowledge_atoms WHERE slug=?1",
            vec![SqlValue::Text("promote-atom".into())],
        )
        .await
        .expect("atom row");
    assert_eq!(
        row_i64(&row, "finalized"),
        Some(1),
        "explicit finalized=true on re-upsert must set the flag"
    );
    assert_eq!(
        row_text(&row, "status").as_deref(),
        Some("reviewed"),
        "explicit finalized=true on a draft must promote status to reviewed"
    );
}
