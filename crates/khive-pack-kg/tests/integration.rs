//! Integration tests for khive-pack-kg.
//!
//! Tests exercise the full dispatch path through KgPack: params deserialize,
//! validation, runtime call, and JSON response. All tests use an in-memory
//! runtime so there is no I/O dependency.

use khive_pack_kg::KgPack;
use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, RuntimeError};
use serde_json::{json, Value};

// ---- Helpers ----

fn pack() -> KgPack {
    let rt = KhiveRuntime::memory().expect("in-memory runtime must succeed");
    KgPack::new(rt)
}

fn is_invalid_input(err: &RuntimeError) -> bool {
    matches!(err, RuntimeError::InvalidInput(_))
}

fn invalid_input_message(err: &RuntimeError) -> &str {
    match err {
        RuntimeError::InvalidInput(msg) => msg.as_str(),
        other => panic!("expected InvalidInput, got: {other:?}"),
    }
}

// ---- PackRuntime trait: verbs() and unknown-verb dispatch ----

#[test]
fn pack_verbs_returns_eleven() {
    let pack = pack();
    assert_eq!(
        pack.verbs().len(),
        11,
        "KgPack must expose exactly 11 verbs"
    );
}

#[test]
fn pack_verbs_names_are_correct() {
    let pack = pack();
    let names: Vec<&str> = pack.verbs().iter().map(|v| v.name).collect();
    for expected in &[
        "create",
        "get",
        "list",
        "update",
        "delete",
        "merge",
        "search",
        "link",
        "neighbors",
        "traverse",
        "query",
    ] {
        assert!(names.contains(expected), "verbs() missing {expected:?}");
    }
}

#[tokio::test]
async fn dispatch_unknown_verb_returns_error() {
    let pack = pack();
    let err = pack.dispatch("frobnicate", json!({})).await.unwrap_err();
    assert!(is_invalid_input(&err), "unknown verb must be InvalidInput");
    assert!(
        invalid_input_message(&err).contains("frobnicate"),
        "error message must name the unknown verb"
    );
}

// ---- Kind validation via create: entities ----

#[tokio::test]
async fn create_entity_valid_kind_concept_succeeds() {
    let pack = pack();
    let result = pack
        .dispatch(
            "create",
            json!({
                "kind": "entity",
                "name": "Attention Is All You Need",
                "entity_kind": "concept"
            }),
        )
        .await;
    assert!(
        result.is_ok(),
        "valid entity_kind 'concept' must succeed: {:?}",
        result
    );
}

#[tokio::test]
async fn create_entity_alias_paper_normalizes_to_document() {
    let pack = pack();
    let result = pack
        .dispatch(
            "create",
            json!({
                "kind": "entity",
                "name": "Attention Paper",
                "entity_kind": "paper"
            }),
        )
        .await
        .expect("alias 'paper' must succeed");
    // The stored kind must be the canonical "document" (field is "kind" in the entity struct)
    let kind = result.get("kind").and_then(Value::as_str);
    assert_eq!(
        kind,
        Some("document"),
        "alias 'paper' must normalize to 'document'; got: {result}"
    );
}

#[tokio::test]
async fn create_entity_invalid_kind_gadget_returns_invalid_input_with_valid_list() {
    let pack = pack();
    let err = pack
        .dispatch(
            "create",
            json!({
                "kind": "entity",
                "name": "Widget",
                "entity_kind": "gadget"
            }),
        )
        .await
        .unwrap_err();
    assert!(
        is_invalid_input(&err),
        "invalid entity_kind must be InvalidInput"
    );
    let msg = invalid_input_message(&err);
    assert!(
        msg.contains("concept") || msg.contains("document"),
        "error must list valid entity kinds; got: {msg}"
    );
}

#[tokio::test]
async fn create_entity_missing_name_returns_invalid_input() {
    let pack = pack();
    let err = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "entity_kind": "concept"}),
        )
        .await
        .unwrap_err();
    assert!(
        is_invalid_input(&err),
        "missing 'name' for entity must be InvalidInput"
    );
    assert!(
        invalid_input_message(&err).contains("name"),
        "error must mention missing 'name'"
    );
}

#[tokio::test]
async fn create_entity_missing_entity_kind_returns_invalid_input() {
    let pack = pack();
    let err = pack
        .dispatch("create", json!({"kind": "entity", "name": "Orphan"}))
        .await
        .unwrap_err();
    assert!(
        is_invalid_input(&err),
        "missing entity_kind must be InvalidInput"
    );
    assert!(
        invalid_input_message(&err).contains("entity_kind"),
        "error must mention missing 'entity_kind'"
    );
}

// ---- Kind validation via create: notes ----

