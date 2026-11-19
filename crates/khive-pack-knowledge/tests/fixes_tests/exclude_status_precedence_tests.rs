// ── exclude_status precedence regression ─────────────────────

// exclude_status= with NO status= must exclude matching atoms.
// Previously the buffer logic silently ignored exclude_status when no status= was set.
#[tokio::test]
async fn exclude_status_without_status_param_excludes_target_status() {
    let f = pack(rt());

    // Seed a reviewed and a draft atom with shared distinctive content.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                {
                    "slug": "prec-reviewed",
                    "name": "Precedence Reviewed Atom",
                    "content": "precedence exclude status regression reviewed dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity unique prec78a"
                },
                {
                    "slug": "prec-draft",
                    "name": "Precedence Draft Atom",
                    "content": "precedence exclude status regression draft dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity unique prec78a"
                },
            ]
        }),
    )
    .await
    .expect("seed atoms");

    // Ensure statuses are set explicitly.
    f.sql_exec(
        "UPDATE knowledge_atoms SET status='reviewed' WHERE slug=?1",
        vec![SqlValue::Text("prec-reviewed".into())],
    )
    .await;
    f.sql_exec(
        "UPDATE knowledge_atoms SET status='draft' WHERE slug=?1",
        vec![SqlValue::Text("prec-draft".into())],
    )
    .await;

    // exclude_status=reviewed, no status= → reviewed atoms must be excluded.
    let resp = f
        .dispatch(
            "knowledge.search",
            json!({
                "query": "precedence exclude status regression unique prec78a",
                "rerank": false,
                "exclude_status": "reviewed",
                "include_drafts": true
            }),
        )
        .await
        .expect("search ok");

    let results = resp["results"].as_array().expect("results");
    let names: Vec<&str> = results.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(
        !names.contains(&"Precedence Reviewed Atom"),
        "exclude_status=reviewed must remove reviewed atoms (no status= set): {names:?}"
    );
    assert!(
        names.contains(&"Precedence Draft Atom"),
        "draft atom must appear when include_drafts=true and exclude_status=reviewed: {names:?}"
    );
}

// When status= is set, exclude_status= must have no effect (status= takes precedence).
#[tokio::test]
async fn exclude_status_is_ignored_when_status_param_is_set() {
    let f = pack(rt());

    // Seed reviewed and draft atoms.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                {
                    "slug": "prec2-reviewed",
                    "name": "Prec2 Reviewed Atom",
                    "content": "precedence2 status override reviewed dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity unique prec78b"
                },
                {
                    "slug": "prec2-draft",
                    "name": "Prec2 Draft Atom",
                    "content": "precedence2 status override draft dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity unique prec78b"
                },
            ]
        }),
    )
    .await
    .expect("seed atoms");

    f.sql_exec(
        "UPDATE knowledge_atoms SET status='reviewed' WHERE slug=?1",
        vec![SqlValue::Text("prec2-reviewed".into())],
    )
    .await;
    f.sql_exec(
        "UPDATE knowledge_atoms SET status='draft' WHERE slug=?1",
        vec![SqlValue::Text("prec2-draft".into())],
    )
    .await;

    // status=reviewed + exclude_status=reviewed: status= wins, reviewed atoms appear.
    let resp = f
        .dispatch(
            "knowledge.search",
            json!({
                "query": "precedence2 status override unique prec78b",
                "rerank": false,
                "status": "reviewed",
                "exclude_status": "reviewed"
            }),
        )
        .await
        .expect("search ok");

    let results = resp["results"].as_array().expect("results");
    let names: Vec<&str> = results.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(
        names.contains(&"Prec2 Reviewed Atom"),
        "status=reviewed overrides exclude_status=reviewed: reviewed atom must appear: {names:?}"
    );
    assert!(
        !names.contains(&"Prec2 Draft Atom"),
        "status=reviewed must not return draft atoms: {names:?}"
    );
}

// blank exclude_status= must behave identically to absent — draft+deprecated excluded by default.
#[tokio::test]
async fn blank_exclude_status_falls_through_to_default_draft_exclusion() {
    let f = pack(rt());

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                {
                    "slug": "blank-ex-reviewed",
                    "name": "Blank Ex Reviewed",
                    "content": "blank exclude status normalization reviewed dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity unique blnk78a"
                },
                {
                    "slug": "blank-ex-draft",
                    "name": "Blank Ex Draft",
                    "content": "blank exclude status normalization draft dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity unique blnk78a"
                },
            ]
        }),
    )
    .await
    .expect("seed atoms");

    f.sql_exec(
        "UPDATE knowledge_atoms SET status='reviewed' WHERE slug=?1",
        vec![SqlValue::Text("blank-ex-reviewed".into())],
    )
    .await;
    f.sql_exec(
        "UPDATE knowledge_atoms SET status='draft' WHERE slug=?1",
        vec![SqlValue::Text("blank-ex-draft".into())],
    )
    .await;

    // exclude_status="" — blank must be treated as absent, so draft is still excluded by default.
    let resp = f
        .dispatch(
            "knowledge.search",
            json!({
                "query": "blank exclude status normalization unique blnk78a",
                "rerank": false,
                "exclude_status": ""
            }),
        )
        .await
        .expect("search ok");

    let results = resp["results"].as_array().expect("results");
    let names: Vec<&str> = results.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(
        !names.contains(&"Blank Ex Draft"),
        "blank exclude_status must not bypass draft exclusion: {names:?}"
    );
    assert!(
        names.contains(&"Blank Ex Reviewed"),
        "reviewed atom must appear with blank exclude_status: {names:?}"
    );
}

// whitespace-padded exclude_status=" draft " must normalize to "draft" and behave
// the same as exclude_status="draft" — NOT apply as a raw " draft " exclusion that
// the ANN post-filter would miss (since the filter uses exact contains comparison).
#[tokio::test]
async fn whitespace_padded_exclude_status_normalizes_to_draft() {
    let f = pack(rt());

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                {
                    "slug": "ws-ex-reviewed",
                    "name": "Ws Ex Reviewed",
                    "content": "whitespace padded exclude status reviewed dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity unique wspad78"
                },
                {
                    "slug": "ws-ex-draft",
                    "name": "Ws Ex Draft",
                    "content": "whitespace padded exclude status draft dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity unique wspad78"
                },
            ]
        }),
    )
    .await
    .expect("seed atoms");

    f.sql_exec(
        "UPDATE knowledge_atoms SET status='reviewed' WHERE slug=?1",
        vec![SqlValue::Text("ws-ex-reviewed".into())],
    )
    .await;
    f.sql_exec(
        "UPDATE knowledge_atoms SET status='draft' WHERE slug=?1",
        vec![SqlValue::Text("ws-ex-draft".into())],
    )
    .await;

    // exclude_status=" draft " with leading/trailing spaces must normalize to "draft"
    // and exclude draft atoms consistently (SQL and ANN use the same normalized value).
    let resp = f
        .dispatch(
            "knowledge.search",
            json!({
                "query": "whitespace padded exclude status unique wspad78",
                "rerank": false,
                "exclude_status": " draft ",
                "include_drafts": true
            }),
        )
        .await
        .expect("search ok");

    let results = resp["results"].as_array().expect("results");
    let names: Vec<&str> = results.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(
        !names.contains(&"Ws Ex Draft"),
        "whitespace-padded \" draft \" must normalize to \"draft\" and exclude draft atoms: {names:?}"
    );
    assert!(
        names.contains(&"Ws Ex Reviewed"),
        "reviewed atom must appear when exclude_status=\" draft \": {names:?}"
    );
}
