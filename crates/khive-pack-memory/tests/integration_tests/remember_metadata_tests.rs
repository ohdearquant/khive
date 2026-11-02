#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_recall_memory_type_filter() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "episodic event about meeting with Alice",
                "memory_type": "episodic",
                "salience": 0.7
            }),
        )
        .await
        .expect("episodic remember");

    let semantic = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "semantic fact about meeting protocols",
                "memory_type": "semantic",
                "salience": 0.7
            }),
        )
        .await
        .expect("semantic remember");
    let semantic_id = semantic["id"].as_str().unwrap().to_string();

    let filtered = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "meeting", "memory_type": "semantic" }),
        )
        .await
        .expect("recall with filter");

    let hits = filtered.as_array().expect("array");
    assert!(!hits.is_empty(), "got results with memory_type filter");
    for hit in hits {
        let mt = hit["memory_type"].as_str().unwrap_or("");
        assert_eq!(mt, "semantic", "only semantic results returned");
    }
    let ids: Vec<&str> = hits.iter().map(|h| h["id"].as_str().unwrap()).collect();
    assert!(
        ids.contains(&semantic_id.as_str()),
        "semantic note is in results"
    );
}

#[test]
fn test_memory_pack_requires_kg() {
    assert_eq!(MemoryPack::REQUIRES, &["kg"]);
    assert_eq!(MemoryPack::NAME, "memory");
    assert_eq!(MemoryPack::NOTE_KINDS, &["memory"]);
}

/// Regression test: source_id must NOT be stored in note properties.
/// The annotates edge is the sole authorized source reference (ADR-036 §4).
#[tokio::test]
async fn test_remember_source_id_not_in_properties() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    // Create a real entity to use as the source (source_id must exist in namespace).
    let source = registry
        .dispatch(
            "create",
            json!({
                "kind": "person",
                "name": "Alice",
                "description": "test source person"
            }),
        )
        .await
        .expect("create source entity");
    let source_uuid = source["id"].as_str().unwrap().to_string();

    let result = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "memory with a source",
                "source": source_uuid
            }),
        )
        .await
        .expect("remember with source_id");

    let note_id: Uuid = result["id"].as_str().unwrap().parse().expect("valid uuid");

    let note_store = rt
        .notes(&rt.authorize(Namespace::local()).unwrap())
        .expect("note store");
    let note = note_store
        .get_note(note_id)
        .await
        .expect("get note")
        .expect("note exists");

    if let Some(props) = &note.properties {
        assert!(
            props.get("source_id").is_none(),
            "source_id must not be stored in note properties; got: {props:?}"
        );
    }
}

/// ADR-021 §4 (F108): decay_factor >= 0 is the only constraint — no upper cap.
/// Values above 1.0 are valid (fast-fading memories with very short effective half-lives).
/// Negative values are rejected with InvalidInput.
#[tokio::test]
async fn test_remember_decay_factor_no_upper_cap() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    // decay_factor = 5.0 is valid — no upper cap per ADR-021 §4
    let result = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "memory with high decay rate",
                "decay": 5.0
            }),
        )
        .await
        .expect("remember with decay_factor > 1.0 should succeed");

    let note_id: Uuid = result["id"].as_str().unwrap().parse().expect("valid uuid");

    let note_store = rt
        .notes(&rt.authorize(Namespace::local()).unwrap())
        .expect("note store");
    let note = note_store
        .get_note(note_id)
        .await
        .expect("get note")
        .expect("note exists");

    let df = note.decay_factor.unwrap_or(0.0);
    // Stored value must match exactly (not clamped to 1.0)
    assert!(
        (df - 5.0).abs() < 1e-10,
        "decay_factor should be stored as-is (5.0), got {df}"
    );
}

/// ADR-021 §4 (F108): negative decay_factor is rejected.
#[tokio::test]
async fn test_remember_decay_factor_negative_rejected() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    let result = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "memory with negative decay",
                "decay": -0.1
            }),
        )
        .await;

    assert!(result.is_err(), "negative decay_factor must be rejected");
}

