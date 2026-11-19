#[tokio::test]
async fn generic_and_store_reject_the_same_blank_session_fields_without_writes() {
    let (_dir, registry) = fixture();
    for blank in ["", " \n\t"] {
        let cases = [
            (
                json!({"content": blank}),
                json!({"content": blank}),
                "content must not be empty",
            ),
            (
                json!({"content": "body", "title": blank}),
                json!({"name": blank}),
                "title must be a non-empty string when provided",
            ),
            (
                json!({"content": "body", "provider": blank}),
                json!({"properties": {"provider": blank}}),
                "provider must be a non-empty string when provided",
            ),
            (
                json!({"content": "body", "provider_session_id": blank}),
                json!({"properties": {"provider_session_id": blank}}),
                "provider_session_id must be a non-empty string when provided",
            ),
            (
                json!({"content": "body", "tags": ["valid", blank]}),
                json!({"tags": ["valid", blank]}),
                "tags entries must be non-empty strings",
            ),
        ];
        for (store, generic, message) in cases {
            assert_eq!(
                refused(&registry, "session.store", store).await,
                format!("session.store: {message}")
            );
            for spelling in [false, true] {
                assert_eq!(
                    refused(&registry, "create", generic_args(spelling, generic.clone())).await,
                    format!("session: {message}")
                );
            }
            assert_note_count(&registry, 0).await;
        }
    }
}

#[tokio::test]
async fn generic_and_store_reject_wrong_types_in_owned_metadata_without_writes() {
    let (_dir, registry) = fixture();
    for (generic_field, store_field) in [
        ("name", "title"),
        ("provider", "provider"),
        ("provider_session_id", "provider_session_id"),
    ] {
        for value in [json!(3), json!(false), json!([]), json!({})] {
            let mut store = json!({"content": "body"});
            store[store_field] = value.clone();
            refused(&registry, "session.store", store).await;
            let mut fields = json!({});
            if generic_field == "name" {
                fields[generic_field] = value;
            } else {
                fields["properties"] = json!({});
                fields["properties"][generic_field] = value;
            }
            for spelling in [false, true] {
                refused(&registry, "create", generic_args(spelling, fields.clone())).await;
            }
        }
    }
    for tags in [
        json!("tag"),
        json!(3),
        json!(false),
        json!({}),
        json!([null]),
        json!([3]),
        json!([false]),
        json!([{}]),
    ] {
        refused(
            &registry,
            "session.store",
            json!({"content": "body", "tags": tags}),
        )
        .await;
        for spelling in [false, true] {
            refused(
                &registry,
                "create",
                generic_args(spelling, json!({"properties": {"tags": tags}})),
            )
            .await;
        }
    }
    for properties in [json!("metadata"), json!(3), json!(false), json!([])] {
        for spelling in [false, true] {
            refused(
                &registry,
                "create",
                generic_args(spelling, json!({"properties": properties})),
            )
            .await;
        }
    }
    assert_note_count(&registry, 0).await;
}
