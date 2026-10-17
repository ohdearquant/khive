// ── Type-differentiated default tests — production path (#84) ───────────────
//
// These tests dispatch through the real handler and assert stored note values
// (via the response) for the omitted/explicit default matrix.  A revert of the
// production defaults in remember.rs would cause these to fail.

/// Omitting salience and decay_factor for an episodic memory must store 0.3 / 0.02.
#[tokio::test]
async fn test_remember_episodic_defaults_stored() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    let result = registry
        .dispatch(
            "memory.remember",
            json!({ "content": "episodic default test", "memory_type": "episodic" }),
        )
        .await
        .expect("memory.remember must succeed");

    let salience = result["salience"].as_f64().expect("salience field present");
    let decay = result["decay_factor"]
        .as_f64()
        .expect("decay_factor field present");
    assert!(
        (salience - 0.3).abs() < 1e-12,
        "episodic default salience must be 0.3, got {salience}"
    );
    assert!(
        (decay - 0.02).abs() < 1e-12,
        "episodic default decay_factor must be 0.02, got {decay}"
    );
}

/// Omitting memory_type defaults to episodic and applies episodic defaults.
#[tokio::test]
async fn test_remember_omitted_memory_type_uses_episodic_defaults() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    let result = registry
        .dispatch(
            "memory.remember",
            json!({ "content": "no memory_type supplied" }),
        )
        .await
        .expect("memory.remember must succeed");

    let mt = result["memory_type"].as_str().expect("memory_type present");
    assert_eq!(
        mt, "episodic",
        "omitted memory_type must default to episodic"
    );
    let salience = result["salience"].as_f64().expect("salience present");
    let decay = result["decay_factor"]
        .as_f64()
        .expect("decay_factor present");
    assert!(
        (salience - 0.3).abs() < 1e-12,
        "omitted-type default salience must be 0.3, got {salience}"
    );
    assert!(
        (decay - 0.02).abs() < 1e-12,
        "omitted-type default decay_factor must be 0.02, got {decay}"
    );
}

/// Omitting salience and decay_factor for a semantic memory must store 0.5 / 0.005.
#[tokio::test]
async fn test_remember_semantic_defaults_stored() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    let result = registry
        .dispatch(
            "memory.remember",
            json!({ "content": "semantic default test", "memory_type": "semantic" }),
        )
        .await
        .expect("memory.remember must succeed");

    let salience = result["salience"].as_f64().expect("salience present");
    let decay = result["decay_factor"]
        .as_f64()
        .expect("decay_factor present");
    assert!(
        (salience - 0.5).abs() < 1e-12,
        "semantic default salience must be 0.5, got {salience}"
    );
    assert!(
        (decay - 0.005).abs() < 1e-12,
        "semantic default decay_factor must be 0.005, got {decay}"
    );
}

/// Explicit salience=0.5 with episodic type must store exactly 0.5 (old flat default wins explicitly).
#[tokio::test]
async fn test_remember_explicit_salience_overrides_episodic_default() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    let result = registry
        .dispatch(
            "memory.remember",
            json!({ "content": "explicit salience test", "memory_type": "episodic", "salience": 0.5 }),
        )
        .await
        .expect("memory.remember must succeed");

    let salience = result["salience"].as_f64().expect("salience present");
    assert!(
        (salience - 0.5).abs() < 1e-12,
        "explicit salience=0.5 must be stored as-is, not replaced by episodic default 0.3; got {salience}"
    );
}

/// Explicit decay_factor=0.01 with episodic type must store exactly 0.01.
#[tokio::test]
async fn test_remember_explicit_decay_overrides_episodic_default() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    let result = registry
        .dispatch(
            "memory.remember",
            json!({ "content": "explicit decay test", "memory_type": "episodic", "decay_factor": 0.01 }),
        )
        .await
        .expect("memory.remember must succeed");

    let decay = result["decay_factor"]
        .as_f64()
        .expect("decay_factor present");
    assert!(
        (decay - 0.01).abs() < 1e-12,
        "explicit decay_factor=0.01 must be stored as-is, not replaced by episodic default 0.02; got {decay}"
    );
}

/// Legacy note (created via KG create_note with no properties.memory_type, no salience,
/// no decay_factor) must be returned by memory.recall(memory_type="episodic") because
/// the resolved memory_type defaults to "episodic" when no stored value is present.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_recall_legacy_note_no_memory_type_returned_as_episodic() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    // Create a bare memory note with no properties.memory_type, no salience, no decay —
    // simulates a note written before the type-differentiated defaults PR.
    let tok = rt.authorize(Namespace::local()).unwrap();
    let legacy_note = rt
        .create_note(
            &tok,
            "memory",
            None,
            "legacy note about transformer attention heads no memory type",
            None,   // no salience
            None,   // no properties (therefore no memory_type)
            vec![], // no annotates edges
        )
        .await
        .expect("create legacy note");
    let legacy_id = legacy_note.id.to_string();

    // recall with explicit memory_type="episodic" must include the legacy note because
    // resolved memory_type defaults to "episodic" when properties.memory_type is absent.
    let result = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": "transformer attention heads no memory type",
                "memory_type": "episodic",
                "limit": 10
            }),
        )
        .await
        .expect("memory.recall must succeed");

    let hits = result.as_array().expect("recall returns array");
    let returned_ids: Vec<&str> = hits
        .iter()
        .map(|h| h["id"].as_str().unwrap_or(""))
        .collect();
    assert!(
        returned_ids.contains(&legacy_id.as_str()),
        "legacy note with no stored memory_type must appear in recall(memory_type=\"episodic\"); \
         returned ids: {returned_ids:?}"
    );

    // Recall hits must carry resolved (read-model) values, not raw stored NULLs.
    // Consumers such as ranking explanations and brain.auto_feedback rely on these fields.
    let legacy_hit = hits
        .iter()
        .find(|h| h["id"].as_str().unwrap_or("") == legacy_id)
        .expect("legacy hit present");

    let hit_memory_type = legacy_hit["memory_type"]
        .as_str()
        .expect("memory_type field present in hit");
    assert_eq!(
        hit_memory_type, "episodic",
        "recall hit memory_type must be resolved to \"episodic\" for legacy note; got {hit_memory_type:?}"
    );

    let hit_salience = legacy_hit["salience"]
        .as_f64()
        .expect("salience field present in hit");
    assert!(
        (hit_salience - 0.3).abs() < 1e-12,
        "recall hit salience must be episodic default 0.3 for legacy note; got {hit_salience}"
    );

    let hit_decay = legacy_hit["decay_factor"]
        .as_f64()
        .expect("decay_factor field present in hit");
    assert!(
        (hit_decay - 0.02).abs() < 1e-12,
        "recall hit decay_factor must be episodic default 0.02 for legacy note; got {hit_decay}"
    );
}
