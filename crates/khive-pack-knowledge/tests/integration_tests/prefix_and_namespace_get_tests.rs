#[tokio::test]
async fn get_prefix_range_preserves_boundaries_ambiguity_and_not_found() {
    const IDS: [&str; 4] = [
        "0abcdefe-ffff-4fff-8fff-ffffffffffff",
        "0abcdeff-0000-4000-8000-000000000001",
        "0abcdeff-1000-4000-8000-000000000002",
        "0abcdf00-0000-4000-8000-000000000003",
    ];

    let runtime = rt();
    let f = pack(runtime.clone());
    let statements = IDS
        .iter()
        .enumerate()
        .map(|(index, id)| SqlStatement {
            sql: "INSERT INTO knowledge_atoms \
                  (id, namespace, slug, name, content, created_at, updated_at) \
                  VALUES (?1, 'local', ?2, ?3, 'range control content', 1, 1)"
                .into(),
            params: vec![
                SqlValue::Text((*id).into()),
                SqlValue::Text(format!("range-control-{index}")),
                SqlValue::Text(format!("Range Control {index}")),
            ],
            label: Some("test.knowledge_get.range_control".into()),
        })
        .collect();
    let mut writer = runtime.sql().writer().await.expect("knowledge writer");
    writer
        .execute_batch(statements)
        .await
        .expect("seed range controls");
    drop(writer);

    let unique = f
        .dispatch("knowledge.get", json!({ "id": "0abcdeff0" }))
        .await
        .expect("ninth compact digit must exclude adjacent UUIDs");
    assert_eq!(unique["id"], IDS[1]);

    let ambiguous = f
        .dispatch("knowledge.get", json!({ "id": "0abcdeff" }))
        .await
        .expect_err("both UUIDs inside the eight-digit range must be ambiguous");
    assert!(
        matches!(
            ambiguous,
            RuntimeError::AmbiguousPrefix { ref matches, .. } if matches.len() == 2
        ),
        "the two in-range records must remain ambiguous: {ambiguous:?}"
    );

    let missing = f
        .dispatch("knowledge.get", json!({ "id": "0abcdeff2" }))
        .await
        .expect_err("adjacent UUIDs must not become prefix hits");
    assert!(
        matches!(missing, RuntimeError::InvalidInput(ref message) if message.contains("no knowledge record matches prefix")),
        "a range containing only adjacent UUIDs must remain not found: {missing:?}"
    );
}

#[tokio::test]
async fn get_by_id_is_namespace_agnostic_and_loads_sections_from_stored_namespace() {
    let f = pack(rt());
    let foreign_namespace = "identifier-contract-foreign";
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "namespace": foreign_namespace,
            "atoms": [{
                "slug": "foreign-prefix-atom",
                "name": "Foreign Prefix Atom",
                "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
            }]
        }),
    )
    .await
    .expect("upsert foreign atom");
    f.dispatch(
        "knowledge.edit",
        json!({
            "namespace": foreign_namespace,
            "id": "foreign-prefix-atom",
            "sections": [{
                "section_type": "overview",
                "content": "This section belongs to the foreign namespace atom and proves that a namespace-agnostic by-ID read loads sections using the resolved record namespace."
            }]
        }),
    )
    .await
    .expect("add foreign section");

    let foreign = f
        .dispatch(
            "knowledge.get",
            json!({ "namespace": foreign_namespace, "id": "foreign-prefix-atom" }),
        )
        .await
        .expect("get foreign atom by scoped slug");
    let full_id = foreign["id"].as_str().expect("foreign atom id").to_string();
    let compact_prefix = full_id.replace('-', "")[..12].to_string();

    f.dispatch("knowledge.get", json!({ "id": "foreign-prefix-atom" }))
        .await
        .expect_err("slug lookup must remain scoped to the caller namespace");

    let by_id = f
        .dispatch(
            "knowledge.get",
            json!({ "id": full_id.clone(), "include_sections": true }),
        )
        .await
        .expect("full UUID read must be namespace-agnostic");
    assert_eq!(by_id["namespace"], foreign_namespace);
    assert_eq!(
        by_id["sections"].as_array().expect("sections array").len(),
        1,
        "section lookup must use the resolved atom's stored namespace"
    );

    let by_prefix = f
        .dispatch(
            "knowledge.get",
            json!({ "id": compact_prefix, "include_sections": true }),
        )
        .await
        .expect("unique prefix read must be namespace-agnostic");
    assert_eq!(by_prefix["id"], full_id);
    assert_eq!(by_prefix["namespace"], foreign_namespace);
    assert_eq!(
        by_prefix["sections"]
            .as_array()
            .expect("prefix sections array")
            .len(),
        1,
        "prefix section lookup must use the resolved atom's stored namespace"
    );
}
