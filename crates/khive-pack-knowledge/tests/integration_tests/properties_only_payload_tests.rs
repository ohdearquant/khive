async fn properties_only_atom_fixture(
    runtime: &KhiveRuntime,
    f: &Fixture,
    slug: &str,
    content: &str,
    namespace: &str,
) -> Value {
    let created = f
        .dispatch(
            "knowledge.upsert_atoms",
            json!({ "atoms": [{
                "slug": slug,
                "name": "Preserved atom name",
                "content": "one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty",
                "tags": ["preserved-tag"],
                "properties": {"old": "remove on replacement"},
                "source_uri": "https://example.com/source",
                "source_type": "article",
                "finalized": true
            }] }),
        )
        .await
        .expect("create ordinary atom before legacy fixture");
    assert_eq!(created, json!({"created": 1, "updated": 0, "total": 1}));
    let atom = f
        .dispatch("knowledge.get", json!({"id": slug}))
        .await
        .expect("read created atom");
    let id = atom["id"].as_str().expect("full atom UUID");
    {
        let mut writer = runtime.sql().writer().await.expect("fixture writer");
        let changed = writer
            .execute(SqlStatement {
                sql: "UPDATE knowledge_atoms SET content=?1, namespace=?2, created_at=5, updated_at=7 WHERE id=?3".into(),
                params: vec![
                    SqlValue::Text(content.into()),
                    SqlValue::Text(namespace.into()),
                    SqlValue::Text(id.into()),
                ],
                label: Some("test.atom_properties_only.legacy_fixture".into()),
            })
            .await
            .expect("seed legacy atom content");
        assert_eq!(changed, 1);
    }
    let before = f
        .dispatch("get", json!({"id": id}))
        .await
        .expect("generic get resolves the legacy atom");
    assert_eq!(before["content"], content);
    assert_eq!(before["namespace"], namespace);
    assert_eq!(
        before["properties"],
        json!({"old": "remove on replacement"})
    );
    before
}

fn assert_properties_only_payload(before: &Value, after: &Value, properties: Value) {
    assert_eq!(after["properties"], properties);
    let mut expected = before.clone();
    expected["properties"] = properties;
    expected["updated_at"] = after["updated_at"].clone();
    assert_eq!(after, &expected, "all other atom fields must be preserved");
}

#[tokio::test]
async fn upsert_atoms_properties_only_preserves_short_content() {
    let runtime = rt();
    let f = pack_via_registry(runtime.clone());
    let content = " \tone two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen\n ";
    assert_eq!(content.split_whitespace().count(), 15);
    let before =
        properties_only_atom_fixture(&runtime, &f, "short-properties", content, "other").await;
    let response = f
        .dispatch(
            "knowledge.upsert_atoms",
            json!({"atoms": [{"id": before["id"], "properties": {"marker": true}}]}),
        )
        .await
        .expect("properties-only update accepts stored short content");
    let after = f
        .dispatch("get", json!({"id": before["id"]}))
        .await
        .expect("generic get after property replacement");
    assert_eq!(response, json!({"created": 0, "updated": 1, "total": 1}));
    assert_properties_only_payload(&before, &after, json!({"marker": true}));
    assert_ne!(before["updated_at"], after["updated_at"]);

    let credential = ["ghp_", "FakeGitHubToken0000000000000000000"].concat();
    let missing = "a82d4e7f-f16c-469d-9c27-3cd7fb2a6ad1";
    assert!(matches!(
        f.dispatch("get", json!({"id": missing})).await,
        Err(RuntimeError::NotFound(_))
    ));
    for (atoms, location) in [
        (
            json!([{"id": before["id"], "properties": {"nested": [credential]}}]),
            "atoms[0].properties",
        ),
        (
            json!([
                {"id": before["id"], "properties": {"safe_prefix": true}},
                {"id": missing, "properties": {"nested": [credential]}}
            ]),
            "atoms[1].properties",
        ),
    ] {
        let error = f
            .dispatch("knowledge.upsert_atoms", json!({"atoms": atoms}))
            .await
            .expect_err("ID properties must receive the recursive secret scan");
        match error.refusal_source() {
            RuntimeError::SecretDetected(found) => {
                assert_eq!(found.location.as_deref(), Some(location));
            }
            other => panic!("expected secret refusal before target lookup, got {other:?}"),
        }
        assert!(!error.to_string().contains(&credential));
        let after_refusal = f
            .dispatch("get", json!({"id": before["id"]}))
            .await
            .expect("safe update survives secret refusal unchanged");
        assert_eq!(after_refusal, after);
        assert!(matches!(
            f.dispatch("get", json!({"id": missing})).await,
            Err(RuntimeError::NotFound(_))
        ));
    }
}

#[tokio::test]
async fn upsert_atoms_properties_only_preserves_empty_content() {
    let runtime = rt();
    let f = pack_via_registry(runtime.clone());
    let before = properties_only_atom_fixture(&runtime, &f, "empty-properties", "", "local").await;
    let canonical = before["id"].as_str().expect("canonical UUID");
    let compact = canonical.replace('-', "");
    assert_eq!(
        uuid::Uuid::parse_str(&compact).expect("complete compact UUID is accepted"),
        uuid::Uuid::parse_str(canonical).expect("canonical UUID")
    );
    for (id, properties) in [
        (canonical, json!({"marker": true})),
        (compact.as_str(), json!(["array", {"nested": true}, 3])),
        (canonical, json!("scalar string")),
        (canonical, json!(42)),
        (canonical, json!(true)),
        (canonical, json!({})),
        (canonical, Value::Null),
    ] {
        let response = f
            .dispatch(
                "knowledge.upsert_atoms",
                json!({"atoms": [{"id": id, "properties": properties}]}),
            )
            .await
            .expect("properties-only update accepts stored empty content");
        let after = f
            .dispatch("get", json!({"id": before["id"]}))
            .await
            .expect("generic get after empty-content property replacement");
        assert_eq!(response, json!({"created": 0, "updated": 1, "total": 1}));
        assert_properties_only_payload(&before, &after, properties);
    }
    let mut reader = runtime.sql().reader().await.expect("property reader");
    let properties = reader
        .query_scalar(SqlStatement {
            sql: "SELECT properties FROM knowledge_atoms WHERE id=?1".into(),
            params: vec![SqlValue::Text(before["id"].as_str().unwrap().into())],
            label: None,
        })
        .await
        .expect("read stored null property");
    assert!(matches!(properties, Some(SqlValue::Null)));
}
