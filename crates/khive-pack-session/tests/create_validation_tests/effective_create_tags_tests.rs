#[tokio::test]
async fn generic_session_validates_effective_tags_with_shared_create_precedence() {
    let (_dir, registry) = fixture();
    for spelling in [false, true] {
        for top_tags in [None, Some(Value::Null), Some(json!([]))] {
            for bad_tags in [json!([" "]), json!("wrong type"), json!([null])] {
                let mut fields = json!({"properties": {"tags": bad_tags}});
                if let Some(tags) = &top_tags {
                    fields["tags"] = tags.clone();
                }
                refused(&registry, "create", generic_args(spelling, fields)).await;
            }
        }
        refused(
            &registry,
            "create",
            generic_args(
                spelling,
                json!({"tags": [" "], "properties": {"tags": ["valid"]}}),
            ),
        )
        .await;
    }
    assert_note_count(&registry, 0).await;

    let mut created = 0;
    for spelling in [false, true] {
        for top_tags in [None, Some(Value::Null), Some(json!([]))] {
            let mut fields = json!({"properties": {"tags": [" kept "], "extra": 7}});
            if let Some(tags) = top_tags {
                fields["tags"] = tags;
            }
            let result = registry
                .dispatch("create", generic_args(spelling, fields))
                .await
                .expect("empty or absent top tags preserve valid property tags");
            let session = resume(&registry, &result["id"]).await;
            assert_eq!(session["tags"], json!([" kept "]));
            assert_eq!(
                session["properties"],
                json!({"tags": [" kept "], "extra": 7})
            );
            created += 1;
        }
        for discarded in [json!([" "]), json!(false), json!(3), json!({})] {
            let result = registry
                .dispatch(
                    "create",
                    generic_args(
                        spelling,
                        json!({"tags": [" replacement "], "properties": {"tags": discarded}}),
                    ),
                )
                .await
                .expect("valid nonempty top tags replace discarded property tags");
            assert_eq!(
                resume(&registry, &result["id"]).await["properties"],
                json!({"tags": [" replacement "]})
            );
            created += 1;
        }
    }
    assert_note_count(&registry, created).await;
}

#[tokio::test]
async fn session_hook_and_writers_use_the_same_effective_create_tags() {
    let (_dir, runtime, registry) = fixture_with_runtime();
    let hook = registry.find_kind_hook("session").expect("session hook");
    // Mutation control: prefer properties.tags inside effective_create_tags.
    // The first case must then fail hook admission, and the existing KG test
    // create_note_top_level_tags_wins_over_properties_tags_conflict must fail
    // its stored-tag assertion. Expected tags here never call the helper.
    for (index, (top_tags, property_tags, expected)) in [
        (Some(json!([" top "])), json!([" "]), json!([" top "])),
        (Some(json!([])), json!([" nested "]), json!([" nested "])),
        (None, json!([" nested "]), json!([" nested "])),
    ]
    .into_iter()
    .enumerate()
    {
        let mut args = generic_args(
            false,
            json!({"properties": {"tags": property_tags, "extra": index}}),
        );
        if let Some(tags) = top_tags {
            args["tags"] = tags;
        }
        let mut validated = args.clone();
        hook.prepare_create(&runtime, &mut validated)
            .await
            .expect("hook validates exactly the tags selected for storage");
        assert_eq!(
            validated, args,
            "validation must not rewrite caller metadata"
        );
        let created = registry
            .dispatch("create", args.clone())
            .await
            .expect("ordinary writer accepts the hook's unchanged input");
        let appended = registry
            .dispatch(
                "stream.append",
                json!({"stream": "same-tags", "note_kind": "session", "record": args, "embed": false}),
            )
            .await
            .expect("stream writer accepts the same promoted metadata");
        for result in [created, appended] {
            let session = resume(&registry, &result["id"]).await;
            assert_eq!(session["tags"], expected);
            assert_eq!(session["properties"]["tags"], expected);
            assert_eq!(session["properties"]["extra"], index);
        }
    }
    assert_note_count(&registry, 6).await;

    for top_tags in [None, Some(json!([])), Some(json!([" "]))] {
        let mut args = generic_args(false, json!({"properties": {"tags": [" "]}}));
        if let Some(tags) = top_tags {
            args["tags"] = tags;
        }
        let mut validated = args.clone();
        let error = hook
            .prepare_create(&runtime, &mut validated)
            .await
            .unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidInput(_)));
        refused(&registry, "create", args.clone()).await;
        refused(
            &registry,
            "stream.append",
            json!({"stream": "same-tags", "note_kind": "session", "record": args, "embed": false}),
        )
        .await;
    }
    assert_note_count(&registry, 6).await;
}