#[tokio::test]
async fn create_note_valid_kind_observation_succeeds() {
    let pack = pack();
    let result = pack
        .dispatch(
            "create",
            json!({
                "kind": "note",
                "content": "Transformers use self-attention.",
                "note_kind": "observation"
            }),
        )
        .await;
    assert!(
        result.is_ok(),
        "valid note_kind 'observation' must succeed: {:?}",
        result
    );
}

#[tokio::test]
async fn create_note_no_kind_defaults_to_observation() {
    // Omitting note_kind must default to "observation" (handler logic lines 207-210)
    let pack = pack();
    let result = pack
        .dispatch(
            "create",
            json!({
                "kind": "note",
                "content": "Default kind test."
            }),
        )
        .await
        .expect("note without note_kind must default to 'observation'");
    let stored_kind = result.get("kind").and_then(Value::as_str);
    assert_eq!(
        stored_kind,
        Some("observation"),
        "default note_kind must be 'observation'; got: {result}"
    );
}

#[tokio::test]
async fn create_note_alias_obs_works() {
    let pack = pack();
    let result = pack
        .dispatch(
            "create",
            json!({
                "kind": "note",
                "content": "Alias obs test.",
                "note_kind": "obs"
            }),
        )
        .await;
    assert!(result.is_ok(), "alias 'obs' must succeed: {:?}", result);
}

#[tokio::test]
async fn create_note_alias_finding_normalizes_to_insight() {
    let pack = pack();
    let result = pack
        .dispatch(
            "create",
            json!({
                "kind": "note",
                "content": "A key finding.",
                "note_kind": "finding"
            }),
        )
        .await
        .expect("alias 'finding' must succeed");
    let stored_kind = result.get("kind").and_then(Value::as_str);
    assert_eq!(
        stored_kind,
        Some("insight"),
        "alias 'finding' must normalize to 'insight'; got: {result}"
    );
}

#[tokio::test]
async fn create_note_invalid_kind_garbage_returns_invalid_input_with_valid_list() {
    let pack = pack();
    let err = pack
        .dispatch(
            "create",
            json!({
                "kind": "note",
                "content": "Some content.",
                "note_kind": "garbage"
            }),
        )
        .await
        .unwrap_err();
    assert!(
        is_invalid_input(&err),
        "invalid note_kind must be InvalidInput"
    );
    let msg = invalid_input_message(&err);
    assert!(
        msg.contains("observation") || msg.contains("insight"),
        "error must list valid note kinds; got: {msg}"
    );
}

#[tokio::test]
async fn create_note_missing_content_returns_invalid_input() {
    let pack = pack();
    let err = pack
        .dispatch(
            "create",
            json!({"kind": "note", "note_kind": "observation"}),
        )
        .await
        .unwrap_err();
    assert!(
        is_invalid_input(&err),
        "missing 'content' for note must be InvalidInput"
    );
    assert!(
        invalid_input_message(&err).contains("content"),
        "error must mention missing 'content'"
    );
}

#[tokio::test]
async fn create_unknown_kind_returns_invalid_input() {
    let pack = pack();
    let err = pack
        .dispatch("create", json!({"kind": "sprocket"}))
        .await
        .unwrap_err();
    assert!(
        is_invalid_input(&err),
        "unknown top-level kind must be InvalidInput"
    );
    let msg = invalid_input_message(&err);
    assert!(
        msg.contains("entity") && msg.contains("note"),
        "error must list valid top-level kinds; got: {msg}"
    );
}

// ---- Basic verb dispatch: create → get roundtrip ----

#[tokio::test]
async fn create_entity_then_get_roundtrip() {
    let pack = pack();

    let created = pack
        .dispatch(
            "create",
            json!({
                "kind": "entity",
                "name": "LoRA",
                "entity_kind": "concept",
                "description": "Low-Rank Adaptation"
            }),
        )
        .await
        .expect("create must succeed");

    let id = created
        .get("id")
        .and_then(Value::as_str)
        .expect("create response must have 'id'");

    let fetched = pack
        .dispatch("get", json!({"id": id}))
        .await
        .expect("get by id must succeed");

    assert_eq!(
        fetched.get("kind").and_then(Value::as_str),
        Some("entity"),
        "get must return kind=entity"
    );
    let data = fetched.get("data").expect("get response must have 'data'");
    assert_eq!(
        data.get("name").and_then(Value::as_str),
        Some("LoRA"),
        "entity name must roundtrip"
    );
    assert_eq!(
        data.get("kind").and_then(Value::as_str),
        Some("concept"),
        "entity kind must roundtrip (field is 'kind' in the entity struct)"
    );
}