/// ADR-021 §4 (F107): remember always writes memory_type to properties.
/// When memory_type is absent, it defaults to "episodic".
#[tokio::test]
async fn test_remember_default_memory_type_written_to_properties() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    let result = registry
        .dispatch(
            "memory.remember",
            json!({ "content": "memory without explicit type" }),
        )
        .await
        .expect("remember without memory_type");

    let note_id: Uuid = result["id"].as_str().unwrap().parse().expect("valid uuid");

    // The response must carry memory_type
    assert_eq!(
        result["memory_type"].as_str(),
        Some("episodic"),
        "response must include default memory_type"
    );

    let note_store = rt
        .notes(&rt.authorize(Namespace::local()).unwrap())
        .expect("note store");
    let note = note_store
        .get_note(note_id)
        .await
        .expect("get note")
        .expect("note exists");

    let stored_type = note
        .properties
        .as_ref()
        .and_then(|p| p.get("memory_type"))
        .and_then(|v| v.as_str());
    assert_eq!(
        stored_type,
        Some("episodic"),
        "memory_type must be written to properties even when not supplied"
    );
}

/// ADR-021 §4 (F109): invalid UUID string in source_id is rejected with an error.
#[tokio::test]
async fn test_remember_invalid_source_id_uuid_rejected() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    let result = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "memory with bad source_id",
                "source": "not-a-valid-uuid"
            }),
        )
        .await;

    assert!(
        result.is_err(),
        "invalid source_id UUID must cause an error, got: {result:?}"
    );
}

/// ADR-021 §4 (F108): salience outside [0, 1] is rejected.
#[tokio::test]
async fn test_remember_salience_out_of_range_rejected() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    let neg = registry
        .dispatch(
            "memory.remember",
            json!({ "content": "test", "salience": -0.1 }),
        )
        .await;
    assert!(neg.is_err(), "negative salience must be rejected");

    let rt2 = make_runtime();
    let registry2 = make_registry(rt2);
    let above = registry2
        .dispatch(
            "memory.remember",
            json!({ "content": "test", "salience": 1.1 }),
        )
        .await;
    assert!(above.is_err(), "salience > 1 must be rejected");
}

/// `properties` passed to `memory.remember` are stored beside `memory_type`
/// and `tags`, and `get` returns them in its `properties` object.
#[tokio::test]
async fn test_remember_properties_round_trip_through_get() {
    let rt = make_runtime();
    let registry = make_registry_with_actor(rt, "alice");

    let created = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "the nightly export finishes before six",
                "memory_type": "semantic",
                "tags": ["ops"],
                "properties": {
                    "author": "alice",
                    "confidence": 0.75,
                    "reviewed": true,
                    "sources": ["runbook", "pager"]
                }
            }),
        )
        .await
        .expect("remember with properties");
    let id = created["id"].as_str().expect("response has id").to_owned();

    let got = registry
        .dispatch("get", json!({ "id": id }))
        .await
        .expect("get by id");

    let properties = &got["properties"];
    assert_eq!(properties["memory_type"], "semantic");
    assert_eq!(properties["tags"], json!(["ops"]));
    assert_eq!(properties["author"], "alice");
    assert_eq!(properties["confidence"], 0.75);
    assert_eq!(properties["reviewed"], true);
    assert_eq!(properties["sources"], json!(["runbook", "pager"]));
}

