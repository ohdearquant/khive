#[tokio::test]
async fn list_filters_before_paginating_sparse_match() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("list_filter_pagination.db"));
    let registry = build_registry(rt);

    // A single matching session, buried behind more than one page of newer
    // nonmatching sessions (sessions list newest-first). An implementation
    // that fetched an unfiltered page and filtered in memory would miss it.
    let matching = registry
        .dispatch(
            "create",
            json!({
                "kind": "session",
                "content": "matches agent filter",
                "properties": {"agent_id": "lambda:target"}
            }),
        )
        .await
        .expect("create matching session");
    let matching_id = matching["id"].as_str().expect("matching id").to_string();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    for i in 0..5 {
        registry
            .dispatch(
                "create",
                json!({
                    "kind": "session",
                    "content": format!("noise {i}"),
                    "properties": {"agent_id": "lambda:other"}
                }),
            )
            .await
            .expect("create nonmatching session");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    let result = registry
        .dispatch(
            "session.list",
            json!({"agent_id": "lambda:target", "limit": 2, "offset": 0}),
        )
        .await
        .expect("filtered and paged list");
    let sessions = result["sessions"].as_array().expect("sessions array");
    assert_eq!(
        sessions.len(),
        1,
        "filtering must apply before the page window, not after fetching a page of newer nonmatches"
    );
    assert_eq!(sessions[0]["id"], matching_id);
    assert_eq!(
        result["total"], 1,
        "total must reflect the filtered count, not the unfiltered total"
    );

    // Combined filters: `since` set to the matching session's own timestamp
    // matches all six sessions (it is the oldest), but adding `agent_id`
    // must still narrow the result to the one matching session.
    let matching_created_at = registry
        .dispatch("session.resume", json!({"id": matching_id}))
        .await
        .expect("resume matching session")["session"]["created_at"]
        .as_str()
        .expect("created_at present")
        .to_string();

    let combined = registry
        .dispatch(
            "session.list",
            json!({
                "agent_id": "lambda:target",
                "since": matching_created_at,
                "limit": 2,
                "offset": 0
            }),
        )
        .await
        .expect("combined-filter list");
    let combined_sessions = combined["sessions"].as_array().expect("sessions array");
    assert_eq!(combined_sessions.len(), 1);
    assert_eq!(combined_sessions[0]["id"], matching_id);
    assert_eq!(combined["total"], 1);
}

#[tokio::test]
async fn list_filters_before_paginating_nonzero_offset() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("list_filter_pagination_offset.db"));
    let registry = build_registry(rt);

    // Three matching sessions interleaved with newer nonmatching sessions, so
    // an implementation that pages the raw (unfiltered) row order before
    // filtering would misalign the offset window against the filtered set.
    let mut matching_ids = Vec::new();
    for round in 0..3 {
        let created = registry
            .dispatch(
                "create",
                json!({
                    "kind": "session",
                    "content": format!("matches agent filter {round}"),
                    "properties": {"agent_id": "lambda:target"}
                }),
            )
            .await
            .expect("create matching session");
        matching_ids.push(created["id"].as_str().expect("matching id").to_string());
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        for i in 0..2 {
            registry
                .dispatch(
                    "create",
                    json!({
                        "kind": "session",
                        "content": format!("noise {round}-{i}"),
                        "properties": {"agent_id": "lambda:other"}
                    }),
                )
                .await
                .expect("create nonmatching session");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    // Filtered, newest-first order is [match2, match1, match0]; offset=1
    // with limit=1 must land on match1, the second matching record.
    let result = registry
        .dispatch(
            "session.list",
            json!({"agent_id": "lambda:target", "limit": 1, "offset": 1}),
        )
        .await
        .expect("filtered and paged list");
    let sessions = result["sessions"].as_array().expect("sessions array");
    assert_eq!(
        sessions.len(),
        1,
        "expected exactly one session in the requested page"
    );
    assert_eq!(
        sessions[0]["id"], matching_ids[1],
        "offset must apply to the filtered set, not an unfiltered pre-filter page"
    );
    assert_eq!(
        result["total"], 3,
        "total must reflect the filtered count across all three matches"
    );
}
