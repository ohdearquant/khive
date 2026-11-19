#[tokio::test]
async fn delete_atoms_soft_deletes_by_slug() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "to-delete", "name": "Will be gone", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert");

    let del_resp = f
        .dispatch("knowledge.delete_atoms", json!({ "ids": ["to-delete"] }))
        .await
        .expect("delete ok");

    assert_eq!(del_resp["deleted"], 1);

    // get should now return not-found.
    let err = f
        .dispatch("knowledge.get", json!({ "id": "to-delete" }))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("not found") || err.to_string().contains("NotFound"),
        "expected not-found after delete, got: {err}"
    );
}

#[tokio::test]
async fn delete_atoms_returns_zero_for_unknown_slug() {
    let f = pack(rt());
    let resp = f
        .dispatch(
            "knowledge.delete_atoms",
            json!({ "ids": ["does-not-exist"] }),
        )
        .await
        .expect("delete ok even for missing");
    assert_eq!(resp["deleted"], 0);
}

#[tokio::test]
async fn delete_atoms_rejects_domain_mirror_slug_without_mutation() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_domains",
        json!({ "domains": [{ "slug": "retrieval", "name": "Retrieval", "description": "Dense and sparse retrieval techniques — covering concepts techniques algorithms implementations applications use cases and design patterns in detail —" }] }),
    )
    .await
    .expect("upsert domain");

    let err = f
        .dispatch("knowledge.delete_atoms", json!({ "ids": ["retrieval"] }))
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "expected InvalidInput, got: {err:?}"
    );

    // Direct lookup and search must agree: the domain is present on both paths.
    let got = f
        .dispatch("knowledge.get", json!({ "id": "retrieval" }))
        .await
        .expect("get must still resolve the domain");
    assert_eq!(got["kind"], "domain");
    assert!(got["members"].is_array());

    let search = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "retrieval", "type": "domain", "rerank": false }),
        )
        .await
        .expect("search ok");
    let results = search["results"].as_array().expect("results array");
    assert!(
        results.iter().any(|r| r["slug"] == "retrieval"),
        "search must still find the domain: {results:?}"
    );
}

#[tokio::test]
async fn delete_atoms_rejects_domain_mirror_uuid_without_mutation() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_domains",
        json!({ "domains": [{ "slug": "embedding-theory", "name": "Embedding Theory", "description": "vector embedding concepts — covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering concepts" }] }),
    )
    .await
    .expect("upsert domain");

    let by_slug = f
        .dispatch("knowledge.get", json!({ "id": "embedding-theory" }))
        .await
        .expect("get domain by slug");
    let uuid = by_slug["id"].as_str().expect("id string").to_string();

    let err = f
        .dispatch("knowledge.delete_atoms", json!({ "ids": [uuid.clone()] }))
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "expected InvalidInput, got: {err:?}"
    );

    // Direct lookup by UUID must agree with search: both say the domain is present.
    let got = f
        .dispatch("knowledge.get", json!({ "id": uuid }))
        .await
        .expect("get must still resolve the domain by uuid");
    assert_eq!(got["kind"], "domain");

    let search = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "embedding", "type": "domain", "rerank": false }),
        )
        .await
        .expect("search ok");
    let results = search["results"].as_array().expect("results array");
    assert!(
        results.iter().any(|r| r["slug"] == "embedding-theory"),
        "search must still find the domain: {results:?}"
    );
}

#[tokio::test]
async fn delete_atoms_mixed_request_with_domain_mirror_leaves_normal_atom_live() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "normal-atom", "name": "Normal Atom", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("seed atom");
    f.dispatch(
        "knowledge.upsert_domains",
        json!({ "domains": [{ "slug": "mixed-domain", "name": "Mixed Domain", "description": "Mixed domain techniques — covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering concepts techniques" }] }),
    )
    .await
    .expect("seed domain");

    let err = f
        .dispatch(
            "knowledge.delete_atoms",
            json!({ "ids": ["normal-atom", "mixed-domain"] }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "expected InvalidInput, got: {err:?}"
    );

    let atom = f
        .dispatch("knowledge.get", json!({ "id": "normal-atom" }))
        .await
        .expect("normal atom must remain live after the rejected mixed request");
    assert_eq!(atom["kind"], "atom");
}

/// Insert live atoms straight into storage so a delete test can seed many
/// rows without upsert validation. Returns each atom's id, in slug order.
async fn seed_raw_atoms(runtime: &KhiveRuntime, slugs: &[String]) -> Vec<String> {
    let ids: Vec<String> = (0..slugs.len())
        .map(|i| format!("d1e1e700-0000-4000-8000-{i:012}"))
        .collect();
    let statements: Vec<SqlStatement> = slugs
        .iter()
        .zip(&ids)
        .map(|(slug, id)| SqlStatement {
            sql: "INSERT INTO knowledge_atoms \
                  (id, namespace, slug, name, content, created_at, updated_at) \
                  VALUES (?1, 'local', ?2, ?2, 'delete control content', 1, 1)"
                .into(),
            params: vec![SqlValue::Text(id.clone()), SqlValue::Text(slug.clone())],
            label: Some("test.knowledge_delete_atoms.seed".into()),
        })
        .collect();
    let mut writer = runtime.sql().writer().await.expect("knowledge writer");
    writer.execute_batch(statements).await.expect("seed atoms");
    ids
}

