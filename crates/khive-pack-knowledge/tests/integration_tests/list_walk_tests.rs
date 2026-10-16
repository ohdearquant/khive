/// #2220: seek pagination must not let a concurrent insert shift the remaining
/// pre-existing rows. An insert behind the issued boundary belongs to a fresh
/// walk; an insert ahead of it may extend this live walk. Both are forced onto
/// the same timestamp so the UUID tiebreak is load-bearing.
#[tokio::test]
async fn list_cursor_walk_has_no_gaps_or_duplicates_during_concurrent_inserts() {
    let runtime = rt();
    let f = pack(runtime.clone());
    let content = "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity";
    let atoms: Vec<Value> = (0..12)
        .map(|n| {
            json!({
                "slug": format!("cursor-original-{n:02}"),
                "name": format!("Cursor Original {n}"),
                "content": content,
            })
        })
        .collect();
    f.dispatch("knowledge.upsert_atoms", json!({ "atoms": atoms }))
        .await
        .expect("seed cursor atoms");

    let shared_created_at = 1_750_000_000_000_000_i64;
    {
        let mut writer = runtime.sql().writer().await.expect("open writer");
        writer
            .execute(SqlStatement {
                sql: "UPDATE knowledge_atoms SET created_at = ?1".into(),
                params: vec![SqlValue::Integer(shared_created_at)],
                label: Some("test.knowledge_list_cursor.shared_timestamp".into()),
            })
            .await
            .expect("force shared timestamp");
    }

    let original_ids: std::collections::HashSet<String> = {
        let mut reader = runtime.sql().reader().await.expect("open reader");
        reader
            .query_all(SqlStatement {
                sql: "SELECT id FROM knowledge_atoms WHERE tags NOT LIKE '%type:domain%'".into(),
                params: vec![],
                label: Some("test.knowledge_list_cursor.original_ids".into()),
            })
            .await
            .expect("read original ids")
            .into_iter()
            .filter_map(|row| match row.get("id") {
                Some(SqlValue::Text(id)) => Some(id.clone()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(original_ids.len(), 12);

    let first_page = f
        .dispatch(
            "knowledge.list",
            json!({
                "after": "",
                "limit": 3,
                "fields": ["id", "slug"],
            }),
        )
        .await
        .expect("first cursor page");
    let mut seen: Vec<String> = first_page["results"]
        .as_array()
        .expect("first results")
        .iter()
        .map(|row| row["id"].as_str().expect("id").to_string())
        .collect();
    let mut after = first_page["next_after"]
        .as_str()
        .expect("non-terminal first cursor")
        .to_string();

    const BEHIND_ID: &str = "00000000-0000-4000-8000-000000000000";
    const AHEAD_ID: &str = "ffffffff-ffff-4fff-bfff-ffffffffffff";
    {
        let mut writer = runtime.sql().writer().await.expect("open insert writer");
        writer
            .execute_batch(vec![
                SqlStatement {
                    sql: "INSERT INTO knowledge_atoms (id, namespace, slug, name, content, created_at, updated_at) VALUES (?1, 'local', 'cursor-behind', 'Cursor Behind', ?2, ?3, ?3)".into(),
                    params: vec![
                        SqlValue::Text(BEHIND_ID.into()),
                        SqlValue::Text(content.into()),
                        SqlValue::Integer(shared_created_at),
                    ],
                    label: Some("test.knowledge_list_cursor.insert_behind".into()),
                },
                SqlStatement {
                    sql: "INSERT INTO knowledge_atoms (id, namespace, slug, name, content, created_at, updated_at) VALUES (?1, 'local', 'cursor-ahead', 'Cursor Ahead', ?2, ?3, ?3)".into(),
                    params: vec![
                        SqlValue::Text(AHEAD_ID.into()),
                        SqlValue::Text(content.into()),
                        SqlValue::Integer(shared_created_at),
                    ],
                    label: Some("test.knowledge_list_cursor.insert_ahead".into()),
                },
            ])
            .await
            .expect("insert concurrent rows");
    }

    let mut terminated = false;
    for _ in 0..10 {
        let page = f
            .dispatch(
                "knowledge.list",
                json!({
                    "after": after,
                    "limit": 3,
                    "fields": ["id", "slug"],
                }),
            )
            .await
            .expect("next cursor page");
        assert_eq!(page["order"], "created_at_asc_id_asc");
        assert!(page.get("total").is_none());
        for row in page["results"].as_array().expect("page results") {
            assert_eq!(row.as_object().expect("projected row").len(), 2);
            seen.push(row["id"].as_str().expect("id").to_string());
        }
        match page["next_after"].as_str() {
            Some(next) => after = next.to_string(),
            None => {
                terminated = true;
                break;
            }
        }
    }

    assert!(
        terminated,
        "cursor walk must terminate with null next_after"
    );
    let unique: std::collections::HashSet<String> = seen.iter().cloned().collect();
    assert_eq!(
        unique.len(),
        seen.len(),
        "cursor pages must not duplicate rows"
    );
    assert!(
        original_ids.is_subset(&unique),
        "every row present before the walk must be returned exactly once"
    );
    assert!(
        unique.contains(AHEAD_ID),
        "an ahead insert extends the live walk"
    );
    assert!(
        !unique.contains(BEHIND_ID),
        "an insert behind an issued boundary belongs to the next walk"
    );
    let mut sorted = seen.clone();
    sorted.sort_unstable();
    assert_eq!(seen, sorted, "equal-timestamp cursor order must be id ASC");
}

/// #1671: a full offset sweep over `knowledge.list` must enumerate every atom
/// exactly once — no duplicates, no misses across page boundaries — even when
/// all atoms share one `created_at` value (the column the primary sort key
/// uses), which this test forces via a direct SQL update.
///
/// The sweep also asserts the concatenated pages follow the documented
/// `created_at DESC, id DESC` order: with one shared `created_at` that is
/// `id DESC`, and uniqueness alone would still pass with a wrong tiebreak
/// direction.
#[tokio::test]
async fn list_offset_sweep_covers_all_atoms_exactly_once() {
    let rt = rt();
    let f = pack(rt.clone());
    let content = "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity";
    for chunk in 0..7 {
        let atoms: Vec<Value> = (0..17)
            .map(|i| {
                let n = chunk * 17 + i;
                json!({ "slug": format!("sweep-{n:03}"), "name": format!("Sweep {n}"), "content": content })
            })
            .collect();
        f.dispatch("knowledge.upsert_atoms", json!({ "atoms": atoms }))
            .await
            .expect("upsert");
    }

    // Force every atom onto one shared `created_at` so the sweep exercises the
    // tiebreak path; otherwise unique microsecond timestamps would let the
    // test pass even without the `id` tiebreak.
    let shared_created_at = 1_750_000_000_000_000_i64;
    {
        let sql = rt.sql();
        let mut writer = sql.writer().await.expect("sql writer must open");
        writer
            .execute(SqlStatement {
                sql: "UPDATE knowledge_atoms SET created_at = ?1".into(),
                params: vec![SqlValue::Integer(shared_created_at)],
                label: None,
            })
            .await
            .expect("force shared created_at");
    }

    let mut seen = std::collections::HashSet::new();
    let mut ordered_ids: Vec<String> = Vec::new();
    let mut offset = 0_u64;
    let page_size = 13_u64;
    loop {
        let page = f
            .dispatch(
                "knowledge.list",
                json!({ "limit": page_size, "offset": offset }),
            )
            .await
            .expect("list page");
        let results = page["results"].as_array().expect("results array");
        if results.is_empty() {
            break;
        }
        for row in results {
            let id = row["id"].as_str().expect("atom id").to_string();
            assert!(seen.insert(id.clone()), "duplicate id across pages: {id}");
            ordered_ids.push(id);
        }
        offset += results.len() as u64;
    }
    assert_eq!(
        seen.len(),
        7 * 17,
        "sweep must cover every atom exactly once"
    );
    // With one shared `created_at` the documented `created_at DESC, id DESC`
    // order is exactly `id DESC`, so the pages concatenated must be the id
    // list sorted descending.
    let mut expected_order: Vec<String> = seen.iter().cloned().collect();
    expected_order.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(
        ordered_ids, expected_order,
        "sweep pages must appear in created_at DESC, id DESC order"
    );
}

/// #1671: `knowledge.list(kind="domain")` pages over
/// `created_at DESC, id DESC`. Seed domains, force one shared `created_at`
/// via direct SQL so the `id` tiebreak is load-bearing, then sweep with a
/// small limit and assert no duplicates, no misses, AND the exact
/// `id DESC` order (one shared timestamp reduces the documented order to it).
#[tokio::test]
async fn list_domains_offset_sweep_covers_equal_created_at_in_order() {
    let rt = rt();
    let f = pack(rt.clone());
    let content = "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity";
    for n in 0..37 {
        f.dispatch(
            "knowledge.upsert_domains",
            json!({ "domains": [{ "slug": format!("dom-{n:03}"), "name": format!("Domain {n}"), "description": content }] }),
        )
        .await
        .expect("upsert domain");
    }

    // Force every domain onto one shared `created_at` so the sweep exercises
    // the tiebreak path; otherwise unique microsecond timestamps would let
    // the test pass even without the `id` tiebreak.
    let shared_created_at = 1_750_000_000_000_000_i64;
    {
        let sql = rt.sql();
        let mut writer = sql.writer().await.expect("sql writer must open");
        writer
            .execute(SqlStatement {
                sql: "UPDATE knowledge_domains SET created_at = ?1".into(),
                params: vec![SqlValue::Integer(shared_created_at)],
                label: None,
            })
            .await
            .expect("force shared created_at");
    }

    let mut seen = std::collections::HashSet::new();
    let mut ordered_ids: Vec<String> = Vec::new();
    let mut offset = 0_u64;
    let page_size = 9_u64;
    loop {
        let page = f
            .dispatch(
                "knowledge.list",
                json!({ "kind": "domain", "limit": page_size, "offset": offset }),
            )
            .await
            .expect("list domains page");
        let results = page["results"].as_array().expect("results array");
        if results.is_empty() {
            break;
        }
        for row in results {
            let id = row["id"].as_str().expect("domain id").to_string();
            assert!(seen.insert(id.clone()), "duplicate id across pages: {id}");
            ordered_ids.push(id);
        }
        offset += results.len() as u64;
    }
    assert_eq!(
        seen.len(),
        37,
        "domain sweep must cover every domain exactly once"
    );
    // One shared `created_at` reduces the documented `created_at DESC,
    // id DESC` order to `id DESC`, so the pages concatenated must be the id
    // list sorted descending.
    let mut expected_order: Vec<String> = seen.iter().cloned().collect();
    expected_order.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(
        ordered_ids, expected_order,
        "domain sweep pages must appear in created_at DESC, id DESC order"
    );
}