/// A recall hit reports the note's namespace, its tags and the whole stored
/// properties object.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_recall_hit_returns_namespace_tags_and_properties() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    let created = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "quartzite flooring needs sealing every spring",
                "memory_type": "semantic",
                "tags": ["home", "care"],
                "properties": { "author": "casey", "revision": 2 }
            }),
        )
        .await
        .expect("remember with properties");
    let id = created["id"].as_str().expect("response has id").to_owned();

    let recalled = registry
        .dispatch("memory.recall", json!({ "query": "quartzite flooring" }))
        .await
        .expect("recall");
    let hits = recalled.as_array().expect("recall returns an array");
    let hit = hits
        .iter()
        .find(|h| h["id"].as_str() == Some(id.as_str()))
        .expect("the stored memory is recalled");

    assert_eq!(hit["namespace"], "local");
    assert_eq!(hit["tags"], json!(["home", "care"]));
    assert_eq!(hit["properties"]["author"], "casey");
    assert_eq!(hit["properties"]["revision"], 2);
    assert_eq!(hit["properties"]["memory_type"], "semantic");
    assert_eq!(hit["properties"]["tags"], hit["tags"]);
}

/// A remember without `properties` stores only `memory_type` and `tags`, and a
/// recall hit reports exactly that object: nothing is added to the stored shape.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_remember_without_properties_keeps_the_stored_shape() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    let created = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "granite countertops resist heat well",
                "memory_type": "semantic",
                "tags": ["kitchen"],
                "namespace": "local"
            }),
        )
        .await
        .expect("remember without properties");
    let id = created["id"].as_str().expect("response has id").to_owned();

    let recalled = registry
        .dispatch("memory.recall", json!({ "query": "granite countertops" }))
        .await
        .expect("recall");
    let hits = recalled.as_array().expect("recall returns an array");
    let hit = hits
        .iter()
        .find(|h| h["id"].as_str() == Some(id.as_str()))
        .expect("the stored memory is recalled");

    assert_eq!(
        hit["properties"],
        json!({ "memory_type": "semantic", "tags": ["kitchen"] })
    );
    assert_eq!(hit["tags"], json!(["kitchen"]));
    assert_eq!(hit["namespace"], "local");
}

/// A malformed `properties` value is refused through dispatch with a message
/// that names the offending key and the rule.
#[tokio::test]
async fn test_remember_refuses_invalid_properties_through_dispatch() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    let reserved = registry
        .dispatch(
            "memory.remember",
            json!({ "content": "reserved key", "properties": { "tags": ["x"] } }),
        )
        .await
        .expect_err("a reserved key is refused")
        .to_string();
    assert!(reserved.contains("tags"), "{reserved}");
    assert!(reserved.contains("reserved"), "{reserved}");

    let not_object = registry
        .dispatch(
            "memory.remember",
            json!({ "content": "not an object", "properties": "flat" }),
        )
        .await
        .expect_err("a non-object is refused")
        .to_string();
    assert!(not_object.contains("must be a JSON object"), "{not_object}");

    let nested = registry
        .dispatch(
            "memory.remember",
            json!({ "content": "nested", "properties": { "inner": { "a": 1 } } }),
        )
        .await
        .expect_err("a nested object is refused")
        .to_string();
    assert!(nested.contains("inner"), "{nested}");
    assert!(
        nested.contains("nested objects are not allowed"),
        "{nested}"
    );
}

/// The unresolved flag follows the caller, not the namespace argument: an anonymous caller
/// that names a namespace is still unattributed, so its row and response carry the flag.
#[tokio::test]
async fn test_anonymous_remember_with_explicit_namespace_is_flagged_unresolved() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    for memory_type in ["semantic", "episodic"] {
        let result = registry
            .dispatch(
                "memory.remember",
                json!({
                    "content": format!("anonymous {memory_type} memory naming the shared namespace"),
                    "memory_type": memory_type,
                    "namespace": "local",
                }),
            )
            .await
            .expect("memory.remember must succeed");
        assert_eq!(
            result["attribution"], "unresolved",
            "{memory_type}: {result}"
        );
        let note_id = result["id"].as_str().expect("response has id").to_owned();

        let got = registry
            .dispatch("get", json!({ "id": note_id }))
            .await
            .expect("get by UUID must succeed");
        assert_eq!(got["namespace"], "local", "{memory_type}: {got}");
        assert_eq!(
            got["properties"]["attribution"], "unresolved",
            "{memory_type}: {got}"
        );
    }
}