async fn live_atom_slugs(runtime: &KhiveRuntime) -> Vec<String> {
    let mut reader = runtime.sql().reader().await.expect("knowledge reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT slug FROM knowledge_atoms WHERE deleted_at IS NULL ORDER BY slug".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("live slugs");
    rows.iter()
        .filter_map(|row| match row.get("slug") {
            Some(SqlValue::Text(slug)) => Some(slug.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn delete_atoms_counts_ids_slugs_duplicates_and_deleted_across_chunks() {
    let runtime = rt();
    let f = pack(runtime.clone());
    let slugs: Vec<String> = (0..6).map(|i| format!("del-{i}")).collect();
    let ids = seed_raw_atoms(&runtime, &slugs).await;

    // del-5 is already deleted before the request below.
    f.dispatch("knowledge.delete_atoms", json!({ "ids": ["del-5"] }))
        .await
        .expect("pre-delete");

    // 2,000 references span three chunks; real atoms sit on both sides of the
    // chunk boundary, one is named twice (slug and id), one is already deleted
    // and one is written with surrounding whitespace.
    let mut request: Vec<String> = (0..2000).map(|i| format!("no-such-atom-{i}")).collect();
    request[0] = slugs[0].clone();
    request[899] = ids[1].clone();
    request[900] = format!("  {}  ", slugs[2]);
    request[1799] = ids[2].clone();
    request[1800] = slugs[5].clone();
    request[1999] = ids[3].clone();
    let resp = f
        .dispatch("knowledge.delete_atoms", json!({ "ids": request }))
        .await
        .expect("delete");

    assert_eq!(resp["deleted"], 4, "del-0..del-3 are deleted once each");
    assert_eq!(resp["requested"], 2000, "requested is the submitted length");
    assert_eq!(live_atom_slugs(&runtime).await, vec!["del-4".to_string()]);
}

#[tokio::test]
async fn delete_atoms_refuses_a_domain_in_a_later_chunk_before_deleting_anything() {
    let runtime = rt();
    let f = pack(runtime.clone());
    seed_raw_atoms(&runtime, &["first-chunk-atom".to_string()]).await;
    f.dispatch(
        "knowledge.upsert_domains",
        json!({ "domains": [{ "slug": "late-domain", "name": "Late Domain", "description": "Late domain techniques — covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering concepts techniques" }] }),
    )
    .await
    .expect("seed domain");

    let mut request: Vec<String> = (0..1500).map(|i| format!("no-such-atom-{i}")).collect();
    request[0] = "first-chunk-atom".to_string();
    request[1400] = "late-domain".to_string();
    let err = f
        .dispatch("knowledge.delete_atoms", json!({ "ids": request }))
        .await
        .unwrap_err();

    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "expected InvalidInput, got: {err:?}"
    );
    let text = err.to_string();
    assert!(
        text.contains("cannot delete domain \"late-domain\""),
        "{text}"
    );
    assert!(
        text.contains("use the generic delete verb by domain UUID"),
        "{text}"
    );
    let live = live_atom_slugs(&runtime).await;
    assert!(
        live.contains(&"first-chunk-atom".to_string()),
        "a refused request must not delete the atoms listed before the domain"
    );
}

#[tokio::test]
async fn delete_atoms_storage_error_rolls_back_the_whole_request() {
    let runtime = rt();
    let f = pack(runtime.clone());
    let slugs: Vec<String> = ["rb-alpha", "rb-beta", "rb-poison", "rb-gamma"]
        .iter()
        .map(|slug| slug.to_string())
        .collect();
    seed_raw_atoms(&runtime, &slugs).await;
    {
        let sql = runtime.sql();
        let mut writer = sql.writer().await.expect("fixture trigger writer");
        writer
            .execute(SqlStatement {
                sql: "CREATE TRIGGER reject_one_delete BEFORE UPDATE OF deleted_at \
                      ON knowledge_atoms WHEN OLD.slug = 'rb-poison' \
                      BEGIN SELECT RAISE(ABORT, 'injected delete failure'); END"
                    .into(),
                params: vec![],
                label: Some("test.knowledge_delete_atoms.reject_one_delete".into()),
            })
            .await
            .expect("install trigger in the private in-memory fixture");
    }

    let err = f
        .dispatch("knowledge.delete_atoms", json!({ "ids": slugs }))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("delete_atoms"),
        "the storage error must surface: {err}"
    );

    // The request is one transaction: the atoms listed before the failing one
    // are not left deleted.
    assert_eq!(
        live_atom_slugs(&runtime).await,
        vec![
            "rb-alpha".to_string(),
            "rb-beta".to_string(),
            "rb-gamma".to_string(),
            "rb-poison".to_string(),
        ]
    );
}
