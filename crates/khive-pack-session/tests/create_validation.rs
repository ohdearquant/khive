//! Public creation routes share session validation without normalizing metadata.

use std::sync::Arc;

use khive_pack_kg::KgPack;
use khive_pack_session::SessionPack;
use khive_runtime::{
    AllowAllGate, BackendId, KhiveRuntime, Namespace, RuntimeConfig, RuntimeError, VerbRegistry,
    VerbRegistryBuilder,
};
use serde_json::{json, Value};
use tempfile::TempDir;

fn fixture() -> (TempDir, VerbRegistry) {
    let (dir, _runtime, registry) = fixture_with_runtime();
    (dir, registry)
}

fn fixture_with_runtime() -> (TempDir, KhiveRuntime, VerbRegistry) {
    let dir = TempDir::new().expect("tempdir");
    let runtime = KhiveRuntime::new(RuntimeConfig {
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: khive_runtime::config::resolve_default_display_timezone(),
        events_split: None,
        db_path: Some(dir.path().join("session-validation.db")),
        blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".into(), "session".into()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        exec: Default::default(),
    })
    .expect("file-backed runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(SessionPack::new(runtime.clone()));
    let registry = builder.build().expect("KG and session registry");
    runtime.install_edge_rules(registry.all_edge_rules());
    (dir, runtime, registry)
}

fn generic_args(explicit_note_kind: bool, fields: Value) -> Value {
    let mut args = if explicit_note_kind {
        json!({"kind": "note", "note_kind": "session", "content": "body"})
    } else {
        json!({"kind": "session", "content": "body"})
    };
    args.as_object_mut()
        .unwrap()
        .extend(fields.as_object().unwrap().clone());
    args
}

async fn refused(registry: &VerbRegistry, verb: &str, args: Value) -> String {
    match registry.dispatch(verb, args.clone()).await {
        Err(RuntimeError::InvalidInput(message)) => message,
        other => panic!("expected InvalidInput for {verb}({args}), got {other:?}"),
    }
}

async fn assert_note_count(registry: &VerbRegistry, expected: usize) {
    let notes = registry
        .dispatch("list", json!({"kind": "note", "limit": 100}))
        .await
        .expect("list notes");
    assert_eq!(notes["items"].as_array().unwrap().len(), expected);
}

async fn resume(registry: &VerbRegistry, id: &Value) -> Value {
    let result = registry
        .dispatch("session.resume", json!({"id": id}))
        .await
        .expect("resume created session");
    assert_eq!(result["ok"], true);
    result["session"].clone()
}

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
