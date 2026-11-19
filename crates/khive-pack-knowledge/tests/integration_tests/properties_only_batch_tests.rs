#[tokio::test]
async fn upsert_atoms_properties_only_mixed_batches_preserve_order_and_atomicity() {
    let valid_content = "one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty";
    for id_first in [false, true] {
        let runtime = rt();
        let f = pack_via_registry(runtime.clone());
        let before = properties_only_atom_fixture(&runtime, &f, "mixed-order", "", "local").await;
        let ordinary = json!({
            "slug": "mixed-order",
            "name": "Ordered replacement",
            "content": valid_content,
            "properties": {"winner": "ordinary"}
        });
        let id_row = json!({"id": before["id"], "properties": {"winner": "id"}});
        let mut atoms = if id_first {
            vec![id_row, ordinary]
        } else {
            vec![ordinary, id_row]
        };
        atoms.push(json!({
            "slug": "mixed-created",
            "name": "Created sibling",
            "content": valid_content,
            "properties": {"created_sibling": true}
        }));
        let response = f
            .dispatch("knowledge.upsert_atoms", json!({"atoms": atoms}))
            .await
            .expect("ordinary and ID forms compose in one successful batch");
        assert_eq!(response, json!({"created": 1, "updated": 2, "total": 3}));
        let after = f
            .dispatch("get", json!({"id": before["id"]}))
            .await
            .expect("read ordered mixed update");
        let mut expected = before.clone();
        expected["name"] = json!("Ordered replacement");
        expected["content"] = json!(valid_content);
        expected["tags"] = json!([]);
        expected["properties"] = if id_first {
            json!({"winner": "ordinary"})
        } else {
            json!({"winner": "id"})
        };
        expected["updated_at"] = after["updated_at"].clone();
        assert_eq!(
            after, expected,
            "last input must win for id_first={id_first}"
        );
        assert_ne!(after["updated_at"], before["updated_at"]);
        let created = f
            .dispatch("knowledge.get", json!({"id": "mixed-created"}))
            .await
            .expect("ordinary sibling must actually be created");
        assert_eq!(created["name"], "Created sibling");
        assert_eq!(created["content"], valid_content);
        assert_eq!(created["properties"], json!({"created_sibling": true}));
        assert_ne!(created["id"], before["id"]);
    }

    let runtime = rt();
    let f = pack_via_registry(runtime.clone());
    let before = properties_only_atom_fixture(&runtime, &f, "mixed-refusal", "", "local").await;
    let missing = "c8da3216-d789-497b-8e61-25574828cb3b";
    assert!(matches!(
        f.dispatch("get", json!({"id": missing})).await,
        Err(RuntimeError::NotFound(_))
    ));
    for (index, invalid_row) in [
        json!({"id": missing, "properties": {}}),
        json!({"id": before["id"], "properties": {"khive:secret_gate": "forbidden"}}),
        json!({"id": before["id"], "properties": {}, "content": valid_content}),
    ]
    .into_iter()
    .enumerate()
    {
        let prefix_slug = format!("mixed-refused-create-{index}");
        let result = f
            .dispatch(
                "knowledge.upsert_atoms",
                json!({"atoms": [
                    {
                        "slug": "mixed-refusal", "name": "Must not replace",
                        "content": valid_content, "properties": {"prefix": true}
                    },
                    {
                        "slug": prefix_slug, "name": "Must not create",
                        "content": valid_content
                    },
                    invalid_row
                ]}),
            )
            .await;
        if index == 0 {
            assert!(
                matches!(
                    result.as_ref().map_err(RuntimeError::refusal_source),
                    Err(RuntimeError::NotFound(_))
                ),
                "{result:?}"
            );
        } else {
            assert!(
                matches!(
                    result.as_ref().map_err(RuntimeError::refusal_source),
                    Err(RuntimeError::InvalidInput(_))
                ),
                "{result:?}"
            );
        }
        let after = f
            .dispatch("get", json!({"id": before["id"]}))
            .await
            .expect("ordinary prefix must remain unchanged");
        assert_eq!(after, before);
        assert!(matches!(
            f.dispatch("knowledge.get", json!({"id": prefix_slug}))
                .await,
            Err(RuntimeError::NotFound(_))
        ));
        assert!(matches!(
            f.dispatch("get", json!({"id": missing})).await,
            Err(RuntimeError::NotFound(_))
        ));
    }
    let result = f
        .dispatch(
            "knowledge.upsert_atoms",
            json!({"atoms": [
                {"id": before["id"], "properties": {"prefix": true}},
                {"slug": "mixed-short-refusal", "name": "Must not create", "content": "too short"}
            ]}),
        )
        .await;
    assert!(
        matches!(result.as_ref().map_err(RuntimeError::refusal_source), Err(RuntimeError::InvalidInput(message)) if message.contains("20 words")),
        "{result:?}"
    );
    let after = f
        .dispatch("get", json!({"id": before["id"]}))
        .await
        .expect("ID prefix must remain unchanged after ordinary content refusal");
    assert_eq!(after, before);
    assert!(matches!(
        f.dispatch("knowledge.get", json!({"id": "mixed-short-refusal"}))
            .await,
        Err(RuntimeError::NotFound(_))
    ));
}
