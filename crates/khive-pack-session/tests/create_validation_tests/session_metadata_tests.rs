#[tokio::test]
async fn generic_keyed_session_preserves_metadata_and_resumes_and_exports() {
    let (_dir, registry) = fixture();
    for spelling in [false, true] {
        let key = format!("session-validation-{spelling}");
        refused(
            &registry,
            "create",
            generic_args(
                spelling,
                json!({"key": key, "content": " ", "embed": false}),
            ),
        )
        .await;
        let properties = json!({
            "provider": " provider ", "provider_session_id": " remote id ",
            "tags": [" first ", "second"], "extra": {"nested": [1, true]}
        });
        let result = registry
            .dispatch(
                "create",
                generic_args(
                    spelling,
                    json!({
                        "key": key, "embed": false, "name": " Title ",
                        "content": " \noriginal body\n ", "salience": 0.4,
                        "properties": properties
                    }),
                ),
            )
            .await
            .expect("refused creation did not reserve the key");
        let note = registry
            .dispatch("get", json!({"key": key, "kind": "session"}))
            .await
            .expect("lookup by key");
        assert_eq!(note["name"], " Title ");
        assert_eq!(note["content"], " \noriginal body\n ");
        assert_eq!(note["properties"], properties);
        assert_eq!(note["salience"], 0.4);

        let session = resume(&registry, &result["id"]).await;
        assert_eq!(session["id"], note["id"]);
        assert_eq!(session["title"], " Title ");
        assert_eq!(session["content"], " \noriginal body\n ");
        assert_eq!(session["provider"], " provider ");
        assert_eq!(session["provider_session_id"], " remote id ");
        assert_eq!(session["tags"], properties["tags"]);
        assert_eq!(session["properties"], properties);
        let exported = registry
            .dispatch(
                "session.export",
                json!({"id": result["id"], "format": "json"}),
            )
            .await
            .expect("export created session");
        assert_eq!(exported["session"], session);
    }
    assert_note_count(&registry, 2).await;
}

#[tokio::test]
async fn session_creation_accepts_omitted_and_null_optionals_without_trimming() {
    let (_dir, registry) = fixture();
    for fields in [
        json!({"content": " body "}),
        json!({"content": " body ", "title": null, "provider": null,
            "provider_session_id": null, "tags": null}),
    ] {
        let result = registry.dispatch("session.store", fields).await.unwrap();
        assert_eq!(result["session"]["title"], Value::Null);
        assert_eq!(result["session"]["provider"], Value::Null);
        assert_eq!(result["session"]["provider_session_id"], Value::Null);
        assert_eq!(result["session"]["tags"], json!([]));
        assert_eq!(result["session"]["content"], " body ");
    }
    let stored = registry
        .dispatch(
            "session.store",
            json!({
                "content": " body ", "title": " title ", "provider": " provider ",
                "provider_session_id": " remote id ", "tags": [" tag "]
            }),
        )
        .await
        .unwrap();
    assert_eq!(stored["session"]["title"], " title ");
    assert_eq!(stored["session"]["provider"], " provider ");
    assert_eq!(stored["session"]["provider_session_id"], " remote id ");
    assert_eq!(stored["session"]["tags"], json!([" tag "]));
    assert_eq!(stored["session"]["content"], " body ");

    for spelling in [false, true] {
        for fields in [
            json!({}),
            json!({"name": null, "tags": null, "properties": {
                "provider": null, "provider_session_id": null, "tags": null, "extra": 7
            }}),
        ] {
            let result = registry
                .dispatch("create", generic_args(spelling, fields.clone()))
                .await
                .expect("optional null values are absent for validation");
            let session = resume(&registry, &result["id"]).await;
            assert_eq!(session["title"], Value::Null);
            assert_eq!(session["provider"], Value::Null);
            assert_eq!(session["provider_session_id"], Value::Null);
            assert_eq!(session["tags"], json!([]));
            if let Some(properties) = fields.get("properties") {
                assert_eq!(&session["properties"], properties);
            }
        }
    }
    assert_note_count(&registry, 7).await;
}
