#[tokio::test]
async fn session_stream_append_validates_promoted_metadata_and_preserves_record_content() {
    let (_dir, registry) = fixture();
    for record in [
        json!({"name": " "}),
        json!({"properties": {"provider": 3}}),
        json!({"properties": {"provider_session_id": " "}}),
        json!({"properties": {"tags": [""]}}),
        json!({"tags": [], "properties": {"tags": [""]}}),
    ] {
        refused(
            &registry,
            "stream.append",
            json!({"stream": "sessions", "note_kind": "session", "record": record, "embed": false}),
        )
        .await;
        assert_note_count(&registry, 0).await;
    }
    let record = json!({
        "name": " Stream title ", "content": "", "tags": [" stream tag "],
        "properties": {"provider": " provider ", "provider_session_id": " remote id ",
            "tags": [" "], "extra": 9}
    });
    let result = registry
        .dispatch(
            "stream.append",
            json!({
                "stream": "sessions", "note_kind": "session", "record": record, "embed": false
            }),
        )
        .await
        .expect("valid effective session metadata");
    assert_eq!(
        result["seq"], 1,
        "refusals must not consume stream positions"
    );
    let session = resume(&registry, &result["id"]).await;
    assert_eq!(session["title"], " Stream title ");
    assert_eq!(session["content"], serde_json::to_string(&record).unwrap());
    assert_eq!(
        session["properties"],
        json!({
            "provider": " provider ", "provider_session_id": " remote id ",
            "tags": [" stream tag "], "extra": 9
        })
    );
    assert_note_count(&registry, 1).await;
}

#[tokio::test]
async fn session_creation_validation_does_not_restrict_observations() {
    let (_dir, registry) = fixture();
    let result = registry
        .dispatch(
            "create",
            json!({
                "kind": "observation", "content": " ",
                "properties": {"provider": 3, "provider_session_id": false}
            }),
        )
        .await
        .expect("session-owned metadata validation must not affect observation notes");
    let note = registry
        .dispatch("get", json!({"id": result["id"]}))
        .await
        .unwrap();
    assert_eq!(note["content"], " ");
    assert_eq!(
        note["properties"],
        json!({"provider": 3, "provider_session_id": false})
    );
    assert_note_count(&registry, 1).await;
}
