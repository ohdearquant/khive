// ── #2732: distinguish corpus size, candidate window and returned rows ────────

#[tokio::test]
async fn issue2732_topic_query_count_distinguishes_corpus_window_and_output() {
    let runtime = rt();
    let f = pack(runtime.clone());
    let core = runtime.core();
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .unwrap();
    const QUERY: &str = "orchardwindowprobe";
    for slot in 0..17 {
        core.create_entity_with_embedding_report(
            &token,
            "concept",
            None,
            &format!("{QUERY} concept {slot:02}"),
            Some("Controlled concept fixture for query window counts."),
            None,
            vec!["count-domain".into()],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();
    }
    // An independent, fixed-bound core search supplies exact candidate IDs and scores.
    let hits = core
        .hybrid_search(&token, QUERY, None, 12, Some("concept"), None, &[], None)
        .await
        .unwrap();
    assert_eq!(
        hits.len(),
        12,
        "real indexed fixture fills the twelve-candidate window"
    );
    let queried = f
        .dispatch("knowledge.topic", json!({"query": QUERY, "limit": 3}))
        .await
        .unwrap();
    assert_eq!(queried["candidate_window_count"], 12);
    assert!(
        queried.get("total").is_none(),
        "query count must not masquerade as corpus total"
    );
    let rows = queried["results"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    for (row, hit) in rows.iter().zip(&hits) {
        assert_eq!(row["full_id"], hit.entity_id.to_string());
        assert_eq!(row["score"], json!(hit.score.to_f64()));
    }
    let listing = f
        .dispatch(
            "knowledge.topic",
            json!({"domain":"count-domain", "limit":3}),
        )
        .await
        .unwrap();
    assert_eq!(listing["total"], 17);
    assert_eq!(listing["results"].as_array().unwrap().len(), 3);
    assert!(listing.get("candidate_window_count").is_none());
    let null_query = f
        .dispatch(
            "knowledge.topic",
            json!({"domain":"count-domain", "query":null, "limit":3}),
        )
        .await
        .unwrap();
    assert_eq!(
        null_query, listing,
        "null query retains unqueried full-count semantics"
    );
    assert!(
        listing["total"].as_u64().unwrap() > queried["candidate_window_count"].as_u64().unwrap()
    );
    assert!(queried["candidate_window_count"].as_u64().unwrap() > rows.len() as u64);
    for (args, count_key, expected, absent) in [
        (
            json!({"query":QUERY, "limit":0}),
            "candidate_window_count",
            0,
            "total",
        ),
        (
            json!({"domain":"count-domain", "limit":0}),
            "total",
            17,
            "candidate_window_count",
        ),
    ] {
        let response = f.dispatch("knowledge.topic", args).await.unwrap();
        assert_eq!(response[count_key], expected);
        assert_eq!(response["results"], json!([]));
        assert!(response.get(absent).is_none());
    }
}

/// An empty or whitespace `query` stays on the query branch.
///
/// ADR-047's 2026-09-15 amendment decides this as behaviour, and `handle_topic`
/// implements it by matching `Some(ref query)` with no trim and no emptiness
/// check. Nothing asserted it. The reason that matters is three lines above the
/// branch, in the same function: the sibling parameter is normalized the other
/// way round.
///
/// ```ignore
/// let domain_filter = p.domain.as_deref()
///     .map(|d| d.trim().to_lowercase())
///     .filter(|d| !d.is_empty());
/// ```
///
/// So inside one handler an empty string means "no filter" for `domain` and
/// "search for nothing" for `query`. Adding `.filter(|q| !q.trim().is_empty())`
/// to the query branch reads as tidying that asymmetry away, and it would move an
/// empty query onto the listing branch, changing which count key the response
/// carries from `candidate_window_count` to `total`. A caller keyed on the
/// presence of one of those two fields sees the shape change with no error.
#[tokio::test]
async fn issue2824_empty_and_whitespace_topic_queries_stay_on_the_query_branch() {
    let runtime = rt();
    let f = pack(runtime.clone());
    let core = runtime.core();
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .unwrap();
    const QUERY: &str = "orchardwindowprobe";
    for slot in 0..17 {
        core.create_entity_with_embedding_report(
            &token,
            "concept",
            None,
            &format!("{QUERY} concept {slot:02}"),
            Some("Controlled concept fixture for query window counts."),
            None,
            vec!["count-domain".into()],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();
    }

    // The three spellings the amendment says are one branch, asserted together so
    // a reader sees them treated alike rather than inferring it from `Some(_)`.
    // The empty and whitespace cases carry no expected count: what is under test
    // is WHICH branch answers, and the number a search for nothing returns is not
    // part of that decision.
    for query in [QUERY, "", "   "] {
        let response = f
            .dispatch("knowledge.topic", json!({"query": query, "limit": 3}))
            .await
            .unwrap();
        assert!(
            response.get("candidate_window_count").is_some(),
            "query {query:?} must answer on the query branch: {response}"
        );
        assert!(
            response.get("total").is_none(),
            "query {query:?} must not report a corpus total: {response}"
        );
    }

    // The positive control for the `total` absence above, in the same arm and on
    // the same fixture. Without it `total.is_none()` would also hold in a build
    // where nothing emits `total` at all, and the negative assertion would read
    // as coverage while testing nothing.
    //
    // One case, not two: the arm above this one already asserts the omitted-query
    // and null-query listings against this same corpus, and a second copy here
    // would be a second place to update rather than a second thing checked.
    let listing = f
        .dispatch(
            "knowledge.topic",
            json!({"domain": "count-domain", "limit": 3}),
        )
        .await
        .unwrap();
    assert_eq!(listing["total"], 17, "{listing}");
    assert!(listing.get("candidate_window_count").is_none(), "{listing}");
}

#[tokio::test]
async fn issue2732_topic_query_count_applies_domain_filter_without_refill() {
    let runtime = rt();
    let f = pack(runtime.clone());
    let core = runtime.core();
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .unwrap();
    const QUERY: &str = "orchardwindowprobe";
    for slot in 0..13 {
        core.create_entity_with_embedding_report(
            &token,
            "concept",
            None,
            &format!("{QUERY} concept {slot:02}"),
            Some("Controlled concept fixture for query window counts."),
            None,
            vec!["other".into()],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();
    }
    let ranked = core
        .hybrid_search(&token, QUERY, None, 13, Some("concept"), None, &[], None)
        .await
        .unwrap();
    assert_eq!(ranked.len(), 13);
    let ids: Vec<_> = ranked.iter().map(|hit| hit.entity_id).collect();
    // Two domain matches inside the four-hit window and two outside it.
    // Only tags change; verify below that this fixture leaves search order intact.
    let access = core.sql();
    let mut writer = access.writer().await.unwrap();
    for position in [0, 2, 5, 8] {
        writer
            .execute(SqlStatement {
                sql:
                    "UPDATE entities SET version = version + 1, tags = '[\"wanted\"]' WHERE id = ?1"
                        .into(),
                params: vec![SqlValue::Text(ids[position].to_string())],
                label: None,
            })
            .await
            .unwrap();
    }
    drop(writer);
    let window = core
        .hybrid_search(&token, QUERY, None, 4, Some("concept"), None, &[], None)
        .await
        .unwrap();
    assert_eq!(
        window.iter().map(|hit| hit.entity_id).collect::<Vec<_>>(),
        ids[..4]
    );
    let filtered = f
        .dispatch(
            "knowledge.topic",
            json!({"query":QUERY, "domain":" WANTED ", "limit":1}),
        )
        .await
        .unwrap();
    assert_eq!(filtered["candidate_window_count"], 2);
    assert!(filtered.get("total").is_none());
    assert_eq!(filtered["results"].as_array().unwrap().len(), 1);
    assert_eq!(filtered["results"][0]["full_id"], ids[0].to_string());
    let listing = f
        .dispatch("knowledge.topic", json!({"domain":"wanted", "limit":1}))
        .await
        .unwrap();
    assert_eq!(
        listing["total"], 4,
        "the corpus has two more matches outside the candidate window"
    );
    assert!(listing.get("candidate_window_count").is_none());
    for (args, key, absent) in [
        (
            json!({"query":QUERY, "domain":"absent", "limit":1}),
            "candidate_window_count",
            "total",
        ),
        (
            json!({"domain":"absent", "limit":1}),
            "total",
            "candidate_window_count",
        ),
    ] {
        let response = f.dispatch("knowledge.topic", args).await.unwrap();
        assert_eq!(response[key], 0);
        assert_eq!(response["results"], json!([]));
        assert!(response.get(absent).is_none());
    }
}
