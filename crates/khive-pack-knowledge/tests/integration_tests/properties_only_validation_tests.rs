#[tokio::test]
async fn upsert_atoms_properties_only_keeps_content_and_shape_validation() {
    let runtime = rt();
    let f = pack_via_registry(runtime.clone());
    let before = properties_only_atom_fixture(
        &runtime,
        &f,
        "content-validation",
        "one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen",
        "local",
    )
    .await;
    let mut floor_results = Vec::new();
    for slug in ["content-validation", "missing-content-create"] {
        for content in [
            Some(json!("too short")),
            Some(json!("")),
            Some(Value::Null),
            None,
        ] {
            let mut atom =
                json!({"slug": slug, "name": "Replacement name", "properties": {"marker": true}});
            if let Some(content) = content {
                atom["content"] = content;
            }
            floor_results.push(
                f.dispatch("knowledge.upsert_atoms", json!({"atoms": [atom]}))
                    .await,
            );
        }
    }
    let mut invalid_rows = vec![
        json!({"id": "not-a-uuid", "properties": {}}),
        json!({"id": &before["id"].as_str().unwrap()[..8], "properties": {}}),
        json!({"id": before["id"]}),
        json!({"id": before["id"], "properties": {"khive:secret_gate": "forbidden"}}),
    ];
    for (field, value) in [
        ("content", json!("too short")),
        ("content", Value::Null),
        ("content", json!("one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty")),
        ("slug", json!("content-validation")),
        ("name", json!("Replacement name")),
        ("tags", json!([])),
        ("source_uri", Value::Null),
        ("source_type", Value::Null),
        ("finalized", json!(false)),
        ("unknown", json!(true)),
    ] {
        let mut row = json!({"id": before["id"], "properties": {"marker": true}});
        row[field] = value;
        invalid_rows.push(row);
    }
    let mut shape_results = Vec::new();
    for row in invalid_rows {
        shape_results.push(
            f.dispatch("knowledge.upsert_atoms", json!({"atoms": [row]}))
                .await,
        );
    }
    let after_refusals = f
        .dispatch("get", json!({"id": before["id"]}))
        .await
        .expect("original atom remains readable");
    let missing_create = f
        .dispatch("knowledge.get", json!({"id": "missing-content-create"}))
        .await;
    let ordinary = f
        .dispatch(
            "knowledge.upsert_atoms",
            json!({"atoms": [{
                "slug": "content-validation",
                "name": "Valid replacement",
                "content": " one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty ",
                "properties": {"ordinary": true}
            }]}),
        )
        .await
        .expect("ordinary valid content update remains accepted");
    let after_ordinary = f
        .dispatch("get", json!({"id": before["id"]}))
        .await
        .expect("generic get after ordinary update");
    for result in floor_results {
        assert!(
            matches!(result.as_ref().map_err(RuntimeError::refusal_source), Err(RuntimeError::InvalidInput(message)) if message.contains("20 words")),
            "{result:?}"
        );
    }
    for result in shape_results {
        assert!(
            matches!(
                result.as_ref().map_err(RuntimeError::refusal_source),
                Err(RuntimeError::InvalidInput(_))
            ),
            "{result:?}"
        );
    }
    assert_eq!(after_refusals, before);
    assert!(matches!(missing_create, Err(RuntimeError::NotFound(_))));
    assert_eq!(ordinary, json!({"created": 0, "updated": 1, "total": 1}));
    assert_eq!(after_ordinary["name"], "Valid replacement");
    assert_eq!(after_ordinary["properties"], json!({"ordinary": true}));
    assert_eq!(after_ordinary["content"], "one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty");
    assert_eq!(after_ordinary["tags"], json!([]));
    assert_eq!(after_ordinary["source_uri"], before["source_uri"]);
    assert_eq!(after_ordinary["source_type"], before["source_type"]);
    assert_eq!(after_ordinary["finalized"], before["finalized"]);
}

#[tokio::test]
async fn upsert_atoms_properties_only_missing_id_is_not_found() {
    let runtime = rt();
    let f = pack_via_registry(runtime.clone());
    let before = properties_only_atom_fixture(&runtime, &f, "missing-id-prefix", "", "local").await;
    let missing = "0d442bda-6d6a-4f47-b2b6-ea3f5da3a43b";
    let missing_before = f.dispatch("get", json!({"id": missing})).await;
    let singleton = f
        .dispatch(
            "knowledge.upsert_atoms",
            json!({"atoms": [{"id": missing, "properties": {"marker": true}}]}),
        )
        .await;
    let batch = f
        .dispatch(
            "knowledge.upsert_atoms",
            json!({"atoms": [
                {"id": before["id"], "properties": {"marker": true}},
                {"id": missing, "properties": {"marker": true}}
            ]}),
        )
        .await;
    let after = f
        .dispatch("get", json!({"id": before["id"]}))
        .await
        .expect("generic get after refused batch");
    let missing_after = f.dispatch("get", json!({"id": missing})).await;
    f.dispatch(
        "knowledge.delete_atoms",
        json!({"ids": ["missing-id-prefix"]}),
    )
    .await
    .expect("soft delete existing atom");
    let deleted = f
        .dispatch(
            "knowledge.upsert_atoms",
            json!({"atoms": [{"id": before["id"], "properties": {"marker": true}}]}),
        )
        .await;
    f.dispatch(
        "knowledge.upsert_domains",
        json!({"domains": [{
            "slug": "properties-domain",
            "name": "Properties domain",
            "description": "one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty"
        }]}),
    )
    .await
    .expect("seed protected domain mirror");
    let domain_before = f
        .dispatch("knowledge.get", json!({"id": "properties-domain"}))
        .await
        .expect("read protected domain");
    let domain = f
        .dispatch(
            "knowledge.upsert_atoms",
            json!({"atoms": [{"id": domain_before["id"], "properties": {"marker": true}}]}),
        )
        .await;
    let domain_after = f
        .dispatch("get", json!({"id": domain_before["id"]}))
        .await
        .expect("generic get protected domain");
    for result in [missing_before, singleton, batch, missing_after, deleted] {
        assert!(
            matches!(
                result.as_ref().map_err(RuntimeError::refusal_source),
                Err(RuntimeError::NotFound(_))
            ),
            "{result:?}"
        );
    }
    assert_eq!(
        after, before,
        "missing target must not commit a valid prefix"
    );
    assert!(
        matches!(
            domain.as_ref().map_err(RuntimeError::refusal_source),
            Err(RuntimeError::InvalidInput(_))
        ),
        "{domain:?}"
    );
    assert_eq!(domain_after, domain_before);
}