#[tokio::test]
async fn get_nonexistent_id_returns_not_found() {
    let pack = pack();
    let err = pack
        .dispatch("get", json!({"id": "00000000-0000-0000-0000-000000000001"}))
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::NotFound(_)),
        "get on nonexistent id must be NotFound, got: {err:?}"
    );
}

// ---- Basic verb dispatch: create → list ----

#[tokio::test]
async fn create_entity_then_list_by_kind_finds_it() {
    let pack = pack();

    pack.dispatch(
        "create",
        json!({
            "kind": "entity",
            "name": "FlashAttention",
            "entity_kind": "concept"
        }),
    )
    .await
    .expect("create must succeed");

    let list = pack
        .dispatch(
            "list",
            json!({"kind": "entity", "entity_kind": "concept", "limit": 50}),
        )
        .await
        .expect("list must succeed");

    let items = list.as_array().expect("list response must be an array");
    let names: Vec<&str> = items
        .iter()
        .filter_map(|v| v.get("name").and_then(Value::as_str))
        .collect();
    assert!(
        names.contains(&"FlashAttention"),
        "list must contain the created entity; got: {names:?}"
    );
}

#[tokio::test]
async fn list_entity_kind_filter_restricts_results() {
    let pack = pack();

    // Create one concept and one project
    pack.dispatch(
        "create",
        json!({"kind": "entity", "name": "RoPE", "entity_kind": "concept"}),
    )
    .await
    .expect("create concept must succeed");

    pack.dispatch(
        "create",
        json!({"kind": "entity", "name": "lattice-infer", "entity_kind": "project"}),
    )
    .await
    .expect("create project must succeed");

    let list = pack
        .dispatch("list", json!({"kind": "entity", "entity_kind": "project"}))
        .await
        .expect("list by project kind must succeed");

    let items = list.as_array().expect("list must be array");
    for item in items {
        assert_eq!(
            item.get("kind").and_then(Value::as_str),
            Some("project"),
            "filter by entity_kind=project must exclude non-projects; got: {item}"
        );
    }
}

#[tokio::test]
async fn list_unknown_kind_returns_invalid_input() {
    let pack = pack();
    let err = pack
        .dispatch("list", json!({"kind": "spaceship"}))
        .await
        .unwrap_err();
    assert!(
        is_invalid_input(&err),
        "unknown list kind must be InvalidInput"
    );
}

// ---- Basic verb dispatch: create two entities → link → neighbors ----

#[tokio::test]
async fn link_two_entities_visible_via_neighbors() {
    let pack = pack();

    let src = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "Transformer", "entity_kind": "concept"}),
        )
        .await
        .expect("create source must succeed");
    let src_id = src
        .get("id")
        .and_then(Value::as_str)
        .expect("must have id")
        .to_string();

    let tgt = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "Attention", "entity_kind": "concept"}),
        )
        .await
        .expect("create target must succeed");
    let tgt_id = tgt
        .get("id")
        .and_then(Value::as_str)
        .expect("must have id")
        .to_string();

    pack.dispatch(
        "link",
        json!({
            "source_id": src_id,
            "target_id": tgt_id,
            "relation": "contains",
            "weight": 0.9
        }),
    )
    .await
    .expect("link must succeed");

    let neighbors = pack
        .dispatch("neighbors", json!({"node_id": src_id, "direction": "out"}))
        .await
        .expect("neighbors must succeed");

    let items = neighbors.as_array().expect("neighbors must be array");
    assert!(
        !items.is_empty(),
        "source must have at least one outbound neighbor after linking"
    );
    // NeighborHit serializes as {node_id, edge_id, relation, weight}
    let node_ids: Vec<&str> = items
        .iter()
        .filter_map(|v| v.get("node_id").and_then(Value::as_str))
        .collect();
    assert!(
        node_ids.iter().any(|&id| id == tgt_id || tgt_id.starts_with(id) || id.starts_with(&tgt_id[..8])),
        "neighbors must include the linked target node; node_ids: {node_ids:?}, expected tgt: {tgt_id}"
    );
}

#[tokio::test]
async fn link_invalid_relation_returns_invalid_input() {
    let pack = pack();

    let e1 = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "E1", "entity_kind": "concept"}),
        )
        .await
        .expect("create must succeed");
    let e2 = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "E2", "entity_kind": "concept"}),
        )
        .await
        .expect("create must succeed");

    let err = pack
        .dispatch(
            "link",
            json!({
                "source_id": e1.get("id").and_then(Value::as_str).unwrap(),
                "target_id": e2.get("id").and_then(Value::as_str).unwrap(),
                "relation": "invented_relation"
            }),
        )
        .await
        .unwrap_err();
    assert!(
        is_invalid_input(&err),
        "unknown relation must be InvalidInput"
    );
    assert!(
        invalid_input_message(&err).contains("contains"),
        "error must list valid relations; got: {}",
        invalid_input_message(&err)
    );
}

// ---- Search returns created notes ----

#[tokio::test]
async fn search_note_returns_created_content() {
    let pack = pack();

    pack.dispatch(
        "create",
        json!({
            "kind": "note",
            "content": "Sparse attention reduces the quadratic complexity of full attention.",
            "note_kind": "observation"
        }),
    )
    .await
    .expect("create note must succeed");

    // FTS search — no embedding model needed in memory runtime
    let result = pack
        .dispatch(
            "search",
            json!({"kind": "note", "query": "sparse attention quadratic", "limit": 5}),
        )
        .await
        .expect("search must succeed");

    let hits = result.as_array().expect("search response must be array");
    assert!(
        !hits.is_empty(),
        "search must return at least one hit for matching content"
    );
    // Every hit must have note_id
    for hit in hits {
        assert!(
            hit.get("note_id").is_some(),
            "each note search hit must have 'note_id'; got: {hit}"
        );
    }
}

#[tokio::test]
async fn search_entity_returns_created_entity() {
    let pack = pack();

    pack.dispatch(
        "create",
        json!({
            "kind": "entity",
            "name": "GradientCheckpointing",
            "entity_kind": "concept",
            "description": "Trade compute for memory by recomputing activations."
        }),
    )
    .await
    .expect("create must succeed");

    let result = pack
        .dispatch(
            "search",
            json!({"kind": "entity", "query": "gradient checkpointing activations", "limit": 5}),
        )
        .await
        .expect("entity search must succeed");

    let hits = result.as_array().expect("search must return array");
    assert!(
        !hits.is_empty(),
        "entity search must return at least one hit"
    );
    for hit in hits {
        assert!(
            hit.get("entity_id").is_some(),
            "each entity search hit must have 'entity_id'; got: {hit}"
        );
        assert!(
            hit.get("score").is_some(),
            "each entity search hit must have 'score'; got: {hit}"
        );
    }
}

#[tokio::test]
async fn search_unknown_kind_returns_invalid_input() {
    let pack = pack();
    let err = pack
        .dispatch("search", json!({"kind": "graph", "query": "x"}))
        .await
        .unwrap_err();
    assert!(
        is_invalid_input(&err),
        "unknown search kind must be InvalidInput"
    );
}

// ---- Traverse ----

#[tokio::test]
async fn traverse_from_root_with_depth_one_returns_linked_node() {
    let pack = pack();

    let root = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "RootConcept", "entity_kind": "concept"}),
        )
        .await
        .expect("create root must succeed");
    let root_id = root.get("id").and_then(Value::as_str).unwrap().to_string();

    let child = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "ChildConcept", "entity_kind": "concept"}),
        )
        .await
        .expect("create child must succeed");
    let child_id = child.get("id").and_then(Value::as_str).unwrap().to_string();

    pack.dispatch(
        "link",
        json!({"source_id": root_id, "target_id": child_id, "relation": "contains"}),
    )
    .await
    .expect("link must succeed");

    let paths = pack
        .dispatch(
            "traverse",
            json!({
                "roots": [root_id],
                "max_depth": 1,
                "direction": "out",
                "include_roots": false
            }),
        )
        .await
        .expect("traverse must succeed");

    // traverse returns an array of paths/nodes
    let arr = paths.as_array().expect("traverse must return an array");
    assert!(
        !arr.is_empty(),
        "traverse must find the child node at depth 1"
    );
}

// ---- Delete ----

#[tokio::test]
async fn soft_delete_entity_not_found_on_get() {
    let pack = pack();

    let created = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "ToDelete", "entity_kind": "concept"}),
        )
        .await
        .expect("create must succeed");
    let id = created
        .get("id")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();

    let del = pack
        .dispatch("delete", json!({"id": id}))
        .await
        .expect("delete must succeed");
    assert_eq!(
        del.get("deleted").and_then(Value::as_bool),
        Some(true),
        "delete response must have deleted=true"
    );

    let err = pack.dispatch("get", json!({"id": id})).await.unwrap_err();
    assert!(
        matches!(err, RuntimeError::NotFound(_)),
        "get after soft-delete must be NotFound, got: {err:?}"
    );
}

#[tokio::test]
async fn delete_nonexistent_id_returns_not_found() {
    let pack = pack();
    let err = pack
        .dispatch(
            "delete",
            json!({"id": "00000000-0000-0000-0000-000000000002"}),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::NotFound(_)),
        "delete on nonexistent id must be NotFound"
    );
}
