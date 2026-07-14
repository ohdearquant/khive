//! Integration tests for khive-pack-kg.
//!
//! Tests exercise the full dispatch path through KgPack: params deserialize,
//! validation, runtime call, and JSON response. All tests use an in-memory
//! runtime so there is no I/O dependency.

use async_trait::async_trait;
use khive_pack_kg::KgPack;
use khive_runtime::pack::{HandlerDef, PackRuntime};
use khive_runtime::{
    EntityCreateSpec, KhiveRuntime, Namespace, NamespaceToken, ParamDef, RuntimeError,
    VerbCategory, VerbRegistry, VerbRegistryBuilder, VerifiedActor, Visibility,
};
use khive_storage::{Note, SqlStatement, SqlValue};
use khive_types::Pack;
use serde_json::{json, Value};

// ---- Helpers ----

/// Test fixture: a `VerbRegistry` containing a freshly registered `KgPack`,
/// plus pass-through metadata methods so existing tests keep working.
///
/// All dispatch goes through the registry — exercising the same path the MCP
/// server uses, including the kind-hook flow introduced in ADR-030.
struct Fixture {
    registry: VerbRegistry,
}

impl Fixture {
    async fn dispatch(&self, verb: &str, args: Value) -> Result<Value, RuntimeError> {
        self.registry.dispatch(verb, args).await
    }

    fn verbs(&self) -> Vec<&'static HandlerDef> {
        self.registry.all_verbs()
    }
}

fn pack() -> Fixture {
    let rt = KhiveRuntime::memory().expect("in-memory runtime must succeed");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt));
    Fixture {
        registry: builder.build().expect("registry builds"),
    }
}

impl Clone for Fixture {
    fn clone(&self) -> Self {
        Fixture {
            registry: self.registry.clone(),
        }
    }
}

fn pack_with_events() -> Fixture {
    let rt = KhiveRuntime::memory().expect("in-memory runtime must succeed");
    let tok = rt.authorize(khive_runtime::Namespace::local()).unwrap();
    let event_store = rt.events(&tok).expect("event store must be available");
    let mut builder = VerbRegistryBuilder::new();
    builder.with_event_store(event_store);
    builder.register(KgPack::new(rt));
    Fixture {
        registry: builder.build().expect("registry build must succeed"),
    }
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

// ADR-046 (cluster-22) added propose, review, and withdraw — bringing the
// handler count from 11 to 14, then 15 with verbs introspection, then 16
// with stats, then 17 with context (ADR-089), then 18 with resolve
// (unified-verb draft ADR Slice 1).
#[test]
fn pack_verbs_returns_eighteen() {
    let pack = pack();
    assert_eq!(
        pack.verbs().len(),
        18,
        "KgPack must expose exactly 18 verbs (17 previous + resolve)"
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
        "stats",
        "update",
        "delete",
        "merge",
        "search",
        "link",
        "neighbors",
        "traverse",
        "query",
        "propose",
        "review",
        "withdraw",
        "verbs",
        "context",
        "resolve",
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

// Regression: bulk `create(items=[...])` must NOT require a redundant top-level
// `kind` — each item carries its own kind. The single-record `kind` requirement
// runs only after the bulk early-exit. Caught when the requirement fired first.
#[tokio::test]
async fn create_bulk_items_without_top_level_kind_succeeds() {
    let pack = pack();
    let result = pack
        .dispatch(
            "create",
            json!({
                "items": [
                    {"kind": "concept", "name": "BulkOne", "entity_type": "theorem"},
                    {"kind": "concept", "name": "BulkTwo"}
                ]
            }),
        )
        .await
        .expect("bulk create without top-level kind must succeed");
    assert_eq!(result.get("attempted").and_then(Value::as_u64), Some(2));
    assert_eq!(result.get("created").and_then(Value::as_u64), Some(2));
    assert_eq!(result.get("failed").and_then(Value::as_u64), Some(0));
}

// Regression: bulk create is atomic — one invalid item (empty name) rejects the
// whole batch and writes nothing.
#[tokio::test]
async fn create_bulk_items_atomic_rejects_on_invalid_item() {
    let pack = pack();
    let err = pack
        .dispatch(
            "create",
            json!({
                "items": [
                    {"kind": "concept", "name": "ShouldNotLand"},
                    {"kind": "concept", "name": ""}
                ]
            }),
        )
        .await
        .expect_err("empty name in a bulk item must reject the batch");
    assert!(
        is_invalid_input(&err),
        "expected InvalidInput, got: {err:?}"
    );
    // Nothing was written: a follow-up search for the valid name finds no entity.
    let listed = pack
        .dispatch("list", json!({"kind": "concept"}))
        .await
        .expect("list must succeed");
    let items = listed.get("items").and_then(Value::as_array);
    let count = items.map(|a| a.len()).unwrap_or(0);
    assert_eq!(
        count, 0,
        "atomic rejection must leave storage empty; got {listed}"
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
async fn create_note_alias_obs_rejected() {
    // Aliases removed per ADR-013 (F071) — only canonical note kind names accepted.
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
    assert!(
        result.is_err(),
        "alias 'obs' must be rejected: {:?}",
        result
    );
}

#[tokio::test]
async fn create_note_alias_finding_rejected() {
    // Aliases removed per ADR-013 (F071) — only canonical note kind names accepted.
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
        .await;
    assert!(
        result.is_err(),
        "alias 'finding' must be rejected: {:?}",
        result
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

    // P-H2: get returns a flat object — entity fields at top level, no data wrapper.
    assert!(
        fetched.get("data").is_none(),
        "get must NOT wrap in {{data: ...}} (P-H2); got: {fetched}"
    );
    assert_eq!(
        fetched.get("name").and_then(Value::as_str),
        Some("LoRA"),
        "entity name must roundtrip at top level"
    );
    // Entity struct carries granular `kind` ("concept") — matches create/list.
    assert_eq!(
        fetched.get("kind").and_then(Value::as_str),
        Some("concept"),
        "entity kind must roundtrip at top level (same shape as create)"
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

/// Regression for #145: `list(kind="entity")` must honor the `offset` parameter.
///
/// The original bug was that the handler forwarded `limit` to the runtime but
/// hardcoded `offset: 0`, so requesting page 2 (offset=N) returned page 1.
/// Test creates 4 entities, requests (limit=2, offset=0) and (limit=2, offset=2),
/// and verifies the pages are disjoint.
#[tokio::test]
async fn list_entities_offset_returns_disjoint_pages() {
    let pack = pack();
    for i in 0..4 {
        pack.dispatch(
            "create",
            json!({
                "kind": "entity",
                "name": format!("page_test_{i:02}"),
                "entity_kind": "concept"
            }),
        )
        .await
        .expect("create must succeed");
    }

    let page1 = pack
        .dispatch(
            "list",
            json!({"kind": "entity", "entity_kind": "concept", "limit": 2, "offset": 0}),
        )
        .await
        .expect("list page 1 must succeed");
    let page2 = pack
        .dispatch(
            "list",
            json!({"kind": "entity", "entity_kind": "concept", "limit": 2, "offset": 2}),
        )
        .await
        .expect("list page 2 must succeed");

    let ids1: Vec<&str> = page1
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.get("id").and_then(Value::as_str))
        .collect();
    let ids2: Vec<&str> = page2
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.get("id").and_then(Value::as_str))
        .collect();

    assert_eq!(ids1.len(), 2, "page 1 must have 2 entities; got {ids1:?}");
    assert_eq!(ids2.len(), 2, "page 2 must have 2 entities; got {ids2:?}");
    for id in &ids1 {
        assert!(
            !ids2.contains(id),
            "#145 regression: pages overlap — id {id} appears on both pages 1 and 2"
        );
    }
}

/// Regression for #145: `list(kind="note")` must honor the `offset` parameter.
#[tokio::test]
async fn list_notes_offset_returns_disjoint_pages() {
    let pack = pack();
    for i in 0..4 {
        pack.dispatch(
            "create",
            json!({
                "kind": "note",
                "content": format!("page_test note #{i:02}"),
                "note_kind": "observation"
            }),
        )
        .await
        .expect("create note must succeed");
    }

    let page1 = pack
        .dispatch(
            "list",
            json!({"kind": "note", "note_kind": "observation", "limit": 2, "offset": 0}),
        )
        .await
        .expect("list page 1 must succeed");
    let page2 = pack
        .dispatch(
            "list",
            json!({"kind": "note", "note_kind": "observation", "limit": 2, "offset": 2}),
        )
        .await
        .expect("list page 2 must succeed");

    let ids1: Vec<&str> = page1
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.get("id").and_then(Value::as_str))
        .collect();
    let ids2: Vec<&str> = page2
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.get("id").and_then(Value::as_str))
        .collect();

    assert_eq!(ids1.len(), 2, "note page 1 must have 2 items; got {ids1:?}");
    assert_eq!(ids2.len(), 2, "note page 2 must have 2 items; got {ids2:?}");
    for id in &ids1 {
        assert!(
            !ids2.contains(id),
            "#145 regression: note pages overlap — id {id} on both pages"
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
    // #148: NeighborHit serializes as {id, edge_id, relation, weight, name?, kind?}
    let node_ids: Vec<&str> = items
        .iter()
        .filter_map(|v| v.get("id").and_then(Value::as_str))
        .collect();
    assert!(
        node_ids
            .iter()
            .any(|&id| id == tgt_id || tgt_id.starts_with(id) || id.starts_with(&tgt_id[..8])),
        "neighbors must include the linked target node; ids: {node_ids:?}, expected tgt: {tgt_id}"
    );
}

/// Regression for #471: `link` must reject relation strings with punctuation
/// (`supports!`, `part/of`, `depends.on`, `competes with`) as InvalidInput
/// instead of silently normalizing them into a canonical relation.
#[tokio::test]
async fn link_edge_relation_bang_returns_invalid_input() {
    let pack = pack();

    let src = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "Alpha", "entity_kind": "concept"}),
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
            json!({"kind": "entity", "name": "Beta", "entity_kind": "concept"}),
        )
        .await
        .expect("create target must succeed");
    let tgt_id = tgt
        .get("id")
        .and_then(Value::as_str)
        .expect("must have id")
        .to_string();

    for bad_relation in ["supports!", "part/of", "depends.on", "competes with"] {
        let err = pack
            .dispatch(
                "link",
                json!({
                    "source_id": src_id,
                    "target_id": tgt_id,
                    "relation": bad_relation,
                    "weight": 0.5
                }),
            )
            .await
            .expect_err(&format!(
                "relation {bad_relation:?} must be rejected as InvalidInput"
            ));
        assert!(
            is_invalid_input(&err),
            "relation {bad_relation:?} must be InvalidInput, got: {err:?}"
        );
    }
}

/// Regression for #160: search response includes `entity_kind` so agents can
/// distinguish hit kinds without an extra `get()` call.
#[tokio::test]
async fn search_entity_response_includes_entity_kind() {
    let pack = pack();
    pack.dispatch(
        "create",
        json!({"kind": "entity", "name": "AlphaSearch", "entity_kind": "concept"}),
    )
    .await
    .unwrap();

    let resp = pack
        .dispatch("search", json!({"kind": "entity", "query": "AlphaSearch"}))
        .await
        .expect("search must succeed");
    let arr = resp.as_array().expect("array");
    assert!(
        !arr.is_empty(),
        "search must return the entity we just created"
    );
    let hit = &arr[0];
    assert_eq!(
        hit.get("entity_kind").and_then(Value::as_str),
        Some("concept"),
        "#160: search response must carry entity_kind; got hit {hit}"
    );
}

/// Regression for #160 (note half): note search response includes `note_kind`.
#[tokio::test]
async fn search_note_response_includes_note_kind() {
    let pack = pack();
    pack.dispatch(
        "create",
        json!({
            "kind": "note",
            "content": "BetaInsight unique_marker_4761",
            "note_kind": "insight"
        }),
    )
    .await
    .unwrap();

    let resp = pack
        .dispatch(
            "search",
            json!({"kind": "note", "query": "unique_marker_4761"}),
        )
        .await
        .expect("note search must succeed");
    let arr = resp.as_array().expect("array");
    assert!(
        !arr.is_empty(),
        "note search must return the note we just created"
    );
    let hit = &arr[0];
    assert_eq!(
        hit.get("note_kind").and_then(Value::as_str),
        Some("insight"),
        "#160 (note half): search response must carry note_kind; got hit {hit}"
    );
}

/// Regression for #163: `search` accepts a `properties` filter that restricts
/// results to entities whose properties contain the given key=value pairs.
#[tokio::test]
async fn search_properties_filter_restricts_results() {
    let pack = pack();
    pack.dispatch(
        "create",
        json!({
            "kind": "entity",
            "name": "EntInference",
            "entity_kind": "concept",
            "properties": {"domain": "inference"}
        }),
    )
    .await
    .unwrap();
    pack.dispatch(
        "create",
        json!({
            "kind": "entity",
            "name": "EntTraining",
            "entity_kind": "concept",
            "properties": {"domain": "training"}
        }),
    )
    .await
    .unwrap();

    // Search with properties filter — only the inference entity must come back.
    let resp = pack
        .dispatch(
            "search",
            json!({
                "kind": "entity",
                "query": "Ent",
                "properties": {"domain": "inference"}
            }),
        )
        .await
        .expect("filtered search must succeed");
    let arr = resp.as_array().expect("array");
    assert!(
        !arr.is_empty(),
        "#163: properties filter must return matching entities; got empty result"
    );
    for hit in arr {
        let name = hit.get("title").and_then(Value::as_str).unwrap_or("");
        assert!(
            name.contains("Inference") || name == "EntInference",
            "#163: properties filter must EXCLUDE entities with domain=training; got hit {hit}"
        );
    }
}

/// #518: entity search with `tags` filter must return only entities whose tags match any
/// of the requested tags (OR semantics, case-insensitive).
#[tokio::test]
async fn search_tags_filter_restricts_results_or_semantics() {
    let pack = pack();
    // Create three entities with overlapping query text but distinct tags.
    pack.dispatch(
        "create",
        json!({
            "kind": "entity",
            "name": "TagSearchRust",
            "entity_kind": "concept",
            "tags": ["rust"],
            "description": "A tag search test entity about rust systems programming",
        }),
    )
    .await
    .unwrap();
    pack.dispatch(
        "create",
        json!({
            "kind": "entity",
            "name": "TagSearchPython",
            "entity_kind": "concept",
            "tags": ["python"],
            "description": "A tag search test entity about python data science",
        }),
    )
    .await
    .unwrap();
    pack.dispatch(
        "create",
        json!({
            "kind": "entity",
            "name": "TagSearchRustML",
            "entity_kind": "concept",
            "tags": ["rust", "ml"],
            "description": "A tag search test entity about rust machine learning",
        }),
    )
    .await
    .unwrap();

    // Search with tags=["python", "ml"] — should include python and rust+ml, exclude rust-only.
    let resp = pack
        .dispatch(
            "search",
            json!({
                "kind": "entity",
                "query": "tag search test entity",
                "tags": ["python", "ml"],
            }),
        )
        .await
        .expect("#518: tag-filtered search must succeed");
    let arr = resp.as_array().expect("response must be an array");

    let titles: Vec<&str> = arr
        .iter()
        .filter_map(|h| h.get("title").and_then(Value::as_str))
        .collect();

    assert!(
        titles.iter().any(|t| t.contains("Python")),
        "#518: python-tagged entity must appear in results; got {titles:?}"
    );
    assert!(
        titles.iter().any(|t| t.contains("RustML")),
        "#518: rust+ml entity must appear in results; got {titles:?}"
    );
    assert!(
        !titles.contains(&"TagSearchRust"),
        "#518: rust-only entity must be excluded from python/ml filter; got {titles:?}"
    );
}

/// Regression for #148: `neighbors` accepts `id` (canonical) AND `node_id` (legacy alias).
/// Both inputs must work and the response must use `id`.
#[tokio::test]
async fn neighbors_accepts_id_alias_and_responds_with_id() {
    let pack = pack();
    let src = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "Src", "entity_kind": "concept"}),
        )
        .await
        .unwrap();
    let tgt = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "Tgt", "entity_kind": "concept"}),
        )
        .await
        .unwrap();
    let src_id = src["id"].as_str().unwrap();
    let tgt_id = tgt["id"].as_str().unwrap();
    pack.dispatch(
        "link",
        json!({"source_id": src_id, "target_id": tgt_id, "relation": "contains", "weight": 1.0}),
    )
    .await
    .unwrap();

    // Canonical `id` argument works.
    let via_id = pack
        .dispatch("neighbors", json!({"id": src_id, "direction": "out"}))
        .await
        .expect("neighbors with id arg must succeed (canonical)");
    // Legacy `node_id` alias also works.
    let via_legacy = pack
        .dispatch("neighbors", json!({"node_id": src_id, "direction": "out"}))
        .await
        .expect("neighbors with node_id arg must succeed (alias)");

    for resp in [&via_id, &via_legacy] {
        let arr = resp.as_array().expect("neighbors returns array");
        assert!(!arr.is_empty(), "expected at least one neighbor");
        let hit = &arr[0];
        // Response uses `id`, NOT `node_id`.
        assert!(
            hit.get("id").is_some(),
            "neighbor hit must serialize as `id` (#148); got keys {:?}",
            hit.as_object().map(|m| m.keys().collect::<Vec<_>>())
        );
        assert!(
            hit.get("node_id").is_none(),
            "neighbor hit must NOT also serialize as `node_id` (#148 wire normalization); got keys {:?}",
            hit.as_object().map(|m| m.keys().collect::<Vec<_>>())
        );
    }
}

/// Regression for #162: neighbor hits include enriched `name` and `kind`
/// from the corresponding entity record.
#[tokio::test]
async fn neighbors_enriches_with_name_and_kind() {
    let pack = pack();
    let src = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "FlashAttention", "entity_kind": "concept"}),
        )
        .await
        .unwrap();
    let tgt = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "GQA", "entity_kind": "concept"}),
        )
        .await
        .unwrap();
    let src_id = src["id"].as_str().unwrap();
    let tgt_id = tgt["id"].as_str().unwrap();
    pack.dispatch(
        "link",
        json!({"source_id": src_id, "target_id": tgt_id, "relation": "extends", "weight": 1.0}),
    )
    .await
    .unwrap();

    let resp = pack
        .dispatch("neighbors", json!({"id": src_id, "direction": "out"}))
        .await
        .expect("neighbors must succeed");
    let arr = resp.as_array().expect("array");
    let hit = arr
        .iter()
        .find(|h| h.get("id").and_then(Value::as_str) == Some(tgt_id))
        .expect("must find tgt in neighbors");

    // #162: enrichment must populate name + kind from the target entity.
    assert_eq!(
        hit.get("name").and_then(Value::as_str),
        Some("GQA"),
        "neighbor hit must carry entity name (#162); hit={hit}"
    );
    assert_eq!(
        hit.get("kind").and_then(Value::as_str),
        Some("concept"),
        "neighbor hit must carry entity kind (#162); hit={hit}"
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
    // Every hit must have id (normalized from substrate-specific note_id — issue #148)
    for hit in hits {
        assert!(
            hit.get("id").is_some(),
            "each note search hit must have 'id'; got: {hit}"
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
            hit.get("id").is_some(),
            "each entity search hit must have 'id'; got: {hit}"
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

// #570: FTS operator regression matrix for KG note and entity search surfaces.
#[tokio::test]
async fn search_operator_matrix_does_not_crash() {
    let pack = pack();

    // Seed one note and one entity for context.
    pack.dispatch(
        "create",
        json!({
            "kind": "note",
            "content": "tenant isolation operator regression anchor content",
            "note_kind": "observation"
        }),
    )
    .await
    .expect("seed note");

    pack.dispatch(
        "create",
        json!({
            "kind": "entity",
            "name": "OperatorMatrixAnchor",
            "entity_kind": "concept",
            "description": "tenant isolation operator regression anchor entity"
        }),
    )
    .await
    .expect("seed entity");

    // Queries to exercise — invariant: no panic, returns Ok (empty or non-empty).
    let cases: &[&str] = &[
        "\"tenant isolation\"",
        "tenant AND isolation",
        "tenant OR isolation",
        "tenant NOT isolation",
        "tenant NEAR(isolation, 5)",
        "tenant*",
        "***",
        "tenant:isolation",
        "tenant ^ isolation",
        "(tenant isolation)",
        "(\"+_~!\")",
        "tenant:foo^bar*",
        "multi-tenant isolation",
        "Bob's tenant",
    ];

    for kind in &["note", "entity"] {
        for query in cases {
            let result = pack
                .dispatch(
                    "search",
                    json!({ "kind": kind, "query": query, "limit": 5 }),
                )
                .await;
            assert!(
                result.is_ok(),
                "#570 KG search kind={kind} query={query:?} must not crash, got: {:?}",
                result.err()
            );
        }
    }
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

/// STORAGE-AUD-003 / #485: an oversized max_depth (> i64::MAX) must reject at
/// the storage boundary, not silently narrow to a negative i64 depth and
/// return an empty or wrong traversal.
#[tokio::test]
async fn traverse_max_depth_over_i64max_rejected() {
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

    let oversized_max_depth = (i64::MAX as u64) + 1;
    let err = pack
        .dispatch(
            "traverse",
            json!({
                "roots": [root_id],
                "max_depth": oversized_max_depth,
                "direction": "out",
                "include_roots": false
            }),
        )
        .await
        .expect_err("traverse with max_depth > i64::MAX must not succeed");

    match err {
        RuntimeError::Storage(khive_storage::StorageError::InvalidInput { .. }) => {}
        RuntimeError::InvalidInput(_) => {}
        other => panic!("expected InvalidInput (directly or via Storage), got {other:?}"),
    }
}

// ---- include_entity_type / include_properties optional enrichment ----

/// neighbors with include_entity_type=true carries entity_type on each neighbor.
/// neighbors without the param (default-off) must not include entity_type in
/// the JSON response: the wire shape must be unchanged from today.
#[tokio::test]
async fn neighbors_include_entity_type_param() {
    let pack = pack();

    let src = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "SrcNode", "entity_kind": "concept", "entity_type": "algorithm"}),
        )
        .await
        .unwrap();
    let tgt = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "TgtNode", "entity_kind": "concept", "entity_type": "technique"}),
        )
        .await
        .unwrap();
    let src_id = src["id"].as_str().unwrap();
    let tgt_id = tgt["id"].as_str().unwrap();
    pack.dispatch(
        "link",
        json!({"source_id": src_id, "target_id": tgt_id, "relation": "extends", "weight": 1.0}),
    )
    .await
    .unwrap();

    // Default (no param): entity_type must be absent from the response.
    let default_resp = pack
        .dispatch("neighbors", json!({"id": src_id, "direction": "out"}))
        .await
        .expect("neighbors default must succeed");
    let default_arr = default_resp.as_array().expect("array");
    let default_hit = default_arr
        .iter()
        .find(|h| h.get("id").and_then(Value::as_str) == Some(tgt_id))
        .expect("must find tgt in default neighbors");
    assert!(
        default_hit.get("entity_type").is_none(),
        "entity_type must be absent from the response when include_entity_type is not set; hit={default_hit}"
    );

    // With include_entity_type=true: entity_type must be present.
    let enriched_resp = pack
        .dispatch(
            "neighbors",
            json!({"id": src_id, "direction": "out", "include_entity_type": true}),
        )
        .await
        .expect("neighbors with include_entity_type must succeed");
    let enriched_arr = enriched_resp.as_array().expect("array");
    let enriched_hit = enriched_arr
        .iter()
        .find(|h| h.get("id").and_then(Value::as_str) == Some(tgt_id))
        .expect("must find tgt when include_entity_type=true");
    assert_eq!(
        enriched_hit.get("entity_type").and_then(Value::as_str),
        Some("technique"),
        "entity_type must equal the stored value when include_entity_type=true; hit={enriched_hit}"
    );
}

/// neighbors with include_entity_type=false must not include entity_type even
/// when the entity has one (explicit false is same as absent).
#[tokio::test]
async fn neighbors_include_entity_type_false_omits_field() {
    let pack = pack();

    let src = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "SrcNode2", "entity_kind": "concept", "entity_type": "algorithm"}),
        )
        .await
        .unwrap();
    let tgt = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "TgtNode2", "entity_kind": "concept", "entity_type": "technique"}),
        )
        .await
        .unwrap();
    let src_id = src["id"].as_str().unwrap();
    let tgt_id = tgt["id"].as_str().unwrap();
    pack.dispatch(
        "link",
        json!({"source_id": src_id, "target_id": tgt_id, "relation": "extends", "weight": 1.0}),
    )
    .await
    .unwrap();

    let resp = pack
        .dispatch(
            "neighbors",
            json!({"id": src_id, "direction": "out", "include_entity_type": false}),
        )
        .await
        .expect("neighbors with include_entity_type=false must succeed");
    let arr = resp.as_array().expect("array");
    let hit = arr
        .iter()
        .find(|h| h.get("id").and_then(Value::as_str) == Some(tgt_id))
        .expect("must find tgt");
    assert!(
        hit.get("entity_type").is_none(),
        "entity_type must be absent when include_entity_type=false; hit={hit}"
    );
}

/// traverse with include_properties=true carries properties on each path node.
/// traverse without the param must not include properties in the JSON response.
#[tokio::test]
async fn traverse_include_properties_param() {
    let pack = pack();

    let props = json!({"year": 2024, "topic": "attention"});
    let root = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "RootWithProps", "entity_kind": "concept", "properties": props}),
        )
        .await
        .expect("create root must succeed");
    let root_id = root.get("id").and_then(Value::as_str).unwrap().to_string();

    let child = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "ChildNode", "entity_kind": "concept"}),
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

    // Default (no param): properties must be absent from the response.
    let default_paths = pack
        .dispatch(
            "traverse",
            json!({"roots": [root_id], "max_depth": 1, "direction": "out", "include_roots": true}),
        )
        .await
        .expect("traverse default must succeed");
    let default_arr = default_paths.as_array().expect("array");
    assert!(!default_arr.is_empty(), "must return at least one path");
    for path in default_arr {
        let nodes = path["nodes"].as_array().expect("nodes array");
        for node in nodes {
            assert!(
                node.get("properties").is_none(),
                "properties must be absent from nodes when include_properties is not set; node={node}"
            );
        }
    }

    // With include_properties=true: root node must carry properties.
    let enriched_paths = pack
        .dispatch(
            "traverse",
            json!({"roots": [root_id], "max_depth": 1, "direction": "out", "include_roots": true, "include_properties": true}),
        )
        .await
        .expect("traverse with include_properties must succeed");
    let enriched_arr = enriched_paths.as_array().expect("array");
    let root_path = enriched_arr.first().expect("must have at least one path");
    let nodes = root_path["nodes"].as_array().expect("nodes array");
    let root_node = nodes
        .iter()
        .find(|n| n.get("id").and_then(Value::as_str) == Some(&root_id))
        .expect("must find root node in path");
    let node_props = root_node
        .get("properties")
        .expect("properties must be present when include_properties=true; root_node={root_node}");
    assert_eq!(
        node_props.get("year").and_then(Value::as_i64),
        Some(2024),
        "stored property year must be present in the response"
    );
}

/// traverse with include_properties=false must not include properties even
/// when entities have them (explicit false is same as absent).
#[tokio::test]
async fn traverse_include_properties_false_omits_field() {
    let pack = pack();

    let props = json!({"key": "value"});
    let root = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "RootOmit", "entity_kind": "concept", "properties": props}),
        )
        .await
        .expect("create must succeed");
    let root_id = root.get("id").and_then(Value::as_str).unwrap().to_string();

    let child = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "ChildOmit", "entity_kind": "concept"}),
        )
        .await
        .expect("create must succeed");
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
            json!({"roots": [root_id], "max_depth": 1, "include_roots": true, "include_properties": false}),
        )
        .await
        .expect("traverse with include_properties=false must succeed");
    let arr = paths.as_array().expect("array");
    for path in arr {
        let nodes = path["nodes"].as_array().expect("nodes array");
        for node in nodes {
            assert!(
                node.get("properties").is_none(),
                "properties must be absent when include_properties=false; node={node}"
            );
        }
    }
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
        .dispatch("delete", json!({"id": id, "kind": "entity"}))
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
            json!({"id": "00000000-0000-0000-0000-000000000002", "kind": "entity"}),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::NotFound(_)),
        "delete on nonexistent id must be NotFound"
    );
}

// ---- ADR-025 contract: KG pack rejects non-KG kinds (single-pack architecture) ----
// The KG pack validates only its own vocabulary. Multi-pack kind-discriminated routing
// is future work beyond the current 5-step plan (see ADR-025 §Limitation).

#[tokio::test]
async fn create_entity_non_kg_kind_rejected_by_pack_validation() {
    let pack = pack();
    let err = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "Router", "entity_kind": "device"}),
        )
        .await
        .unwrap_err();
    assert!(
        is_invalid_input(&err),
        "non-KG entity_kind must be rejected in single-pack mode"
    );
}

#[tokio::test]
async fn create_note_non_kg_kind_rejected_by_pack_validation() {
    let pack = pack();
    let err = pack
        .dispatch(
            "create",
            json!({"kind": "note", "content": "Task content", "note_kind": "task"}),
        )
        .await
        .unwrap_err();
    assert!(
        is_invalid_input(&err),
        "non-KG note_kind must be rejected in single-pack mode"
    );
}

// ── search-kind unification: registry-driven granular kind routing ────────────
//
// These tests prove that the `resolve_kind_spec` routing in `handle_search` is
// driven entirely by `VerbRegistry.all_entity_kinds()` / `all_note_kinds()`,
// with no hard-coded kind list. A fake MemoryPack registers `"memory"` as a
// note kind (ADR-036: one kind, advisory memory_type property). Once registered,
// `search(kind="memory")` must route to note-search (not error), and
// `search(kind="bogus")` must list `"memory"` among the valid options.

/// A minimal second pack that declares `"memory"` as a note kind (ADR-036).
/// It does not handle any verbs itself — dispatch falls through to the KG pack
/// that owns `search`. Requires "kg" per ADR-037 so topo sort puts kg first.
struct FakeMemoryPack;

impl Pack for FakeMemoryPack {
    const NAME: &'static str = "memory";
    const NOTE_KINDS: &'static [&'static str] = &["memory"];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[];
    const REQUIRES: &'static [&'static str] = &["kg"];
}

#[async_trait]
impl PackRuntime for FakeMemoryPack {
    fn name(&self) -> &str {
        FakeMemoryPack::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        FakeMemoryPack::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        FakeMemoryPack::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        FakeMemoryPack::HANDLERS
    }

    fn requires(&self) -> &'static [&'static str] {
        FakeMemoryPack::REQUIRES
    }

    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &khive_runtime::NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Err(RuntimeError::InvalidInput(format!(
            "FakeMemoryPack does not handle verb {verb:?}"
        )))
    }
}

/// Build a registry with KgPack + FakeMemoryPack (simulating the two-pack
/// configuration that will exist once Lane B lands).
fn pack_with_memory() -> Fixture {
    let rt = KhiveRuntime::memory().expect("in-memory runtime must succeed");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt));
    builder.register(FakeMemoryPack);
    Fixture {
        registry: builder.build().expect("registry builds"),
    }
}

#[tokio::test]
async fn registry_exposes_memory_kind_once_memory_pack_registered() {
    // When FakeMemoryPack is loaded, `all_note_kinds()` must include "memory"
    // (ADR-036: one kind, advisory memory_type property).
    let fixture = pack_with_memory();
    let note_kinds = fixture.registry.all_note_kinds();
    assert!(
        note_kinds.contains(&"memory"),
        "registry must advertise 'memory' once memory pack is loaded; got: {note_kinds:?}"
    );
    assert!(
        !note_kinds.contains(&"semantic") && !note_kinds.contains(&"episodic"),
        "memory_type must not be exposed as separate note kinds; got: {note_kinds:?}"
    );
}

#[tokio::test]
async fn search_kind_memory_routes_to_note_substrate_via_registry() {
    let fixture = pack_with_memory();

    let created = fixture
        .dispatch(
            "create",
            json!({
                "kind": "memory",
                "content": "registry driven kind routing for memory notes",
                "properties": {"memory_type": "semantic"}
            }),
        )
        .await
        .expect("create with kind=memory must succeed when memory pack is loaded");
    assert_eq!(
        created.get("kind").and_then(Value::as_str),
        Some("memory"),
        "note created with kind=memory must be stored as kind=memory; got: {created}"
    );

    let result = fixture
        .dispatch(
            "search",
            json!({"kind": "memory", "query": "registry driven kind routing", "limit": 5}),
        )
        .await
        .expect("search(kind=\"memory\") must succeed once memory pack registers the kind");

    let hits = result.as_array().expect("search result must be array");
    assert!(
        !hits.is_empty(),
        "search(kind=\"memory\") must find the note we just created; got: {hits:?}"
    );
    for hit in hits {
        assert!(
            hit.get("id").is_some(),
            "note-substrate hit must have 'id'; got: {hit}"
        );
    }
}

#[tokio::test]
async fn search_kind_entity_still_works_alongside_memory_pack() {
    // Regression guard: loading FakeMemoryPack must not break entity search.
    let fixture = pack_with_memory();

    fixture
        .dispatch(
            "create",
            json!({
                "kind": "entity",
                "entity_kind": "concept",
                "name": "SemanticsConceptNode",
                "description": "entity search alongside memory pack"
            }),
        )
        .await
        .expect("create entity must succeed with memory pack loaded");

    let result = fixture
        .dispatch(
            "search",
            json!({"kind": "entity", "query": "SemanticsConceptNode", "limit": 5}),
        )
        .await
        .expect("search(kind=\"entity\") must still work when memory pack is loaded");

    let hits = result.as_array().expect("search result must be array");
    assert!(
        !hits.is_empty(),
        "entity search must find the created concept; got: {hits:?}"
    );
    for hit in hits {
        assert!(
            hit.get("id").is_some(),
            "entity-substrate hit must have 'id'; got: {hit}"
        );
    }
}

#[tokio::test]
async fn search_bogus_kind_lists_memory_in_error() {
    // The error message for an unknown kind must list ALL registered kinds,
    // including those contributed by FakeMemoryPack. This proves the error
    // path walks the full merged registry, not a hard-coded list.
    let fixture = pack_with_memory();

    let err = fixture
        .dispatch("search", json!({"kind": "bogus", "query": "anything"}))
        .await
        .unwrap_err();

    assert!(
        is_invalid_input(&err),
        "unknown kind must be InvalidInput; got: {err:?}"
    );
    let msg = invalid_input_message(&err);
    assert!(msg.contains("bogus"), "error must name the bad kind: {msg}");
    assert!(msg.contains("entity"), "error must list 'entity': {msg}");
    assert!(msg.contains("note"), "error must list 'note': {msg}");
    assert!(msg.contains("concept"), "error must list 'concept': {msg}");
    assert!(
        msg.contains("observation"),
        "error must list 'observation': {msg}"
    );
    assert!(
        msg.contains("memory"),
        "error must list 'memory' (contributed by memory pack): {msg}"
    );
    assert!(
        !msg.contains("semantic") && !msg.contains("episodic"),
        "memory_type values must not be listed as note kinds: {msg}"
    );
}

// ── ADR-038: Events Surface ────────────────────────────────────────────────────

#[tokio::test]
async fn create_event_kind_returns_immutable_error() {
    let pack = pack();
    let err = pack
        .dispatch("create", json!({"kind": "event"}))
        .await
        .unwrap_err();
    assert!(
        is_invalid_input(&err),
        "create(kind=event) must return InvalidInput; got: {err:?}"
    );
    assert_eq!(
        invalid_input_message(&err),
        "events are immutable — create/update/delete are not permitted",
        "immutable-event message must match exactly"
    );
}

// ── Issue #65: link verb name resolution ─────────────────────────────────────
//
// When `source_id` or `target_id` is not a UUID or hex prefix, the link handler
// must treat the value as an entity name and resolve it to a UUID.

#[tokio::test]
async fn link_by_name_exact_match_succeeds() {
    let pack = pack();

    // Create two entities with well-known names.
    pack.dispatch(
        "create",
        json!({"kind": "entity", "name": "SourceEntity", "entity_kind": "concept"}),
    )
    .await
    .expect("create SourceEntity must succeed");

    pack.dispatch(
        "create",
        json!({"kind": "entity", "name": "TargetEntity", "entity_kind": "concept"}),
    )
    .await
    .expect("create TargetEntity must succeed");

    // Link using names instead of UUIDs.
    let result = pack
        .dispatch(
            "link",
            json!({
                "source_id": "SourceEntity",
                "target_id": "TargetEntity",
                "relation": "extends"
            }),
        )
        .await;
    assert!(
        result.is_ok(),
        "link by entity name must succeed; got: {result:?}"
    );
}

#[tokio::test]
async fn link_by_name_with_underscore_resolves_exactly() {
    let pack = pack();

    // Entity name contains a literal underscore, a SQLite LIKE
    // single-character wildcard (#818).
    pack.dispatch(
        "create",
        json!({"kind": "entity", "name": "a_b", "entity_kind": "concept"}),
    )
    .await
    .expect("create 'a_b' must succeed");

    // A decoy whose name only the *unescaped* wildcard form of "a_b" would
    // match (`_` matching any single character).
    pack.dispatch(
        "create",
        json!({"kind": "entity", "name": "aXb", "entity_kind": "concept"}),
    )
    .await
    .expect("create 'aXb' must succeed");

    pack.dispatch(
        "create",
        json!({"kind": "entity", "name": "TargetEntity", "entity_kind": "concept"}),
    )
    .await
    .expect("create TargetEntity must succeed");

    // Resolving "a_b" by name must find the exact entity, not be rejected as
    // ambiguous with "aXb" and not silently resolve to the decoy.
    let result = pack
        .dispatch(
            "link",
            json!({
                "source_id": "a_b",
                "target_id": "TargetEntity",
                "relation": "extends"
            }),
        )
        .await;
    assert!(
        result.is_ok(),
        "link by exact name 'a_b' must succeed despite an unescaped-wildcard-matching decoy; got: {result:?}"
    );
}

#[tokio::test]
async fn link_by_name_with_percent_resolves_exactly() {
    let pack = pack();

    // Entity name contains a literal percent sign, a SQLite LIKE
    // any-sequence wildcard (#818).
    pack.dispatch(
        "create",
        json!({"kind": "entity", "name": "50%off", "entity_kind": "concept"}),
    )
    .await
    .expect("create '50%off' must succeed");

    // A decoy whose name only the *unescaped* wildcard form of "50%off"
    // would match (`%` matching any sequence, including across "-").
    pack.dispatch(
        "create",
        json!({"kind": "entity", "name": "50-off-promo", "entity_kind": "concept"}),
    )
    .await
    .expect("create '50-off-promo' must succeed");

    pack.dispatch(
        "create",
        json!({"kind": "entity", "name": "TargetEntity", "entity_kind": "concept"}),
    )
    .await
    .expect("create TargetEntity must succeed");

    let result = pack
        .dispatch(
            "link",
            json!({
                "source_id": "50%off",
                "target_id": "TargetEntity",
                "relation": "extends"
            }),
        )
        .await;
    assert!(
        result.is_ok(),
        "link by exact name '50%off' must succeed despite an unescaped-wildcard-matching decoy; got: {result:?}"
    );
}

/// Regression for a gap on #818/#834: the two tests above prove
/// escaping keeps wildcard decoys out of the WHERE clause entirely, so they
/// pass even without exact-match-first ordering. This test isolates that
/// ordering through the real `link`-by-name verb path: every decoy genuinely
/// matches the name-prefix "Base" (no wildcard characters, so escaping plays
/// no role) and is created after the exact match, filling the 100-row
/// resolution page with newer rows. It fails if the `ORDER BY CASE` in
/// `query_entities` were removed, even though the WHERE clause and escaping
/// remain correct.
#[tokio::test]
async fn link_by_name_exact_match_wins_over_many_prefix_matching_decoys() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime must succeed");
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let f = Fixture {
        registry: builder.build().expect("registry builds"),
    };

    // The exact-match entity is created first (oldest by created_at).
    rt.create_entity(&token, "concept", None, "Base", None, None, vec![])
        .await
        .expect("create 'Base' must succeed");

    // 150 decoys that genuinely match the name-prefix "Base" (no wildcard
    // chars), created after "Base", seeded directly through the runtime to
    // skip dispatch overhead.
    for i in 0..150 {
        rt.create_entity(
            &token,
            "concept",
            None,
            &format!("Base-{i:03}"),
            None,
            None,
            vec![],
        )
        .await
        .expect("create decoy must succeed");
    }

    f.dispatch(
        "create",
        json!({"kind": "entity", "name": "TargetEntity", "entity_kind": "concept"}),
    )
    .await
    .expect("create TargetEntity must succeed");

    let result = f
        .dispatch(
            "link",
            json!({
                "source_id": "Base",
                "target_id": "TargetEntity",
                "relation": "extends"
            }),
        )
        .await;
    assert!(
        result.is_ok(),
        "link by exact name 'Base' must resolve despite 150 newer, genuinely prefix-matching \
         decoys filling the resolution page; got: {result:?}"
    );
}

#[tokio::test]
async fn list_event_kind_returns_array() {
    let pack = pack_with_events();
    // Create an entity first so there are audit events to find.
    pack.dispatch("create", json!({"kind": "concept", "name": "AuditTarget"}))
        .await
        .expect("create must succeed");

    let result = pack
        .dispatch(
            "list",
            json!({"kind": "event", "verb": "create", "limit": 10}),
        )
        .await
        .expect("list(kind=event) must succeed");

    let arr = result.as_array().expect("list must return a JSON array");
    assert!(
        !arr.is_empty(),
        "at least one create audit event must be present"
    );
    assert!(
        arr.iter()
            .all(|e| e.get("verb").and_then(Value::as_str) == Some("create")),
        "all returned events must have verb=create when filtered"
    );
    assert!(
        arr.iter()
            .all(|e| e.get("outcome").and_then(Value::as_str) == Some("success")),
        "all returned events must have outcome=success"
    );
}

#[tokio::test]
async fn get_event_uuid_returns_event_wrapper() {
    let pack = pack_with_events();
    pack.dispatch(
        "create",
        json!({"kind": "concept", "name": "GetEventTarget"}),
    )
    .await
    .expect("create must succeed");

    // List create events to get an event UUID.
    let list_result = pack
        .dispatch(
            "list",
            json!({"kind": "event", "verb": "create", "limit": 1}),
        )
        .await
        .expect("list must succeed");
    let events = list_result.as_array().expect("list must be array");
    assert!(!events.is_empty(), "must have at least one create event");
    let event_id = events[0]
        .get("id")
        .and_then(Value::as_str)
        .expect("event must have id field")
        .to_string();

    let get_result = pack
        .dispatch("get", json!({"id": event_id}))
        .await
        .expect("get(id=event_uuid) must succeed");

    // P-H2: get returns flat — events don't have a kind field in the struct,
    // so flatten_get_result injects kind="event" at the top level.
    assert_eq!(
        get_result.get("kind").and_then(Value::as_str),
        Some("event"),
        "get must have kind=event at top level"
    );
    assert!(
        get_result.get("data").is_none(),
        "get must NOT wrap in {{data: ...}} (P-H2); got: {get_result}"
    );
    assert_eq!(
        get_result.get("id").and_then(Value::as_str),
        Some(event_id.as_str()),
        "id must match the requested event UUID"
    );
    assert_eq!(
        get_result.get("verb").and_then(Value::as_str),
        Some("create"),
        "verb must be create"
    );
    assert_eq!(
        get_result.get("outcome").and_then(Value::as_str),
        Some("success"),
        "outcome must be success"
    );
}

/// #425 regression: `get(id=<event_uuid>)` from a caller in a DIFFERENT
/// namespace than the event must succeed, matching the ADR-007 Rev 6 pattern
/// #393 already established for entity/note/edge. Guards against the residual
/// event-UUID resolver path #393 did not cover.
#[tokio::test]
async fn get_event_uuid_cross_namespace_succeeds() {
    let pack = pack_with_events();
    pack.dispatch(
        "create",
        json!({"kind": "concept", "name": "CrossNsEventTarget"}),
    )
    .await
    .expect("create must succeed");

    let list_result = pack
        .dispatch(
            "list",
            json!({"kind": "event", "verb": "create", "limit": 1}),
        )
        .await
        .expect("list must succeed");
    let events = list_result.as_array().expect("list must be array");
    assert!(!events.is_empty(), "must have at least one create event");
    let event_id = events[0]
        .get("id")
        .and_then(Value::as_str)
        .expect("event must have id field")
        .to_string();
    let stored_namespace = events[0]
        .get("namespace")
        .and_then(Value::as_str)
        .expect("event must have namespace field")
        .to_string();
    assert_ne!(
        stored_namespace, "ns-caller",
        "event must be stored in a namespace other than the cross-namespace caller"
    );

    let get_result = pack
        .dispatch("get", json!({"id": event_id, "namespace": "ns-caller"}))
        .await;
    assert!(
        get_result.is_ok(),
        "#425: get(id=<event_uuid>) from a different namespace must succeed; got: {get_result:?}"
    );
    let get_result = get_result.unwrap();
    assert_eq!(
        get_result.get("kind").and_then(Value::as_str),
        Some("event"),
        "#425: get must have kind=event at top level"
    );
    assert_eq!(
        get_result.get("id").and_then(Value::as_str),
        Some(event_id.as_str()),
        "#425: id must match the requested event UUID"
    );
    assert_eq!(
        get_result.get("namespace").and_then(Value::as_str),
        Some(stored_namespace.as_str()),
        "#425: returned event's namespace must be preserved, not overwritten by the caller's"
    );
}

// ADR-045 §5: event `created_at` must be an ISO-8601 string at the MCP boundary,
// not a raw microsecond integer.

#[tokio::test]
async fn list_event_created_at_is_iso8601_string() {
    let pack = pack_with_events();
    pack.dispatch("create", json!({"kind": "concept", "name": "IsoEventList"}))
        .await
        .expect("create must succeed");

    let result = pack
        .dispatch("list", json!({"kind": "event", "limit": 5}))
        .await
        .expect("list(kind=event) must succeed");

    let arr = result.as_array().expect("list must return a JSON array");
    assert!(!arr.is_empty(), "must have at least one event");

    for event in arr {
        let created_at = event
            .get("created_at")
            .expect("event must have created_at field");
        let s = created_at
            .as_str()
            .expect("created_at must be a string, not an integer");
        // ISO-8601 datetime: starts with YYYY-MM-DDTHH:
        assert!(
            s.len() >= 16
                && s.as_bytes()[4] == b'-'
                && s.as_bytes()[7] == b'-'
                && s.as_bytes()[10] == b'T'
                && s.as_bytes()[13] == b':',
            "created_at must be ISO-8601, got: {s}"
        );
        assert!(
            s.starts_with("20"),
            "created_at must look like a year-2000+ timestamp, got: {s}"
        );
    }
}

#[tokio::test]
async fn get_event_created_at_is_iso8601_string() {
    let pack = pack_with_events();
    pack.dispatch("create", json!({"kind": "concept", "name": "IsoEventGet"}))
        .await
        .expect("create must succeed");

    let list_result = pack
        .dispatch("list", json!({"kind": "event", "limit": 1}))
        .await
        .expect("list must succeed");
    let events = list_result.as_array().expect("list must be array");
    assert!(!events.is_empty(), "must have at least one event");
    let event_id = events[0]
        .get("id")
        .and_then(Value::as_str)
        .expect("event must have id field")
        .to_string();

    let get_result = pack
        .dispatch("get", json!({"id": event_id}))
        .await
        .expect("get(id=event_uuid) must succeed");

    let created_at = get_result
        .get("created_at")
        .expect("event must have created_at field");
    let s = created_at
        .as_str()
        .expect("created_at must be a string, not an integer");
    assert!(
        s.len() >= 16
            && s.as_bytes()[4] == b'-'
            && s.as_bytes()[7] == b'-'
            && s.as_bytes()[10] == b'T'
            && s.as_bytes()[13] == b':',
        "created_at must be ISO-8601, got: {s}"
    );
    assert!(
        s.starts_with("20"),
        "created_at must look like a year-2000+ timestamp, got: {s}"
    );
}

#[tokio::test]
async fn update_event_uuid_returns_immutable_error() {
    let pack = pack_with_events();
    pack.dispatch(
        "create",
        json!({"kind": "concept", "name": "UpdateEventTarget"}),
    )
    .await
    .expect("create must succeed");

    let list_result = pack
        .dispatch(
            "list",
            json!({"kind": "event", "verb": "create", "limit": 1}),
        )
        .await
        .expect("list must succeed");
    let events = list_result.as_array().expect("list must be array");
    let event_id = events[0]
        .get("id")
        .and_then(Value::as_str)
        .expect("event must have id")
        .to_string();

    let err = pack
        .dispatch(
            "update",
            json!({"id": event_id, "kind": "event", "name": "should-not-apply"}),
        )
        .await
        .unwrap_err();
    assert!(
        is_invalid_input(&err),
        "update on event UUID must return InvalidInput; got: {err:?}"
    );
    assert_eq!(
        invalid_input_message(&err),
        "events are immutable — create/update/delete are not permitted"
    );
}

#[tokio::test]
async fn delete_event_uuid_returns_immutable_error_and_event_persists() {
    let pack = pack_with_events();
    pack.dispatch(
        "create",
        json!({"kind": "concept", "name": "DeleteEventTarget"}),
    )
    .await
    .expect("create must succeed");

    let list_result = pack
        .dispatch(
            "list",
            json!({"kind": "event", "verb": "create", "limit": 1}),
        )
        .await
        .expect("list must succeed");
    let events = list_result.as_array().expect("list must be array");
    let event_id = events[0]
        .get("id")
        .and_then(Value::as_str)
        .expect("event must have id")
        .to_string();

    let err = pack
        .dispatch("delete", json!({"id": event_id, "kind": "event"}))
        .await
        .unwrap_err();
    assert!(
        is_invalid_input(&err),
        "delete on event UUID must return InvalidInput; got: {err:?}"
    );
    assert_eq!(
        invalid_input_message(&err),
        "events are immutable — create/update/delete are not permitted"
    );

    // Event must still be fetchable after the failed delete.
    let get_result = pack
        .dispatch("get", json!({"id": event_id}))
        .await
        .expect("get after failed delete must succeed");
    assert_eq!(
        get_result.get("kind").and_then(Value::as_str),
        Some("event"),
        "event must still exist after failed delete"
    );
}

#[tokio::test]
async fn list_events_pagination_returns_distinct_pages() {
    let pack = pack_with_events();
    // Create three entities to generate three create audit events.
    for name in ["Paginable-A", "Paginable-B", "Paginable-C"] {
        pack.dispatch("create", json!({"kind": "concept", "name": name}))
            .await
            .expect("create must succeed");
    }

    let page1 = pack
        .dispatch(
            "list",
            json!({"kind": "event", "verb": "create", "limit": 2, "offset": 0}),
        )
        .await
        .expect("page 1 must succeed");
    let arr1 = page1.as_array().expect("must be array");
    assert_eq!(arr1.len(), 2, "page 1 must contain exactly 2 events");

    let page2 = pack
        .dispatch(
            "list",
            json!({"kind": "event", "verb": "create", "limit": 2, "offset": 2}),
        )
        .await
        .expect("page 2 must succeed");
    let arr2 = page2.as_array().expect("must be array");
    assert!(
        !arr2.is_empty(),
        "page 2 must contain at least 1 event (3 creates total)"
    );

    let id1 = arr1[0].get("id").and_then(Value::as_str).unwrap();
    let id2_first = arr2[0].get("id").and_then(Value::as_str).unwrap();
    assert_ne!(
        id1, id2_first,
        "first event on page 1 and first event on page 2 must differ"
    );
}

#[tokio::test]
async fn list_events_pagination_four_items_full_disjointness() {
    let pack = pack_with_events();
    for name in ["Pg4-A", "Pg4-B", "Pg4-C", "Pg4-D"] {
        pack.dispatch("create", json!({"kind": "concept", "name": name}))
            .await
            .expect("create must succeed");
    }

    let page1 = pack
        .dispatch(
            "list",
            json!({"kind": "event", "verb": "create", "limit": 2, "offset": 0}),
        )
        .await
        .expect("page 1 must succeed");
    let arr1 = page1.as_array().expect("must be array");
    assert_eq!(arr1.len(), 2, "page 1 must have exactly 2 events");

    let page2 = pack
        .dispatch(
            "list",
            json!({"kind": "event", "verb": "create", "limit": 2, "offset": 2}),
        )
        .await
        .expect("page 2 must succeed");
    let arr2 = page2.as_array().expect("must be array");
    assert_eq!(
        arr2.len(),
        2,
        "page 2 must have exactly 2 events with 4 total creates"
    );

    let ids1: std::collections::HashSet<&str> = arr1
        .iter()
        .map(|v| v.get("id").and_then(Value::as_str).unwrap())
        .collect();
    let ids2: std::collections::HashSet<&str> = arr2
        .iter()
        .map(|v| v.get("id").and_then(Value::as_str).unwrap())
        .collect();
    assert!(
        ids1.is_disjoint(&ids2),
        "page 1 and page 2 must have no events in common: page1={ids1:?} page2={ids2:?}"
    );
}

#[tokio::test]
async fn list_events_pagination_offset_beyond_end_returns_empty() {
    let pack = pack_with_events();
    for name in ["BeyondEnd-A", "BeyondEnd-B", "BeyondEnd-C"] {
        pack.dispatch("create", json!({"kind": "concept", "name": name}))
            .await
            .expect("create must succeed");
    }

    let result = pack
        .dispatch(
            "list",
            json!({"kind": "event", "verb": "create", "limit": 2, "offset": 99}),
        )
        .await
        .expect("large offset must not error");
    let arr = result.as_array().expect("must be array");
    assert!(
        arr.is_empty(),
        "offset beyond total event count must return empty page"
    );
}

#[tokio::test]
async fn list_unknown_kind_includes_event_in_valid_list() {
    let pack = pack();
    let err = pack
        .dispatch("list", json!({"kind": "bogus"}))
        .await
        .unwrap_err();
    let msg = invalid_input_message(&err);
    assert!(
        msg.contains("event"),
        "unknown-kind error must list 'event' as valid: {msg}"
    );
}

#[tokio::test]
async fn link_by_name_case_insensitive_match_succeeds() {
    let pack = pack();

    pack.dispatch(
        "create",
        json!({"kind": "entity", "name": "CaseSource", "entity_kind": "concept"}),
    )
    .await
    .expect("create CaseSource must succeed");

    pack.dispatch(
        "create",
        json!({"kind": "entity", "name": "CaseTarget", "entity_kind": "concept"}),
    )
    .await
    .expect("create CaseTarget must succeed");

    // Lowercase versions of the names should still resolve.
    let result = pack
        .dispatch(
            "link",
            json!({
                "source_id": "casesource",
                "target_id": "casetarget",
                "relation": "extends"
            }),
        )
        .await;
    assert!(
        result.is_ok(),
        "link with lowercase name must succeed (case-insensitive match); got: {result:?}"
    );
}

#[tokio::test]
async fn link_by_name_not_found_returns_not_found_error() {
    let pack = pack();

    pack.dispatch(
        "create",
        json!({"kind": "entity", "name": "ExistingEntity", "entity_kind": "concept"}),
    )
    .await
    .expect("create ExistingEntity must succeed");

    let err = pack
        .dispatch(
            "link",
            json!({
                "source_id": "ExistingEntity",
                "target_id": "NoSuchEntity",
                "relation": "extends"
            }),
        )
        .await
        .unwrap_err();

    assert!(
        matches!(err, RuntimeError::NotFound(_)),
        "link with non-existent name must return NotFound; got: {err:?}"
    );
    let msg = match &err {
        RuntimeError::NotFound(m) => m.as_str(),
        _ => unreachable!(),
    };
    assert!(
        msg.contains("NoSuchEntity"),
        "error must name the missing entity: {msg}"
    );
}

#[tokio::test]
async fn link_by_name_ambiguous_returns_ambiguous_error() {
    let pack = pack();

    // Create two entities with the same name in the same namespace.
    // The underlying store allows duplicate names (no unique constraint).
    pack.dispatch(
        "create",
        json!({"kind": "entity", "name": "DuplicateName", "entity_kind": "concept"}),
    )
    .await
    .expect("create first DuplicateName must succeed");

    pack.dispatch(
        "create",
        json!({"kind": "entity", "name": "DuplicateName", "entity_kind": "concept"}),
    )
    .await
    .expect("create second DuplicateName must succeed");

    pack.dispatch(
        "create",
        json!({"kind": "entity", "name": "UniqueTarget", "entity_kind": "concept"}),
    )
    .await
    .expect("create UniqueTarget must succeed");

    let err = pack
        .dispatch(
            "link",
            json!({
                "source_id": "DuplicateName",
                "target_id": "UniqueTarget",
                "relation": "extends"
            }),
        )
        .await
        .unwrap_err();

    assert!(
        matches!(err, RuntimeError::Ambiguous(_)),
        "link with ambiguous name must return Ambiguous; got: {err:?}"
    );
    let msg = match &err {
        RuntimeError::Ambiguous(m) => m.as_str(),
        _ => unreachable!(),
    };
    assert!(
        msg.contains("DuplicateName"),
        "error must name the ambiguous entity: {msg}"
    );
    assert!(
        msg.contains("found 2"),
        "error must report the count of matches: {msg}"
    );
}

// ── Issue #66: MCP display formatting ────────────────────────────────────────
//
// MCP responses always return full UUIDs and ISO 8601 timestamps.
// Display formatting (short IDs, compact dates) belongs in the CLI/UI layer.

#[tokio::test]
async fn search_event_kind_returns_invalid_input() {
    let pack = pack();
    let err = pack
        .dispatch("search", json!({"kind": "event", "query": "anything"}))
        .await
        .unwrap_err();
    assert!(
        is_invalid_input(&err),
        "search(kind=event) must return InvalidInput; got: {err:?}"
    );
}

#[tokio::test]
async fn link_output_returns_full_uuids_and_iso_dates() {
    let pack = pack();

    pack.dispatch(
        "create",
        json!({"kind": "entity", "name": "FullSrc", "entity_kind": "concept"}),
    )
    .await
    .expect("create FullSrc must succeed");

    pack.dispatch(
        "create",
        json!({"kind": "entity", "name": "FullTgt", "entity_kind": "concept"}),
    )
    .await
    .expect("create FullTgt must succeed");

    let result = pack
        .dispatch(
            "link",
            json!({
                "source_id": "FullSrc",
                "target_id": "FullTgt",
                "relation": "extends"
            }),
        )
        .await
        .expect("link must succeed");

    let id = result
        .get("id")
        .and_then(|v| v.as_str())
        .expect("id must be present");
    assert_eq!(
        id.len(),
        36,
        "MCP response must return full UUID; got: {id:?}"
    );

    let src_id = result
        .get("source_id")
        .and_then(|v| v.as_str())
        .expect("source_id must be present");
    assert_eq!(
        src_id.len(),
        36,
        "source_id must be full UUID; got: {src_id:?}"
    );

    let created_at = result
        .get("created_at")
        .and_then(|v| v.as_str())
        .expect("created_at must be a string");
    assert!(
        created_at.contains('T'),
        "created_at must be ISO 8601; got: {created_at:?}"
    );
}

// ── Bulk link: entry limit, dedup, and response shape ────────────────────────

// Fix 2: >1000 entries must return InvalidInput immediately.
#[tokio::test]
async fn bulk_link_over_1000_entries_returns_error() {
    let pack = pack();
    let a = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "BulkA", "entity_kind": "concept"}),
        )
        .await
        .unwrap();
    let a_id = a.get("id").and_then(Value::as_str).unwrap().to_string();
    let b = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "BulkB", "entity_kind": "concept"}),
        )
        .await
        .unwrap();
    let b_id = b.get("id").and_then(Value::as_str).unwrap().to_string();

    let entries: Vec<Value> = (0..1001)
        .map(|_| {
            json!({
                "source_id": a_id,
                "target_id": b_id,
                "relation": "extends",
            })
        })
        .collect();

    let err = pack
        .dispatch("link", json!({"links": entries}))
        .await
        .expect_err("1001 entries must return an error");
    assert!(
        matches!(err, khive_runtime::RuntimeError::InvalidInput(_)),
        "expected InvalidInput for >1000 bulk entries, got {err:?}"
    );
}

// Fix 3: duplicate entries in a bulk request must be deduplicated (skipped count > 0).
// Fix 4: response shape must have attempted/created/skipped/failed keys.
#[tokio::test]
async fn bulk_link_dedup_and_response_shape() {
    let pack = pack();
    let a = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "DedupA", "entity_kind": "concept"}),
        )
        .await
        .unwrap();
    let a_id = a.get("id").and_then(Value::as_str).unwrap().to_string();
    let b = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "DedupB", "entity_kind": "concept"}),
        )
        .await
        .unwrap();
    let b_id = b.get("id").and_then(Value::as_str).unwrap().to_string();
    let c = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "DedupC", "entity_kind": "concept"}),
        )
        .await
        .unwrap();
    let c_id = c.get("id").and_then(Value::as_str).unwrap().to_string();

    // 3 entries: A->B extends, A->B extends (dup), A->C extends.
    let result = pack
        .dispatch(
            "link",
            json!({
                "links": [
                    {"source_id": a_id, "target_id": b_id, "relation": "extends"},
                    {"source_id": a_id, "target_id": b_id, "relation": "extends"},
                    {"source_id": a_id, "target_id": c_id, "relation": "extends"},
                ],
                "atomic": true,
            }),
        )
        .await
        .expect("bulk link must succeed");

    assert_eq!(
        result.get("attempted").and_then(Value::as_u64),
        Some(3),
        "attempted must be 3; got {result:?}"
    );
    assert_eq!(
        result.get("created").and_then(Value::as_u64),
        Some(2),
        "created must be 2 (one dup skipped); got {result:?}"
    );
    assert_eq!(
        result.get("skipped").and_then(Value::as_u64),
        Some(1),
        "skipped must be 1; got {result:?}"
    );
    assert_eq!(
        result.get("failed").and_then(Value::as_u64),
        Some(0),
        "failed must be 0; got {result:?}"
    );
    // ADR-038: edges key must be absent when verbose is not set (F205).
    assert!(
        result.get("edges").is_none(),
        "edges must be absent without verbose=true (ADR-038 F205); got {result:?}"
    );
}

// F205: bulk link with verbose=true must include edges array; without verbose it must be absent.
#[tokio::test]
async fn bulk_link_verbose_controls_edges_key() {
    let pack = pack();
    let a = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "VerbA", "entity_kind": "concept"}),
        )
        .await
        .unwrap();
    let a_id = a.get("id").and_then(Value::as_str).unwrap().to_string();
    let b = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "VerbB", "entity_kind": "concept"}),
        )
        .await
        .unwrap();
    let b_id = b.get("id").and_then(Value::as_str).unwrap().to_string();

    // Without verbose: no edges key.
    let result_no_verbose = pack
        .dispatch(
            "link",
            json!({
                "links": [{"source_id": a_id, "target_id": b_id, "relation": "extends"}],
            }),
        )
        .await
        .expect("bulk link must succeed");
    assert!(
        result_no_verbose.get("edges").is_none(),
        "edges must be absent without verbose=true (ADR-038 F205); got {result_no_verbose:?}"
    );

    // With verbose=true: edges key present.
    let c = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "VerbC", "entity_kind": "concept"}),
        )
        .await
        .unwrap();
    let c_id = c.get("id").and_then(Value::as_str).unwrap().to_string();
    let result_verbose = pack
        .dispatch(
            "link",
            json!({
                "links": [{"source_id": a_id, "target_id": c_id, "relation": "extends"}],
                "verbose": true,
            }),
        )
        .await
        .expect("bulk link with verbose must succeed");
    assert!(
        result_verbose
            .get("edges")
            .and_then(Value::as_array)
            .is_some(),
        "edges must be present with verbose=true (ADR-038 F205); got {result_verbose:?}"
    );
}

// ---- ADR-014 curation event payload regression tests ----

/// Update an entity → list entity_updated events → assert payload has id, namespace,
/// changed_fields per ADR-014.
#[tokio::test]
async fn curation_update_entity_event_payload_has_adr014_fields() {
    let pack = pack_with_events();

    // Create then update with a name change.
    let created = pack
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "PayloadTestEntity"}),
        )
        .await
        .expect("create must succeed");
    let entity_id = created
        .get("id")
        .and_then(Value::as_str)
        .expect("create must return id")
        .to_string();

    pack.dispatch(
        "update",
        json!({"id": entity_id, "kind": "entity", "name": "PayloadTestEntityRenamed"}),
    )
    .await
    .expect("update must succeed");

    // Retrieve the entity_updated event.
    let events = pack
        .dispatch(
            "list",
            json!({"kind": "event", "event_kind": "entity_updated", "limit": 10}),
        )
        .await
        .expect("list entity_updated events must succeed");
    let arr = events.as_array().expect("list must return array");
    assert!(
        !arr.is_empty(),
        "at least one entity_updated event must be present after update"
    );

    // Find the event for our specific entity (by target_id).
    let our_event = arr
        .iter()
        .find(|e| {
            e.get("target_id")
                .and_then(Value::as_str)
                .is_some_and(|t| t == entity_id || t.starts_with(&entity_id[..8]))
        })
        .unwrap_or(&arr[0]);

    let payload = our_event
        .get("payload")
        .expect("event must have payload field");
    assert!(
        payload.get("id").is_some(),
        "entity_updated payload must contain 'id'; got {payload}"
    );
    assert!(
        payload.get("namespace").is_some(),
        "entity_updated payload must contain 'namespace'; got {payload}"
    );
    let changed = payload
        .get("changed_fields")
        .and_then(Value::as_array)
        .expect("entity_updated payload must contain 'changed_fields' array");
    assert!(
        changed.iter().any(|v| v.as_str() == Some("name")),
        "changed_fields must include 'name' when name was updated; got {changed:?}"
    );
}

/// Merge two entities → list entity_merged events → assert payload has into_id, from_id,
/// policy, edges_rewired per ADR-014.
#[tokio::test]
async fn curation_merge_entity_event_payload_has_adr014_fields() {
    let pack = pack_with_events();

    let into_e = pack
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "MergeIntoEntity"}),
        )
        .await
        .expect("create into must succeed");
    let into_id = into_e
        .get("id")
        .and_then(Value::as_str)
        .expect("create must return id")
        .to_string();

    let from_e = pack
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "MergeFromEntity"}),
        )
        .await
        .expect("create from must succeed");
    let from_id = from_e
        .get("id")
        .and_then(Value::as_str)
        .expect("create must return id")
        .to_string();

    pack.dispatch("merge", json!({"into_id": into_id, "from_id": from_id}))
        .await
        .expect("merge must succeed");

    let events = pack
        .dispatch(
            "list",
            json!({"kind": "event", "event_kind": "entity_merged", "limit": 10}),
        )
        .await
        .expect("list entity_merged events must succeed");
    let arr = events.as_array().expect("list must return array");
    assert!(
        !arr.is_empty(),
        "at least one entity_merged event must be present"
    );

    let event = &arr[0];
    let payload = event.get("payload").expect("event must have payload field");
    assert!(
        payload.get("into_id").is_some(),
        "entity_merged payload must contain 'into_id'; got {payload}"
    );
    assert!(
        payload.get("from_id").is_some(),
        "entity_merged payload must contain 'from_id'; got {payload}"
    );
    assert!(
        payload.get("policy").is_some(),
        "entity_merged payload must contain 'policy'; got {payload}"
    );
    assert!(
        payload.get("edges_rewired").is_some(),
        "entity_merged payload must contain 'edges_rewired'; got {payload}"
    );
    assert!(
        payload.get("content_strategy").is_some(),
        "entity_merged payload must contain 'content_strategy' (PR #814); got {payload}"
    );
}

/// Handler-wiring test for PR #814: `content_strategy`
/// passed on the wire through `merge(kind="entity", ...)` must reach
/// `KhiveRuntime::merge_entity` and be honored independently of the default
/// entity `strategy` (`prefer_into`).
#[tokio::test]
async fn merge_handler_wires_explicit_prefer_from_content_strategy() {
    let p = pack();

    let into = p
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "Into", "description": "desc A"}),
        )
        .await
        .expect("create into must succeed");
    let into_id = into
        .get("id")
        .and_then(Value::as_str)
        .expect("create must return id")
        .to_string();

    let from = p
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "From", "description": "desc B"}),
        )
        .await
        .expect("create from must succeed");
    let from_id = from
        .get("id")
        .and_then(Value::as_str)
        .expect("create must return id")
        .to_string();

    // strategy is left at its default (prefer_into); only content_strategy is
    // set explicitly. If the handler/runtime wiring collapses content_strategy
    // onto the entity policy, the into-description ("desc A") survives instead.
    p.dispatch(
        "merge",
        json!({"into_id": &into_id, "from_id": &from_id, "content_strategy": "prefer_from"}),
    )
    .await
    .expect("merge must succeed");

    let merged = p
        .dispatch("get", json!({"id": &into_id}))
        .await
        .expect("get must succeed");
    assert_eq!(
        merged.get("description").and_then(Value::as_str),
        Some("desc B"),
        "content_strategy=prefer_from on the wire must win over the default \
         prefer_into entity policy; got {merged}"
    );
}

/// Delete an entity with hard=true → list entity_deleted events → assert payload has
/// id, namespace, hard=true per ADR-014.
#[tokio::test]
async fn curation_delete_entity_hard_event_payload_has_adr014_fields() {
    let pack = pack_with_events();

    let created = pack
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "HardDeletePayloadEntity"}),
        )
        .await
        .expect("create must succeed");
    let entity_id = created
        .get("id")
        .and_then(Value::as_str)
        .expect("create must return id")
        .to_string();

    pack.dispatch(
        "delete",
        json!({"id": entity_id, "kind": "entity", "hard": true}),
    )
    .await
    .expect("hard delete must succeed");

    let events = pack
        .dispatch(
            "list",
            json!({"kind": "event", "event_kind": "entity_deleted", "limit": 10}),
        )
        .await
        .expect("list entity_deleted events must succeed");
    let arr = events.as_array().expect("list must return array");
    assert!(
        !arr.is_empty(),
        "at least one entity_deleted event must be present"
    );

    let event = &arr[0];
    let payload = event.get("payload").expect("event must have payload field");
    assert!(
        payload.get("id").is_some(),
        "entity_deleted payload must contain 'id'; got {payload}"
    );
    assert!(
        payload.get("namespace").is_some(),
        "entity_deleted payload must contain 'namespace'; got {payload}"
    );
    assert_eq!(
        payload.get("hard").and_then(Value::as_bool),
        Some(true),
        "entity_deleted payload must have hard=true for hard delete; got {payload}"
    );
}

// ---- ADR-022 provenance filter regression tests ----

/// list(kind="event", observed=[uuid]) must pass the filter down to storage and
/// return only events whose observed list contains that UUID.
#[tokio::test]
async fn list_event_observed_filter_is_wired_through_to_storage() {
    let pack = pack_with_events();

    // Create an entity so we have at least one known-good UUID to search with.
    let created = pack
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "ObservedFilterEntity"}),
        )
        .await
        .expect("create must succeed");
    let entity_id = created
        .get("id")
        .and_then(Value::as_str)
        .expect("create must return id")
        .to_string();

    // Query with observed=[entity_id] — may return 0 results if the store has no
    // observed projections for this entity, but must NOT return an error.
    // What we validate: the filter parses and reaches storage without a parse error.
    let result = pack
        .dispatch(
            "list",
            json!({"kind": "event", "observed": [entity_id], "limit": 10}),
        )
        .await
        .expect("list(kind=event, observed=[...]) must not return an error");
    assert!(
        result.as_array().is_some(),
        "list with observed filter must return an array; got {result}"
    );
}

/// list(kind="event", selected=[uuid]) must pass the filter down to storage without
/// returning a parse error.
#[tokio::test]
async fn list_event_selected_filter_is_wired_through_to_storage() {
    let pack = pack_with_events();

    let created = pack
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "SelectedFilterEntity"}),
        )
        .await
        .expect("create must succeed");
    let entity_id = created
        .get("id")
        .and_then(Value::as_str)
        .expect("create must return id")
        .to_string();

    let result = pack
        .dispatch(
            "list",
            json!({"kind": "event", "selected": [entity_id], "limit": 10}),
        )
        .await
        .expect("list(kind=event, selected=[...]) must not return an error");
    assert!(
        result.as_array().is_some(),
        "list with selected filter must return an array; got {result}"
    );
}

/// list(kind="event", observed=["not-a-uuid"]) must return InvalidInput.
#[tokio::test]
async fn list_event_observed_filter_invalid_uuid_returns_invalid_input() {
    let pack = pack_with_events();
    let err = pack
        .dispatch(
            "list",
            json!({"kind": "event", "observed": ["not-a-valid-uuid"], "limit": 10}),
        )
        .await
        .unwrap_err();
    assert!(
        is_invalid_input(&err),
        "invalid UUID in observed must return InvalidInput; got {err:?}"
    );
}

// ── Response-layer status remap: non-lifecycle notes unaffected ───────────────
//
// Non-task note kinds (observation, insight, etc.) do NOT carry a
// pack-owned lifecycle in `properties.status`.  The remap must leave these
// notes unchanged — `status` stays as the row-visibility value ("active"),
// and no spurious `lifecycle` field is injected.

/// create(kind=observation) → get → data.status == "active" (row-visibility, no remap)
#[tokio::test]
async fn get_observation_note_status_is_row_visibility_unchanged() {
    let pack = pack();
    let created = pack
        .dispatch(
            "create",
            json!({"kind": "observation", "content": "row-visibility test content"}),
        )
        .await
        .expect("create observation must succeed");

    let note_id = created["id"].as_str().expect("id field must be present");
    let got = pack
        .dispatch("get", json!({"id": note_id}))
        .await
        .expect("get must succeed");

    // P-H2: get returns flat — note fields at top level.
    // Non-task notes must NOT be remapped: status stays as row-visibility.
    assert_eq!(
        got["status"], "active",
        "observation note status must be row-visibility 'active'; got: {got}"
    );
    // No lifecycle field injected for non-lifecycle notes.
    assert!(
        got.get("lifecycle").is_none(),
        "observation note must NOT have a lifecycle field; got: {got}"
    );
    assert!(
        got.get("data").is_none(),
        "get must NOT wrap in {{data: ...}} (P-H2); got: {got}"
    );
}

/// list(kind=observation) → items have row-visibility status, no lifecycle field
#[tokio::test]
async fn list_observation_notes_status_is_row_visibility_unchanged() {
    let pack = pack();
    pack.dispatch(
        "create",
        json!({"kind": "observation", "content": "list remap guard content"}),
    )
    .await
    .expect("create must succeed");

    let list_resp = pack
        .dispatch("list", json!({"kind": "observation"}))
        .await
        .expect("list must succeed");
    let items = list_resp.as_array().expect("list must return array");
    assert!(!items.is_empty(), "expected at least one observation");

    for item in items {
        assert_eq!(
            item["status"], "active",
            "observation status must be row-visibility 'active'; got item: {item}"
        );
        assert!(
            item.get("lifecycle").is_none(),
            "observation must NOT have lifecycle field; got item: {item}"
        );
    }
}

// ---- Fix 1: update/delete accept absent `kind`, resolving substrate from UUID ----

/// ADR-014: `update` without `kind` resolves the substrate from the UUID.
#[tokio::test]
async fn update_entity_without_kind_resolves_from_uuid() {
    let pack = pack();

    let created = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "TestEntity", "entity_kind": "concept"}),
        )
        .await
        .expect("create must succeed");
    let id = created
        .get("id")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();

    // `update` without `kind` — substrate must be inferred from the UUID.
    let updated = pack
        .dispatch(
            "update",
            json!({"id": id, "description": "updated via UUID inference"}),
        )
        .await
        .expect("update without kind must succeed (ADR-014)");

    let desc = updated
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        desc.contains("updated via UUID inference"),
        "updated entity must carry new description; got: {updated}"
    );
}

/// ADR-014: `delete` without `kind` resolves the substrate from the UUID.
#[tokio::test]
async fn delete_entity_without_kind_resolves_from_uuid() {
    let pack = pack();

    let created = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "ToDeleteNoKind", "entity_kind": "concept"}),
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
        .expect("delete without kind must succeed (ADR-014)");

    assert_eq!(
        del.get("deleted").and_then(Value::as_bool),
        Some(true),
        "delete without kind must return deleted=true"
    );

    // Verify the entity is gone.
    let err = pack.dispatch("get", json!({"id": id})).await.unwrap_err();
    assert!(
        matches!(err, RuntimeError::NotFound(_)),
        "get after delete-without-kind must be NotFound, got: {err:?}"
    );
}

/// ADR-014: `update` without `kind` on a nonexistent UUID returns NotFound.
#[tokio::test]
async fn update_nonexistent_uuid_without_kind_returns_not_found() {
    let pack = pack();
    let err = pack
        .dispatch(
            "update",
            json!({"id": "00000000-0000-0000-0000-000000000099", "description": "ghost"}),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::NotFound(_)),
        "update on nonexistent UUID without kind must be NotFound, got: {err:?}"
    );
}

// ---- Fix 2: traverse/neighbors exclude soft-deleted entity nodes ----

/// Soft-deleted entities must not appear in `neighbors` results.
#[tokio::test]
async fn neighbors_excludes_soft_deleted_entity() {
    let pack = pack();

    let alive = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "Alive", "entity_kind": "concept"}),
        )
        .await
        .expect("create alive must succeed");
    let alive_id = alive.get("id").and_then(Value::as_str).unwrap().to_string();

    let deleted = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "ToSoftDelete", "entity_kind": "concept"}),
        )
        .await
        .expect("create deleted must succeed");
    let deleted_id = deleted
        .get("id")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();

    // Link alive → deleted.
    pack.dispatch(
        "link",
        json!({"source_id": alive_id, "target_id": deleted_id, "relation": "contains"}),
    )
    .await
    .expect("link must succeed");

    // Soft-delete the target.
    pack.dispatch("delete", json!({"id": deleted_id, "kind": "entity"}))
        .await
        .expect("delete must succeed");

    // neighbors from alive must NOT include the soft-deleted node.
    let neighbors = pack
        .dispatch(
            "neighbors",
            json!({"node_id": alive_id, "direction": "out"}),
        )
        .await
        .expect("neighbors must succeed");

    let items = neighbors.as_array().expect("neighbors must be array");
    // NeighborHit serializes `node_id` as `id` using the full 36-char hyphenated UUID.
    // `deleted_id` is the 8-char short ID from the create response, so we match by prefix.
    let ids: Vec<&str> = items
        .iter()
        .filter_map(|v| v.get("id").and_then(Value::as_str))
        .collect();
    assert!(
        !ids.iter().any(|&id| id.starts_with(deleted_id.as_str())),
        "neighbors must not include soft-deleted node (short_id={deleted_id}); ids: {ids:?}"
    );
}

/// Soft-deleted entities must not appear in `traverse` results.
#[tokio::test]
async fn traverse_excludes_soft_deleted_entity() {
    let pack = pack();

    let root = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "TraverseRoot", "entity_kind": "concept"}),
        )
        .await
        .expect("create root must succeed");
    let root_id = root.get("id").and_then(Value::as_str).unwrap().to_string();

    let ghost = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "GhostNode", "entity_kind": "concept"}),
        )
        .await
        .expect("create ghost must succeed");
    let ghost_id = ghost.get("id").and_then(Value::as_str).unwrap().to_string();

    // Link root → ghost.
    pack.dispatch(
        "link",
        json!({"source_id": root_id, "target_id": ghost_id, "relation": "contains"}),
    )
    .await
    .expect("link must succeed");

    // Soft-delete ghost.
    pack.dispatch("delete", json!({"id": ghost_id, "kind": "entity"}))
        .await
        .expect("delete must succeed");

    // traverse from root must not include the deleted node.
    let paths = pack
        .dispatch(
            "traverse",
            json!({"roots": [root_id], "max_depth": 2, "direction": "out", "include_roots": false}),
        )
        .await
        .expect("traverse must succeed");

    let arr = paths.as_array().expect("traverse must return array");
    // Each element is a GraphPath: {root_id, nodes: [{id, ...}], total_weight}.
    let all_ids: Vec<String> = arr
        .iter()
        .flat_map(|path| {
            path.get("nodes")
                .and_then(Value::as_array)
                .map(|nodes| {
                    nodes
                        .iter()
                        .filter_map(|n| n.get("id").and_then(Value::as_str))
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
        .collect();
    assert!(
        !all_ids
            .iter()
            .any(|id| ghost_id.starts_with(id.as_str()) || id.starts_with(&ghost_id[..8])),
        "traverse must not include soft-deleted node; ids: {all_ids:?}"
    );
}

// ---- #748: neighbors() must also screen soft-deleted NOTE targets ----

/// #748: a soft-deleted note reached via an `annotates` edge must not appear
/// in `neighbors()`. The original Fix-2 screen (`deleted_entity_ids`) only
/// queried the `entities` table, so a soft-deleted note-kind neighbor leaked
/// through. View-layer only — no edges are touched, no data is mutated.
#[tokio::test]
async fn neighbors_excludes_soft_deleted_note() {
    let pack = pack();

    let entity = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "Nvk748Target", "entity_kind": "concept"}),
        )
        .await
        .expect("create entity must succeed");
    let entity_id = entity
        .get("id")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();

    // Annotating note: creates a note --annotates--> entity edge.
    let note = pack
        .dispatch(
            "create",
            json!({
                "kind": "note",
                "content": "nvk748 note that will be soft-deleted",
                "annotates": [entity_id.clone()],
            }),
        )
        .await
        .expect("create annotating note must succeed");
    let note_id = note.get("id").and_then(Value::as_str).unwrap().to_string();

    // A second, non-deleted annotating note — must remain visible after the
    // first note is soft-deleted (regression: the fix must not over-filter).
    let live_note = pack
        .dispatch(
            "create",
            json!({
                "kind": "note",
                "content": "nvk748 note that stays alive",
                "annotates": [entity_id.clone()],
            }),
        )
        .await
        .expect("create second annotating note must succeed");
    let live_note_id = live_note
        .get("id")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();

    // Before delete: both notes are visible as incoming neighbors of the entity.
    let before = pack
        .dispatch(
            "neighbors",
            json!({"node_id": entity_id, "direction": "in"}),
        )
        .await
        .expect("neighbors (pre-delete) must succeed");
    let before_ids: Vec<&str> = before
        .as_array()
        .expect("neighbors must be array")
        .iter()
        .filter_map(|v| v.get("id").and_then(Value::as_str))
        .collect();
    assert!(
        before_ids.iter().any(|&id| id == note_id),
        "#748 setup: note must appear as a neighbor before delete; ids={before_ids:?}"
    );
    assert!(
        before_ids.iter().any(|&id| id == live_note_id),
        "#748 setup: live_note must appear as a neighbor before delete; ids={before_ids:?}"
    );

    // Soft-delete the first note.
    pack.dispatch("delete", json!({"id": note_id, "kind": "note"}))
        .await
        .expect("delete note must succeed");

    let after = pack
        .dispatch(
            "neighbors",
            json!({"node_id": entity_id, "direction": "in"}),
        )
        .await
        .expect("neighbors (post-delete) must succeed");
    let after_ids: Vec<&str> = after
        .as_array()
        .expect("neighbors must be array")
        .iter()
        .filter_map(|v| v.get("id").and_then(Value::as_str))
        .collect();
    assert!(
        !after_ids.iter().any(|&id| id == note_id),
        "#748: soft-deleted note must not appear in neighbors(); ids={after_ids:?}"
    );
    assert!(
        after_ids.iter().any(|&id| id == live_note_id),
        "#748: non-deleted note must still appear in neighbors(); ids={after_ids:?}"
    );
}

/// #748 regression guard: the pre-existing entity-only soft-delete screen
/// (Fix 2) must keep working after extending `deleted_entity_ids` to also
/// cover notes.
#[tokio::test]
async fn neighbors_still_excludes_soft_deleted_entity_after_note_fix() {
    let pack = pack();

    let alive = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "Nvk748AliveEntity", "entity_kind": "concept"}),
        )
        .await
        .expect("create alive must succeed");
    let alive_id = alive.get("id").and_then(Value::as_str).unwrap().to_string();

    let deleted = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "Nvk748DeletedEntity", "entity_kind": "concept"}),
        )
        .await
        .expect("create deleted must succeed");
    let deleted_id = deleted
        .get("id")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();

    pack.dispatch(
        "link",
        json!({"source_id": alive_id, "target_id": deleted_id, "relation": "contains"}),
    )
    .await
    .expect("link must succeed");

    pack.dispatch("delete", json!({"id": deleted_id, "kind": "entity"}))
        .await
        .expect("delete must succeed");

    let neighbors = pack
        .dispatch(
            "neighbors",
            json!({"node_id": alive_id, "direction": "out"}),
        )
        .await
        .expect("neighbors must succeed");
    let ids: Vec<&str> = neighbors
        .as_array()
        .expect("neighbors must be array")
        .iter()
        .filter_map(|v| v.get("id").and_then(Value::as_str))
        .collect();
    assert!(
        !ids.iter().any(|&id| id == deleted_id),
        "#748 regression: soft-deleted entity must still be excluded; ids={ids:?}"
    );
}

// ---- K-C1: link response preserves caller source/target for symmetric relations ----

/// K-C1 regression: for `competes_with` (symmetric), the runtime canonicalises
/// endpoint order (lower UUID first). The response MUST reflect the caller's
/// original source_id / target_id, not the internal storage order.
#[tokio::test]
async fn link_symmetric_relation_response_preserves_caller_order() {
    let pack = pack();

    let a = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "EntityA", "entity_kind": "concept"}),
        )
        .await
        .expect("create A must succeed");
    let a_full_id = a
        .get("full_id")
        .and_then(Value::as_str)
        .or_else(|| a.get("id").and_then(Value::as_str))
        .expect("A must have an id")
        .to_string();

    let b = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "EntityB", "entity_kind": "concept"}),
        )
        .await
        .expect("create B must succeed");
    let b_full_id = b
        .get("full_id")
        .and_then(Value::as_str)
        .or_else(|| b.get("id").and_then(Value::as_str))
        .expect("B must have an id")
        .to_string();

    // Link A → B with competes_with. One of A/B will have the lower UUID.
    let edge = pack
        .dispatch(
            "link",
            json!({
                "source_id": a_full_id,
                "target_id": b_full_id,
                "relation": "competes_with"
            }),
        )
        .await
        .expect("link competes_with must succeed");

    // K-C1: regardless of internal canonical ordering, the response source_id
    // must equal A's id and target_id must equal B's id.
    let resp_source = edge
        .get("source_id")
        .and_then(Value::as_str)
        .expect("response must have source_id");
    let resp_target = edge
        .get("target_id")
        .and_then(Value::as_str)
        .expect("response must have target_id");

    assert!(
        resp_source.starts_with(&a_full_id[..8]) || a_full_id.starts_with(resp_source),
        "K-C1: response source_id must be A's id; got source={resp_source}, expected A={a_full_id}"
    );
    assert!(
        resp_target.starts_with(&b_full_id[..8]) || b_full_id.starts_with(resp_target),
        "K-C1: response target_id must be B's id; got target={resp_target}, expected B={b_full_id}"
    );
}

// ---- K-C2: neighbors direction filter is respected ----

/// K-C2 regression: `direction="incoming"` must return edges where the queried
/// node is the target; `direction="outgoing"` must return edges where it is the
/// source. Both canonical (`"in"` / `"out"`) and verbose (`"incoming"` / `"outgoing"`)
/// spellings must work.
#[tokio::test]
async fn neighbors_direction_filter_incoming_outgoing() {
    let pack = pack();

    // Create A, B, C. Link A-->B and C-->B (so B has one outgoing and two incoming).
    let a = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "NeighDir_A", "entity_kind": "concept"}),
        )
        .await
        .expect("create A");
    let a_id = a.get("id").and_then(Value::as_str).unwrap().to_string();

    let b = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "NeighDir_B", "entity_kind": "concept"}),
        )
        .await
        .expect("create B");
    let b_id = b.get("id").and_then(Value::as_str).unwrap().to_string();

    let c = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "NeighDir_C", "entity_kind": "concept"}),
        )
        .await
        .expect("create C");
    let c_id = c.get("id").and_then(Value::as_str).unwrap().to_string();

    // A-->B (B is target of A, so B has incoming from A)
    pack.dispatch(
        "link",
        json!({"source_id": a_id, "target_id": b_id, "relation": "extends"}),
    )
    .await
    .expect("link A->B");

    // B-->C (B is source, C is target, so B has outgoing to C)
    pack.dispatch(
        "link",
        json!({"source_id": b_id, "target_id": c_id, "relation": "extends"}),
    )
    .await
    .expect("link B->C");

    // neighbors(B, incoming) must return A.
    for dir_spelling in ["in", "incoming"] {
        let incoming = pack
            .dispatch("neighbors", json!({"id": b_id, "direction": dir_spelling}))
            .await
            .expect("neighbors incoming must succeed");
        let items = incoming.as_array().expect("must be array");
        let node_ids: Vec<&str> = items
            .iter()
            .filter_map(|v| v.get("id").and_then(Value::as_str))
            .collect();
        assert!(
            node_ids
                .iter()
                .any(|&id| id == a_id || a_id.starts_with(id) || id.starts_with(&a_id[..8])),
            "K-C2: neighbors(B, {dir_spelling}) must return A; got: {node_ids:?}"
        );
        assert!(
            !node_ids
                .iter()
                .any(|&id| id == c_id || c_id.starts_with(id) || id.starts_with(&c_id[..8])),
            "K-C2: neighbors(B, {dir_spelling}) must NOT return C; got: {node_ids:?}"
        );
    }

    // neighbors(B, outgoing) must return C.
    for dir_spelling in ["out", "outgoing"] {
        let outgoing = pack
            .dispatch("neighbors", json!({"id": b_id, "direction": dir_spelling}))
            .await
            .expect("neighbors outgoing must succeed");
        let items = outgoing.as_array().expect("must be array");
        let node_ids: Vec<&str> = items
            .iter()
            .filter_map(|v| v.get("id").and_then(Value::as_str))
            .collect();
        assert!(
            node_ids
                .iter()
                .any(|&id| id == c_id || c_id.starts_with(id) || id.starts_with(&c_id[..8])),
            "K-C2: neighbors(B, {dir_spelling}) must return C; got: {node_ids:?}"
        );
        assert!(
            !node_ids
                .iter()
                .any(|&id| id == a_id || a_id.starts_with(id) || id.starts_with(&a_id[..8])),
            "K-C2: neighbors(B, {dir_spelling}) must NOT return A; got: {node_ids:?}"
        );
    }
}

/// #445 regression: omitted `direction` must default to "both" (per the
/// advertised `help=true` contract), not outgoing-only. On an incoming-only
/// graph (A --extends--> B, no outgoing edges from B), `neighbors(B)` with no
/// `direction` must still surface the incoming edge from A.
#[tokio::test]
async fn neighbors_default_direction_includes_incoming_edges() {
    let pack = pack();

    let a = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "DefaultDir_A", "entity_kind": "concept"}),
        )
        .await
        .expect("create A");
    let a_id = a.get("id").and_then(Value::as_str).unwrap().to_string();

    let b = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "DefaultDir_B", "entity_kind": "concept"}),
        )
        .await
        .expect("create B");
    let b_id = b.get("id").and_then(Value::as_str).unwrap().to_string();

    // A --extends--> B; B has no outgoing edges.
    pack.dispatch(
        "link",
        json!({"source_id": a_id, "target_id": b_id, "relation": "extends"}),
    )
    .await
    .expect("link A->B");

    let result = pack
        .dispatch("neighbors", json!({"id": b_id}))
        .await
        .expect("neighbors with omitted direction must succeed");
    let items = result.as_array().expect("must be array");
    let node_ids: Vec<&str> = items
        .iter()
        .filter_map(|v| v.get("id").and_then(Value::as_str))
        .collect();
    assert!(
        node_ids
            .iter()
            .any(|&id| id == a_id || a_id.starts_with(id) || id.starts_with(&a_id[..8])),
        "#445: neighbors(B) with omitted direction must default to both and surface A; got: {node_ids:?}"
    );
}

/// #480 regression: an unrecognized `direction` string (e.g. a plausible typo
/// like "inbound") must be rejected with a descriptive error, not silently
/// coerced to outgoing-only.
#[tokio::test]
async fn neighbors_invalid_direction_is_rejected() {
    let pack = pack();

    let b = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "InvalidDir_B", "entity_kind": "concept"}),
        )
        .await
        .expect("create B");
    let b_id = b.get("id").and_then(Value::as_str).unwrap().to_string();

    let result = pack
        .dispatch("neighbors", json!({"id": b_id, "direction": "inbound"}))
        .await;
    assert!(
        result.is_err(),
        "#480: neighbors with direction=\"inbound\" must be rejected, not silently coerced"
    );
    let err = result.unwrap_err();
    assert!(is_invalid_input(&err), "must be InvalidInput; got: {err:?}");
    let msg = invalid_input_message(&err);
    assert!(
        msg.contains("unknown direction") && msg.contains("out | outgoing | in | incoming | both"),
        "#480: error must list valid direction values; got: {msg}"
    );
}

/// #445 regression (traverse variant): omitted `direction` must default to
/// "both", so traversing from an incoming-only node still expands via the
/// incoming edge.
#[tokio::test]
async fn traverse_default_direction_includes_incoming_edges() {
    let pack = pack();

    let a = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "TraverseDefaultDir_A", "entity_kind": "concept"}),
        )
        .await
        .expect("create A");
    let a_id = a.get("id").and_then(Value::as_str).unwrap().to_string();

    let b = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "TraverseDefaultDir_B", "entity_kind": "concept"}),
        )
        .await
        .expect("create B");
    let b_id = b.get("id").and_then(Value::as_str).unwrap().to_string();

    pack.dispatch(
        "link",
        json!({"source_id": a_id, "target_id": b_id, "relation": "extends"}),
    )
    .await
    .expect("link A->B");

    let result = pack
        .dispatch(
            "traverse",
            json!({"roots": [b_id], "max_depth": 1, "include_roots": false}),
        )
        .await
        .expect("traverse with omitted direction must succeed");
    let items = result.as_array().expect("must be array");
    assert!(
        !items.is_empty(),
        "#445: traverse(roots=[B]) with omitted direction must default to both and expand via the incoming edge from A"
    );
}

/// #480 regression (traverse variant): an unrecognized `direction` string must
/// be rejected with a descriptive error.
#[tokio::test]
async fn traverse_invalid_direction_is_rejected() {
    let pack = pack();

    let b = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "TraverseInvalidDir_B", "entity_kind": "concept"}),
        )
        .await
        .expect("create B");
    let b_id = b.get("id").and_then(Value::as_str).unwrap().to_string();

    let result = pack
        .dispatch("traverse", json!({"roots": [b_id], "direction": "inbound"}))
        .await;
    assert!(
        result.is_err(),
        "#480: traverse with direction=\"inbound\" must be rejected, not silently coerced"
    );
    let err = result.unwrap_err();
    assert!(is_invalid_input(&err), "must be InvalidInput; got: {err:?}");
    let msg = invalid_input_message(&err);
    assert!(
        msg.contains("unknown direction") && msg.contains("out | outgoing | in | incoming | both"),
        "#480: error must list valid direction values; got: {msg}"
    );
}

// ── verbs() dispatch-level tests ────────
//
// A fake pack with one public verb and one subhandler so we can verify that
// `verbs()` excludes subhandlers and that category/pack filters work correctly.

static FAKE_SUBHANDLER_HANDLERS: [HandlerDef; 2] = [
    HandlerDef {
        name: "fake.pub",
        description: "Public verb on fake pack",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    },
    HandlerDef {
        name: "fake.internal",
        description: "Internal subhandler on fake pack",
        visibility: Visibility::Subhandler,
        category: VerbCategory::Assertive,
        params: &[ParamDef {
            name: "input",
            param_type: "string",
            required: false,
            description: "Internal embedding input.",
        }],
    },
];

/// A minimal pack that exposes one public verb and one internal subhandler.
/// Used to verify that `verbs()` excludes subhandlers across pack boundaries.
struct FakeSubhandlerPack;

impl Pack for FakeSubhandlerPack {
    const NAME: &'static str = "fake";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &FAKE_SUBHANDLER_HANDLERS;
    const REQUIRES: &'static [&'static str] = &["kg"];
}

#[async_trait]
impl PackRuntime for FakeSubhandlerPack {
    fn name(&self) -> &str {
        FakeSubhandlerPack::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        FakeSubhandlerPack::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        FakeSubhandlerPack::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        FakeSubhandlerPack::HANDLERS
    }

    fn requires(&self) -> &'static [&'static str] {
        FakeSubhandlerPack::REQUIRES
    }

    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Err(RuntimeError::InvalidInput(format!(
            "FakeSubhandlerPack does not handle verb {verb:?}"
        )))
    }
}

fn pack_with_subhandler_pack() -> Fixture {
    let rt = KhiveRuntime::memory().expect("in-memory runtime must succeed");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt));
    builder.register(FakeSubhandlerPack);
    Fixture {
        registry: builder.build().expect("registry builds"),
    }
}

/// `verbs()` with no filters returns all public verbs (unfiltered output).
#[tokio::test]
async fn verbs_dispatch_unfiltered_returns_public_verbs() {
    let pack = pack();
    let result = pack
        .dispatch("verbs", json!({}))
        .await
        .expect("verbs() must succeed");

    let verbs_arr = result["verbs"].as_array().expect("verbs must be an array");
    assert!(
        !verbs_arr.is_empty(),
        "verbs() must return at least one verb; got empty array"
    );
    // `verbs` itself must appear in the list.
    let names: Vec<&str> = verbs_arr
        .iter()
        .filter_map(|v| v["verb"].as_str())
        .collect();
    assert!(
        names.contains(&"verbs"),
        "verbs() output must include 'verbs' itself; got: {names:?}"
    );
    // `create` (an Assertive kg verb) must appear.
    assert!(
        names.contains(&"create"),
        "verbs() output must include 'create'; got: {names:?}"
    );
    // `total` must equal the array length.
    let total = result["total"].as_u64().expect("total must be an integer");
    assert_eq!(
        total as usize,
        verbs_arr.len(),
        "verbs.total must match verbs array length"
    );
}

/// `verbs(category="Assertive")` returns only Assertive verbs and no others.
#[tokio::test]
async fn verbs_dispatch_category_filter_assertive() {
    let pack = pack();
    let result = pack
        .dispatch("verbs", json!({"category": "Assertive"}))
        .await
        .expect("verbs(category=Assertive) must succeed");

    let verbs_arr = result["verbs"].as_array().expect("verbs must be an array");
    assert!(
        !verbs_arr.is_empty(),
        "category=Assertive must match at least one verb"
    );
    // Every returned verb must be Assertive.
    for entry in verbs_arr {
        let cat = entry["category"].as_str().unwrap_or("");
        assert_eq!(
            cat, "Assertive",
            "verbs(category=Assertive) must only return Assertive verbs; got: {entry}"
        );
    }
    // `search` is Assertive — must appear.
    let names: Vec<&str> = verbs_arr
        .iter()
        .filter_map(|v| v["verb"].as_str())
        .collect();
    assert!(
        names.contains(&"search"),
        "verbs(category=Assertive) must include 'search'; got: {names:?}"
    );
}

/// `verbs(pack="kg")` returns only kg-owned verbs and no verbs from other packs.
#[tokio::test]
async fn verbs_dispatch_pack_filter_kg() {
    let pack = pack_with_subhandler_pack();
    let result = pack
        .dispatch("verbs", json!({"pack": "kg"}))
        .await
        .expect("verbs(pack=kg) must succeed");

    let verbs_arr = result["verbs"].as_array().expect("verbs must be an array");
    assert!(
        !verbs_arr.is_empty(),
        "pack=kg must match at least one verb"
    );
    // Every returned entry must belong to "kg".
    for entry in verbs_arr {
        let p = entry["pack"].as_str().unwrap_or("");
        assert_eq!(
            p, "kg",
            "verbs(pack=kg) must only return kg verbs; got: {entry}"
        );
    }
    // The `fake.pub` verb from FakeSubhandlerPack must NOT appear.
    let names: Vec<&str> = verbs_arr
        .iter()
        .filter_map(|v| v["verb"].as_str())
        .collect();
    assert!(
        !names.contains(&"fake.pub"),
        "verbs(pack=kg) must not include fake.pub; got: {names:?}"
    );
}

/// `verbs()` must exclude subhandlers even when a second pack has them.
#[tokio::test]
async fn verbs_dispatch_excludes_subhandlers_across_packs() {
    let pack = pack_with_subhandler_pack();
    let result = pack
        .dispatch("verbs", json!({}))
        .await
        .expect("verbs() with fake+kg packs must succeed");

    let verbs_arr = result["verbs"].as_array().expect("verbs must be an array");
    let names: Vec<&str> = verbs_arr
        .iter()
        .filter_map(|v| v["verb"].as_str())
        .collect();

    // `fake.pub` is Verb-visibility — must appear.
    assert!(
        names.contains(&"fake.pub"),
        "verbs() must include public verb fake.pub; got: {names:?}"
    );
    // `fake.internal` is Subhandler-visibility — must NOT appear.
    assert!(
        !names.contains(&"fake.internal"),
        "verbs() must NOT include subhandler fake.internal; got: {names:?}"
    );
}

/// `verbs(pack="fake")` returns the one public fake verb and excludes the subhandler.
#[tokio::test]
async fn verbs_dispatch_pack_filter_fake_excludes_subhandler() {
    let pack = pack_with_subhandler_pack();
    let result = pack
        .dispatch("verbs", json!({"pack": "fake"}))
        .await
        .expect("verbs(pack=fake) must succeed");

    let verbs_arr = result["verbs"].as_array().expect("verbs must be an array");
    let names: Vec<&str> = verbs_arr
        .iter()
        .filter_map(|v| v["verb"].as_str())
        .collect();

    assert_eq!(
        names,
        vec!["fake.pub"],
        "verbs(pack=fake) must return exactly [fake.pub], not the subhandler"
    );
}

// M2 / H1 regression: three parallel singleton link() calls for the same
// (source, target, relation) triple must all return the same edge ID and the
// database must contain exactly one edge row for that triple.
//
// Before the H1 fix, each call generated a fresh UUID before the insert; the
// losing calls returned their locally-generated IDs even though the database
// stored a different (winning) row ID.  After the fix, link() reads back the
// persisted row by natural key so every caller receives the same stored ID.
#[tokio::test]
async fn parallel_link_same_triple_returns_identical_ids() {
    let pack = pack();

    // Create two entities to link.
    let a = pack
        .dispatch("create", json!({"kind": "concept", "name": "ParLinkA"}))
        .await
        .expect("create A must succeed");
    let a_id = a.get("id").and_then(Value::as_str).unwrap().to_string();

    let b = pack
        .dispatch("create", json!({"kind": "concept", "name": "ParLinkB"}))
        .await
        .expect("create B must succeed");
    let b_id = b.get("id").and_then(Value::as_str).unwrap().to_string();

    // Fire three concurrent link() calls for the same (A→B, extends) triple.
    // tokio::join! runs all branches as concurrent tasks on the same executor;
    // the shared in-memory KhiveRuntime uses a pool writer, so the three upserts
    // are serialised at the DB level while being logically concurrent at the call
    // site — exactly the scenario that exposed the phantom-ID bug.
    let link_args = json!({"source_id": a_id, "target_id": b_id, "relation": "extends"});

    let p1 = pack.clone();
    let p2 = pack.clone();
    let p3 = pack.clone();
    let a1 = link_args.clone();
    let a2 = link_args.clone();
    let a3 = link_args.clone();

    let (r1, r2, r3) = tokio::join!(
        p1.dispatch("link", a1),
        p2.dispatch("link", a2),
        p3.dispatch("link", a3),
    );

    let edge1 = r1.expect("link call 1 must succeed");
    let edge2 = r2.expect("link call 2 must succeed");
    let edge3 = r3.expect("link call 3 must succeed");

    let id1 = edge1.get("id").and_then(Value::as_str).unwrap_or("");
    let id2 = edge2.get("id").and_then(Value::as_str).unwrap_or("");
    let id3 = edge3.get("id").and_then(Value::as_str).unwrap_or("");

    assert!(
        !id1.is_empty() && id1 == id2 && id2 == id3,
        "H1: all three parallel link() calls must return the same edge ID; got: {id1:?}, {id2:?}, {id3:?}"
    );

    // Exactly one edge row must exist for this triple.
    let list_result = pack
        .dispatch(
            "list",
            json!({"kind": "edge", "source_id": a_id, "target_id": b_id, "relations": ["extends"]}),
        )
        .await
        .expect("list edges must succeed");
    let edges = list_result.as_array().expect("list must return array");
    assert_eq!(
        edges.len(),
        1,
        "H1: exactly one edge row must exist for the triple after three parallel link() calls; got: {edges:?}"
    );
}

// R4 / H1 regression: singleton link() must go through runtime.link()
// upsert even when the triple already exists, so caller-supplied weight and metadata
// are persisted (ADR-009 §edge-upsert contract).
//
// Before the r4 fix the handler pre-read the existing edge and returned it directly,
// silently dropping any new weight / metadata the caller passed.
#[tokio::test]
async fn singleton_link_updates_weight_and_metadata_on_existing_triple() {
    let pack = pack();

    // Create two entities.
    let a = pack
        .dispatch("create", json!({"kind": "concept", "name": "R4LinkA"}))
        .await
        .expect("create A must succeed");
    let a_id = a.get("id").and_then(Value::as_str).unwrap().to_string();

    let b = pack
        .dispatch("create", json!({"kind": "concept", "name": "R4LinkB"}))
        .await
        .expect("create B must succeed");
    let b_id = b.get("id").and_then(Value::as_str).unwrap().to_string();

    // First link: weight=0.3, metadata={"first": "v1"}.
    let edge1 = pack
        .dispatch(
            "link",
            json!({
                "source_id": a_id,
                "target_id": b_id,
                "relation": "extends",
                "weight": 0.3,
                "metadata": {"first": "v1"}
            }),
        )
        .await
        .expect("first link must succeed");
    let id1 = edge1
        .get("id")
        .and_then(Value::as_str)
        .expect("first link must return an id")
        .to_string();

    // Second link on the same triple: weight=0.8, metadata={"second": "v2"}.
    let edge2 = pack
        .dispatch(
            "link",
            json!({
                "source_id": a_id,
                "target_id": b_id,
                "relation": "extends",
                "weight": 0.8,
                "metadata": {"second": "v2"}
            }),
        )
        .await
        .expect("second link must succeed");
    let id2 = edge2
        .get("id")
        .and_then(Value::as_str)
        .expect("second link must return an id")
        .to_string();

    // IDs must be the same persisted row.
    assert_eq!(
        id1, id2,
        "R4: singleton link() on existing triple must return the same stable edge ID"
    );

    // Fetch the row and assert weight and metadata were updated.
    let fetched = pack
        .dispatch("get", json!({"id": id1}))
        .await
        .expect("get edge by id must succeed");

    let stored_weight = fetched
        .get("weight")
        .and_then(Value::as_f64)
        .expect("fetched edge must have weight");
    assert!(
        (stored_weight - 0.8).abs() < 1e-9,
        "R4: weight must be updated to 0.8 by second link() call; got {stored_weight}"
    );

    let stored_meta = fetched
        .get("metadata")
        .expect("fetched edge must have metadata");
    assert!(
        stored_meta.get("second").is_some(),
        "R4: metadata must contain 'second' key from second link() call; got {stored_meta}"
    );

    // Exactly one edge row for this triple.
    let list_result = pack
        .dispatch(
            "list",
            json!({"kind": "edge", "source_id": a_id, "target_id": b_id, "relations": ["extends"]}),
        )
        .await
        .expect("list edges must succeed");
    let edges = list_result.as_array().expect("list must return array");
    assert_eq!(
        edges.len(),
        1,
        "R4: exactly one edge row must exist for the triple; got: {edges:?}"
    );
}

// ---- Merge symmetric-relation canonicalization regression (ADR-002 §134) ----

/// After merging B into A, B's `competes_with` edge to C is rewired to A→C.
/// If A already has a `competes_with` edge to C, the rewire is a conflict:
/// exactly ONE live edge must survive, and its stored endpoints must satisfy
/// `source_uuid < target_uuid` (canonical form for symmetric relations).
#[tokio::test]
async fn merge_rewire_symmetric_relation_canonicalization() {
    let pack = pack();

    // Create A, B, C.
    let a_id = pack
        .dispatch("create", json!({"kind": "concept", "name": "MergeSymA"}))
        .await
        .expect("create A")
        .get("id")
        .and_then(Value::as_str)
        .expect("A must have id")
        .to_string();

    let b_id = pack
        .dispatch("create", json!({"kind": "concept", "name": "MergeSymB"}))
        .await
        .expect("create B")
        .get("id")
        .and_then(Value::as_str)
        .expect("B must have id")
        .to_string();

    let c_id = pack
        .dispatch("create", json!({"kind": "concept", "name": "MergeSymC"}))
        .await
        .expect("create C")
        .get("id")
        .and_then(Value::as_str)
        .expect("C must have id")
        .to_string();

    // A competes_with C — canonical form stored by the runtime.
    pack.dispatch(
        "link",
        json!({"source_id": a_id, "target_id": c_id, "relation": "competes_with"}),
    )
    .await
    .expect("link A competes_with C");

    // B competes_with C — also stored in canonical form.
    pack.dispatch(
        "link",
        json!({"source_id": b_id, "target_id": c_id, "relation": "competes_with"}),
    )
    .await
    .expect("link B competes_with C");

    // Merge B into A. B's competes_with edge to C should be rewired to A→C,
    // but A already owns that triple → exactly one live edge must survive.
    pack.dispatch("merge", json!({"into_id": a_id, "from_id": b_id}))
        .await
        .expect("merge B into A must succeed");

    // Assert: neighbors(A, competes_with) returns exactly C (one neighbor).
    let neighbors = pack
        .dispatch(
            "neighbors",
            json!({"id": a_id, "relations": ["competes_with"]}),
        )
        .await
        .expect("neighbors of A with competes_with must succeed");
    let items = neighbors.as_array().expect("neighbors must return array");
    let neighbor_ids: Vec<&str> = items
        .iter()
        .filter_map(|v| v.get("id").and_then(Value::as_str))
        .collect();
    assert_eq!(
        neighbor_ids.len(),
        1,
        "merge must leave exactly one competes_with neighbor of A; got: {neighbor_ids:?}"
    );
    assert!(
        neighbor_ids[0] == c_id
            || c_id.starts_with(neighbor_ids[0])
            || neighbor_ids[0].starts_with(&c_id[..8]),
        "the sole competes_with neighbor of A must be C; got: {:?}",
        neighbor_ids[0]
    );

    // Bonus: the surviving edge row must have source_uuid < target_uuid (canonical form).
    // list(kind=edge, relations=[competes_with], source_id=<canonical_src>) where
    // canonical_src is the lower of the two UUIDs.
    let (canon_src, canon_tgt) = if a_id < c_id {
        (a_id.as_str(), c_id.as_str())
    } else {
        (c_id.as_str(), a_id.as_str())
    };
    let edge_list = pack
        .dispatch(
            "list",
            json!({"kind": "edge", "source_id": canon_src, "target_id": canon_tgt, "relations": ["competes_with"]}),
        )
        .await
        .expect("list edge in canonical order must succeed");
    let edges = edge_list.as_array().expect("list must return array");
    assert_eq!(
        edges.len(),
        1,
        "exactly one canonically-ordered edge row must exist; got: {edges:?}"
    );
}

// ---- H1: update_edge canonicalizes symmetric relations ----

/// H1-a: updating an edge from a non-symmetric relation to `competes_with`
/// must store the row with `source_uuid < target_uuid` (canonical form).
#[tokio::test]
async fn update_edge_to_symmetric_relation_canonicalizes_endpoints() {
    let pack = pack();

    let a_id = pack
        .dispatch("create", json!({"kind": "concept", "name": "UpdateSymA"}))
        .await
        .expect("create A")
        .get("id")
        .and_then(Value::as_str)
        .expect("A must have id")
        .to_string();

    let b_id = pack
        .dispatch("create", json!({"kind": "concept", "name": "UpdateSymB"}))
        .await
        .expect("create B")
        .get("id")
        .and_then(Value::as_str)
        .expect("B must have id")
        .to_string();

    // Determine which UUID is larger so we can link in non-canonical order.
    // We want the initial link to be stored as the HIGHER uuid → LOWER uuid so that
    // when we change to competes_with the canonical form requires a swap.
    let (src_id, tgt_id) = if a_id > b_id {
        (a_id.as_str(), b_id.as_str())
    } else {
        (b_id.as_str(), a_id.as_str())
    };

    // Link src -[extends]-> tgt (non-symmetric; valid in either direction).
    let link_resp = pack
        .dispatch(
            "link",
            json!({"source_id": src_id, "target_id": tgt_id, "relation": "extends"}),
        )
        .await
        .expect("link extends must succeed");
    let edge_id = link_resp
        .get("id")
        .and_then(Value::as_str)
        .expect("link must return edge id")
        .to_string();

    // Update the relation to competes_with (symmetric).
    let updated = pack
        .dispatch(
            "update",
            json!({"kind": "edge", "id": edge_id, "relation": "competes_with"}),
        )
        .await
        .expect("update to competes_with must succeed");

    // The returned edge must satisfy canonical order: source_uuid < target_uuid.
    let ret_src = updated
        .get("source_id")
        .and_then(Value::as_str)
        .expect("returned edge must have source_id")
        .to_string();
    let ret_tgt = updated
        .get("target_id")
        .and_then(Value::as_str)
        .expect("returned edge must have target_id")
        .to_string();
    assert!(
        ret_src < ret_tgt,
        "H1-a: update_edge to symmetric relation must canonicalize source < target; \
         got source={ret_src}, target={ret_tgt}"
    );

    // Verify by listing with the canonical triple — exactly one edge must exist.
    let canon_s = ret_src.as_str();
    let canon_t = ret_tgt.as_str();
    let edge_list = pack
        .dispatch(
            "list",
            json!({"kind": "edge", "source_id": canon_s, "target_id": canon_t, "relations": ["competes_with"]}),
        )
        .await
        .expect("list canonical edge must succeed");
    let listed: &Vec<Value> = edge_list.as_array().expect("list must return array");
    assert_eq!(
        listed.len(),
        1,
        "H1-a: exactly one canonical competes_with edge must exist; got: {listed:?}"
    );
}

/// H1-b: if a canonical `B -[competes_with]-> A` row already exists (B < A),
/// then updating `A -[extends]-> B` to `competes_with` must not create a
/// duplicate — the existing canonical row must survive as the sole edge.
#[tokio::test]
async fn update_edge_to_symmetric_relation_no_duplicate_when_canonical_exists() {
    let pack = pack();

    let a_id = pack
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "UpdateSymDupA"}),
        )
        .await
        .expect("create A")
        .get("id")
        .and_then(Value::as_str)
        .expect("A must have id")
        .to_string();

    let b_id = pack
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "UpdateSymDupB"}),
        )
        .await
        .expect("create B")
        .get("id")
        .and_then(Value::as_str)
        .expect("B must have id")
        .to_string();

    // Create the canonical competes_with edge B↔A (runtime stores it in canonical form).
    pack.dispatch(
        "link",
        json!({"source_id": a_id, "target_id": b_id, "relation": "competes_with"}),
    )
    .await
    .expect("pre-create canonical competes_with must succeed");

    // Determine non-canonical direction for extends: higher_uuid → lower_uuid.
    let (ext_src, ext_tgt) = if a_id > b_id {
        (a_id.as_str(), b_id.as_str())
    } else {
        (b_id.as_str(), a_id.as_str())
    };

    // Link the non-canonical direction with a non-symmetric relation.
    let link_resp = pack
        .dispatch(
            "link",
            json!({"source_id": ext_src, "target_id": ext_tgt, "relation": "extends"}),
        )
        .await
        .expect("link extends must succeed");
    let edge_id = link_resp
        .get("id")
        .and_then(Value::as_str)
        .expect("link must return edge id")
        .to_string();

    // Update to competes_with. The canonical row already exists → the extends
    // edge should be absorbed (no duplicate created).
    pack.dispatch(
        "update",
        json!({"kind": "edge", "id": edge_id, "relation": "competes_with"}),
    )
    .await
    .expect("update to competes_with when canonical exists must succeed");

    // List ALL competes_with edges between A and B — must be exactly one.
    let (canon_s, canon_t) = if a_id < b_id {
        (a_id.as_str(), b_id.as_str())
    } else {
        (b_id.as_str(), a_id.as_str())
    };
    let edge_list = pack
        .dispatch(
            "list",
            json!({"kind": "edge", "source_id": canon_s, "target_id": canon_t, "relations": ["competes_with"]}),
        )
        .await
        .expect("list canonical competes_with after update must succeed");
    let listed: &Vec<Value> = edge_list.as_array().expect("list must return array");
    assert_eq!(
        listed.len(),
        1,
        "H1-b: exactly one competes_with edge must exist after update (no duplicate); got: {listed:?}"
    );
}

/// Cross-namespace update_edge (weight-only, non-symmetric): must succeed.
///
/// ADR-007 rule 2: by-ID update applies NO namespace filter. UUID v4 is globally
/// unique. The caller's namespace is attribution only.
#[tokio::test]
async fn update_edge_cross_namespace_weight_update_succeeds() {
    let f = pack();

    let src = f
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "CrossNsEdgeSrc"}),
        )
        .await
        .expect("create src");
    let tgt = f
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "CrossNsEdgeTgt"}),
        )
        .await
        .expect("create tgt");
    let src_id = src["id"].as_str().unwrap();
    let tgt_id = tgt["id"].as_str().unwrap();

    // Link in the default (local) namespace.
    let link = f
        .dispatch(
            "link",
            json!({"source_id": src_id, "target_id": tgt_id, "relation": "extends", "weight": 0.5}),
        )
        .await
        .expect("link extends must succeed");
    let edge_id = link["id"].as_str().unwrap().to_string();

    // Update weight from a DIFFERENT namespace — must succeed (by-ID contract).
    let updated = f
        .dispatch(
            "update",
            json!({"kind": "edge", "id": edge_id, "weight": 0.9, "namespace": "ns-caller"}),
        )
        .await;
    assert!(
        updated.is_ok(),
        "cross-namespace weight update must succeed (ADR-007); got: {updated:?}"
    );
    let updated = updated.unwrap();
    assert_eq!(
        updated.get("weight").and_then(Value::as_f64),
        Some(0.9),
        "weight must be updated to 0.9"
    );
    // The stored namespace must remain the record's original namespace (local), not the caller's.
    assert_eq!(
        updated.get("namespace").and_then(Value::as_str),
        Some("local"),
        "edge namespace must remain the record's stored namespace after cross-ns update"
    );

    // Non-vacuity: re-fetch the edge from storage and assert the persisted row has
    // the new weight and original record namespace. This test FAILS (RC 101) if the
    // non-symmetric upsert_edge persistence call is removed/no-op'd.
    let fetched = f
        .dispatch("get", json!({"id": edge_id}))
        .await
        .expect("get edge after cross-ns update must succeed");
    assert_eq!(
        fetched.get("weight").and_then(Value::as_f64),
        Some(0.9),
        "persisted weight must be 0.9 after cross-ns update (storage persistence check)"
    );
    assert_eq!(
        fetched.get("namespace").and_then(Value::as_str),
        Some("local"),
        "persisted namespace must remain 'local' (record ns) after cross-ns update"
    );
}

/// Cross-namespace update_edge (weight-only, symmetric relation): must succeed.
///
/// Symmetric relations use the raw-SQL path with namespace predicates. Confirm that
/// `record_ns` (not caller ns) is used so the UPDATE matches the row.
#[tokio::test]
async fn update_edge_cross_namespace_weight_update_symmetric_succeeds() {
    let f = pack();

    let a = f
        .dispatch("create", json!({"kind": "concept", "name": "CrossNsSymA"}))
        .await
        .expect("create A");
    let b = f
        .dispatch("create", json!({"kind": "concept", "name": "CrossNsSymB"}))
        .await
        .expect("create B");
    let a_id = a["id"].as_str().unwrap();
    let b_id = b["id"].as_str().unwrap();

    // Link as competes_with (symmetric) in the default namespace.
    let link = f
        .dispatch(
            "link",
            json!({"source_id": a_id, "target_id": b_id, "relation": "competes_with", "weight": 0.4}),
        )
        .await
        .expect("link competes_with must succeed");
    let edge_id = link["id"].as_str().unwrap().to_string();

    // Update weight from a DIFFERENT namespace (symmetric raw-SQL path).
    let updated = f
        .dispatch(
            "update",
            json!({"kind": "edge", "id": edge_id, "weight": 0.8, "namespace": "ns-caller"}),
        )
        .await;
    assert!(
        updated.is_ok(),
        "cross-namespace weight update (symmetric) must succeed; got: {updated:?}"
    );
    assert_eq!(
        updated.unwrap().get("weight").and_then(Value::as_f64),
        Some(0.8),
        "weight must be updated to 0.8"
    );
}

/// Cross-namespace update_edge (relation change): must succeed.
///
/// Relation validation uses `record_tok` (not caller token) to look up endpoints.
/// Endpoints live in the edge's own namespace (local); caller is in ns-caller.
#[tokio::test]
async fn update_edge_cross_namespace_relation_change_succeeds() {
    let f = pack();

    let src = f
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "CrossNsRelSrc"}),
        )
        .await
        .expect("create src");
    let tgt = f
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "CrossNsRelTgt"}),
        )
        .await
        .expect("create tgt");
    let src_id = src["id"].as_str().unwrap();
    let tgt_id = tgt["id"].as_str().unwrap();

    // Link as extends in the default (local) namespace.
    let link = f
        .dispatch(
            "link",
            json!({"source_id": src_id, "target_id": tgt_id, "relation": "extends"}),
        )
        .await
        .expect("link extends must succeed");
    let edge_id = link["id"].as_str().unwrap().to_string();

    // Change relation to supersedes from a DIFFERENT namespace.
    // Endpoint validation must use record_tok (local) — src and tgt are in local ns.
    let updated = f
        .dispatch(
            "update",
            json!({"kind": "edge", "id": edge_id, "relation": "supersedes", "namespace": "ns-caller"}),
        )
        .await;
    assert!(
        updated.is_ok(),
        "cross-namespace relation change must succeed when endpoints are in record ns; got: {updated:?}"
    );
    assert_eq!(
        updated.unwrap().get("relation").and_then(Value::as_str),
        Some("supersedes"),
        "relation must be updated to supersedes"
    );
}

/// Update a note's content from a DIFFERENT namespace — must succeed.
///
/// ADR-007 rule 2: by-ID update applies NO namespace filter. This test exercises
/// `infer_kind_from_uuid`, which uses `resolve_by_id` (not `resolve`). The old
/// `resolve` gated notes on the visible-set, which excluded the record's namespace
/// when the caller token's primary ns differed — causing spurious NotFound.
///
/// Non-vacuity: this test FAILS if `infer_kind_from_uuid` calls `resolve` instead
/// of `resolve_by_id`, because `resolve` applies `ensure_namespace_visible`, which
/// rejects the note in "local" when the token's primary is "ns-caller".
#[tokio::test]
async fn update_note_cross_namespace_succeeds() {
    let f = pack();

    // Create note in the default (local) namespace.
    let created = f
        .dispatch(
            "create",
            json!({"kind": "observation", "content": "original content"}),
        )
        .await
        .expect("create note must succeed");
    let note_id = created["id"].as_str().unwrap().to_string();

    // Update the note from a DIFFERENT namespace — must succeed (by-ID contract).
    let updated = f
        .dispatch(
            "update",
            json!({"id": note_id, "content": "updated content", "namespace": "ns-caller"}),
        )
        .await;
    assert!(
        updated.is_ok(),
        "cross-namespace note update must succeed (ADR-007 rule 2); got: {updated:?}"
    );
    assert_eq!(
        updated.unwrap().get("content").and_then(Value::as_str),
        Some("updated content"),
        "content must be updated"
    );
}

/// Soft-delete a note from a DIFFERENT namespace — must succeed.
///
/// Same ADR-007 rule 2 contract as above, but exercises the soft-delete path
/// through `infer_kind_from_uuid`. Non-vacuity: FAILS with old `resolve`.
#[tokio::test]
async fn delete_note_cross_namespace_succeeds() {
    let f = pack();

    let created = f
        .dispatch(
            "create",
            json!({"kind": "observation", "content": "delete me cross ns"}),
        )
        .await
        .expect("create note must succeed");
    let note_id = created["id"].as_str().unwrap().to_string();

    let result = f
        .dispatch("delete", json!({"id": note_id, "namespace": "ns-caller"}))
        .await;
    assert!(
        result.is_ok(),
        "cross-namespace soft delete of a note must succeed (ADR-007); got: {result:?}"
    );
    assert_eq!(
        result.unwrap().get("deleted").and_then(Value::as_bool),
        Some(true)
    );
}

// ADR-045 §5: payload-level Timestamp fields must be ISO-8601
// strings at the MCP boundary, not raw integer microseconds.
//
// khive_types::Timestamp derives serde as a transparent u64, so
// ProposalCreatedPayload.expiry and ProposalAppliedPayload.applied_at
// serialize as integers unless normalize_event_timestamps recurses into
// the payload object.

/// propose with expiry → list(kind="event", event_kind="proposal_created") →
/// assert payload.expiry is a JSON string starting with "20" (ISO year prefix),
/// NOT a bare integer.
#[tokio::test]
async fn proposal_created_event_expiry_is_iso8601_string() {
    let pack = pack_with_events();

    // Use a concrete far-future microsecond timestamp as expiry.
    // 2026-04-25 in microseconds (approx): 1745539200000000
    let expiry_micros: i64 = 1_745_539_200_000_000;

    let propose_result = pack
        .dispatch(
            "propose",
            json!({
                "title": "ExpiryTimestampTest",
                "description": "expiry must be ISO string",
                "changeset": {"kind": "add_note", "note": {"kind": "observation", "content": "test note"}},
                "expiry": expiry_micros
            }),
        )
        .await
        .expect("propose must succeed");
    assert!(
        propose_result.get("id").is_some(),
        "propose must return id; got {propose_result}"
    );
    assert!(
        propose_result.get("proposal_id").is_none(),
        "propose must NOT emit the old proposal_id key (clean break); got {propose_result}"
    );

    // List proposal_created events.
    let events = pack
        .dispatch(
            "list",
            json!({"kind": "event", "event_kind": "proposal_created", "limit": 10}),
        )
        .await
        .expect("list proposal_created events must succeed");
    let arr = events.as_array().expect("list must return array");
    assert!(
        !arr.is_empty(),
        "at least one proposal_created event must exist"
    );

    // Find the event for our proposal (match on payload.title via the changeset).
    let event = &arr[arr.len() - 1]; // most recent is ours
    let payload = event.get("payload").expect("event must have payload field");

    // expiry must be a string, not a number.
    let expiry_val = payload.get("expiry").expect("payload must contain expiry");
    assert!(
        expiry_val.is_string(),
        "payload.expiry must be an ISO-8601 string, not a number; got: {expiry_val}"
    );
    let expiry_str = expiry_val.as_str().unwrap();
    assert!(
        expiry_str.starts_with("20"),
        "payload.expiry must look like a year-2000+ ISO timestamp; got: {expiry_str}"
    );
    // Basic ISO-8601 structure check: YYYY-MM-DDTHH:
    assert!(
        expiry_str.len() >= 16
            && expiry_str.as_bytes()[4] == b'-'
            && expiry_str.as_bytes()[7] == b'-'
            && expiry_str.as_bytes()[10] == b'T'
            && expiry_str.as_bytes()[13] == b':',
        "payload.expiry must be ISO-8601, got: {expiry_str}"
    );
}

// ---- Recursive event payload timestamp normalization ----
//
// This walks the entire event Value recursively (no depth limit) so that
// Timestamp integers at any nesting level — nested objects, array elements — are
// converted to ISO-8601 strings before reaching the MCP boundary.

/// Regression: verifies the recursive walker is wired into the live
/// propose→approve→applied dispatch path and processes `payload.applied_at`.
///
/// The name reflects what this test actually asserts: a direct payload child
/// (`applied_at`) on a `ProposalApplied` event is returned as an ISO-8601
/// string by the full handler path.  Genuine nested-object recursion
/// (e.g. `payload.result.applied_at`) is proven by the unit tests at
/// `handlers.rs:2713` and `handlers.rs:2729` — injecting such a shape through
/// the live event store would require bypassing the typed payload structs.
#[tokio::test]
async fn proposal_applied_event_payload_applied_at_via_live_dispatch() {
    // We exercise the recursive walker through the live propose→approve→applied
    // dispatch path. The ProposalAppliedPayload has applied_at at the payload
    // top level; the recursive walker must handle it regardless of depth.
    // This test also guards that the wiring of walk_timestamps into
    // normalize_event_timestamps is actually live.
    let pack = pack_with_events();

    let propose_result = pack
        .dispatch(
            "propose",
            json!({
                "title": "NestedTimestampTest",
                "description": "recursive walker must handle any depth",
                "changeset": {"kind": "add_note", "note": {"kind": "observation", "content": "nested-ts test"}}
            }),
        )
        .await
        .expect("propose must succeed");

    let proposal_id = propose_result["id"]
        .as_str()
        .expect("must have id")
        .to_string();
    assert!(
        propose_result.get("proposal_id").is_none(),
        "propose must NOT emit old proposal_id key; got {propose_result}"
    );

    let review_result = pack
        .dispatch("review", json!({"id": proposal_id, "decision": "approve"}))
        .await
        .expect("approve must succeed");
    assert!(
        review_result.get("id").is_some(),
        "review must return id; got {review_result}"
    );
    assert!(
        review_result.get("proposal_id").is_none(),
        "review must NOT emit old proposal_id key; got {review_result}"
    );

    let events = pack
        .dispatch(
            "list",
            json!({"kind": "event", "event_kind": "proposal_applied", "limit": 10}),
        )
        .await
        .expect("list proposal_applied must succeed");
    let arr = events.as_array().expect("must be array");
    assert!(
        !arr.is_empty(),
        "must have at least one proposal_applied event"
    );

    for event in arr {
        let payload = event.get("payload").expect("event must have payload");
        // applied_at is a direct payload child stored as Timestamp (u64 serde).
        // The recursive walker must convert it regardless of where it appears.
        if let Some(applied_at_val) = payload.get("applied_at") {
            assert!(
                applied_at_val.is_string(),
                "payload.applied_at must be ISO-8601 string (recursive walker); got: {applied_at_val}"
            );
            let s = applied_at_val.as_str().unwrap();
            assert!(
                s.starts_with("20") && s.contains('T'),
                "payload.applied_at must look like ISO-8601; got: {s}"
            );
        }
    }
}

/// Regression: verifies that all events returned by `list(kind="event")`
/// have ISO-8601 `created_at` strings — confirming the array branch of
/// `normalize_event_timestamps_array` is live in the dispatch path.
///
/// The name reflects what this test actually asserts: top-level `event.created_at`
/// on listed events.  Array-element recursion (e.g. `payload.steps[].updated_at`)
/// is proven by the unit test at `handlers.rs:2752` — injecting a synthetic
/// array-shaped payload through the live event store requires bypassing the typed
/// payload structs.
#[tokio::test]
async fn event_list_created_at_normalized_via_live_dispatch() {
    // All created_at values on events from list(kind="event") must be ISO strings.
    // This confirms the array path of walk_timestamps is wired into normalize_event_timestamps_array.
    let pack = pack_with_events();
    pack.dispatch(
        "create",
        json!({"kind": "concept", "name": "ArrayWalkerGuard"}),
    )
    .await
    .expect("create must succeed");

    let events = pack
        .dispatch("list", json!({"kind": "event", "limit": 10}))
        .await
        .expect("list must succeed");
    let arr = events.as_array().expect("must be array");
    assert!(!arr.is_empty(), "must have at least one event");

    // All created_at values must be ISO strings (the array walker normalizes each
    // element — this confirms the array branch of walk_timestamps is live).
    for event in arr {
        let created_at = event.get("created_at").expect("event must have created_at");
        assert!(
            created_at.is_string(),
            "event.created_at must be ISO-8601 string after array walk; got: {created_at}"
        );
        let s = created_at.as_str().unwrap();
        assert!(
            s.starts_with("20") && s.contains('T'),
            "event.created_at must be ISO-8601; got: {s}"
        );
    }
}

/// Regression: verifies that `payload.expiry` on a `ProposalCreated`
/// event is returned as an ISO-8601 string by the full dispatch path.
///
/// The name reflects what this test actually asserts: `payload.expiry` (a direct
/// payload child stored as `Option<Timestamp>` / u64 serde) on a listed event.
/// The actual signed i64 branch of `walk_timestamps` is proven by the unit test
/// at `handlers.rs:2713` — the live event store does not expose a raw i64 field
/// that bypasses the typed payload structs.
#[tokio::test]
async fn proposal_created_event_expiry_normalized_via_live_dispatch() {
    let pack = pack_with_events();

    // Use a concrete far-past microsecond timestamp that fits in i64.
    // 1970-01-02T00:00:00Z = 86400 * 1_000_000 microseconds
    let expiry_micros: i64 = 86_400_000_000i64;

    pack.dispatch(
        "propose",
        json!({
            "title": "LegacyI64TimestampTest",
            "description": "i64 timestamps in payload must normalize",
            "changeset": {"kind": "add_note", "note": {"kind": "observation", "content": "i64-ts test"}},
            "expiry": expiry_micros
        }),
    )
    .await
    .expect("propose must succeed");

    let events = pack
        .dispatch(
            "list",
            json!({"kind": "event", "event_kind": "proposal_created", "limit": 10}),
        )
        .await
        .expect("list proposal_created must succeed");
    let arr = events.as_array().expect("must be array");
    assert!(
        !arr.is_empty(),
        "must have at least one proposal_created event"
    );

    let event = &arr[arr.len() - 1]; // most recent is ours
    let payload = event.get("payload").expect("event must have payload");
    let expiry_val = payload.get("expiry").expect("payload must contain expiry");
    assert!(
        expiry_val.is_string(),
        "payload.expiry must be ISO-8601 string (i64 branch); got: {expiry_val}"
    );
    let s = expiry_val.as_str().unwrap();
    // 1970-01-02 would start with "1970-"
    assert!(
        s.contains('T') && s.contains('-'),
        "payload.expiry must be ISO-8601; got: {s}"
    );
}

/// propose → review(decision=approve) → apply triggers ProposalApplied →
/// list(kind="event", event_kind="proposal_applied") →
/// assert payload.applied_at is a JSON string starting with "20", NOT an integer.
#[tokio::test]
async fn proposal_applied_event_applied_at_is_iso8601_string() {
    let pack = pack_with_events();

    let propose_result = pack
        .dispatch(
            "propose",
            json!({
                "title": "AppliedAtTimestampTest",
                "description": "applied_at must be ISO string",
                "changeset": {"kind": "add_note", "note": {"kind": "observation", "content": "applied-at-test note"}}
            }),
        )
        .await
        .expect("propose must succeed");
    let proposal_id = propose_result
        .get("id")
        .and_then(Value::as_str)
        .expect("propose must return id")
        .to_string();

    // Approve the proposal — actor is "local" so self-approval is allowed.
    pack.dispatch("review", json!({"id": proposal_id, "decision": "approve"}))
        .await
        .expect("review(approve) must succeed");

    // List proposal_applied events.
    let events = pack
        .dispatch(
            "list",
            json!({"kind": "event", "event_kind": "proposal_applied", "limit": 10}),
        )
        .await
        .expect("list proposal_applied events must succeed");
    let arr = events.as_array().expect("list must return array");
    assert!(
        !arr.is_empty(),
        "at least one proposal_applied event must exist after approval"
    );

    let event = &arr[arr.len() - 1]; // most recent is ours
    let payload = event.get("payload").expect("event must have payload field");

    // applied_at must be a string, not a number.
    let applied_at_val = payload
        .get("applied_at")
        .expect("payload must contain applied_at");
    assert!(
        applied_at_val.is_string(),
        "payload.applied_at must be an ISO-8601 string, not a number; got: {applied_at_val}"
    );
    let applied_at_str = applied_at_val.as_str().unwrap();
    assert!(
        applied_at_str.starts_with("20"),
        "payload.applied_at must look like a year-2000+ ISO timestamp; got: {applied_at_str}"
    );
    assert!(
        applied_at_str.len() >= 16
            && applied_at_str.as_bytes()[4] == b'-'
            && applied_at_str.as_bytes()[7] == b'-'
            && applied_at_str.as_bytes()[10] == b'T'
            && applied_at_str.as_bytes()[13] == b':',
        "payload.applied_at must be ISO-8601, got: {applied_at_str}"
    );
}

// ---- Note expires_at normalization ----
//
// This adds `expires_at` to the `normalize_entity_timestamps` key set.
// Any note row with a non-null `expires_at` (stored as i64 microseconds) must
// cross the MCP boundary as an ISO-8601 string, not a raw integer.

/// Regression: `get(id=<note>)` and `list(kind="note")` must return
/// `expires_at` as an ISO-8601 string when the field is non-null.
///
/// We insert a note with `expires_at` set directly via the `NoteStore` (the
/// handler's `create` verb does not currently expose `expires_at` as a param),
/// then verify both the `get` and `list` response paths normalize the field.
#[tokio::test]
async fn note_expires_at_is_normalized_to_iso8601() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime must succeed");
    let tok = rt.authorize(khive_runtime::Namespace::local()).unwrap();

    // Insert a note with expires_at set to a concrete microsecond value.
    // 2025-01-01T00:00:00Z = 1735689600 seconds → 1_735_689_600_000_000 µs
    let expires_micros: i64 = 1_735_689_600_000_000;
    let mut note = Note::new(
        tok.namespace().as_str(),
        "observation",
        "r7 expires_at test",
    );
    note.expires_at = Some(expires_micros);
    let note_id = note.id;

    let note_store = rt.notes(&tok).expect("note store must be available");
    note_store
        .upsert_note(note)
        .await
        .expect("upsert must succeed");

    // Build the registry (same pack() pattern) so dispatch goes through the
    // full handler path.
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt));
    let registry = builder.build().expect("registry must build");

    // ---- get path ----
    let get_result = registry
        .dispatch("get", json!({"id": note_id.to_string()}))
        .await
        .expect("get must succeed");
    let record = get_result.get("record").unwrap_or(&get_result);
    let expires_val = record
        .get("expires_at")
        .expect("get response must contain expires_at");
    assert!(
        expires_val.is_string(),
        "get: expires_at must be an ISO-8601 string, not an integer; got: {expires_val}"
    );
    let s = expires_val.as_str().unwrap();
    assert!(
        s.starts_with("2025") && s.contains('T'),
        "get: expires_at must be ISO-8601 for 2025-01-01; got: {s}"
    );

    // ---- list path ----
    let list_result = registry
        .dispatch("list", json!({"kind": "note", "limit": 100}))
        .await
        .expect("list must succeed");
    let items = list_result.as_array().expect("list must return an array");
    let found = items
        .iter()
        .find(|v| v.get("id").and_then(Value::as_str) == Some(&note_id.to_string()))
        .or_else(|| {
            // id may be short-form — match on full_id too
            items
                .iter()
                .find(|v| v.get("full_id").and_then(Value::as_str) == Some(&note_id.to_string()))
        });
    // The note must appear in the list.  If it doesn't, the test infrastructure
    // rather than the normalization logic is at fault — we still assert on all
    // items to catch any integer leaks in the batch path.
    for item in items {
        if let Some(ea) = item.get("expires_at") {
            if !ea.is_null() {
                assert!(
                    ea.is_string(),
                    "list: expires_at must be ISO-8601 string, not integer; got: {ea} in {item}"
                );
            }
        }
    }
    assert!(
        found.is_some(),
        "list must include the note we inserted (id={note_id}); got {items:?}"
    );
}

// ── Wave 5 proposal lifecycle regression tests ──────────────────────────────

fn changeset_add_entity() -> Value {
    json!({
        "kind": "add_entity",
        "entity": {"kind": "concept", "name": "TestNode"}
    })
}

/// BUG-3 regression: `list(kind=proposal)` must return `last_decision` as a
/// bare string ("approve") not a double-JSON-encoded string ("\"approve\"").
#[tokio::test]
async fn list_proposal_last_decision_is_bare_string_not_json_encoded() {
    let f = pack_with_events();
    let propose = f
        .dispatch(
            "propose",
            json!({
                "title": "BUG-3 test",
                "description": "Verify last_decision encoding",
                "changeset": changeset_add_entity(),
            }),
        )
        .await
        .expect("propose must succeed");
    let pid = propose["id"].as_str().expect("id");

    f.dispatch("review", json!({ "id": pid, "decision": "approve" }))
        .await
        .expect("review must succeed");

    // list(kind=proposal) returns a JSON array directly (not wrapped in {"items":[...]}).
    let list = f
        .dispatch("list", json!({"kind": "proposal"}))
        .await
        .expect("list proposals must succeed");
    let items = list
        .as_array()
        .expect("list(kind=proposal) must return a JSON array");
    let proposal = items
        .iter()
        .find(|v| {
            v["id"]
                .as_str()
                .is_some_and(|id| id == pid || id.starts_with(&pid[..8]))
        })
        .or_else(|| items.first())
        .expect("at least one proposal in list");

    let last_decision = proposal["last_decision"].as_str().unwrap_or("");
    assert!(
        !last_decision.starts_with('"'),
        "BUG-3: last_decision must be a bare string, not JSON-quoted; got: {last_decision:?}"
    );
    assert_eq!(
        last_decision, "approve",
        "BUG-3: last_decision must be 'approve' (bare), not '\"approve\"'; got: {last_decision:?}"
    );
}

/// BUG-5 regression: `review(approve)` on an already-approved proposal must
/// return an error, not silently increment approve_count.
#[tokio::test]
async fn review_approve_on_already_approved_proposal_returns_error() {
    let f = pack_with_events();
    let propose = f
        .dispatch(
            "propose",
            json!({
                "title": "BUG-5 test",
                "description": "Review on approved proposal should fail",
                "changeset": changeset_add_entity(),
            }),
        )
        .await
        .expect("propose must succeed");
    let pid = propose["id"].as_str().expect("id");

    f.dispatch("review", json!({ "id": pid, "decision": "approve" }))
        .await
        .expect("first review(approve) must succeed");

    let second_review = f
        .dispatch("review", json!({ "id": pid, "decision": "approve" }))
        .await;

    assert!(
        second_review.is_err(),
        "BUG-5: second review(approve) on approved/applied proposal must return error; got: {second_review:?}"
    );
    // The apply worker may have run inline and moved the status to 'applied' before the second
    // review attempt.  Either 'approved' or 'applied' in the error message is correct — both
    // indicate the proposal is in a terminal state for review purposes.
    let err_msg = format!("{:?}", second_review.unwrap_err());
    assert!(
        err_msg.contains("approved") || err_msg.contains("applied"),
        "BUG-5: error must mention 'approved' or 'applied'; got: {err_msg}"
    );
}

/// BUG-6 regression: `propose` with a non-existent `parent_id` must return
/// an `InvalidInput` error, not silently create an orphaned proposal.
#[tokio::test]
async fn propose_with_nonexistent_parent_id_returns_error() {
    let f = pack_with_events();
    let fake_parent = "00000000-0000-0000-0000-000000000042";
    let result = f
        .dispatch(
            "propose",
            json!({
                "title": "BUG-6 amendment",
                "description": "Amending a non-existent proposal",
                "changeset": changeset_add_entity(),
                "parent_id": fake_parent,
            }),
        )
        .await;

    assert!(
        result.is_err(),
        "BUG-6: propose with non-existent parent_id must return error; got: {result:?}"
    );
    let err = result.unwrap_err();
    assert!(
        is_invalid_input(&err),
        "BUG-6: error must be InvalidInput; got: {err:?}"
    );
    let msg = invalid_input_message(&err);
    assert!(
        msg.contains(fake_parent),
        "BUG-6: error must quote the offending parent_id; got: {msg}"
    );
}

/// BUG-4 regression: two concurrent `withdraw` calls on the same proposal must
/// result in exactly one success and one error (CAS enforcement).
/// Note: SQLite in WAL mode is effectively single-writer; this test exercises
/// the SQL-level CAS by issuing two sequential withdraw calls after the status
/// is already 'withdrawn' from the first.
#[tokio::test]
async fn withdraw_on_already_withdrawn_proposal_returns_error() {
    let f = pack_with_events();
    let propose = f
        .dispatch(
            "propose",
            json!({
                "title": "BUG-4 withdraw race",
                "description": "Second withdraw must fail",
                "changeset": changeset_add_entity(),
            }),
        )
        .await
        .expect("propose must succeed");
    let pid = propose["id"].as_str().expect("id");

    f.dispatch("withdraw", json!({ "id": pid }))
        .await
        .expect("first withdraw must succeed");

    let second_withdraw = f.dispatch("withdraw", json!({ "id": pid })).await;

    assert!(
        second_withdraw.is_err(),
        "BUG-4: second withdraw must return error (proposal already withdrawn); got: {second_withdraw:?}"
    );
}

// ---- Issue #489: create_linked — entity creation with immediate edge attachment ----

/// Happy path: create an entity with a valid edge spec.
/// Response must include the created entity fields plus an `edges` array with one entry.
#[tokio::test]
async fn create_entity_with_edges_returns_entity_and_edge() {
    let f = pack();

    let target = f
        .dispatch(
            "create",
            json!({ "kind": "concept", "name": "TargetConcept489" }),
        )
        .await
        .expect("create target entity");
    let target_id = target["id"].as_str().expect("target id");

    let result = f
        .dispatch(
            "create",
            json!({
                "kind": "concept",
                "name": "SourceConcept489",
                "edges": [
                    { "target_id": target_id, "relation": "extends" }
                ],
            }),
        )
        .await
        .expect("create with edges must succeed");

    assert!(
        result["id"].as_str().is_some(),
        "#489: response must contain entity id; got: {result}"
    );
    assert_eq!(
        result["kind"].as_str(),
        Some("concept"),
        "#489: response must carry entity kind"
    );

    let edges = result["edges"]
        .as_array()
        .expect("#489: response must contain 'edges' array");
    assert_eq!(
        edges.len(),
        1,
        "#489: exactly one edge must have been created; got: {edges:?}"
    );

    assert!(
        result.get("edge_errors").is_none(),
        "#489: no edge_errors expected; got: {result}"
    );
}

/// When `edges` is absent the response is unchanged from the normal create response.
#[tokio::test]
async fn create_entity_without_edges_returns_normal_response() {
    let f = pack();

    let result = f
        .dispatch(
            "create",
            json!({ "kind": "concept", "name": "NormalConcept489" }),
        )
        .await
        .expect("create without edges must succeed");

    assert!(
        result["id"].as_str().is_some(),
        "#489: response must contain entity id; got: {result}"
    );
    assert!(
        result.get("edges").is_none(),
        "#489: no edges key expected when edges param absent; got: {result}"
    );
}

/// When an edge spec has an invalid relation the entity is still created and the
/// error is reported in `edge_errors` (individual failure, no rollback).
#[tokio::test]
async fn create_entity_with_invalid_edge_relation_reports_error_entity_survives() {
    let f = pack();

    let target = f
        .dispatch(
            "create",
            json!({ "kind": "concept", "name": "EdgeTarget489b" }),
        )
        .await
        .expect("create target");
    let target_id = target["id"].as_str().expect("target id");

    let result = f
        .dispatch(
            "create",
            json!({
                "kind": "concept",
                "name": "EdgeSource489b",
                "edges": [
                    { "target_id": target_id, "relation": "not_a_real_relation" }
                ],
            }),
        )
        .await
        .expect("create must succeed even when edge spec is invalid");

    assert!(
        result["id"].as_str().is_some(),
        "#489: entity must be created even when edge fails; got: {result}"
    );

    let edges = result["edges"]
        .as_array()
        .expect("#489: edges array must be present");
    assert!(
        edges.is_empty(),
        "#489: edges must be empty when all fail; got: {edges:?}"
    );

    let errs = result["edge_errors"]
        .as_array()
        .expect("#489: edge_errors must be present when edge fails");
    assert_eq!(errs.len(), 1, "#489: exactly one edge error; got: {errs:?}");
    assert_eq!(
        errs[0]["index"].as_u64(),
        Some(0),
        "#489: error index must be 0"
    );
}

/// When an edge spec has an unknown target_id the entity is still created and
/// the lookup failure is reported in `edge_errors`.
#[tokio::test]
async fn create_entity_with_unknown_target_id_reports_error_entity_survives() {
    let f = pack();

    let result = f
        .dispatch(
            "create",
            json!({
                "kind": "concept",
                "name": "EdgeSource489c",
                "edges": [
                    { "target_id": "00000000-0000-0000-0000-000000000001", "relation": "extends" }
                ],
            }),
        )
        .await
        .expect("create must succeed even when target does not exist");

    assert!(
        result["id"].as_str().is_some(),
        "#489: entity must be created even when target lookup fails; got: {result}"
    );

    let errs = result["edge_errors"]
        .as_array()
        .expect("#489: edge_errors must be present when target is not found");
    assert_eq!(errs.len(), 1, "#489: one error expected; got: {errs:?}");
}

/// Multiple edges: one valid, one with an invalid relation.
/// Entity created, one edge in results, one error in edge_errors.
#[tokio::test]
async fn create_entity_with_mixed_edges_partial_success() {
    let f = pack();

    let target = f
        .dispatch(
            "create",
            json!({ "kind": "concept", "name": "EdgeTarget489d" }),
        )
        .await
        .expect("create target");
    let target_id = target["id"].as_str().expect("target id");

    let result = f
        .dispatch(
            "create",
            json!({
                "kind": "concept",
                "name": "EdgeSource489d",
                "edges": [
                    { "target_id": target_id, "relation": "extends" },
                    { "target_id": target_id, "relation": "totally_invalid" },
                ],
            }),
        )
        .await
        .expect("create must succeed with partial edge failure");

    let edges = result["edges"]
        .as_array()
        .expect("#489: edges array must be present");
    assert_eq!(edges.len(), 1, "#489: one successful edge; got: {edges:?}");

    let errs = result["edge_errors"]
        .as_array()
        .expect("#489: edge_errors must be present");
    assert_eq!(errs.len(), 1, "#489: one failed edge; got: {errs:?}");
}

// ---- Issue #487: dedup guard tests ----

// Creating a uniquely-named entity produces no `similar_existing` field.
#[tokio::test]
async fn create_entity_dedup_no_similar_when_unique() {
    let pack = pack();
    let result = pack
        .dispatch(
            "create",
            json!({
                "kind": "concept",
                "name": "Completely Unique Entity XYZ123",
            }),
        )
        .await
        .expect("create must succeed");

    assert!(
        result.get("similar_existing").is_none(),
        "#487: no similar_existing when no duplicates exist; got: {result}"
    );
}

// Creating a second entity with the same name should surface the first as
// similar_existing.
#[tokio::test]
async fn create_entity_dedup_surfaces_similar_entity() {
    let pack = pack();

    // Create the first entity.
    let _first = pack
        .dispatch(
            "create",
            json!({
                "kind": "concept",
                "name": "FlashAttention",
            }),
        )
        .await
        .expect("first create must succeed");

    // Create a second entity with the same name. The first should appear in
    // similar_existing.
    let second = pack
        .dispatch(
            "create",
            json!({
                "kind": "concept",
                "name": "FlashAttention",
            }),
        )
        .await
        .expect("second create must succeed (dedup is advisory only)");

    let similar = second.get("similar_existing");
    assert!(
        similar.is_some(),
        "#487: similar_existing must be present when a duplicate name exists; got: {second}"
    );
    let arr = similar
        .unwrap()
        .as_array()
        .expect("similar_existing must be an array");
    assert!(
        !arr.is_empty(),
        "#487: similar_existing array must be non-empty; got: {second}"
    );
    let first_hit = &arr[0];
    assert!(
        first_hit.get("id").is_some(),
        "#487: each similar entry must have an id field; got: {first_hit}"
    );
    assert!(
        first_hit.get("score").is_some(),
        "#487: each similar entry must have a score field; got: {first_hit}"
    );
}

// skip_dedup_check=true suppresses the similarity search entirely.
#[tokio::test]
async fn create_entity_dedup_skipped_when_skip_flag_set() {
    let pack = pack();

    // Create baseline entity.
    pack.dispatch(
        "create",
        json!({ "kind": "concept", "name": "SkipDedupTestEntity" }),
    )
    .await
    .expect("first create must succeed");

    // Create duplicate with skip_dedup_check=true.
    let result = pack
        .dispatch(
            "create",
            json!({
                "kind": "concept",
                "name": "SkipDedupTestEntity",
                "skip_dedup_check": true,
            }),
        )
        .await
        .expect("create with skip_dedup_check must succeed");

    assert!(
        result.get("similar_existing").is_none(),
        "#487: skip_dedup_check=true must suppress similar_existing; got: {result}"
    );
}

// Note creates never run the dedup check.
#[tokio::test]
async fn create_note_dedup_never_runs() {
    let pack = pack();

    // Create a note — the dedup field must not appear in the response.
    let result = pack
        .dispatch(
            "create",
            json!({
                "kind": "observation",
                "content": "Some observation content",
            }),
        )
        .await
        .expect("note create must succeed");

    assert!(
        result.get("similar_existing").is_none(),
        "#487: dedup guard must not run for note creates; got: {result}"
    );
}

// ---- Issue #393: propose→review→apply/reject/withdraw lifecycle tests ----

/// Full lifecycle: propose → review(approve) → proposal auto-applies.
///
/// After approval the proposal status must be "applied" (via the
/// ProposalApplyWorker) and at least one `proposal_applied` event must exist.
#[tokio::test]
async fn propose_review_approve_lifecycle() {
    let f = pack_with_events();

    let propose = f
        .dispatch(
            "propose",
            json!({
                "title": "#393 approve lifecycle",
                "description": "propose → review(approve) → applied",
                "changeset": changeset_add_entity(),
            }),
        )
        .await
        .expect("propose must succeed");
    let pid = propose["id"].as_str().expect("id");

    // Approve — single-reviewer, self-approval is allowed on local actor.
    let review = f
        .dispatch("review", json!({ "id": pid, "decision": "approve" }))
        .await
        .expect("review(approve) must succeed");

    // review must acknowledge the approval.
    let status_after = review["status"].as_str().unwrap_or("");
    assert!(
        status_after == "approved" || status_after == "applied",
        "#393 approve: review response status must be 'approved' or 'applied', got {status_after:?}; full: {review}"
    );

    // list(kind=proposal, status=applied) must contain this proposal.
    let list = f
        .dispatch("list", json!({ "kind": "proposal", "status": "applied" }))
        .await
        .expect("list proposals must succeed");
    let items = list.as_array().expect("list must return an array");
    let found = items
        .iter()
        .any(|v| v["id"].as_str().is_some_and(|id| id == pid));
    assert!(
        found,
        "#393 approve: proposal {pid} not found in list(status=applied); items: {list}"
    );

    // A proposal_applied event must exist.
    let events = f
        .dispatch(
            "list",
            json!({ "kind": "event", "event_kind": "proposal_applied", "limit": 50 }),
        )
        .await
        .expect("list proposal_applied events must succeed");
    let evts = events.as_array().expect("event list must be array");
    assert!(
        !evts.is_empty(),
        "#393 approve: no proposal_applied event emitted after approval"
    );
}

/// Lifecycle: propose → review(reject) → status becomes "rejected".
#[tokio::test]
async fn propose_review_reject_lifecycle() {
    let f = pack_with_events();

    let propose = f
        .dispatch(
            "propose",
            json!({
                "title": "#393 reject lifecycle",
                "description": "propose → review(reject) → rejected",
                "changeset": changeset_add_entity(),
            }),
        )
        .await
        .expect("propose must succeed");
    let pid = propose["id"].as_str().expect("id");
    assert!(
        propose.get("proposal_id").is_none(),
        "propose must NOT emit old proposal_id key; got {propose}"
    );

    // Reject the proposal.
    let review = f
        .dispatch("review", json!({ "id": pid, "decision": "reject" }))
        .await
        .expect("review(reject) must succeed");

    let status_after = review["status"].as_str().unwrap_or("");
    assert_eq!(
        status_after, "rejected",
        "#393 reject: review response status must be 'rejected', got {status_after:?}; full: {review}"
    );
    assert!(
        review.get("proposal_id").is_none(),
        "review must NOT emit old proposal_id key; got {review}"
    );

    // list(kind=proposal, status=rejected) must contain this proposal.
    let list = f
        .dispatch("list", json!({ "kind": "proposal", "status": "rejected" }))
        .await
        .expect("list proposals must succeed");
    let items = list.as_array().expect("list must return an array");
    let found = items
        .iter()
        .any(|v| v["id"].as_str().is_some_and(|id| id == pid));
    assert!(
        found,
        "#393 reject: proposal {pid} not found in list(status=rejected); items: {list}"
    );
    // list rows must not expose the old key either.
    let row = items
        .iter()
        .find(|v| v["id"].as_str().is_some_and(|id| id == pid))
        .expect("rejected proposal row must be findable");
    assert!(
        row.get("proposal_id").is_none(),
        "list(kind=proposal) row must NOT contain proposal_id key; got {row}"
    );
}

/// Lifecycle: propose → withdraw → status becomes "withdrawn".
#[tokio::test]
async fn propose_withdraw_lifecycle() {
    let f = pack_with_events();

    let propose = f
        .dispatch(
            "propose",
            json!({
                "title": "#393 withdraw lifecycle",
                "description": "propose → withdraw → withdrawn",
                "changeset": changeset_add_entity(),
            }),
        )
        .await
        .expect("propose must succeed");
    let pid = propose["id"].as_str().expect("id");
    assert!(
        propose.get("proposal_id").is_none(),
        "propose must NOT emit old proposal_id key; got {propose}"
    );

    // Withdraw the proposal.
    let withdraw = f
        .dispatch("withdraw", json!({ "id": pid }))
        .await
        .expect("withdraw must succeed");

    let status_after = withdraw["status"].as_str().unwrap_or("");
    assert_eq!(
        status_after, "withdrawn",
        "#393 withdraw: response status must be 'withdrawn', got {status_after:?}; full: {withdraw}"
    );
    assert!(
        withdraw.get("proposal_id").is_none(),
        "withdraw must NOT emit old proposal_id key; got {withdraw}"
    );

    // list(kind=proposal, status=withdrawn) must contain this proposal.
    let list = f
        .dispatch("list", json!({ "kind": "proposal", "status": "withdrawn" }))
        .await
        .expect("list proposals must succeed");
    let items = list.as_array().expect("list must return an array");
    let found = items
        .iter()
        .any(|v| v["id"].as_str().is_some_and(|id| id == pid));
    assert!(
        found,
        "#393 withdraw: proposal {pid} not found in list(status=withdrawn); items: {list}"
    );
    // list rows must not expose the old key.
    let row = items
        .iter()
        .find(|v| v["id"].as_str().is_some_and(|id| id == pid))
        .expect("withdrawn proposal row must be findable");
    assert!(
        row.get("proposal_id").is_none(),
        "list(kind=proposal) row must NOT contain proposal_id key; got {row}"
    );
}

/// Status filter: list(kind=proposal, status=open) returns only open proposals.
///
/// Creates two proposals: one left open, one immediately withdrawn.
/// list(status=open) must contain the open one and must NOT contain the withdrawn one.
#[tokio::test]
async fn list_proposals_status_filter() {
    let f = pack_with_events();

    // Proposal A — stays open.
    let pa = f
        .dispatch(
            "propose",
            json!({
                "title": "#393 list-filter open",
                "description": "remains open for filtering",
                "changeset": changeset_add_entity(),
            }),
        )
        .await
        .expect("propose A must succeed");
    let pid_open = pa["id"].as_str().expect("id");

    // Proposal B — immediately withdrawn.
    let pb = f
        .dispatch(
            "propose",
            json!({
                "title": "#393 list-filter withdrawn",
                "description": "will be withdrawn immediately",
                "changeset": changeset_add_entity(),
            }),
        )
        .await
        .expect("propose B must succeed");
    let pid_withdrawn = pb["id"].as_str().expect("id");

    f.dispatch("withdraw", json!({ "id": pid_withdrawn }))
        .await
        .expect("withdraw B must succeed");

    // list(status=open) must include A but not B.
    let list_open = f
        .dispatch("list", json!({ "kind": "proposal", "status": "open" }))
        .await
        .expect("list(status=open) must succeed");
    let open_items = list_open.as_array().expect("list must return an array");

    let has_open = open_items
        .iter()
        .any(|v| v["id"].as_str().is_some_and(|id| id == pid_open));
    let has_withdrawn = open_items
        .iter()
        .any(|v| v["id"].as_str().is_some_and(|id| id == pid_withdrawn));

    assert!(
        has_open,
        "#393 list-filter: open proposal {pid_open} missing from list(status=open); items: {list_open}"
    );
    assert!(
        !has_withdrawn,
        "#393 list-filter: withdrawn proposal {pid_withdrawn} must not appear in list(status=open); items: {list_open}"
    );
}

/// Negative path: withdraw on an applied proposal must fail.
/// propose → approve (auto-applies) → withdraw → expect error mentioning "applied".
#[tokio::test]
async fn withdraw_after_apply_returns_error() {
    let f = pack_with_events();

    let propose = f
        .dispatch(
            "propose",
            json!({
                "title": "withdraw-after-apply guard",
                "description": "Applied proposals cannot be withdrawn",
                "changeset": changeset_add_entity(),
            }),
        )
        .await
        .expect("propose must succeed");
    let pid = propose["id"].as_str().expect("id");

    f.dispatch("review", json!({ "id": pid, "decision": "approve" }))
        .await
        .expect("review(approve) must succeed");

    let withdraw_result = f.dispatch("withdraw", json!({ "id": pid })).await;

    assert!(
        withdraw_result.is_err(),
        "withdraw on applied proposal must fail; got: {withdraw_result:?}"
    );
    let err_msg = format!("{:?}", withdraw_result.unwrap_err());
    assert!(
        err_msg.contains("applied") || err_msg.contains("approved") || err_msg.contains("terminal"),
        "error must mention terminal state; got: {err_msg}"
    );
}

/// Negative path: review on a rejected proposal must fail.
/// propose → reject → attempt second review → expect error mentioning "rejected".
#[tokio::test]
async fn review_after_reject_returns_error() {
    let f = pack_with_events();

    let propose = f
        .dispatch(
            "propose",
            json!({
                "title": "review-after-reject guard",
                "description": "Rejected proposals cannot be re-reviewed",
                "changeset": changeset_add_entity(),
            }),
        )
        .await
        .expect("propose must succeed");
    let pid = propose["id"].as_str().expect("id");

    f.dispatch("review", json!({ "id": pid, "decision": "reject" }))
        .await
        .expect("review(reject) must succeed");

    let second_review = f
        .dispatch("review", json!({ "id": pid, "decision": "approve" }))
        .await;

    assert!(
        second_review.is_err(),
        "review on rejected proposal must fail; got: {second_review:?}"
    );
    let err_msg = format!("{:?}", second_review.unwrap_err());
    assert!(
        err_msg.contains("rejected"),
        "error must mention 'rejected'; got: {err_msg}"
    );
}

/// CAS divergence: withdraw after concurrent approval.
/// propose → approve (status moves to applied) → withdraw → CAS fails.
/// This exercises the SQL-level CAS guard in `withdrawn_and_emit`:
/// the precheck sees "open" but by the time CAS runs, status is "applied".
///
/// In practice with SQLite WAL mode, the approve+apply commits before the
/// withdraw starts. This test verifies the CAS guard catches the terminal state.
#[tokio::test]
async fn withdraw_cas_divergence_after_approval() {
    let f = pack_with_events();

    let propose = f
        .dispatch(
            "propose",
            json!({
                "title": "CAS divergence test",
                "description": "Tests CAS guard when status shifts under us",
                "changeset": changeset_add_entity(),
            }),
        )
        .await
        .expect("propose must succeed");
    let pid = propose["id"].as_str().expect("id");

    // Approve moves status → approved → applied (inline apply worker).
    f.dispatch("review", json!({ "id": pid, "decision": "approve" }))
        .await
        .expect("review(approve) must succeed");

    // Withdraw now — status is applied, CAS should reject.
    let result = f.dispatch("withdraw", json!({ "id": pid })).await;
    assert!(
        result.is_err(),
        "CAS divergence: withdraw must fail after approval; got: {result:?}"
    );

    // Verify the proposal is still "applied" (not corrupted by the failed withdraw).
    let list = f
        .dispatch("list", json!({ "kind": "proposal", "status": "applied" }))
        .await
        .expect("list must succeed");
    let items = list.as_array().expect("must be array");
    let found = items
        .iter()
        .any(|v| v["id"].as_str().is_some_and(|id| id == pid));
    assert!(
        found,
        "CAS divergence: proposal must still be 'applied' after failed withdraw; items: {list}"
    );
}

// ---- KG pack edge endpoint extensions (ADR-002 v0.2.4) ----
//
// These tests verify the 9 new endpoint pairs declared in KG_EDGE_RULES.
// Each test constructs a fixture with edge rules installed (mirroring what the
// MCP transport does at startup per ADR-031) before calling link().

fn pack_with_edge_rules() -> (Fixture, KhiveRuntime) {
    let rt = KhiveRuntime::memory().expect("in-memory runtime must succeed");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");
    rt.install_edge_rules(registry.all_edge_rules());
    (Fixture { registry }, rt)
}

/// person→org with part_of must succeed after edge rules are installed.
#[tokio::test]
async fn link_person_to_org_part_of_succeeds() {
    let (f, _rt) = pack_with_edge_rules();

    let person = f
        .dispatch(
            "create",
            json!({ "kind": "person", "name": "Alice Researcher" }),
        )
        .await
        .expect("create person");
    let org = f
        .dispatch("create", json!({ "kind": "org", "name": "DeepMind" }))
        .await
        .expect("create org");

    let result = f
        .dispatch(
            "link",
            json!({
                "source_id": person["id"],
                "target_id": org["id"],
                "relation": "part_of",
            }),
        )
        .await;

    assert!(
        result.is_ok(),
        "person→org part_of must succeed with KG edge rules installed; got: {result:?}"
    );
    let edge = result.unwrap();
    assert_eq!(edge["relation"], "part_of");
}

/// person→project with part_of must succeed after edge rules are installed (#60).
#[tokio::test]
async fn link_person_to_project_part_of_succeeds() {
    let (f, _rt) = pack_with_edge_rules();

    let person = f
        .dispatch(
            "create",
            json!({ "kind": "person", "name": "Alice Researcher" }),
        )
        .await
        .expect("create person");
    let project = f
        .dispatch("create", json!({ "kind": "project", "name": "khive" }))
        .await
        .expect("create project");

    let result = f
        .dispatch(
            "link",
            json!({
                "source_id": person["id"],
                "target_id": project["id"],
                "relation": "part_of",
            }),
        )
        .await;

    assert!(
        result.is_ok(),
        "person→project part_of must succeed with KG edge rules installed; got: {result:?}"
    );
    let edge = result.unwrap();
    assert_eq!(edge["relation"], "part_of");
}

/// person→project with instance_of must succeed after edge rules are installed (#60).
#[tokio::test]
async fn link_person_to_project_instance_of_succeeds() {
    let (f, _rt) = pack_with_edge_rules();

    let person = f
        .dispatch(
            "create",
            json!({ "kind": "person", "name": "Bob Engineer" }),
        )
        .await
        .expect("create person");
    let project = f
        .dispatch("create", json!({ "kind": "project", "name": "lattice" }))
        .await
        .expect("create project");

    let result = f
        .dispatch(
            "link",
            json!({
                "source_id": person["id"],
                "target_id": project["id"],
                "relation": "instance_of",
            }),
        )
        .await;

    assert!(
        result.is_ok(),
        "person→project instance_of must succeed with KG edge rules installed; got: {result:?}"
    );
    let edge = result.unwrap();
    assert_eq!(edge["relation"], "instance_of");
}

/// org→org with depends_on must succeed after edge rules are installed.
#[tokio::test]
async fn link_org_to_org_depends_on_succeeds() {
    let (f, _rt) = pack_with_edge_rules();

    let org_a = f
        .dispatch("create", json!({ "kind": "org", "name": "SubsidiaryInc" }))
        .await
        .expect("create org_a");
    let org_b = f
        .dispatch("create", json!({ "kind": "org", "name": "ParentCorp" }))
        .await
        .expect("create org_b");

    let result = f
        .dispatch(
            "link",
            json!({
                "source_id": org_a["id"],
                "target_id": org_b["id"],
                "relation": "depends_on",
            }),
        )
        .await;

    assert!(
        result.is_ok(),
        "org→org depends_on must succeed with KG edge rules installed; got: {result:?}"
    );
    let edge = result.unwrap();
    assert_eq!(edge["relation"], "depends_on");
}

/// Regression: concept→concept extends must still work after adding KG edge rules.
#[tokio::test]
async fn link_concept_to_concept_extends_still_works() {
    let (f, _rt) = pack_with_edge_rules();

    let parent = f
        .dispatch("create", json!({ "kind": "concept", "name": "Attention" }))
        .await
        .expect("create parent concept");
    let child = f
        .dispatch(
            "create",
            json!({ "kind": "concept", "name": "FlashAttention" }),
        )
        .await
        .expect("create child concept");

    let result = f
        .dispatch(
            "link",
            json!({
                "source_id": child["id"],
                "target_id": parent["id"],
                "relation": "extends",
            }),
        )
        .await;

    assert!(
        result.is_ok(),
        "concept→concept extends must still succeed (regression); got: {result:?}"
    );
    let edge = result.unwrap();
    assert_eq!(edge["relation"], "extends");
}

// ── Secret-gate: proposal path regression tests ───────────────────────────────

fn is_secret_detected(err: &RuntimeError) -> bool {
    matches!(err, RuntimeError::SecretDetected(_))
}

/// propose.description containing a fake AWS key must be rejected.
#[tokio::test]
async fn propose_blocks_secret_in_description() {
    let f = pack_with_events();
    let result = f
        .dispatch(
            "propose",
            json!({
                "title": "Test proposal",
                "description": "Access key: AKIAFAKEKEY000000000", // gitleaks:allow
                "changeset": { "kind": "add_entity", "entity": { "kind": "concept", "name": "Test" } },
            }),
        )
        .await;
    assert!(
        result.as_ref().err().is_some_and(is_secret_detected),
        "propose with secret in description must be rejected; got: {result:?}"
    );
}

/// propose.changeset containing a fake AWS key in proposed entity properties must be rejected.
#[tokio::test]
async fn propose_blocks_secret_in_changeset_entity_properties() {
    let f = pack_with_events();
    let result = f
        .dispatch(
            "propose",
            json!({
                "title": "Credential slip",
                "description": "A description without secrets.",
                "changeset": {
                    "kind": "add_entity",
                    "entity": {
                        "kind": "concept",
                        "name": "SomeEntity",
                        "properties": { "api_key": "AKIAFAKEKEY000000000" } // gitleaks:allow
                    }
                },
            }),
        )
        .await;
    assert!(
        result.as_ref().err().is_some_and(is_secret_detected),
        "propose with secret in changeset entity properties must be rejected; got: {result:?}"
    );
}

/// propose changeset with a secret in proposed note content must be rejected.
#[tokio::test]
async fn propose_blocks_secret_in_changeset_note_content() {
    let f = pack_with_events();
    let result = f
        .dispatch(
            "propose",
            json!({
                "title": "Note proposal",
                "description": "A description without secrets.",
                "changeset": {
                    "kind": "add_note",
                    "note": {
                        "kind": "observation",
                        "content": "My token: ghp_FakeGitHubToken0000000000000000000" // gitleaks:allow
                    }
                },
            }),
        )
        .await;
    assert!(
        result.as_ref().err().is_some_and(is_secret_detected),
        "propose with secret in changeset note content must be rejected; got: {result:?}"
    );
}

/// review.comment containing a fake credential must be rejected.
#[tokio::test]
async fn review_blocks_secret_in_comment() {
    let f = pack_with_events();

    // First submit a clean proposal to review.
    let propose_result = f
        .dispatch(
            "propose",
            json!({
                "title": "Clean proposal",
                "description": "No secrets here.",
                "changeset": { "kind": "add_entity", "entity": { "kind": "concept", "name": "SafeEntity" } },
            }),
        )
        .await
        .expect("clean propose must succeed");
    let proposal_id = propose_result["id"].as_str().expect("id in response");

    // Review with a secret in the comment must be rejected.
    let result = f
        .dispatch(
            "review",
            json!({
                "id": proposal_id,
                "decision": "comment",
                "comment": "Here is my secret: AKIAFAKEKEY000000000", // gitleaks:allow
            }),
        )
        .await;
    assert!(
        result.as_ref().err().is_some_and(is_secret_detected),
        "review with secret in comment must be rejected; got: {result:?}"
    );
}

/// withdraw.rationale containing a fake credential must be rejected.
#[tokio::test]
async fn withdraw_blocks_secret_in_rationale() {
    let f = pack_with_events();

    // Submit a clean proposal first so we have something to withdraw.
    let propose_result = f
        .dispatch(
            "propose",
            json!({
                "title": "Withdrawal test proposal",
                "description": "Clean description.",
                "changeset": { "kind": "add_entity", "entity": { "kind": "concept", "name": "WithdrawEntity" } },
            }),
        )
        .await
        .expect("clean propose must succeed");
    let proposal_id = propose_result["id"].as_str().expect("id in response");

    // Withdraw with a secret in the rationale must be rejected.
    let result = f
        .dispatch(
            "withdraw",
            json!({
                "id": proposal_id,
                "rationale": "Withdrawn: token AKIAFAKEKEY000000000", // gitleaks:allow
            }),
        )
        .await;
    assert!(
        result.as_ref().err().is_some_and(is_secret_detected),
        "withdraw with secret in rationale must be rejected; got: {result:?}"
    );
}

/// propose.changeset with a credential as an object KEY must be rejected.
#[tokio::test]
async fn propose_blocks_secret_as_changeset_key() {
    let f = pack_with_events();
    let result = f
        .dispatch(
            "propose",
            json!({
                "title": "Key-as-credential test",
                "description": "No secrets in values.",
                "changeset": {
                    "kind": "add_entity",
                    "entity": {
                        "kind": "concept",
                        "name": "KeyTest",
                        "properties": { "ghp_FakeGitHubToken0000000000000000000": "redacted" } // gitleaks:allow
                    }
                },
            }),
        )
        .await;
    assert!(
        result.as_ref().err().is_some_and(is_secret_detected),
        "propose with secret as changeset property key must be rejected; got: {result:?}"
    );
}

// ---- #71 regression: hard delete must purge soft-deleted records ----

/// Soft-delete an entity, then hard-delete it — must succeed and leave the row gone.
#[tokio::test]
async fn hard_delete_purges_soft_deleted_entity() {
    let f = pack();

    let created = f
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "PurgeMeEntity"}),
        )
        .await
        .expect("create must succeed");
    let id = created["id"]
        .as_str()
        .expect("create must return id")
        .to_string();

    f.dispatch("delete", json!({"id": id}))
        .await
        .expect("soft delete must succeed");

    let purge = f.dispatch("delete", json!({"id": id, "hard": true})).await;
    assert!(
        purge.is_ok(),
        "hard delete of a soft-deleted entity must succeed (issue #71); got: {purge:?}"
    );
    assert_eq!(
        purge.unwrap().get("deleted").and_then(Value::as_bool),
        Some(true)
    );

    let second = f.dispatch("delete", json!({"id": id, "hard": true})).await;
    assert!(
        second.is_err(),
        "second hard delete must return not-found (row is physically gone); got: {second:?}"
    );
}

/// Hard-delete a soft-deleted entity resolved via short prefix must succeed.
#[tokio::test]
async fn hard_delete_soft_deleted_entity_by_prefix() {
    let f = pack();

    let created = f
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "PurgeMeByPrefix"}),
        )
        .await
        .expect("create must succeed");
    let full_id = created["id"]
        .as_str()
        .expect("create must return id")
        .to_string();
    let prefix = &full_id[..8];

    f.dispatch("delete", json!({"id": full_id}))
        .await
        .expect("soft delete must succeed");

    let purge = f
        .dispatch("delete", json!({"id": prefix, "hard": true}))
        .await;
    assert!(
        purge.is_ok(),
        "hard delete by short prefix of a soft-deleted entity must succeed (issue #71); got: {purge:?}"
    );
    assert_eq!(
        purge.unwrap().get("deleted").and_then(Value::as_bool),
        Some(true)
    );
}

/// Hard-delete a soft-deleted entity cascades its incident edges.
#[tokio::test]
async fn hard_delete_soft_deleted_entity_cascades_edges() {
    let (f, _rt) = pack_with_edge_rules();

    let source = f
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "CascadeSource"}),
        )
        .await
        .expect("create source must succeed");
    let target = f
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "CascadeTarget"}),
        )
        .await
        .expect("create target must succeed");
    let source_id = source["id"].as_str().unwrap().to_string();
    let target_id = target["id"].as_str().unwrap().to_string();

    f.dispatch(
        "link",
        json!({"source_id": source_id, "target_id": target_id, "relation": "extends"}),
    )
    .await
    .expect("link must succeed");

    f.dispatch("delete", json!({"id": source_id}))
        .await
        .expect("soft delete must succeed");

    f.dispatch("delete", json!({"id": source_id, "hard": true}))
        .await
        .expect("hard delete of soft-deleted entity must succeed (issue #71)");

    let neighbors = f
        .dispatch(
            "neighbors",
            json!({"node_id": target_id, "direction": "in"}),
        )
        .await
        .expect("neighbors query must succeed");
    let arr = neighbors.as_array().expect("neighbors must return array");
    assert!(
        arr.is_empty(),
        "hard delete must cascade and remove incident edges; got: {arr:?}"
    );
}

/// Hard-delete a soft-deleted entity from a different namespace must SUCCEED.
///
/// ADR-007 rule 2: by-ID delete applies NO namespace filter. UUID v4 is globally unique.
/// The caller's namespace is attribution only — it does not gate the operation.
#[tokio::test]
async fn hard_delete_soft_deleted_entity_cross_namespace_succeeds() {
    let f = pack();

    // Create entity in the default (local) namespace.
    let created = f
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "CrossNsConcept"}),
        )
        .await
        .expect("create must succeed");
    let id = created["id"].as_str().unwrap().to_string();

    // Soft-delete from the record's own namespace.
    f.dispatch("delete", json!({"id": id}))
        .await
        .expect("soft delete must succeed");

    // Hard-delete from a DIFFERENT namespace — must succeed (by-ID contract).
    let result = f
        .dispatch(
            "delete",
            json!({"id": id, "hard": true, "namespace": "ns-caller"}),
        )
        .await;
    assert!(
        result.is_ok(),
        "cross-namespace hard delete of a soft-deleted entity must succeed (ADR-007); got: {result:?}"
    );
    assert_eq!(
        result.unwrap().get("deleted").and_then(Value::as_bool),
        Some(true),
        "hard delete result must carry deleted=true"
    );

    // Confirm the row is physically gone — a second hard delete must return not-found.
    let second = f.dispatch("delete", json!({"id": id, "hard": true})).await;
    assert!(
        second.is_err(),
        "second hard delete must fail — entity row is physically gone; got: {second:?}"
    );
}

/// Soft-delete a note, then hard-delete it — must succeed and leave the row gone.
#[tokio::test]
async fn hard_delete_purges_soft_deleted_note() {
    let f = pack();

    let created = f
        .dispatch(
            "create",
            json!({"kind": "observation", "content": "purge me note content"}),
        )
        .await
        .expect("create note must succeed");
    let id = created["id"]
        .as_str()
        .expect("create must return id")
        .to_string();

    f.dispatch("delete", json!({"id": id}))
        .await
        .expect("soft delete must succeed");

    let purge = f.dispatch("delete", json!({"id": id, "hard": true})).await;
    assert!(
        purge.is_ok(),
        "hard delete of a soft-deleted note must succeed (issue #71); got: {purge:?}"
    );
    assert_eq!(
        purge.unwrap().get("deleted").and_then(Value::as_bool),
        Some(true)
    );

    let second = f.dispatch("delete", json!({"id": id, "hard": true})).await;
    assert!(
        second.is_err(),
        "second hard delete must return not-found (note row is physically gone); got: {second:?}"
    );
}

/// Hard-delete a soft-deleted note from a different namespace must SUCCEED.
///
/// ADR-007 rule 2: by-ID delete applies NO namespace filter. UUID v4 is globally unique.
#[tokio::test]
async fn hard_delete_soft_deleted_note_cross_namespace_succeeds() {
    let f = pack();

    // Create note in the default (local) namespace.
    let created = f
        .dispatch(
            "create",
            json!({"kind": "observation", "content": "cross-ns note purge target"}),
        )
        .await
        .expect("create note must succeed");
    let id = created["id"].as_str().unwrap().to_string();

    // Soft-delete from the record's own namespace.
    f.dispatch("delete", json!({"id": id}))
        .await
        .expect("soft delete must succeed");

    // Hard-delete from a DIFFERENT namespace — must succeed (by-ID contract).
    let result = f
        .dispatch(
            "delete",
            json!({"id": id, "hard": true, "namespace": "ns-caller"}),
        )
        .await;
    assert!(
        result.is_ok(),
        "cross-namespace hard delete of a soft-deleted note must succeed (ADR-007); got: {result:?}"
    );
    assert_eq!(
        result.unwrap().get("deleted").and_then(Value::as_bool),
        Some(true),
        "hard delete result must carry deleted=true"
    );

    // Confirm the row is physically gone — a second hard delete must return not-found.
    let second = f.dispatch("delete", json!({"id": id, "hard": true})).await;
    assert!(
        second.is_err(),
        "second hard delete must fail — note row is physically gone; got: {second:?}"
    );
}

// ---- #391 §3: by-ID CRUD prefix resolution is fully unfiltered ----
//
// The `*_cross_namespace_succeeds` tests above already lock in that a FULL
// UUID by-ID lookup is namespace-agnostic (ADR-007 Rev 2). Before #391, the
// *prefix* form of the same by-ID lookup fell through to the
// primary-namespace-only `resolve_prefix`, so a caller in a different
// namespace than the record got `NotFound` for a prefix that would have
// resolved fine as a full UUID — the outage's by-ID-adjacent half. These
// tests lock in the fix: prefix resolution is now unfiltered too, matching
// the full-UUID contract. by-ID CRUD has no visibility boundary at all; the
// Gate is the authz seam (ADR-007 Rev 6). This shares the namespace-agnostic
// by-ID contract that `brain.feedback` now follows
// (`brain_feedback_accepts_foreign_namespace_target_id` in
// `khive-pack-brain/tests/dispatch_hook.rs`, #498).

/// `get` by short prefix from a caller in a DIFFERENT namespace than the
/// record must now succeed (was `NotFound` pre-#391).
#[tokio::test]
async fn get_entity_by_prefix_cross_namespace_succeeds() {
    let f = pack();

    let created = f
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "PrefixCrossNsGet"}),
        )
        .await
        .expect("create must succeed");
    let full_id = created["id"].as_str().unwrap().to_string();
    let prefix = &full_id[..8];

    let result = f
        .dispatch("get", json!({"id": prefix, "namespace": "ns-caller"}))
        .await;
    assert!(
        result.is_ok(),
        "get by prefix from a different namespace must succeed (#391 §3); got: {result:?}"
    );
    assert_eq!(result.unwrap()["id"].as_str(), Some(full_id.as_str()));
}

/// `get` by full UUID from a different namespace still succeeds — guards the
/// already-working path this fix must not regress.
#[tokio::test]
async fn get_entity_by_full_uuid_cross_namespace_still_succeeds() {
    let f = pack();

    let created = f
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "FullUuidCrossNsGet"}),
        )
        .await
        .expect("create must succeed");
    let full_id = created["id"].as_str().unwrap().to_string();

    let result = f
        .dispatch("get", json!({"id": full_id, "namespace": "ns-caller"}))
        .await;
    assert!(
        result.is_ok(),
        "get by full UUID from a different namespace must still succeed; got: {result:?}"
    );
}

/// `update` by short prefix from a different namespace must now succeed.
#[tokio::test]
async fn update_entity_by_prefix_cross_namespace_succeeds() {
    let f = pack();

    let created = f
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "PrefixCrossNsUpdate"}),
        )
        .await
        .expect("create must succeed");
    let full_id = created["id"].as_str().unwrap().to_string();
    let prefix = &full_id[..8];

    let result = f
        .dispatch(
            "update",
            json!({"id": prefix, "namespace": "ns-caller", "description": "updated via prefix"}),
        )
        .await;
    assert!(
        result.is_ok(),
        "update by prefix from a different namespace must succeed (#391 §3); got: {result:?}"
    );
}

/// `delete` (soft) by short prefix from a different namespace must now succeed.
#[tokio::test]
async fn soft_delete_entity_by_prefix_cross_namespace_succeeds() {
    let f = pack();

    let created = f
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "PrefixCrossNsSoftDelete"}),
        )
        .await
        .expect("create must succeed");
    let full_id = created["id"].as_str().unwrap().to_string();
    let prefix = &full_id[..8];

    let result = f
        .dispatch("delete", json!({"id": prefix, "namespace": "ns-caller"}))
        .await;
    assert!(
        result.is_ok(),
        "soft delete by prefix from a different namespace must succeed (#391 §3); got: {result:?}"
    );
}

/// `delete(hard=true)` by short prefix from a different namespace must now
/// succeed — mirrors `hard_delete_soft_deleted_entity_cross_namespace_succeeds`
/// but resolves via prefix instead of full UUID.
#[tokio::test]
async fn hard_delete_entity_by_prefix_cross_namespace_succeeds() {
    let f = pack();

    let created = f
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "PrefixCrossNsHardDelete"}),
        )
        .await
        .expect("create must succeed");
    let full_id = created["id"].as_str().unwrap().to_string();
    let prefix = &full_id[..8];

    f.dispatch("delete", json!({"id": full_id}))
        .await
        .expect("soft delete must succeed");

    let result = f
        .dispatch(
            "delete",
            json!({"id": prefix, "hard": true, "namespace": "ns-caller"}),
        )
        .await;
    assert!(
        result.is_ok(),
        "hard delete by prefix from a different namespace must succeed (#391 §3); got: {result:?}"
    );
    assert_eq!(
        result.unwrap().get("deleted").and_then(Value::as_bool),
        Some(true)
    );
}

/// `merge` resolves both `into_id` and `from_id` by short prefix even when the
/// caller is in a DIFFERENT namespace than the records — id resolution is no
/// longer the blocker (pre-#391 this failed at resolution with
/// `InvalidInput("no record matches prefix...")`). `merge_entity` separately
/// enforces its OWN namespace-ownership check deep in the merge transaction
/// (`curation.rs::merge_entity`, `belongs to namespace` — entirely downstream
/// of and unrelated to id resolution). That is a deliberate, pre-existing
/// mutation-safety constraint, not part of the #391 outage, and out of scope
/// here (SPEC's own "merge if cheap" hedge: this is not cheap, it is a
/// distinct design question). This test locks in exactly that boundary:
/// resolution now succeeds, the ownership check still legitimately blocks the
/// cross-namespace merge, and the failure mode is the ownership error, not a
/// resolution error.
#[tokio::test]
async fn merge_entities_by_prefix_cross_namespace_resolves_then_hits_ownership_check() {
    let f = pack();

    let into_full = f
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "PrefixCrossNsMergeInto"}),
        )
        .await
        .expect("create into-entity must succeed")["id"]
        .as_str()
        .unwrap()
        .to_string();
    let from_full = f
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "PrefixCrossNsMergeFrom"}),
        )
        .await
        .expect("create from-entity must succeed")["id"]
        .as_str()
        .unwrap()
        .to_string();

    let into_prefix = &into_full[..8];
    let from_prefix = &from_full[..8];

    let err = f
        .dispatch(
            "merge",
            json!({"into_id": into_prefix, "from_id": from_prefix, "namespace": "ns-caller"}),
        )
        .await
        .expect_err(
            "cross-namespace merge must still fail (ownership check), just NOT at id resolution",
        );

    let msg = err.to_string();
    assert!(
        !msg.contains("no record matches prefix"),
        "#391 §3: prefix resolution must no longer be the failure — got a resolution-layer error: {msg}"
    );
    assert!(
        msg.contains("belongs to namespace"),
        "expected the deeper merge ownership check to fail instead; got: {msg}"
    );
}

/// `merge` by short prefix within the records' own namespace still succeeds —
/// guards the already-working path.
#[tokio::test]
async fn merge_entities_by_prefix_same_namespace_succeeds() {
    let f = pack();

    let into_full = f
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "PrefixSameNsMergeInto"}),
        )
        .await
        .expect("create into-entity must succeed")["id"]
        .as_str()
        .unwrap()
        .to_string();
    let from_full = f
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "PrefixSameNsMergeFrom"}),
        )
        .await
        .expect("create from-entity must succeed")["id"]
        .as_str()
        .unwrap()
        .to_string();

    let into_prefix = &into_full[..8];
    let from_prefix = &from_full[..8];

    let result = f
        .dispatch(
            "merge",
            json!({"into_id": into_prefix, "from_id": from_prefix}),
        )
        .await;
    assert!(
        result.is_ok(),
        "merge by prefix within the records' own namespace must succeed; got: {result:?}"
    );
}

/// Negative: a hex prefix matching nothing resolves to `NotFound` even though
/// resolution is now unfiltered — unfiltered means "no namespace predicate",
/// not "match anything" (#391 §3).
#[tokio::test]
async fn get_by_prefix_with_no_match_returns_not_found() {
    let f = pack();

    let err = f
        .dispatch("get", json!({"id": "deadbeef"}))
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::NotFound(_)),
        "unmatched prefix must return NotFound; got: {err:?}"
    );
}

/// Soft-delete an edge, then hard-delete it WITHOUT supplying explicit kind — must
/// succeed and leave the row physically gone (ADR-002: public delete auto-detects).
#[tokio::test]
async fn hard_delete_soft_deleted_edge_without_kind_purges_row() {
    use khive_storage::{SqlStatement, SqlValue};
    use uuid::Uuid;

    let (f, rt) = pack_with_edge_rules();

    let source = f
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "EdgePurgeSource"}),
        )
        .await
        .expect("create source must succeed");
    let target = f
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "EdgePurgeTarget"}),
        )
        .await
        .expect("create target must succeed");
    let source_id = source["id"].as_str().unwrap().to_string();
    let target_id = target["id"].as_str().unwrap().to_string();

    let linked = f
        .dispatch(
            "link",
            json!({"source_id": source_id, "target_id": target_id, "relation": "extends"}),
        )
        .await
        .expect("link must succeed");
    let edge_id = linked["id"]
        .as_str()
        .expect("link must return id")
        .to_string();
    let edge_uuid = Uuid::parse_str(&edge_id).expect("edge id must be valid UUID");

    // Soft-delete the edge.
    f.dispatch("delete", json!({"id": edge_id}))
        .await
        .expect("soft delete must succeed");

    // Hard-delete WITHOUT explicit kind — exercises infer_kind_from_uuid_including_deleted.
    let purge = f
        .dispatch("delete", json!({"id": edge_id, "hard": true}))
        .await;
    assert!(
        purge.is_ok(),
        "hard delete of a soft-deleted edge without kind must succeed (ADR-002); got: {purge:?}"
    );

    // Verify the row is physically gone via raw SQL (no deleted_at filter).
    let mut reader = rt.sql().reader().await.expect("sql reader must open");
    let count = reader
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM graph_edges WHERE id = ?1".into(),
            params: vec![SqlValue::Text(edge_uuid.to_string())],
            label: Some("count_edge_row_by_id".into()),
        })
        .await
        .expect("count query must succeed");
    let row_count = match count {
        Some(SqlValue::Integer(n)) => n as u64,
        _ => 0,
    };
    assert_eq!(
        row_count, 0,
        "hard delete must physically remove the edge row from graph_edges"
    );
}

// ── PR #121: proposal_id → id wire-key clean break ───────────────────────────
//
// These tests pin the contract that the old `proposal_id` wire key is ABSENT
// from every handler output and that the old input param name is rejected.
// Positive `id` presence is already asserted in the lifecycle tests above;
// here we add the complementary absence / negative-input coverage.

/// get(id=<proposal_uuid>) result must expose `id`, NOT `proposal_id`.
///
/// The get handler (get.rs:211-214) removes `proposal_id` from the
/// deserialized ProposalCreatedPayload and inserts `id`.  This test asserts
/// the absence side so a dual-emit regression would be caught immediately.
#[tokio::test]
async fn get_proposal_wire_key_is_id_not_proposal_id() {
    let f = pack_with_events();

    let propose_result = f
        .dispatch(
            "propose",
            json!({
                "title": "WireKeyAbsenceTest",
                "description": "get must return id, not proposal_id",
                "changeset": changeset_add_entity(),
            }),
        )
        .await
        .expect("propose must succeed");
    let pid = propose_result["id"]
        .as_str()
        .expect("propose must return id")
        .to_string();
    assert!(
        propose_result.get("proposal_id").is_none(),
        "propose result must NOT contain proposal_id; got {propose_result}"
    );

    let get_result = f
        .dispatch("get", json!({ "id": pid }))
        .await
        .expect("get must succeed");

    assert!(
        get_result.get("id").is_some(),
        "get(id=proposal_uuid) must return id field; got {get_result}"
    );
    assert!(
        get_result.get("proposal_id").is_none(),
        "get(id=proposal_uuid) must NOT return proposal_id (clean break); got {get_result}"
    );
}

/// #773: `get(id=<compact-hex-prefix>)` on an open proposal must resolve via
/// `proposals_open`'s own `LIKE` scan (get.rs:181), independent of the
/// `resolve_prefix_inner` fan-in point. Before the fix, any compact prefix
/// longer than 8 chars structurally could not match the hyphenated
/// `proposal_id` column.
#[tokio::test]
async fn get_proposal_by_compact_hex_prefix_over_8_chars() {
    let f = pack_with_events();

    let propose_result = f
        .dispatch(
            "propose",
            json!({
                "title": "CompactPrefixProposalTest",
                "description": "get must resolve a >8-char compact prefix",
                "changeset": changeset_add_entity(),
            }),
        )
        .await
        .expect("propose must succeed");
    let pid = propose_result["id"]
        .as_str()
        .expect("propose must return id")
        .to_string();

    let compact = pid.replace('-', "");
    let prefix = &compact[..16];

    let get_result = f
        .dispatch("get", json!({ "id": prefix }))
        .await
        .expect("get must resolve the proposal via compact prefix");

    assert_eq!(
        get_result.get("id").and_then(|v| v.as_str()),
        Some(pid.as_str()),
        "get(id=<compact prefix>) must resolve to the proposal; got {get_result}"
    );
}

/// PR #816: `review(id=<compact-hex-prefix>)` must resolve
/// via `resolve_proposal_uuid` (proposal.rs), independent of `get`'s own
/// `proposals_open` scan. Before the fix, `resolve_proposal_uuid` built the
/// `LIKE` pattern from the raw compact prefix instead of normalizing it with
/// `hex_prefix_to_uuid_pattern`, so any prefix longer than 8 chars
/// structurally could not match the hyphenated `proposal_id` column.
#[tokio::test]
async fn review_proposal_by_compact_hex_prefix_over_8_chars() {
    let f = pack_with_events();

    let propose_result = f
        .dispatch(
            "propose",
            json!({
                "title": "CompactPrefixReviewTest",
                "description": "review must resolve a >8-char compact prefix",
                "changeset": changeset_add_entity(),
            }),
        )
        .await
        .expect("propose must succeed");
    let pid = propose_result["id"]
        .as_str()
        .expect("propose must return id")
        .to_string();

    let compact = pid.replace('-', "");
    let prefix = &compact[..16];

    let review_result = f
        .dispatch("review", json!({ "id": prefix, "decision": "approve" }))
        .await
        .expect("review must resolve the proposal via compact prefix");

    assert_eq!(
        review_result.get("id").and_then(|v| v.as_str()),
        Some(pid.as_str()),
        "review(id=<compact prefix>) must resolve to the proposal; got {review_result}"
    );
}

/// PR #816: `withdraw(id=<compact-hex-prefix>)` must resolve
/// via `resolve_proposal_uuid` (proposal.rs) the same way `review` does.
#[tokio::test]
async fn withdraw_proposal_by_compact_hex_prefix_over_8_chars() {
    let f = pack_with_events();

    let propose_result = f
        .dispatch(
            "propose",
            json!({
                "title": "CompactPrefixWithdrawTest",
                "description": "withdraw must resolve a >8-char compact prefix",
                "changeset": changeset_add_entity(),
            }),
        )
        .await
        .expect("propose must succeed");
    let pid = propose_result["id"]
        .as_str()
        .expect("propose must return id")
        .to_string();

    let compact = pid.replace('-', "");
    let prefix = &compact[..16];

    let withdraw_result = f
        .dispatch("withdraw", json!({ "id": prefix }))
        .await
        .expect("withdraw must resolve the proposal via compact prefix");

    assert_eq!(
        withdraw_result.get("id").and_then(|v| v.as_str()),
        Some(pid.as_str()),
        "withdraw(id=<compact prefix>) must resolve to the proposal; got {withdraw_result}"
    );
}

/// review(id=..., proposal_id=...) must be rejected by #[serde(deny_unknown_fields)].
///
/// ReviewParams declares `id` (required) with deny_unknown_fields — `proposal_id` is
/// unknown. Supplying both keys proves the rejection is triggered by the unknown field,
/// not by a missing required field: if deny_unknown_fields were removed the call would
/// succeed (id is present, proposal_id silently ignored), so a passing test here is a
/// genuine regression guard.
#[tokio::test]
async fn review_with_old_proposal_id_param_is_rejected() {
    let f = pack_with_events();

    let propose = f
        .dispatch(
            "propose",
            json!({
                "title": "deny_unknown review guard",
                "description": "guard test",
                "changeset": changeset_add_entity(),
            }),
        )
        .await
        .expect("propose must succeed");
    let pid = propose["id"].as_str().expect("id");

    let err = f
        .dispatch(
            "review",
            json!({
                "id": pid,
                "proposal_id": pid,
                "decision": "reject"
            }),
        )
        .await;

    assert!(
        err.is_err(),
        "review(id=..., proposal_id=...) must be rejected by deny_unknown_fields; got Ok"
    );
    let e = err.unwrap_err();
    assert!(
        is_invalid_input(&e),
        "review(id=..., proposal_id=...) must produce InvalidInput; got {e:?}"
    );
    let msg = invalid_input_message(&e);
    assert!(
        msg.contains("unknown field"),
        "error must mention 'unknown field'; got: {msg}"
    );
    assert!(
        msg.contains("proposal_id"),
        "error must mention 'proposal_id'; got: {msg}"
    );
}

// ── dispatch_as: verified-actor dispatch for embedding hosts ────────────────

/// `VerbRegistry::dispatch_as` must make the supplied verified actor become
/// exactly the `reviewer` a `review` handler observes — the same
/// `token.actor().id` field `handle_review` already reads for every dispatch
/// path. This is the end-to-end proof that an embedding host's out-of-band
/// authenticated principal becomes the acting principal through this pack's
/// handlers, with no changes to `proposal.rs` itself.
#[tokio::test]
async fn dispatch_as_routes_verified_actor_to_review_handler() {
    let f = pack_with_events();

    let propose = f
        .dispatch(
            "propose",
            json!({
                "title": "dispatch_as reviewer routing",
                "description": "verify reviewer field reflects verified_actor",
                "changeset": changeset_add_entity(),
            }),
        )
        .await
        .expect("propose must succeed");
    let pid = propose["id"].as_str().expect("id").to_string();

    let review = f
        .registry
        .dispatch_as(
            "review",
            json!({ "id": pid, "decision": "approve" }),
            VerifiedActor::new("gateway:verified-principal").unwrap(),
        )
        .await
        .expect("dispatch_as(review) must succeed");

    assert_eq!(
        review.get("reviewer").and_then(|v| v.as_str()),
        Some("gateway:verified-principal"),
        "review handler must observe the verified_actor supplied to dispatch_as as \
         the reviewer; got {review}"
    );
}

/// Regression: `VerbRegistry::dispatch` (no verified actor) is unaffected by
/// the existence of `dispatch_as` — the default/anonymous actor path through
/// `propose`/`review` behaves exactly as before this API was added.
#[tokio::test]
async fn dispatch_unchanged_after_dispatch_as_added() {
    let f = pack_with_events();

    let propose = f
        .dispatch(
            "propose",
            json!({
                "title": "plain dispatch regression",
                "description": "reviewer must still default to anonymous local actor",
                "changeset": changeset_add_entity(),
            }),
        )
        .await
        .expect("propose must succeed");
    let pid = propose["id"].as_str().expect("id").to_string();
    assert_eq!(
        propose.get("proposer").and_then(|v| v.as_str()),
        Some("local")
    );

    let review = f
        .dispatch("review", json!({ "id": pid, "decision": "approve" }))
        .await
        .expect("plain dispatch(review) must still succeed");

    assert_eq!(
        review.get("reviewer").and_then(|v| v.as_str()),
        Some("local"),
        "plain dispatch() must keep resolving the anonymous/local actor, unchanged \
         by dispatch_as being added to the registry"
    );
}

/// A request `params` payload cannot inject an actor through `dispatch_as`:
/// `review`'s params schema has no `actor` field and denies unknown fields,
/// so an `actor` key in `params` is rejected exactly like any other unknown
/// field — it never reaches (and can never override) the verified actor.
#[tokio::test]
async fn dispatch_as_rejects_actor_key_in_params() {
    let f = pack_with_events();

    let propose = f
        .dispatch(
            "propose",
            json!({
                "title": "dispatch_as actor injection guard",
                "description": "an actor key in params must be rejected, not honored",
                "changeset": changeset_add_entity(),
            }),
        )
        .await
        .expect("propose must succeed");
    let pid = propose["id"].as_str().expect("id").to_string();

    let err = f
        .registry
        .dispatch_as(
            "review",
            json!({ "id": pid, "decision": "approve", "actor": "spoofed-actor" }),
            VerifiedActor::new("gateway:verified-principal").unwrap(),
        )
        .await;

    assert!(
        err.is_err(),
        "review(..., actor=<spoofed>) via dispatch_as must be rejected; got Ok"
    );
    let e = err.unwrap_err();
    assert!(
        is_invalid_input(&e),
        "actor key in params must produce InvalidInput; got {e:?}"
    );
    let msg = invalid_input_message(&e);
    assert!(
        msg.contains("unknown field"),
        "error must mention 'unknown field'; got: {msg}"
    );
}

/// withdraw(id=..., proposal_id=...) must be rejected by #[serde(deny_unknown_fields)].
///
/// WithdrawParams declares `id` (required) with deny_unknown_fields — `proposal_id` is
/// unknown. Supplying both keys proves the rejection is triggered by the unknown field,
/// not by a missing required field: if deny_unknown_fields were removed the call would
/// succeed (id is present, proposal_id silently ignored), so a passing test here is a
/// genuine regression guard.
#[tokio::test]
async fn withdraw_with_old_proposal_id_param_is_rejected() {
    let f = pack_with_events();

    let propose = f
        .dispatch(
            "propose",
            json!({
                "title": "deny_unknown withdraw guard",
                "description": "guard test",
                "changeset": changeset_add_entity(),
            }),
        )
        .await
        .expect("propose must succeed");
    let pid = propose["id"].as_str().expect("id");

    let err = f
        .dispatch(
            "withdraw",
            json!({
                "id": pid,
                "proposal_id": pid
            }),
        )
        .await;

    assert!(
        err.is_err(),
        "withdraw(id=..., proposal_id=...) must be rejected by deny_unknown_fields; got Ok"
    );
    let e = err.unwrap_err();
    assert!(
        is_invalid_input(&e),
        "withdraw(id=..., proposal_id=...) must produce InvalidInput; got {e:?}"
    );
    let msg = invalid_input_message(&e);
    assert!(
        msg.contains("unknown field"),
        "error must mention 'unknown field'; got: {msg}"
    );
    assert!(
        msg.contains("proposal_id"),
        "error must mention 'proposal_id'; got: {msg}"
    );
}

// ── #176 regression: search(kind="note") must honour `tags` and `properties` filters ──

/// #176 / ADR-029: search(kind="note", tags=["target"]) must exclude notes whose stored
/// `properties.tags` array does not contain "target".
///
/// Both notes share identical query text (so the search engine returns both), but only
/// one carries `{"tags": ["target"]}` in its properties. After the fix the handler must
/// return exactly 1 hit; before the fix it returns 2 (filter is silently dropped).
///
/// Tags on notes are stored inside `properties["tags"]` (same convention as
/// `memory.remember`); there is no separate tags column on the notes table.
/// Notes are created via `pack.dispatch("create", ...)` so FTS indexing occurs.
#[tokio::test]
async fn search_note_tag_filter_excludes_non_matching_notes() {
    let pack = pack();

    // Note 1: shared searchable text + target tag stored in properties["tags"].
    let note_a = pack
        .dispatch(
            "create",
            json!({
                "kind": "note",
                "note_kind": "observation",
                "content": "shared tag search target text alpha nvk176",
                "properties": {"tags": ["target"]}
            }),
        )
        .await
        .expect("create note_a must succeed");
    let note_a_id = note_a.get("id").and_then(Value::as_str).expect("id");

    // Note 2: same text, different tag.
    pack.dispatch(
        "create",
        json!({
            "kind": "note",
            "note_kind": "observation",
            "content": "shared tag search target text alpha nvk176",
            "properties": {"tags": ["other"]}
        }),
    )
    .await
    .expect("create note_b must succeed");

    let resp = pack
        .dispatch(
            "search",
            json!({
                "kind": "note",
                "query": "shared tag search target text alpha nvk176",
                "tags": ["target"],
            }),
        )
        .await
        .expect("#176: search with tags filter must not error");

    let arr = resp.as_array().expect("response must be array");
    let ids: Vec<&str> = arr
        .iter()
        .filter_map(|h| h.get("id").and_then(Value::as_str))
        .collect();

    assert_eq!(
        ids.len(),
        1,
        "#176: tags filter must return exactly 1 hit; got {}: ids={ids:?}",
        ids.len()
    );
    assert_eq!(
        ids[0], note_a_id,
        "#176: the matching note must be note_a; got {ids:?}"
    );
}

/// #176 / ADR-029: search(kind="note", properties={"domain":"inference"}) must exclude
/// notes whose properties don't match.
///
/// Both notes share identical query text; only note_a has `{"domain":"inference"}`.
/// After the fix the handler returns exactly 1 hit.
/// Notes are created via `pack.dispatch("create", ...)` so FTS indexing occurs.
#[tokio::test]
async fn search_note_properties_filter_excludes_non_matching_notes() {
    let pack = pack();

    // Note 1: matching property value.
    let note_a = pack
        .dispatch(
            "create",
            json!({
                "kind": "note",
                "note_kind": "observation",
                "content": "shared props search target text beta nvk176",
                "properties": {"domain": "inference"}
            }),
        )
        .await
        .expect("create note_a must succeed");
    let note_a_id = note_a.get("id").and_then(Value::as_str).expect("id");

    // Note 2: different property value.
    pack.dispatch(
        "create",
        json!({
            "kind": "note",
            "note_kind": "observation",
            "content": "shared props search target text beta nvk176",
            "properties": {"domain": "training"}
        }),
    )
    .await
    .expect("create note_b must succeed");

    let resp = pack
        .dispatch(
            "search",
            json!({
                "kind": "note",
                "query": "shared props search target text beta nvk176",
                "properties": {"domain": "inference"},
            }),
        )
        .await
        .expect("#176: search with properties filter must not error");

    let arr = resp.as_array().expect("response must be array");
    let ids: Vec<&str> = arr
        .iter()
        .filter_map(|h| h.get("id").and_then(Value::as_str))
        .collect();

    assert_eq!(
        ids.len(),
        1,
        "#176: properties filter must return exactly 1 hit; got {}: ids={ids:?}",
        ids.len()
    );
    assert_eq!(
        ids[0], note_a_id,
        "#176: the matching note must be note_a; got {ids:?}"
    );
}

/// #223 recall-cliff regression: verifies that a note carrying the target tag
/// but ranking below position 40 (the old `(limit*4).min(100)=40` handler cap
/// with limit=10) is still returned when the NEW cap of 500 is in effect.
///
/// Design:
///   - 45 "noise" notes whose content repeats the query term 8× — BM25 ranks
///     them well above the rare note which uses the term only once.
///   - 1 "rare" note with the term once — ranks at position 46 in the
///     unfiltered corpus, safely beyond the old cap of 40 but well within 500.
///
/// Two-phase assertion:
///   1. Unfiltered search (limit=500) confirms the rare note's rank is > 40
///      and <= 500 — proving the geometry. This assertion FAILS under the old
///      cap (the rare note would be absent from a limit=40 result set).
///   2. Filtered search (tags=["rare"], limit=10) asserts the rare note IS
///      returned — proving the NEW cap of 500 reaches it.
///
/// If someone reverts the handler cap to `(limit*4).min(100)`, phase 2 will
/// fail because search_notes(limit=40) truncates before reaching rank 46.
#[tokio::test]
async fn search_note_tag_filter_recall_cliff_bounded_scan() {
    let pack = pack();

    // 45 noise notes: content repeats the query term 8× so BM25 places them
    // well above the rare note (1 occurrence). This pushes the rare note to
    // rank ~46, beyond the old handler cap of (10*4).min(100)=40.
    for i in 0..45u32 {
        let repeated = "cliffterm nvk223rc ".repeat(8);
        pack.dispatch(
            "create",
            json!({
                "kind": "note",
                "note_kind": "observation",
                "content": format!("{repeated}noise {i}"),
                "properties": {"tags": ["noise"]}
            }),
        )
        .await
        .unwrap_or_else(|e| panic!("create noise note {i} must succeed: {e}"));
    }

    // The rare note uses the query term only once → ranks below all 45 noise
    // notes in BM25 order (position ~46).
    let rare = pack
        .dispatch(
            "create",
            json!({
                "kind": "note",
                "note_kind": "observation",
                "content": "cliffterm nvk223rc rare",
                "properties": {"tags": ["rare"]}
            }),
        )
        .await
        .expect("create rare note must succeed");
    let rare_id = rare.get("id").and_then(Value::as_str).expect("id");

    // Phase 1 — geometry check: unfiltered search with limit=500 must include
    // the rare note, and its rank must be > 40 (the old cap) and <= 500.
    // This asserts the rare note sits in the window the NEW cap opens up.
    let unfiltered = pack
        .dispatch(
            "search",
            json!({
                "kind": "note",
                "query": "cliffterm nvk223rc",
                "limit": 500,
            }),
        )
        .await
        .expect("#223: unfiltered search must not error");

    let all_ids: Vec<String> = unfiltered
        .as_array()
        .expect("response must be array")
        .iter()
        .filter_map(|h| h.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();

    let rare_rank = all_ids
        .iter()
        .position(|id| id == rare_id)
        .map(|pos| pos + 1) // 1-based rank
        .expect("#223[phase1]: rare note must appear in the unfiltered result set");

    assert!(
        rare_rank > 40,
        "#223[phase1]: rare note rank must be > 40 (old handler cap) so the cliff is real; got rank={rare_rank}"
    );
    assert!(
        rare_rank <= 500,
        "#223[phase1]: rare note rank must be <= 500 (new handler cap); got rank={rare_rank}"
    );

    // Phase 2 — filter check: with tag filter "rare" and limit=10 the new
    // handler cap of 500 must reach the rare note; the old cap of 40 would not.
    let resp = pack
        .dispatch(
            "search",
            json!({
                "kind": "note",
                "query": "cliffterm nvk223rc",
                "tags": ["rare"],
                "limit": 10,
            }),
        )
        .await
        .expect("#223: filtered search must not error");

    let arr = resp.as_array().expect("response must be array");
    let filtered_ids: Vec<&str> = arr
        .iter()
        .filter_map(|h| h.get("id").and_then(Value::as_str))
        .collect();

    assert!(
        filtered_ids.contains(&rare_id),
        "#223[phase2]: rare note (rank={rare_rank} > 40) must be returned with new cap=500; ids={filtered_ids:?}"
    );
    assert!(
        filtered_ids.len() <= 10,
        "#223[phase2]: result must not exceed limit=10; got {}: ids={filtered_ids:?}",
        filtered_ids.len()
    );
}

/// #223: search(kind="note", tags=[nonexistent]) where no note matches must return empty.
#[tokio::test]
async fn search_note_tag_filter_no_match_returns_empty() {
    let pack = pack();

    pack.dispatch(
        "create",
        json!({
            "kind": "note",
            "note_kind": "observation",
            "content": "no match tag filter test nvk223",
            "properties": {"tags": ["existing"]}
        }),
    )
    .await
    .expect("create note must succeed");

    let resp = pack
        .dispatch(
            "search",
            json!({
                "kind": "note",
                "query": "no match tag filter test nvk223",
                "tags": ["nonexistent-tag-xyz"],
            }),
        )
        .await
        .expect("#223: search with no-match tag filter must not error");

    let arr = resp.as_array().expect("response must be array");
    assert!(
        arr.is_empty(),
        "#223: tag filter matching nothing must return empty array; got: {arr:?}"
    );
}

/// #223: search(kind="note") with combined property+tag filter must honour both
/// predicates simultaneously (AND semantics). Notes matching only one predicate
/// are excluded.
#[tokio::test]
async fn search_note_combined_property_and_tag_filter() {
    let pack = pack();

    // Note A: matches both property AND tag.
    let note_a = pack
        .dispatch(
            "create",
            json!({
                "kind": "note",
                "note_kind": "observation",
                "content": "combined filter test nvk223 alpha",
                "properties": {"domain": "inference", "tags": ["target"]}
            }),
        )
        .await
        .expect("create note_a must succeed");
    let note_a_id = note_a.get("id").and_then(Value::as_str).expect("id");

    // Note B: matches tag but NOT property.
    pack.dispatch(
        "create",
        json!({
            "kind": "note",
            "note_kind": "observation",
            "content": "combined filter test nvk223 alpha",
            "properties": {"domain": "training", "tags": ["target"]}
        }),
    )
    .await
    .expect("create note_b must succeed");

    // Note C: matches property but NOT tag.
    pack.dispatch(
        "create",
        json!({
            "kind": "note",
            "note_kind": "observation",
            "content": "combined filter test nvk223 alpha",
            "properties": {"domain": "inference", "tags": ["other"]}
        }),
    )
    .await
    .expect("create note_c must succeed");

    let resp = pack
        .dispatch(
            "search",
            json!({
                "kind": "note",
                "query": "combined filter test nvk223 alpha",
                "properties": {"domain": "inference"},
                "tags": ["target"],
            }),
        )
        .await
        .expect("#223: combined filter search must not error");

    let arr = resp.as_array().expect("response must be array");
    let ids: Vec<&str> = arr
        .iter()
        .filter_map(|h| h.get("id").and_then(Value::as_str))
        .collect();

    assert_eq!(
        ids.len(),
        1,
        "#223: combined property+tag filter must return exactly 1 hit; got {}: ids={ids:?}",
        ids.len()
    );
    assert_eq!(
        ids[0], note_a_id,
        "#223: only note_a matches both predicates; got {ids:?}"
    );
}

// ---- Formal pack gate: default-vs-formal control test ----
//
// These tests prove that the formal pack's EDGE_RULES are actually wired into
// the link path — not just that the rule table is non-empty. Specifically:
//
// - With only `kg` loaded (default surface), `depends_on` between typed formal
//   concept entities (theorem→definition) MUST be rejected.
// - With `kg,formal` loaded, the same link MUST be accepted.
//
// This mirrors what the MCP transport does at startup (ADR-031): build the
// VerbRegistry, then call `rt.install_edge_rules(registry.all_edge_rules())`.

fn pack_kg_only() -> (Fixture, KhiveRuntime) {
    let rt = KhiveRuntime::memory().expect("in-memory runtime must succeed");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let registry = builder.build().expect("kg-only registry builds");
    rt.install_edge_rules(registry.all_edge_rules());
    (Fixture { registry }, rt)
}

fn pack_kg_and_formal() -> (Fixture, KhiveRuntime) {
    use khive_pack_formal::FormalPack;

    let rt = KhiveRuntime::memory().expect("in-memory runtime must succeed");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(FormalPack::new(rt.clone()));
    let registry = builder.build().expect("kg+formal registry builds");
    rt.install_edge_rules(registry.all_edge_rules());
    (Fixture { registry }, rt)
}

/// Without formal pack: depends_on between typed concept entities (theorem→definition)
/// must be rejected. Proves the formal rules are not present in the default surface.
#[tokio::test]
async fn formal_depends_on_rejected_without_formal_pack() {
    let (f, _rt) = pack_kg_only();

    let thm = f
        .dispatch(
            "create",
            json!({ "kind": "concept", "entity_type": "theorem", "name": "Nat.add_comm" }),
        )
        .await
        .expect("create theorem concept");

    let def = f
        .dispatch(
            "create",
            json!({ "kind": "concept", "entity_type": "definition", "name": "Nat.add" }),
        )
        .await
        .expect("create definition concept");

    let result = f
        .dispatch(
            "link",
            json!({
                "source_id": thm["id"],
                "target_id": def["id"],
                "relation": "depends_on",
            }),
        )
        .await;

    assert!(
        result.is_err(),
        "depends_on between theorem→definition must be rejected without formal pack loaded; \
         got: {result:?}"
    );
}

/// With formal pack: depends_on between typed concept entities (theorem→definition)
/// must be accepted. Proves the pack installs its EDGE_RULES into the link path.
#[tokio::test]
async fn formal_depends_on_accepted_with_formal_pack() {
    let (f, _rt) = pack_kg_and_formal();

    let thm = f
        .dispatch(
            "create",
            json!({ "kind": "concept", "entity_type": "theorem", "name": "Nat.add_comm" }),
        )
        .await
        .expect("create theorem concept");

    let def = f
        .dispatch(
            "create",
            json!({ "kind": "concept", "entity_type": "definition", "name": "Nat.add" }),
        )
        .await
        .expect("create definition concept");

    let result = f
        .dispatch(
            "link",
            json!({
                "source_id": thm["id"],
                "target_id": def["id"],
                "relation": "depends_on",
            }),
        )
        .await;

    assert!(
        result.is_ok(),
        "depends_on between theorem→definition must succeed with formal pack loaded; \
         got: {result:?}"
    );
    let edge = result.unwrap();
    assert_eq!(edge["relation"], "depends_on");
}

// ── Med-1 regression: malformed `items` must not fall through to singleton ──────

/// A malformed bulk payload (unknown field in an item) must return an error and
/// must NOT create the top-level entity that was named in the enclosing params.
///
/// Pre-fix: `serde_json::from_value(...).ok()` swallowed the parse error, the
/// bulk path was skipped, and the singleton path ran — creating "TopLevelCreated"
/// silently even though the caller intended a bulk create.
#[tokio::test]
async fn create_bulk_items_malformed_unknown_field_returns_error_creates_nothing() {
    let pack = pack();
    let err = pack
        .dispatch(
            "create",
            json!({
                "kind": "concept",
                "name": "TopLevelCreated",
                "items": [{"kind": "concept", "name": "ShouldBeBulk", "extra_unknown": 1}]
            }),
        )
        .await
        .expect_err("malformed items must produce an error");
    assert!(
        is_invalid_input(&err),
        "malformed items must return InvalidInput, got: {err:?}"
    );
    // Neither the bulk item nor the top-level entity must have been written.
    let listed = pack
        .dispatch("list", json!({"kind": "concept"}))
        .await
        .expect("list must succeed");
    let count = listed
        .get("items")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    assert_eq!(
        count, 0,
        "malformed items rejection must create nothing; got {listed}"
    );
}

// ── entity-type validator is installed by normal pack registration ─────────

/// Build a `(KhiveRuntime, VerbRegistry)` pair using the same boot sequence as
/// the production MCP server: register the pack, build the registry, install edge
/// rules + entity-type validators.  No manual call to
/// `install_entity_type_validator` is made — the validator must arrive via
/// `call_register_entity_type_validators`.
fn pack_with_validators() -> (KhiveRuntime, VerbRegistry) {
    let rt = KhiveRuntime::memory().expect("in-memory runtime must succeed");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");
    rt.install_edge_rules(registry.all_edge_rules());
    // Production boot: call entity-type validator registration without manually
    // calling install_entity_type_validator.
    registry.call_register_entity_type_validators(&rt);
    (rt, registry)
}

/// After normal pack registration (`call_register_entity_type_validators`),
/// direct `runtime.create_many` must reject unregistered `entity_type` values
/// without any manual call to `install_entity_type_validator`.
#[tokio::test]
async fn create_many_runtime_validator_installed_via_pack_registration() {
    let (rt, _registry) = pack_with_validators();
    let tok = rt.authorize(Namespace::local()).unwrap();

    let bad_specs = vec![EntityCreateSpec {
        kind: "concept".into(),
        entity_type: Some("not_a_registered_type".into()),
        name: "ShouldNotLand".into(),
        description: None,
        properties: None,
        tags: vec![],
    }];

    let result = rt.create_many(&tok, bad_specs).await;
    assert!(
        matches!(result, Err(RuntimeError::InvalidInput(_))),
        "direct runtime.create_many must reject unknown entity_type once \
         validator is installed via pack registration; got {result:?}"
    );

    // Zero rows written.
    let rows = rt.list_entities(&tok, None, None, 100, 0).await.unwrap();
    assert_eq!(
        rows.len(),
        0,
        "rejected create_many must write nothing; got {rows:?}"
    );
}

// ── Multi-backend per-pack runtime validator regression ──

/// In a multi-backend deployment the KG pack is constructed with its OWN runtime
/// (not the `default_runtime`).  After `call_register_entity_type_validators` the
/// KG pack's per-pack runtime must reject an unknown `entity_type` — even though
/// the call passes the `default_runtime`, not the per-pack one.
///
/// This is the production path: `PackRegistry::register_packs_with_runtimes` gives
/// the `kg` pack its own runtime, then serve.rs calls
/// `registry.call_register_entity_type_validators(&default_runtime)`.  Without the
/// R3 fix (self.runtime install), the per-pack runtime has no validator and
/// `runtime.create_many` with an unknown `entity_type` silently succeeds.
#[tokio::test]
async fn create_many_runtime_validator_installed_on_per_pack_runtime_multi_backend() {
    use khive_runtime::{EntityCreateSpec, PackRegistry};
    use std::collections::HashMap;

    // Two separate in-memory runtimes simulate a multi-backend deployment.
    let default_runtime = KhiveRuntime::memory().expect("default runtime");
    let kg_runtime = KhiveRuntime::memory().expect("kg per-pack runtime");

    // Map "kg" → kg_runtime (the distinct per-pack backend).
    let mut runtimes: HashMap<String, KhiveRuntime> = HashMap::new();
    runtimes.insert("kg".to_string(), kg_runtime.clone());

    // Register packs the production way: kg uses kg_runtime, everything else uses default.
    let mut builder = VerbRegistryBuilder::new();
    PackRegistry::register_packs_with_runtimes(
        &["kg".to_string()],
        &runtimes,
        &default_runtime,
        &mut builder,
    )
    .expect("pack registration must succeed");

    let registry = builder.build().expect("registry build must succeed");

    // Install edge rules on both runtimes (mirrors serve.rs).
    default_runtime.install_edge_rules(registry.all_edge_rules());
    kg_runtime.install_edge_rules(registry.all_edge_rules());

    // Install entity-type validators — serve.rs passes &default_runtime here.
    // With the R3 fix KgPack ignores the arg and installs onto self.runtime
    // (kg_runtime).  Without the fix kg_runtime has no validator.
    registry.call_register_entity_type_validators(&default_runtime);

    // Now drive create_many directly on the KG pack's OWN runtime with an
    // unknown entity_type — no manual install_entity_type_validator call.
    let tok = kg_runtime.authorize(Namespace::local()).expect("authorize");
    let bad_specs = vec![EntityCreateSpec {
        kind: "concept".into(),
        entity_type: Some("not_a_registered_type".into()),
        name: "ShouldBeRejected".into(),
        description: None,
        properties: None,
        tags: vec![],
    }];

    let result = kg_runtime.create_many(&tok, bad_specs).await;
    assert!(
        matches!(result, Err(RuntimeError::InvalidInput(_))),
        "per-pack runtime must reject unknown entity_type once the KG pack's \
         register_entity_type_validator installs onto self.runtime; got {result:?}"
    );

    // Zero rows written.
    let rows = kg_runtime
        .list_entities(&tok, None, None, 100, 0)
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        0,
        "rejected create_many must write nothing; got {rows:?}"
    );
}

// ── invalid entity_type in bulk items is rejected at the handler layer ──────

/// An invalid `entity_type` inside a bulk item must be rejected with the valid
/// types listed, and must write nothing.
#[tokio::test]
async fn create_bulk_items_invalid_entity_type_rejects_batch() {
    let pack = pack();
    let err = pack
        .dispatch(
            "create",
            json!({
                "items": [
                    {"kind": "concept", "name": "ValidConcept", "entity_type": "not_a_registered_type"}
                ]
            }),
        )
        .await
        .expect_err("unknown entity_type in bulk item must be rejected");
    assert!(
        is_invalid_input(&err),
        "unknown entity_type must return InvalidInput, got: {err:?}"
    );
    let listed = pack
        .dispatch("list", json!({"kind": "concept"}))
        .await
        .expect("list must succeed");
    let count = listed
        .get("items")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    assert_eq!(
        count, 0,
        "entity_type rejection must write nothing; got {listed}"
    );
}

// ── Proposal lifecycle: changeset application verified in KG ─────────────────
//
// The Wave-5 lifecycle tests (propose_review_approve_lifecycle et al.) assert
// that the proposal status flips to "applied" but do NOT verify the changeset
// payload was actually executed.  These tests close that gap: after apply, the
// entity or note named in the changeset must exist in the KG store.

/// Full lifecycle (add_entity changeset): propose → review(approve) → applied.
///
/// Covers:
///  1. propose returns status "open", `id` key present, `proposal_id` absent.
///  2. review(approve) returns "approved" or "applied".
///  3. proposal appears in list(kind=proposal, status=applied).
///  4. (non-vacuous) entity from the changeset EXISTS in the KG — this is the
///     assertion that catches a silent apply-worker failure even when steps 1-3 pass.
#[tokio::test]
async fn proposal_add_entity_changeset_entity_exists_in_kg_after_apply() {
    let f = pack_with_events();

    // Unique name so the search below is unambiguous in a clean in-memory store.
    let entity_name = "ProposalApplyEntityTarget_nvk_lifecycle_42";

    // Step 1: propose.
    let propose = f
        .dispatch(
            "propose",
            json!({
                "title": "add-entity changeset verify",
                "description": "Verify that the changeset entity lands in the KG after apply",
                "changeset": {
                    "kind": "add_entity",
                    "entity": { "kind": "concept", "name": entity_name }
                },
            }),
        )
        .await
        .expect("propose must succeed");

    let pid = propose["id"].as_str().expect("propose must return id");
    assert_eq!(
        propose["status"].as_str(),
        Some("open"),
        "propose: initial status must be 'open'; got {propose}"
    );
    assert!(
        propose.get("proposal_id").is_none(),
        "propose must NOT emit old proposal_id key; got {propose}"
    );

    // Step 2: review(approve) — "local" actor is exempt from the self-approval guard.
    let review = f
        .dispatch("review", json!({ "id": pid, "decision": "approve" }))
        .await
        .expect("review(approve) must succeed");

    let review_status = review["status"].as_str().unwrap_or("");
    assert!(
        review_status == "approved" || review_status == "applied",
        "review: status must be 'approved' or 'applied'; got {review_status:?}; full: {review}"
    );
    assert!(
        review.get("proposal_id").is_none(),
        "review must NOT emit old proposal_id key; got {review}"
    );

    // Step 3: proposal must appear in the applied projection.
    let list = f
        .dispatch("list", json!({ "kind": "proposal", "status": "applied" }))
        .await
        .expect("list(kind=proposal, status=applied) must succeed");
    let items = list.as_array().expect("list must return an array");
    assert!(
        items
            .iter()
            .any(|v| v["id"].as_str().is_some_and(|id| id == pid)),
        "proposal {pid} must appear in list(status=applied); items: {list}"
    );

    // Step 4 (non-vacuous): the entity named in the changeset must exist in the KG.
    // search(kind="entity") returns a bare array of hits; a non-empty result proves
    // the apply worker ran and executed the add_entity operation.
    let search = f
        .dispatch("search", json!({ "kind": "entity", "query": entity_name }))
        .await
        .expect("search(kind=entity) must succeed");
    let hits = search.as_array().expect("search must return array");
    assert!(
        !hits.is_empty(),
        "entity '{entity_name}' must exist in KG after proposal apply; got empty search result"
    );
}

/// Full lifecycle (add_note changeset): propose → review(approve) → applied.
///
/// Covers:
///  1. propose returns status "open", `id` present, `proposal_id` absent.
///  2. review(approve) returns "approved" or "applied".
///  3. proposal appears in list(kind=proposal, status=applied).
///  4. (non-vacuous) note from the changeset EXISTS in the note store — this is
///     the assertion that catches a silent apply-worker failure for note changesets.
#[tokio::test]
async fn proposal_add_note_changeset_note_exists_in_kg_after_apply() {
    let f = pack_with_events();

    // Unique content so the search below is unambiguous.
    let note_content = "ProposalApplyNoteContent_nvk_lifecycle_42";

    // Step 1: propose.
    let propose = f
        .dispatch(
            "propose",
            json!({
                "title": "add-note changeset verify",
                "description": "Verify that the changeset note lands in the KG after apply",
                "changeset": {
                    "kind": "add_note",
                    "note": { "kind": "observation", "content": note_content }
                },
            }),
        )
        .await
        .expect("propose must succeed");

    let pid = propose["id"].as_str().expect("propose must return id");
    assert_eq!(
        propose["status"].as_str(),
        Some("open"),
        "propose: initial status must be 'open'; got {propose}"
    );
    assert!(
        propose.get("proposal_id").is_none(),
        "propose must NOT emit old proposal_id key; got {propose}"
    );

    // Step 2: review(approve).
    let review = f
        .dispatch("review", json!({ "id": pid, "decision": "approve" }))
        .await
        .expect("review(approve) must succeed");

    let review_status = review["status"].as_str().unwrap_or("");
    assert!(
        review_status == "approved" || review_status == "applied",
        "review: status must be 'approved' or 'applied'; got {review_status:?}; full: {review}"
    );
    assert!(
        review.get("proposal_id").is_none(),
        "review must NOT emit old proposal_id key; got {review}"
    );

    // Step 3: proposal must appear in the applied projection.
    let list = f
        .dispatch("list", json!({ "kind": "proposal", "status": "applied" }))
        .await
        .expect("list(kind=proposal, status=applied) must succeed");
    let items = list.as_array().expect("list must return an array");
    assert!(
        items
            .iter()
            .any(|v| v["id"].as_str().is_some_and(|id| id == pid)),
        "proposal {pid} must appear in list(status=applied); items: {list}"
    );

    // Step 4 (non-vacuous): the note from the changeset must exist in the note store.
    // search(kind="note") with the unique content string as the query returns hits
    // only if the note was actually written by the apply worker.
    let search = f
        .dispatch("search", json!({ "kind": "note", "query": note_content }))
        .await
        .expect("search(kind=note) must succeed");
    let hits = search.as_array().expect("search must return array");
    assert!(
        !hits.is_empty(),
        "note with content '{note_content}' must exist in KG after proposal apply; \
         got empty search result"
    );
}

// ── delivered filter must reach past 200 delivered notes ────────────
//
// Regression: before the fix, list(kind=message, direction=outbound) fetched
// the 200 most-recent notes and applied the delivered_at check in Rust — so an
// old undelivered note was permanently stranded behind the delivered ones.
// After the fix, the query layer filters on delivered=false BEFORE applying the
// 200-note page, so undelivered notes are reachable past the old 200-row strand.
// This does NOT make the scan unbounded: `list` still caps total rows scanned at
// MAX_SCAN_TOTAL (10_000, see list.rs), so an undelivered note older than 10,000
// newer message rows is still unreachable. Tracked as a follow-up to push the
// `delivered` filter into the storage-level NoteFilter/SQL path.

#[tokio::test]
async fn list_delivered_filter_finds_undelivered_note_past_200_delivered() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = rt.authorize(Namespace::local()).expect("authorize local");

    // Write the OLD undelivered note first (smallest created_at → sorted last by DB).
    let old_undelivered = rt
        .create_note(
            &token,
            "message",
            None,
            "the old undelivered note",
            None,
            Some(serde_json::json!({
                "direction": "outbound",
                "from": "email:mailbox@example.com",
                "to": "email:user@example.com",
                "to_actor": "email:user@example.com",
            })),
            vec![],
        )
        .await
        .expect("create old undelivered note");
    let old_id = old_undelivered.id.as_hyphenated().to_string();

    // Write 210 NEWER delivered notes (these sort before the old one in DESC order).
    let store = rt.notes(&token).expect("note store");
    for i in 0..210u32 {
        let note = Note::new("local", "message", format!("delivered {i}")).with_properties(
            serde_json::json!({
                "direction": "outbound",
                "from": "email:mailbox@example.com",
                "to": "email:user@example.com",
                "to_actor": "email:user@example.com",
                "delivered_at": "2026-01-01T00:00:00Z",
            }),
        );
        store
            .upsert_note(note)
            .await
            .expect("upsert delivered note");
    }

    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");

    // delivered=false must return the old undelivered note (not strand it behind 210 delivered).
    // Use kind="note" (the generic substrate level) so the test registry (KgPack-only) can
    // resolve the kind without needing CommPack's "message" granular registration.
    let result = registry
        .dispatch(
            "list",
            serde_json::json!({
                "kind": "note",
                "direction": "outbound",
                "delivered": false,
                "limit": 10,
            }),
        )
        .await
        .expect("list(delivered=false) must succeed");

    let items = result.as_array().expect("list returns array");
    assert_eq!(
        items.len(),
        1,
        "list(delivered=false) must find the 1 undelivered note past 210 delivered; \
         got {} items",
        items.len()
    );
    let returned_id = items[0].get("id").and_then(|v| v.as_str()).unwrap_or("");
    assert_eq!(
        returned_id, old_id,
        "returned note id={returned_id} must equal the undelivered note id={old_id}"
    );

    // delivered=true must return only delivered notes.
    // Note: the note list handler caps at 200 per page, so with 210 delivered notes
    // we request 200 (the cap) and expect 200 — all of them must have delivered_at.
    let delivered_result = registry
        .dispatch(
            "list",
            serde_json::json!({
                "kind": "note",
                "direction": "outbound",
                "delivered": true,
                "limit": 200,
            }),
        )
        .await
        .expect("list(delivered=true) must succeed");
    let delivered_items = delivered_result.as_array().expect("array");
    assert_eq!(
        delivered_items.len(),
        200,
        "list(delivered=true) must return 200 delivered notes (page cap); \
         got {}",
        delivered_items.len()
    );
    for item in delivered_items {
        let da = item
            .get("properties")
            .and_then(|p| p.get("delivered_at"))
            .and_then(|v| v.as_str());
        assert!(
            da.is_some(),
            "list(delivered=true) items must all have delivered_at; got {item}"
        );
    }

    // Absent delivered param must return the page cap worth of notes (back-compat — no extra filter).
    let all_result = registry
        .dispatch(
            "list",
            serde_json::json!({
                "kind": "note",
                "direction": "outbound",
                "limit": 200,
            }),
        )
        .await
        .expect("list(no delivered filter) must succeed");
    let all_items = all_result.as_array().expect("array");
    assert_eq!(
        all_items.len(),
        200,
        "list(no delivered filter) must return 200 notes (page cap of 211 total); \
         got {}",
        all_items.len()
    );
}

// ---- context (ADR-089): entity-anchored graph context in one call ----

#[tokio::test]
async fn context_requires_query_or_entity_ids() {
    let pack = pack();
    let err = pack
        .dispatch("context", json!({}))
        .await
        .expect_err("context with neither query nor entity_ids must be rejected");
    assert!(is_invalid_input(&err), "must be InvalidInput; got: {err:?}");
    assert!(
        invalid_input_message(&err).contains("query")
            && invalid_input_message(&err).contains("entity_ids"),
        "error must name both query and entity_ids; got: {}",
        invalid_input_message(&err)
    );
}

#[tokio::test]
async fn context_empty_entity_ids_and_absent_query_is_rejected() {
    let pack = pack();
    let err = pack
        .dispatch("context", json!({"entity_ids": []}))
        .await
        .expect_err("context with an empty entity_ids array and no query must be rejected");
    assert!(is_invalid_input(&err), "must be InvalidInput; got: {err:?}");
}

#[tokio::test]
async fn context_entity_ids_anchor_carries_full_entity_record() {
    let pack = pack();
    let a = pack
        .dispatch(
            "create",
            json!({
                "kind": "entity",
                "name": "CtxAnchorA",
                "entity_kind": "concept",
                "description": "the anchor concept",
                "properties": {"domain": "attention"}
            }),
        )
        .await
        .expect("create A");
    let a_id = a["id"].as_str().unwrap().to_string();

    let resp = pack
        .dispatch("context", json!({"entity_ids": [a_id], "hops": 0}))
        .await
        .expect("context must succeed");

    let anchors = resp["anchors"].as_array().expect("anchors array");
    assert_eq!(anchors.len(), 1, "exactly one anchor expected");
    let entity = &anchors[0]["entity"];
    assert_eq!(entity["id"], a_id);
    assert_eq!(entity["name"], "CtxAnchorA");
    assert_eq!(entity["kind"], "concept");
    assert_eq!(entity["description"], "the anchor concept");
    assert_eq!(entity["properties"]["domain"], "attention");
    assert_eq!(
        anchors[0]["neighbors"].as_array().unwrap().len(),
        0,
        "hops=0 must return anchors only, no expansion"
    );
    assert_eq!(resp["truncated"], false);
    assert_eq!(resp["dropped"]["anchors"], 0);
    assert_eq!(resp["dropped"]["neighbors"], 0);
}

#[tokio::test]
async fn context_entity_ids_random_nonexistent_uuid_is_rejected() {
    // a syntactically valid but nonexistent UUID must
    // error, not silently vanish from the response.
    let pack = pack();
    let random_id = uuid::Uuid::new_v4().to_string();

    let err = pack
        .dispatch(
            "context",
            json!({"entity_ids": [random_id.clone()], "hops": 0}),
        )
        .await
        .expect_err("a nonexistent entity_ids UUID must be rejected");
    assert!(
        matches!(err, RuntimeError::NotFound(_)),
        "must be NotFound; got: {err:?}"
    );
    if let RuntimeError::NotFound(msg) = &err {
        assert!(
            msg.contains(&random_id),
            "error must name the offending id; got: {msg}"
        );
    }
}

#[tokio::test]
async fn context_entity_ids_note_uuid_is_rejected_as_non_entity() {
    // A note's UUID is syntactically a valid UUID but not an entity substrate;
    // it must be rejected, not silently dropped.
    let pack = pack();
    let note = pack
        .dispatch(
            "create",
            json!({"kind": "note", "content": "a plain observation", "note_kind": "observation"}),
        )
        .await
        .expect("create note");
    let note_id = note["id"].as_str().unwrap().to_string();

    let err = pack
        .dispatch(
            "context",
            json!({"entity_ids": [note_id.clone()], "hops": 0}),
        )
        .await
        .expect_err("a note UUID passed as entity_ids must be rejected");
    assert!(
        matches!(err, RuntimeError::NotFound(_)),
        "must be NotFound; got: {err:?}"
    );
    if let RuntimeError::NotFound(msg) = &err {
        assert!(
            msg.contains(&note_id),
            "error must name the offending id; got: {msg}"
        );
    }
}

#[tokio::test]
async fn context_entity_ids_edge_uuid_is_rejected_as_non_entity() {
    // An edge's UUID is syntactically a valid UUID but not an entity substrate;
    // it must be rejected, not silently dropped.
    let pack = pack();
    let a = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "CtxEdgeIdRejectA", "entity_kind": "concept"}),
        )
        .await
        .expect("create A");
    let b = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "CtxEdgeIdRejectB", "entity_kind": "concept"}),
        )
        .await
        .expect("create B");
    let a_id = a["id"].as_str().unwrap().to_string();
    let b_id = b["id"].as_str().unwrap().to_string();
    let link_resp = pack
        .dispatch(
            "link",
            json!({"source_id": a_id, "target_id": b_id, "relation": "extends"}),
        )
        .await
        .expect("link A-extends->B");
    let edge_id = link_resp["id"].as_str().unwrap().to_string();

    let err = pack
        .dispatch(
            "context",
            json!({"entity_ids": [edge_id.clone()], "hops": 0}),
        )
        .await
        .expect_err("an edge UUID passed as entity_ids must be rejected");
    assert!(
        matches!(err, RuntimeError::NotFound(_)),
        "must be NotFound; got: {err:?}"
    );
    if let RuntimeError::NotFound(msg) = &err {
        assert!(
            msg.contains(&edge_id),
            "error must name the offending id; got: {msg}"
        );
    }
}

#[tokio::test]
async fn context_entity_ids_unresolvable_prefix_is_rejected() {
    // A hex-looking prefix that matches nothing must error at resolution time
    // (existing resolve_uuid_async behavior), not fall through silently.
    let pack = pack();
    let err = pack
        .dispatch("context", json!({"entity_ids": ["deadbeef"], "hops": 0}))
        .await
        .expect_err("an unresolvable prefix must be rejected");
    assert!(is_invalid_input(&err), "must be InvalidInput; got: {err:?}");
}

#[tokio::test]
async fn context_entity_ids_unresolvable_name_is_rejected() {
    // A non-hex string that resolves through the name-lookup fallback and
    // matches nothing must error, not fall through silently.
    let pack = pack();
    let err = pack
        .dispatch(
            "context",
            json!({"entity_ids": ["NoSuchEntityCtxNameLookup"], "hops": 0}),
        )
        .await
        .expect_err("an unresolvable name must be rejected");
    assert!(
        matches!(err, RuntimeError::NotFound(_)),
        "must be NotFound; got: {err:?}"
    );
}

#[tokio::test]
async fn context_hop1_neighbor_carries_relation_direction_hop_and_null_via() {
    let pack = pack();
    let a = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "CtxHop1A", "entity_kind": "concept"}),
        )
        .await
        .expect("create A");
    let b = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "CtxHop1B", "entity_kind": "concept"}),
        )
        .await
        .expect("create B");
    let a_id = a["id"].as_str().unwrap().to_string();
    let b_id = b["id"].as_str().unwrap().to_string();

    pack.dispatch(
        "link",
        json!({"source_id": a_id, "target_id": b_id, "relation": "extends", "weight": 0.9}),
    )
    .await
    .expect("link A->B");

    let resp = pack
        .dispatch(
            "context",
            json!({"entity_ids": [a_id], "hops": 1, "direction": "outgoing"}),
        )
        .await
        .expect("context must succeed");

    let neighbors = resp["anchors"][0]["neighbors"].as_array().unwrap();
    assert_eq!(neighbors.len(), 1, "expected exactly one hop-1 neighbor");
    let n = &neighbors[0];
    assert_eq!(n["id"], b_id);
    assert_eq!(n["name"], "CtxHop1B");
    assert_eq!(n["relation"], "extends");
    assert_eq!(n["direction"], "outgoing");
    assert_eq!(n["hop"], 1);
    assert_eq!(n["via"], Value::Null);
    assert!((n["weight"].as_f64().unwrap() - 0.9).abs() < 1e-9);
}

#[tokio::test]
async fn context_default_direction_is_both_unlike_neighbors_outgoing_default() {
    let pack = pack();
    let a = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "CtxDirA", "entity_kind": "concept"}),
        )
        .await
        .expect("create A");
    let b = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "CtxDirB", "entity_kind": "concept"}),
        )
        .await
        .expect("create B");
    let a_id = a["id"].as_str().unwrap().to_string();
    let b_id = b["id"].as_str().unwrap().to_string();

    // B --extends--> A: from A's perspective this is an incoming edge only.
    pack.dispatch(
        "link",
        json!({"source_id": b_id, "target_id": a_id, "relation": "extends"}),
    )
    .await
    .expect("link B->A");

    let resp = pack
        .dispatch("context", json!({"entity_ids": [a_id]}))
        .await
        .expect("context with omitted direction must succeed");
    let neighbors = resp["anchors"][0]["neighbors"].as_array().unwrap();
    assert!(
        neighbors.iter().any(|n| n["id"] == b_id),
        "default direction=\"both\" must surface B via the incoming edge; got: {neighbors:?}"
    );
    let hit = neighbors.iter().find(|n| n["id"] == b_id).unwrap();
    assert_eq!(hit["direction"], "incoming");
}

#[tokio::test]
async fn context_hops_two_expands_second_hop_with_via_set_to_hop1_parent() {
    let pack = pack();
    let a = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "CtxChainA", "entity_kind": "concept"}),
        )
        .await
        .expect("create A");
    let b = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "CtxChainB", "entity_kind": "concept"}),
        )
        .await
        .expect("create B");
    let c = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "CtxChainC", "entity_kind": "concept"}),
        )
        .await
        .expect("create C");
    let a_id = a["id"].as_str().unwrap().to_string();
    let b_id = b["id"].as_str().unwrap().to_string();
    let c_id = c["id"].as_str().unwrap().to_string();

    pack.dispatch(
        "link",
        json!({"source_id": a_id, "target_id": b_id, "relation": "extends"}),
    )
    .await
    .expect("link A->B");
    pack.dispatch(
        "link",
        json!({"source_id": b_id, "target_id": c_id, "relation": "extends"}),
    )
    .await
    .expect("link B->C");

    let resp = pack
        .dispatch(
            "context",
            json!({"entity_ids": [a_id], "hops": 2, "direction": "outgoing"}),
        )
        .await
        .expect("context must succeed");

    let neighbors = resp["anchors"][0]["neighbors"].as_array().unwrap();
    let hop1: Vec<&Value> = neighbors.iter().filter(|n| n["hop"] == 1).collect();
    let hop2: Vec<&Value> = neighbors.iter().filter(|n| n["hop"] == 2).collect();
    assert_eq!(hop1.len(), 1, "expected B as the sole hop-1 neighbor");
    assert_eq!(hop1[0]["id"], b_id);
    assert_eq!(hop2.len(), 1, "expected C as the sole hop-2 neighbor");
    assert_eq!(hop2[0]["id"], c_id);
    assert_eq!(
        hop2[0]["via"], b_id,
        "hop-2 record must carry via = hop-1 parent id"
    );

    // Deterministic order: hop-1 records precede hop-2 records for the anchor.
    let hop_values: Vec<i64> = neighbors
        .iter()
        .map(|n| n["hop"].as_i64().unwrap())
        .collect();
    assert_eq!(
        hop_values,
        vec![1, 2],
        "hop-1 must precede hop-2 in output order"
    );
}

#[tokio::test]
async fn context_hops_zero_does_not_expand_even_with_edges_present() {
    let pack = pack();
    let a = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "CtxHop0A", "entity_kind": "concept"}),
        )
        .await
        .expect("create A");
    let b = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "CtxHop0B", "entity_kind": "concept"}),
        )
        .await
        .expect("create B");
    let a_id = a["id"].as_str().unwrap().to_string();
    let b_id = b["id"].as_str().unwrap().to_string();
    pack.dispatch(
        "link",
        json!({"source_id": a_id, "target_id": b_id, "relation": "extends"}),
    )
    .await
    .expect("link A->B");

    let resp = pack
        .dispatch("context", json!({"entity_ids": [a_id], "hops": 0}))
        .await
        .expect("context must succeed");
    assert_eq!(resp["anchors"][0]["neighbors"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn context_dedup_across_anchors_neighbor_appears_once_under_first_anchor() {
    let pack = pack();
    let a = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "CtxDedupA", "entity_kind": "concept"}),
        )
        .await
        .expect("create A");
    let b = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "CtxDedupB", "entity_kind": "concept"}),
        )
        .await
        .expect("create B");
    let shared = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "CtxDedupShared", "entity_kind": "concept"}),
        )
        .await
        .expect("create Shared");
    let a_id = a["id"].as_str().unwrap().to_string();
    let b_id = b["id"].as_str().unwrap().to_string();
    let shared_id = shared["id"].as_str().unwrap().to_string();

    pack.dispatch(
        "link",
        json!({"source_id": a_id, "target_id": shared_id, "relation": "extends"}),
    )
    .await
    .expect("link A->Shared");
    pack.dispatch(
        "link",
        json!({"source_id": b_id, "target_id": shared_id, "relation": "extends"}),
    )
    .await
    .expect("link B->Shared");

    // a_id listed first in entity_ids => selection order puts A before B.
    let resp = pack
        .dispatch(
            "context",
            json!({"entity_ids": [a_id, b_id], "hops": 1, "direction": "outgoing"}),
        )
        .await
        .expect("context must succeed");

    let anchors = resp["anchors"].as_array().unwrap();
    assert_eq!(anchors.len(), 2);
    let a_neighbors = anchors[0]["neighbors"].as_array().unwrap();
    let b_neighbors = anchors[1]["neighbors"].as_array().unwrap();
    assert!(
        a_neighbors.iter().any(|n| n["id"] == shared_id),
        "Shared must appear under the first anchor (A)"
    );
    assert!(
        !b_neighbors.iter().any(|n| n["id"] == shared_id),
        "Shared must NOT be repeated under the second anchor (B) — global visited-set dedup"
    );
}

#[tokio::test]
async fn context_anchor_never_relisted_as_neighbor_of_another_anchor() {
    let pack = pack();
    let a = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "CtxSelfA", "entity_kind": "concept"}),
        )
        .await
        .expect("create A");
    let b = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "CtxSelfB", "entity_kind": "concept"}),
        )
        .await
        .expect("create B");
    let a_id = a["id"].as_str().unwrap().to_string();
    let b_id = b["id"].as_str().unwrap().to_string();

    // A --extends--> B, and B is ALSO an explicit anchor.
    pack.dispatch(
        "link",
        json!({"source_id": a_id, "target_id": b_id, "relation": "extends"}),
    )
    .await
    .expect("link A->B");

    let resp = pack
        .dispatch(
            "context",
            json!({"entity_ids": [a_id, b_id], "hops": 1, "direction": "outgoing"}),
        )
        .await
        .expect("context must succeed");

    let a_neighbors = resp["anchors"][0]["neighbors"].as_array().unwrap();
    assert!(
        !a_neighbors.iter().any(|n| n["id"] == b_id),
        "B is already a top-level anchor and must not be duplicated inside A's neighbor list; got {a_neighbors:?}"
    );
}

#[tokio::test]
async fn context_explicit_entity_ids_never_clamped_by_limit() {
    let pack = pack();
    let mut ids = Vec::new();
    for i in 0..7 {
        let e = pack
            .dispatch(
                "create",
                json!({"kind": "entity", "name": format!("CtxManyAnchor{i}"), "entity_kind": "concept"}),
            )
            .await
            .expect("create entity");
        ids.push(e["id"].as_str().unwrap().to_string());
    }

    // limit=1 (its floor-adjacent clamp) must not truncate explicit entity_ids.
    let resp = pack
        .dispatch(
            "context",
            json!({"entity_ids": ids.clone(), "limit": 1, "hops": 0}),
        )
        .await
        .expect("context must succeed");
    assert_eq!(
        resp["anchors"].as_array().unwrap().len(),
        7,
        "all 7 explicit entity_ids must be honored regardless of `limit`"
    );
}

#[tokio::test]
async fn context_query_selects_anchors_via_hybrid_search() {
    let pack = pack();
    pack.dispatch(
        "create",
        json!({"kind": "entity", "name": "CtxQueryRoPE", "entity_kind": "concept", "description": "rotary position embedding"}),
    )
    .await
    .expect("create RoPE");
    pack.dispatch(
        "create",
        json!({"kind": "entity", "name": "CtxQueryUnrelated", "entity_kind": "concept", "description": "completely different topic"}),
    )
    .await
    .expect("create unrelated");

    let resp = pack
        .dispatch(
            "context",
            json!({"query": "CtxQueryRoPE rotary position embedding", "hops": 0}),
        )
        .await
        .expect("context must succeed");
    let anchors = resp["anchors"].as_array().unwrap();
    assert!(
        !anchors.is_empty(),
        "query-based anchor selection must return at least one anchor"
    );
    assert!(
        anchors
            .iter()
            .any(|a| a["entity"]["name"] == "CtxQueryRoPE"),
        "expected the matching entity among query-selected anchors; got: {anchors:?}"
    );
}

#[tokio::test]
async fn context_query_and_entity_ids_combine_explicit_ids_first_then_search_fills() {
    let pack = pack();
    let explicit = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "CtxComboExplicit", "entity_kind": "concept", "description": "explicit anchor"}),
        )
        .await
        .expect("create explicit");
    let explicit_id = explicit["id"].as_str().unwrap().to_string();
    pack.dispatch(
        "create",
        json!({"kind": "entity", "name": "CtxComboSearchHit", "entity_kind": "concept", "description": "distinctive combo search text"}),
    )
    .await
    .expect("create search hit");

    let resp = pack
        .dispatch(
            "context",
            json!({
                "entity_ids": [explicit_id.clone()],
                "query": "CtxComboSearchHit distinctive combo search text",
                "hops": 0,
            }),
        )
        .await
        .expect("context must succeed");

    let anchors = resp["anchors"].as_array().unwrap();
    assert_eq!(
        anchors[0]["entity"]["id"], explicit_id,
        "explicit entity_ids must come first in selection order"
    );
    assert!(
        anchors
            .iter()
            .any(|a| a["entity"]["name"] == "CtxComboSearchHit"),
        "query must contribute additional anchors after explicit ids; got: {anchors:?}"
    );
}

#[tokio::test]
async fn context_query_fill_reaches_limit_after_top_hit_duplicates_explicit_anchor() {
    // if the query's top hit is also an explicit anchor,
    // the query leg must still fill up to `limit` DISTINCT non-explicit anchors
    // rather than silently returning fewer once the duplicate collapses.
    let pack = pack();
    let explicit = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "CtxFillDupExplicit", "entity_kind": "concept", "description": "ctxfilldup shared anchor text"}),
        )
        .await
        .expect("create explicit");
    let explicit_id = explicit["id"].as_str().unwrap().to_string();

    // A second entity with the SAME distinctive text so it ranks as the top query
    // hit alongside (or above) the explicit anchor.
    let extra = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "CtxFillDupExtra", "entity_kind": "concept", "description": "ctxfilldup shared anchor text"}),
        )
        .await
        .expect("create extra");
    let extra_id = extra["id"].as_str().unwrap().to_string();

    let resp = pack
        .dispatch(
            "context",
            json!({
                "entity_ids": [explicit_id.clone()],
                "query": "ctxfilldup shared anchor text",
                "limit": 1,
                "hops": 0,
            }),
        )
        .await
        .expect("context must succeed");

    let anchors = resp["anchors"].as_array().unwrap();
    let anchor_ids: Vec<&str> = anchors
        .iter()
        .map(|a| a["entity"]["id"].as_str().unwrap())
        .collect();
    assert!(
        anchor_ids.contains(&explicit_id.as_str()),
        "explicit anchor must always be present; got: {anchor_ids:?}"
    );
    assert!(
        anchor_ids.contains(&extra_id.as_str()),
        "query leg must still deliver 1 distinct non-explicit anchor when its \
         top hit collapses into the explicit id; got: {anchor_ids:?}"
    );
    assert_eq!(
        anchors.len(),
        2,
        "explicit anchor + exactly `limit`=1 distinct query anchor expected"
    );
}

#[tokio::test]
async fn context_relations_filter_restricts_expansion() {
    let pack = pack();
    let a = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "CtxRelFilterA", "entity_kind": "concept"}),
        )
        .await
        .expect("create A");
    let b = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "CtxRelFilterB", "entity_kind": "concept"}),
        )
        .await
        .expect("create B");
    let c = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "CtxRelFilterC", "entity_kind": "concept"}),
        )
        .await
        .expect("create C");
    let a_id = a["id"].as_str().unwrap().to_string();
    let b_id = b["id"].as_str().unwrap().to_string();
    let c_id = c["id"].as_str().unwrap().to_string();

    pack.dispatch(
        "link",
        json!({"source_id": a_id, "target_id": b_id, "relation": "extends"}),
    )
    .await
    .expect("link A-extends->B");
    pack.dispatch(
        "link",
        json!({"source_id": a_id, "target_id": c_id, "relation": "part_of"}),
    )
    .await
    .expect("link A-part_of->C");

    let resp = pack
        .dispatch(
            "context",
            json!({"entity_ids": [a_id], "hops": 1, "direction": "outgoing", "relations": ["extends"]}),
        )
        .await
        .expect("context must succeed");
    let neighbors = resp["anchors"][0]["neighbors"].as_array().unwrap();
    assert_eq!(
        neighbors.len(),
        1,
        "only the extends-filtered neighbor must appear"
    );
    assert_eq!(neighbors[0]["id"], b_id);
}

#[tokio::test]
async fn context_symmetric_relation_direction_is_reported_as_both() {
    let pack = pack();
    let a = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "CtxSymA", "entity_kind": "concept"}),
        )
        .await
        .expect("create A");
    let b = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "CtxSymB", "entity_kind": "concept"}),
        )
        .await
        .expect("create B");
    let a_id = a["id"].as_str().unwrap().to_string();
    let b_id = b["id"].as_str().unwrap().to_string();

    pack.dispatch(
        "link",
        json!({"source_id": a_id, "target_id": b_id, "relation": "competes_with"}),
    )
    .await
    .expect("link A competes_with B");

    let resp = pack
        .dispatch(
            "context",
            json!({"entity_ids": [a_id], "hops": 1, "direction": "outgoing", "relations": ["competes_with"]}),
        )
        .await
        .expect("context must succeed");
    let neighbors = resp["anchors"][0]["neighbors"].as_array().unwrap();
    assert_eq!(neighbors.len(), 1);
    assert_eq!(
        neighbors[0]["direction"], "both",
        "symmetric relations have no directionality and must be tagged \"both\" \
         regardless of the requested direction"
    );
}

#[tokio::test]
async fn context_fanout_caps_neighbors_per_node_per_hop() {
    let pack = pack();
    let a = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "CtxFanoutA", "entity_kind": "concept"}),
        )
        .await
        .expect("create A");
    let a_id = a["id"].as_str().unwrap().to_string();

    for i in 0..5 {
        let n = pack
            .dispatch(
                "create",
                json!({"kind": "entity", "name": format!("CtxFanoutN{i}"), "entity_kind": "concept"}),
            )
            .await
            .expect("create neighbor");
        let n_id = n["id"].as_str().unwrap().to_string();
        pack.dispatch(
            "link",
            json!({"source_id": a_id, "target_id": n_id, "relation": "extends", "weight": 0.1 + (i as f64) * 0.1}),
        )
        .await
        .expect("link A->neighbor");
    }

    let resp = pack
        .dispatch(
            "context",
            json!({"entity_ids": [a_id], "hops": 1, "direction": "outgoing", "fanout": 2}),
        )
        .await
        .expect("context must succeed");
    let neighbors = resp["anchors"][0]["neighbors"].as_array().unwrap();
    assert_eq!(
        neighbors.len(),
        2,
        "fanout=2 must cap hop-1 neighbors at 2 even though 5 edges exist"
    );
}

#[tokio::test]
async fn context_direction_outgoing_fanout_keeps_highest_weight_not_node_id_order() {
    // khive-runtime's neighbors_with_query re-sorts hits by
    // (node_id, edge_id) for dedup and, before this fix, never restored the
    // weight-descending order the storage layer established. That meant
    // context(direction="outgoing") returned neighbors in arbitrary node_id
    // order instead of ADR-089's weight-descending, UUID-ascending-tiebreak
    // contract, so a narrowing `fanout` could keep a low-weight neighbor over
    // a high-weight one purely because its UUID sorted first.
    //
    // Node UUIDs are random v4 and uncorrelated with weight, so if ordering
    // regresses to node_id order, the top-`fanout` set (and its relative
    // order) will not match weight-descending with overwhelming probability.
    let pack = pack();
    let a = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "CtxOrderA", "entity_kind": "concept"}),
        )
        .await
        .expect("create A");
    let a_id = a["id"].as_str().unwrap().to_string();

    let mut created = Vec::new();
    for i in 0..5 {
        let n = pack
            .dispatch(
                "create",
                json!({"kind": "entity", "name": format!("CtxOrderN{i}"), "entity_kind": "concept"}),
            )
            .await
            .expect("create neighbor");
        let n_id = n["id"].as_str().unwrap().to_string();
        let weight = 0.1 + (i as f64) * 0.1;
        pack.dispatch(
            "link",
            json!({"source_id": a_id, "target_id": n_id, "relation": "extends", "weight": weight}),
        )
        .await
        .expect("link A->neighbor");
        created.push((n_id, weight));
    }
    // Highest two weights: N4 (0.5) and N3 (0.4).
    let highest_id = created[4].0.clone();
    let second_id = created[3].0.clone();
    let lowest_id = created[0].0.clone();

    let resp = pack
        .dispatch(
            "context",
            json!({"entity_ids": [a_id], "hops": 1, "direction": "outgoing", "fanout": 2}),
        )
        .await
        .expect("context must succeed");
    let neighbors = resp["anchors"][0]["neighbors"].as_array().unwrap();
    assert_eq!(neighbors.len(), 2, "fanout=2 must cap at 2 neighbors");

    let ids: Vec<&str> = neighbors
        .iter()
        .map(|n| n["id"].as_str().unwrap())
        .collect();
    assert!(
        !ids.contains(&lowest_id.as_str()),
        "lowest-weight neighbor must be dropped by fanout, not survive via node_id order; got {ids:?}"
    );
    assert_eq!(
        ids[0], highest_id,
        "highest-weight neighbor must appear first (weight-descending contract); got {ids:?}"
    );
    assert_eq!(
        ids[1], second_id,
        "second-highest-weight neighbor must appear second; got {ids:?}"
    );
    let weights: Vec<f64> = neighbors
        .iter()
        .map(|n| n["weight"].as_f64().unwrap())
        .collect();
    assert!(
        weights[0] > weights[1],
        "neighbor weights must be strictly descending; got {weights:?}"
    );
}

#[tokio::test]
async fn context_both_direction_mixed_weights_interleave_in_global_order() {
    // ADR-089 context-verb optimization (single UNION ALL query for
    // direction="both" expand, replacing two separate direction-scoped
    // calls): outgoing and incoming edge weights interleave here so the
    // assertion proves global post-union ordering (weight DESC, node_id ASC
    // tiebreak) rather than "all outgoing, then all incoming" branch order.
    let pack = pack();
    let a = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "CtxBothA", "entity_kind": "concept"}),
        )
        .await
        .expect("create A");
    let a_id = a["id"].as_str().unwrap().to_string();

    let mut out_ids = Vec::new();
    for (i, w) in [0.9, 0.5].into_iter().enumerate() {
        let n = pack
            .dispatch(
                "create",
                json!({"kind": "entity", "name": format!("CtxBothOut{i}"), "entity_kind": "concept"}),
            )
            .await
            .expect("create outgoing neighbor");
        let n_id = n["id"].as_str().unwrap().to_string();
        pack.dispatch(
            "link",
            json!({"source_id": a_id, "target_id": n_id, "relation": "extends", "weight": w}),
        )
        .await
        .expect("link A->neighbor");
        out_ids.push((n_id, w));
    }

    let mut in_ids = Vec::new();
    for (i, w) in [0.7, 0.2].into_iter().enumerate() {
        let n = pack
            .dispatch(
                "create",
                json!({"kind": "entity", "name": format!("CtxBothIn{i}"), "entity_kind": "concept"}),
            )
            .await
            .expect("create incoming neighbor");
        let n_id = n["id"].as_str().unwrap().to_string();
        pack.dispatch(
            "link",
            json!({"source_id": n_id, "target_id": a_id, "relation": "extends", "weight": w}),
        )
        .await
        .expect("link neighbor->A");
        in_ids.push((n_id, w));
    }

    // Expected global order by weight DESC: 0.9(out), 0.7(in), 0.5(out), 0.2(in).
    let expected_order = vec![
        (out_ids[0].0.clone(), "outgoing"),
        (in_ids[0].0.clone(), "incoming"),
        (out_ids[1].0.clone(), "outgoing"),
        (in_ids[1].0.clone(), "incoming"),
    ];

    let resp = pack
        .dispatch(
            "context",
            json!({"entity_ids": [a_id], "hops": 1, "direction": "both", "fanout": 10}),
        )
        .await
        .expect("context must succeed");
    let neighbors = resp["anchors"][0]["neighbors"].as_array().unwrap();
    assert_eq!(neighbors.len(), 4, "all four neighbors must be present");

    let got: Vec<(String, String)> = neighbors
        .iter()
        .map(|n| {
            (
                n["id"].as_str().unwrap().to_string(),
                n["direction"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let expected: Vec<(String, String)> = expected_order
        .into_iter()
        .map(|(id, dir)| (id, dir.to_string()))
        .collect();
    assert_eq!(
        got, expected,
        "direction=\"both\" must interleave outgoing/incoming by global weight DESC order, \
         with each hit's real direction label preserved"
    );

    // Tight fanout must keep the top-K by GLOBAL weight, not per-direction top-K.
    let resp_capped = pack
        .dispatch(
            "context",
            json!({"entity_ids": [a_id], "hops": 1, "direction": "both", "fanout": 2}),
        )
        .await
        .expect("context with fanout=2 must succeed");
    let capped = resp_capped["anchors"][0]["neighbors"].as_array().unwrap();
    assert_eq!(
        capped.len(),
        2,
        "fanout=2 must cap at 2 across both directions"
    );
    assert_eq!(
        capped[0]["id"], out_ids[0].0,
        "highest-weight (outgoing) neighbor survives"
    );
    assert_eq!(
        capped[1]["id"], in_ids[0].0,
        "second-highest-weight (incoming) neighbor survives"
    );
    assert_eq!(capped[0]["direction"], "outgoing");
    assert_eq!(capped[1]["direction"], "incoming");
}

#[tokio::test]
async fn context_budget_truncation_sets_flag_and_dropped_counts() {
    let pack = pack();
    let a = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "CtxBudgetA", "entity_kind": "concept", "description": "anchor with several neighbors"}),
        )
        .await
        .expect("create A");
    let a_id = a["id"].as_str().unwrap().to_string();

    for i in 0..5 {
        let n = pack
            .dispatch(
                "create",
                json!({"kind": "entity", "name": format!("CtxBudgetN{i}"), "entity_kind": "concept", "description": "a neighbor entity with enough text to consume budget"}),
            )
            .await
            .expect("create neighbor");
        let n_id = n["id"].as_str().unwrap().to_string();
        pack.dispatch(
            "link",
            json!({"source_id": a_id, "target_id": n_id, "relation": "extends"}),
        )
        .await
        .expect("link A->neighbor");
    }

    // Small budget: only the anchor entity (and maybe a neighbor or two) fit.
    let resp = pack
        .dispatch(
            "context",
            json!({"entity_ids": [a_id], "hops": 1, "direction": "outgoing", "budget": 256}),
        )
        .await
        .expect("context must succeed");

    assert_eq!(
        resp["truncated"], true,
        "a 256-char budget must truncate 5 verbose neighbors"
    );
    let dropped_neighbors = resp["dropped"]["neighbors"].as_i64().unwrap();
    assert!(
        dropped_neighbors > 0,
        "expected at least one dropped neighbor; got dropped={:?}",
        resp["dropped"]
    );
}

#[tokio::test]
async fn context_ample_budget_reports_no_truncation() {
    let pack = pack();
    let a = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "CtxAmpleA", "entity_kind": "concept"}),
        )
        .await
        .expect("create A");
    let a_id = a["id"].as_str().unwrap().to_string();
    let b = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "CtxAmpleB", "entity_kind": "concept"}),
        )
        .await
        .expect("create B");
    let b_id = b["id"].as_str().unwrap().to_string();
    pack.dispatch(
        "link",
        json!({"source_id": a_id, "target_id": b_id, "relation": "extends"}),
    )
    .await
    .expect("link A->B");

    let resp = pack
        .dispatch(
            "context",
            json!({"entity_ids": [a_id], "hops": 1, "budget": 65536}),
        )
        .await
        .expect("context must succeed");
    assert_eq!(resp["truncated"], false);
    assert_eq!(resp["dropped"]["anchors"], 0);
    assert_eq!(resp["dropped"]["neighbors"], 0);
}

#[tokio::test]
async fn context_out_of_range_params_are_clamped_not_rejected() {
    let pack = pack();
    let a = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "CtxClampA", "entity_kind": "concept"}),
        )
        .await
        .expect("create A");
    let a_id = a["id"].as_str().unwrap().to_string();

    // hops=99 clamps to 2; budget=1 clamps to 256; fanout=0 clamps to 1;
    // limit=0 clamps to 1 — none of these should error.
    let resp = pack
        .dispatch(
            "context",
            json!({"entity_ids": [a_id], "hops": 99, "budget": 1, "fanout": 0, "limit": 0}),
        )
        .await
        .expect("out-of-range params must be clamped, not rejected");
    assert!(resp["anchors"].is_array());
}

#[tokio::test]
async fn context_unknown_relation_in_filter_is_rejected() {
    let pack = pack();
    let a = pack
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "CtxBadRelA", "entity_kind": "concept"}),
        )
        .await
        .expect("create A");
    let a_id = a["id"].as_str().unwrap().to_string();

    let err = pack
        .dispatch(
            "context",
            json!({"entity_ids": [a_id], "relations": ["not_a_real_relation"]}),
        )
        .await
        .expect_err("unknown relation in filter must be rejected");
    assert!(is_invalid_input(&err), "must be InvalidInput; got: {err:?}");
}

#[tokio::test]
async fn context_verb_is_registered_in_kg_handlers() {
    let pack = pack();
    let names: Vec<&str> = pack.verbs().iter().map(|h| h.name).collect();
    assert!(
        names.contains(&"context"),
        "context must be a registered KgPack verb"
    );
}

// ---- #631: link() endpoint resolution must match get()'s by-ID contract ----
//
// Two divergences between `link`'s endpoint resolution and `get`'s by-ID
// resolution, both surfaced on the documented
// `remember(...) | link(source_id=$prev.id, target_id=<old-uuid>, relation="supersedes")`
// chain from a namespace-stamped caller:
//
// 1. Short-prefix resolution: `link`'s source_id/target_id prefix form used
//    `resolve_uuid_async` (namespace-scoped `resolve_prefix`), while `get`
//    already used the unfiltered `resolve_uuid_unfiltered` (#391 §3). A short
//    prefix of a record stamped outside the caller's own namespace resolved
//    for `get` but not for `link`.
// 2. Endpoint existence validation: `validate_edge_relation_endpoints` checked
//    each endpoint via `resolve_primary`, which requires
//    `record.namespace == token.namespace()`. Per ADR-007 Rule 2, by-ID ops
//    are namespace-agnostic — `link` consumes two by-ID endpoints, so its
//    existence check must follow the same contract as `get()`, not re-add a
//    namespace equality check the by-ID contract already forbids.
//
// Both are fixed by resolving link endpoints through the same namespace-
// agnostic path get() uses (`resolve_uuid_unfiltered` for the string form,
// `resolve_edge_endpoint`/`substrate_exists_by_id` for existence). These
// tests lock in both fixes independently.

/// `link` by short prefix, with the caller in a DIFFERENT namespace than both
/// endpoints, must resolve like `get()` does (was
/// `InvalidInput("no record matches prefix...")` pre-fix).
#[tokio::test]
async fn link_by_short_prefix_cross_namespace_endpoints_resolves() {
    let f = pack();

    let a_full = f
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "Link631PrefixA", "entity_kind": "concept"}),
        )
        .await
        .expect("create A must succeed")["id"]
        .as_str()
        .unwrap()
        .to_string();
    let b_full = f
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "Link631PrefixB", "entity_kind": "concept"}),
        )
        .await
        .expect("create B must succeed")["id"]
        .as_str()
        .unwrap()
        .to_string();

    let a_prefix = &a_full[..8];
    let b_prefix = &b_full[..8];

    // Both entities live in "local"; the caller acts from a different namespace.
    let result = f
        .dispatch(
            "link",
            json!({
                "source_id": a_prefix,
                "target_id": b_prefix,
                "relation": "extends",
                "namespace": "ns-caller",
            }),
        )
        .await;
    assert!(
        result.is_ok(),
        "link by short prefix from a different namespace must resolve like get() (#631); got: {result:?}"
    );
    let edge = result.unwrap();
    assert_eq!(
        edge.get("source_id").and_then(Value::as_str),
        Some(a_full.as_str())
    );
    assert_eq!(
        edge.get("target_id").and_then(Value::as_str),
        Some(b_full.as_str())
    );
}

/// `link` between two entities in a namespace OTHER than the caller's own
/// namespace must succeed via full UUIDs — the by-ID endpoint existence check
/// is namespace-agnostic (ADR-007 Rule 2), not scoped to the caller's primary
/// namespace (was `NotFound("link source/target ... not found in namespace")`
/// pre-fix).
#[tokio::test]
async fn link_entities_cross_namespace_endpoints_succeeds() {
    let f = pack();

    let a_full = f
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "Link631EntA", "entity_kind": "concept"}),
        )
        .await
        .expect("create A must succeed")["id"]
        .as_str()
        .unwrap()
        .to_string();
    let b_full = f
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "Link631EntB", "entity_kind": "concept"}),
        )
        .await
        .expect("create B must succeed")["id"]
        .as_str()
        .unwrap()
        .to_string();

    let result = f
        .dispatch(
            "link",
            json!({
                "source_id": a_full,
                "target_id": b_full,
                "relation": "extends",
                "namespace": "ns-caller",
            }),
        )
        .await;
    assert!(
        result.is_ok(),
        "link between records in a different namespace than the caller must succeed (#631); got: {result:?}"
    );
}

/// The canonical `remember | supersedes` chain, modeled as note-to-note
/// `supersedes`: linking a note that supersedes another note must succeed
/// when the caller's namespace differs from both notes' stored namespace.
/// Pre-fix this hit the same `resolve_primary` endpoint-existence check as
/// entity-to-entity links and failed with
/// `NotFound("link source/target ... not found in namespace")`.
#[tokio::test]
async fn link_supersedes_notes_cross_namespace_succeeds() {
    let f = pack();

    let old_note = f
        .dispatch(
            "create",
            json!({"kind": "observation", "content": "old memory content"}),
        )
        .await
        .expect("create old note must succeed")["id"]
        .as_str()
        .unwrap()
        .to_string();
    let new_note = f
        .dispatch(
            "create",
            json!({"kind": "observation", "content": "corrected memory content"}),
        )
        .await
        .expect("create new note must succeed")["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Both notes live in "local"; the caller supersedes from a different namespace,
    // mirroring an actor-namespaced memory.remember() | link(relation="supersedes") caller.
    let result = f
        .dispatch(
            "link",
            json!({
                "source_id": new_note,
                "target_id": old_note,
                "relation": "supersedes",
                "namespace": "ns-caller",
            }),
        )
        .await;
    assert!(
        result.is_ok(),
        "supersedes link between notes in a different namespace than the caller must succeed (#631); got: {result:?}"
    );
    let edge = result.unwrap();
    assert_eq!(
        edge.get("relation").and_then(Value::as_str),
        Some("supersedes")
    );
    assert_eq!(
        edge.get("source_id").and_then(Value::as_str),
        Some(new_note.as_str())
    );
    assert_eq!(
        edge.get("target_id").and_then(Value::as_str),
        Some(old_note.as_str())
    );
}

// ---- #638 ----

/// `create(kind="note", annotates=[...])` must resolve a short hex prefix of an
/// `annotates` target the same way `get()` resolves it, even when the caller
/// is in a different namespace than the target. Pre-fix,
/// `create.rs`'s `annotates[]` handling used `resolve_uuid_async`, whose
/// prefix branch is namespace-scoped (`resolve_prefix`), so a short prefix to
/// a foreign-namespace target failed with
/// `InvalidInput("no record matches prefix...")` before the runtime-side
/// `substrate_exists_by_id` fix ever saw a UUID.
#[tokio::test]
async fn create_note_annotates_by_short_prefix_cross_namespace_succeeds() {
    let f = pack();

    let target_full = f
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "Create638AnnotTarget", "entity_kind": "concept"}),
        )
        .await
        .expect("create target entity must succeed")["id"]
        .as_str()
        .unwrap()
        .to_string();
    let target_prefix = &target_full[..8];

    // The target lives in "local"; the caller creates the annotating note from a
    // different namespace.
    let result = f
        .dispatch(
            "create",
            json!({
                "kind": "note",
                "content": "annotates via short prefix cross-namespace",
                "annotates": [target_prefix],
                "namespace": "ns-caller",
            }),
        )
        .await;
    assert!(
        result.is_ok(),
        "create note annotates by short prefix from a different namespace must succeed (#638); got: {result:?}"
    );
}

/// `create(kind=entity, edges=[{target_id, relation}])` must resolve a short
/// hex prefix of an edge target the same way `get()` resolves it, even when
/// the caller is in a different namespace than the target (the second call
/// site for this fix, `create.rs:427`).
#[tokio::test]
async fn create_entity_with_edges_target_by_short_prefix_cross_namespace_succeeds() {
    let f = pack();

    let target_full = f
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "Create638EdgeTarget", "entity_kind": "concept"}),
        )
        .await
        .expect("create target entity must succeed")["id"]
        .as_str()
        .unwrap()
        .to_string();
    let target_prefix = &target_full[..8];

    let created = f
        .dispatch(
            "create",
            json!({
                "kind": "entity",
                "name": "Create638EdgeSource",
                "entity_kind": "concept",
                "edges": [{"target_id": target_prefix, "relation": "extends"}],
                "namespace": "ns-caller",
            }),
        )
        .await
        .expect("create with edges must succeed");

    // No per-edge error should report the target as unresolved, and the edge
    // must actually have been created.
    assert!(
        created.get("edge_errors").is_none(),
        "edges[].target_id by short prefix from a different namespace must resolve (#638); got: {created:?}"
    );
    let edges = created
        .get("edges")
        .and_then(Value::as_array)
        .expect("response must include edges[]");
    assert_eq!(
        edges.len(),
        1,
        "expected exactly one created edge; got: {created:?}"
    );
}

/// Cross-namespace `depends_on` link between two `project` entities must still
/// infer `dependency_kind = "build"` (ADR-002 §inference default), even though
/// endpoint validation now allows the target to live outside the caller's
/// namespace (#638). Pre-fix, `link`'s `dependency_kind`
/// inference used the visible-set-scoped `resolve`, which silently dropped
/// the inferred metadata for an endpoint outside the caller's visible set —
/// the edge was created, but without the ADR-002 default.
#[tokio::test]
async fn link_depends_on_project_to_project_cross_namespace_infers_dependency_kind() {
    let f = pack();

    let a_full = f
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "Link638DependsOnA", "entity_kind": "project"}),
        )
        .await
        .expect("create A must succeed")["id"]
        .as_str()
        .unwrap()
        .to_string();
    let b_full = f
        .dispatch(
            "create",
            json!({"kind": "entity", "name": "Link638DependsOnB", "entity_kind": "project"}),
        )
        .await
        .expect("create B must succeed")["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Both projects live in "local"; the caller links from a different namespace.
    let result = f
        .dispatch(
            "link",
            json!({
                "source_id": a_full,
                "target_id": b_full,
                "relation": "depends_on",
                "namespace": "ns-caller",
            }),
        )
        .await;
    assert!(
        result.is_ok(),
        "cross-namespace project->project depends_on link must succeed (#631); got: {result:?}"
    );
    let edge = result.unwrap();
    assert_eq!(
        edge.get("metadata")
            .and_then(|m| m.get("dependency_kind"))
            .and_then(Value::as_str),
        Some("build"),
        "cross-namespace depends_on must still infer dependency_kind=build (#638); got edge: {edge:?}"
    );
}

// ---------------------------------------------------------------------------
// #701 / #702: edge listing pagination + per-relation stats
// ---------------------------------------------------------------------------

/// Create a `concept` entity via the `create` verb and return its UUID.
async fn create_concept(f: &Fixture, name: &str) -> String {
    f.dispatch("create", json!({"kind": "concept", "name": name}))
        .await
        .expect("create must succeed")["id"]
        .as_str()
        .unwrap()
        .to_string()
}

/// #701 regression, exercised through the `list` verb: `offset` must page
/// through the full edge set instead of returning the identical first page
/// every time, and an out-of-range offset must return an empty array.
#[tokio::test]
async fn list_kind_edge_offset_pages_through_full_set() {
    let f = pack();
    let a = create_concept(&f, "Offset701A").await;
    for i in 0..5 {
        let t = create_concept(&f, &format!("Offset701T{i}")).await;
        f.dispatch(
            "link",
            json!({"source_id": a, "target_id": t, "relation": "extends"}),
        )
        .await
        .expect("link must succeed");
    }

    async fn list_page(f: &Fixture, a: &str, offset: u64) -> Vec<Value> {
        f.dispatch(
            "list",
            json!({"kind": "edge", "source_id": a, "relations": ["extends"], "limit": 2, "offset": offset}),
        )
        .await
        .expect("list(kind=edge) must succeed")
        .as_array()
        .expect("bare array response")
        .clone()
    }

    let page0 = list_page(&f, &a, 0).await;
    let page1 = list_page(&f, &a, 2).await;
    let page2 = list_page(&f, &a, 4).await;
    assert_eq!(page0.len(), 2);
    assert_eq!(page1.len(), 2);
    assert_eq!(page2.len(), 1);
    assert_ne!(page0, page1, "page 2 must differ from page 1 (#701)");

    let ids = |p: &[Value]| {
        p.iter()
            .map(|e| e["id"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    let mut all_ids = ids(&page0);
    all_ids.extend(ids(&page1));
    all_ids.extend(ids(&page2));
    all_ids.sort();
    all_ids.dedup();
    assert_eq!(all_ids.len(), 5, "pages must tile the full edge set");

    let empty = list_page(&f, &a, 100).await;
    assert!(
        empty.is_empty(),
        "offset past the end must return empty, not page 1"
    );
}

/// #702.2: `list(kind=edge, after=<cursor>)` walks the full edge set via a
/// keyset cursor and reports `next_after` until exhausted.
#[tokio::test]
async fn list_kind_edge_after_cursor_tiles_full_set() {
    let f = pack();
    let a = create_concept(&f, "Cursor702A").await;
    for i in 0..5 {
        let t = create_concept(&f, &format!("Cursor702T{i}")).await;
        f.dispatch(
            "link",
            json!({"source_id": a, "target_id": t, "relation": "extends"}),
        )
        .await
        .expect("link must succeed");
    }

    let mut seen = Vec::new();
    // Empty string opts into cursor-mode pagination starting from the
    // beginning of the set (no prior page).
    let mut after = String::new();
    for _ in 0..20 {
        let args = json!({"kind": "edge", "source_id": a, "relations": ["extends"], "limit": 2, "after": after});
        let result = f.dispatch("list", args).await.expect("list must succeed");
        // Cursor-mode responses are an object, not a bare array (#702.2) —
        // distinguishable at the JSON-shape level so plain offset callers
        // are unaffected.
        let obj = result
            .as_object()
            .expect("cursor-mode response is an object");
        let edges = obj["edges"].as_array().expect("edges must be an array");
        if edges.is_empty() {
            break;
        }
        seen.extend(edges.iter().map(|e| e["id"].as_str().unwrap().to_string()));
        match obj["next_after"].as_str() {
            Some(next) => after = next.to_string(),
            None => break,
        }
    }
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 5, "cursor walk must tile the full edge set");
}

/// #702.3: `stats()` must break edge counts down by relation, matching the
/// created fixtures.
#[tokio::test]
async fn stats_reports_edges_by_relation() {
    let f = pack();
    let a = create_concept(&f, "Stats702A").await;
    let b = create_concept(&f, "Stats702B").await;
    let c = create_concept(&f, "Stats702C").await;

    f.dispatch(
        "link",
        json!({"source_id": a, "target_id": b, "relation": "extends"}),
    )
    .await
    .expect("link A->B must succeed");
    f.dispatch(
        "link",
        json!({"source_id": a, "target_id": c, "relation": "extends"}),
    )
    .await
    .expect("link A->C must succeed");
    f.dispatch(
        "link",
        json!({"source_id": b, "target_id": c, "relation": "enables"}),
    )
    .await
    .expect("link B->C must succeed");

    let result = f
        .dispatch("stats", json!({}))
        .await
        .expect("stats must succeed");
    let by_relation = result
        .get("edges_by_relation")
        .and_then(Value::as_object)
        .expect("stats must include edges_by_relation");
    assert_eq!(by_relation.get("extends").and_then(Value::as_u64), Some(2));
    assert_eq!(by_relation.get("enables").and_then(Value::as_u64), Some(1));
}

// ── #747 regression: create(kind=note, tags=[...]) must persist tags ──────

/// #747: top-level `tags` on `create(kind="note", ...)` must round-trip
/// through `properties["tags"]` (notes have no dedicated tags column — same
/// convention already used by `memory.remember` and honored by this pack's
/// own `search`/`list` note-tag filters). Before the fix, `tags` was
/// whitelisted as an accepted param but never read in the note branch, so it
/// was silently dropped.
#[tokio::test]
async fn create_note_persists_top_level_tags_into_properties() {
    let f = pack();

    let created = f
        .dispatch(
            "create",
            json!({
                "kind": "note",
                "note_kind": "observation",
                "content": "note nvk747 top-level tags must persist",
                "tags": ["alpha", "beta"],
            }),
        )
        .await
        .expect("create note with tags must succeed");
    let id = created.get("id").and_then(Value::as_str).expect("id");

    let fetched = f
        .dispatch("get", json!({"id": id}))
        .await
        .expect("get must succeed");
    let stored_tags: Vec<&str> = fetched
        .get("properties")
        .and_then(|p| p.get("tags"))
        .and_then(Value::as_array)
        .expect("properties.tags must be present")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(
        stored_tags,
        vec!["alpha", "beta"],
        "#747: note tags must persist into properties[\"tags\"]"
    );
}

/// #747: absent/empty `tags` must leave `properties` unchanged — no spurious
/// `properties["tags"]` key when the caller never supplied tags.
#[tokio::test]
async fn create_note_without_tags_leaves_properties_unchanged() {
    let f = pack();

    let created = f
        .dispatch(
            "create",
            json!({
                "kind": "note",
                "note_kind": "observation",
                "content": "note nvk747b no tags supplied",
                "properties": {"domain": "test747"},
            }),
        )
        .await
        .expect("create note without tags must succeed");
    let id = created.get("id").and_then(Value::as_str).expect("id");

    let fetched = f
        .dispatch("get", json!({"id": id}))
        .await
        .expect("get must succeed");
    let props = fetched
        .get("properties")
        .and_then(Value::as_object)
        .expect("properties must be present");
    assert_eq!(props.get("domain").and_then(Value::as_str), Some("test747"));
    assert!(
        !props.contains_key("tags"),
        "#747: absent tags param must not inject a properties[\"tags\"] key; got {props:?}"
    );

    // Empty tags array is equivalent to absent — must not inject the key either.
    let created2 = f
        .dispatch(
            "create",
            json!({
                "kind": "note",
                "note_kind": "observation",
                "content": "note nvk747c empty tags array",
                "tags": [],
            }),
        )
        .await
        .expect("create note with empty tags must succeed");
    let id2 = created2.get("id").and_then(Value::as_str).expect("id");
    let fetched2 = f
        .dispatch("get", json!({"id": id2}))
        .await
        .expect("get must succeed");
    let has_tags_key = fetched2
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|p| p.contains_key("tags"));
    assert!(
        !has_tags_key,
        "#747: empty tags array must not inject a properties[\"tags\"] key"
    );
}

/// #747: when the caller supplies BOTH a top-level `tags` param AND a
/// conflicting `properties.tags`, the top-level `tags` param wins (documented
/// precedence rule — the explicit, typed param overrides the nested-object
/// value rather than silently merging or erroring).
#[tokio::test]
async fn create_note_top_level_tags_wins_over_properties_tags_conflict() {
    let f = pack();

    let created = f
        .dispatch(
            "create",
            json!({
                "kind": "note",
                "note_kind": "observation",
                "content": "note nvk747d conflicting tags sources",
                "properties": {"tags": ["from-properties"], "domain": "test747d"},
                "tags": ["from-top-level"],
            }),
        )
        .await
        .expect("create note with conflicting tags must succeed");
    let id = created.get("id").and_then(Value::as_str).expect("id");

    let fetched = f
        .dispatch("get", json!({"id": id}))
        .await
        .expect("get must succeed");
    let props = fetched
        .get("properties")
        .and_then(Value::as_object)
        .expect("properties must be present");
    let stored_tags: Vec<&str> = props
        .get("tags")
        .and_then(Value::as_array)
        .expect("tags must be present")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(
        stored_tags,
        vec!["from-top-level"],
        "#747: top-level `tags` param must win over properties.tags on conflict"
    );
    // The rest of `properties` (unrelated keys) must survive the merge untouched.
    assert_eq!(
        props.get("domain").and_then(Value::as_str),
        Some("test747d")
    );
}

// =============================================================================
// list() row-cap signal (issue #894)
//
// Every server-side page cap (entity 500, note 200, edge/event 1000) was
// silently binding: a `limit` above the cap returned fewer rows than asked
// for with no signal, so a caller striding a pagination loop by its own
// requested `limit` could silently skip rows between pages. These tests
// exercise the response contract end-to-end through the real handler,
// runtime, and storage layers.
// =============================================================================

/// Build a `Fixture` (registry-wrapped dispatch, same path the MCP server
/// uses) alongside the underlying `KhiveRuntime` and a `local`-namespace
/// token, so tests can seed data via fast direct runtime calls (bypassing
/// the JSON `create` verb) while still exercising the real `list` verb
/// through the registry for assertions.
fn pack_and_runtime() -> (Fixture, KhiveRuntime, NamespaceToken) {
    let rt = KhiveRuntime::memory().expect("in-memory runtime must succeed");
    let tok = rt.authorize(Namespace::local()).expect("authorize local");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");
    (Fixture { registry }, rt, tok)
}

/// `list(kind="entity")`: a `limit` at or under the cap (500) keeps the
/// existing bare-array response shape.
#[tokio::test]
async fn list_entity_limit_under_cap_honored_exactly() {
    let (pack, rt, tok) = pack_and_runtime();
    for i in 0..5u32 {
        rt.create_entity(
            &tok,
            "concept",
            None,
            &format!("cap894-under-{i}"),
            None,
            None,
            vec![],
        )
        .await
        .unwrap_or_else(|e| panic!("create entity {i} must succeed: {e}"));
    }

    let resp = pack
        .dispatch("list", json!({"kind": "entity", "limit": 3}))
        .await
        .expect("#894: list under cap must succeed");

    let items = resp.as_array().expect("list must return an array");
    assert_eq!(
        items.len(),
        3,
        "limit=3 under the entity cap (500) must return exactly 3 rows"
    );
}

/// An over-cap request reports the effective limit even when the result set
/// happens to contain fewer rows than the cap.
#[tokio::test]
async fn list_entity_limit_over_cap_reports_effective_limit() {
    let (pack, rt, tok) = pack_and_runtime();
    for i in 0..10u32 {
        rt.create_entity(
            &tok,
            "concept",
            None,
            &format!("cap894-fewmatch-{i}"),
            None,
            None,
            vec![],
        )
        .await
        .unwrap_or_else(|e| panic!("create entity {i} must succeed: {e}"));
    }

    let resp = pack
        .dispatch("list", json!({"kind": "entity", "limit": 1000}))
        .await
        .expect("#894: an over-cap limit must succeed with metadata");

    let items = resp["items"].as_array().expect("items must be an array");
    assert_eq!(
        items.len(),
        10,
        "only 10 entities exist; all must be returned"
    );
    assert_eq!(resp["requested_limit"], 1000);
    assert_eq!(resp["effective_limit"], 500);
    assert_eq!(resp["limit_clamped"], true);
}

/// `limit` above the cap AND the true match count exceeds the cap: this is
/// issue #894's actual repro. The response must truncate to the cap (500)
/// and carries the same machine-readable limit metadata.
#[tokio::test]
async fn list_entity_limit_over_cap_truncates_with_metadata() {
    let (pack, rt, tok) = pack_and_runtime();
    for i in 0..501u32 {
        rt.create_entity(
            &tok,
            "concept",
            None,
            &format!("cap894-over-{i}"),
            None,
            None,
            vec![],
        )
        .await
        .unwrap_or_else(|e| panic!("create entity {i} must succeed: {e}"));
    }

    let resp = pack
        .dispatch("list", json!({"kind": "entity", "limit": 600}))
        .await
        .expect("#894: list must succeed even when the cap binds");

    let items = resp["items"].as_array().expect("items must be an array");
    assert_eq!(
        items.len(),
        500,
        "must truncate to the entity cap (500) exactly, not silently return fewer/more"
    );

    assert_eq!(resp["requested_limit"], 600);
    assert_eq!(resp["effective_limit"], 500);
    assert_eq!(resp["limit_clamped"], true);
}

/// Same "under cap honored exactly" behavior as the entity test, at the note
/// cap (200).
#[tokio::test]
async fn list_note_limit_under_cap_honored_exactly() {
    let (pack, rt, tok) = pack_and_runtime();
    for i in 0..4u32 {
        rt.create_note(
            &tok,
            "observation",
            None,
            &format!("cap894 note under {i}"),
            None,
            None,
            vec![],
        )
        .await
        .unwrap_or_else(|e| panic!("create note {i} must succeed: {e}"));
    }

    let resp = pack
        .dispatch("list", json!({"kind": "note", "limit": 2}))
        .await
        .expect("#894: list notes under cap must succeed");
    let items = resp.as_array().expect("list must return an array");
    assert_eq!(
        items.len(),
        2,
        "limit=2 under the note cap (200) must return exactly 2 rows"
    );
}

/// Same truncation metadata as the entity path, at the note cap (200).
#[tokio::test]
async fn list_note_limit_over_cap_truncates_with_metadata() {
    let (pack, rt, tok) = pack_and_runtime();
    for i in 0..201u32 {
        rt.create_note(
            &tok,
            "observation",
            None,
            &format!("cap894 note over {i}"),
            None,
            None,
            vec![],
        )
        .await
        .unwrap_or_else(|e| panic!("create note {i} must succeed: {e}"));
    }

    let resp = pack
        .dispatch("list", json!({"kind": "note", "limit": 300}))
        .await
        .expect("#894: list notes must succeed even when the cap binds");
    let items = resp["items"].as_array().expect("items must be an array");
    assert_eq!(
        items.len(),
        200,
        "must truncate to the note cap (200) exactly"
    );
    assert_eq!(resp["requested_limit"], 300);
    assert_eq!(resp["effective_limit"], 200);
    assert_eq!(resp["limit_clamped"], true);
}

/// `list(kind="edge")` offset mode keeps the existing bare-array shape when
/// the request does not exceed the cap.
#[tokio::test]
async fn list_edge_offset_mode_limit_under_cap_honored_exactly() {
    use khive_storage::EdgeRelation;

    let (pack, rt, tok) = pack_and_runtime();
    let mut node_ids = Vec::new();
    for i in 0..6u32 {
        let e = rt
            .create_entity(
                &tok,
                "concept",
                None,
                &format!("cap894-edgenode-{i}"),
                None,
                None,
                vec![],
            )
            .await
            .unwrap_or_else(|e| panic!("create node {i} must succeed: {e}"));
        node_ids.push(e.id);
    }
    for i in 0..5usize {
        rt.link(
            &tok,
            node_ids[i],
            node_ids[i + 1],
            EdgeRelation::Extends,
            1.0,
            None,
        )
        .await
        .unwrap_or_else(|e| panic!("create edge {i} must succeed: {e}"));
    }

    let resp = pack
        .dispatch("list", json!({"kind": "edge", "limit": 3}))
        .await
        .expect("#894: list edges under cap must succeed");
    let items = resp.as_array().expect("list must return an array");
    assert_eq!(
        items.len(),
        3,
        "limit=3 under the edge cap must return exactly 3 rows"
    );
}

/// `list(kind="edge", limit=<over EDGE_LIST_MAX_LIMIT>)`: the cap genuinely
/// binds and the response truncates with explicit metadata, like the entity/note
/// cases in both offset mode (`{"items": [...]}`) and cursor mode
/// (`{"edges": [...], "next_after": ...}`). Seeds via distinct concept pairs
/// (edges are unique per
/// (source, target, relation) triple, `idx_graph_edges_unique_triple`) to
/// reach 1001 real matching edges, one past `EDGE_LIST_MAX_LIMIT` (1000).
#[tokio::test]
async fn list_edge_limit_over_cap_truncates_with_metadata_in_both_modes() {
    use khive_storage::EdgeRelation;

    let (pack, rt, tok) = pack_and_runtime();
    let n_nodes = 34usize; // 34*33 = 1122 distinct ordered pairs >= 1001 needed
    let mut node_ids = Vec::with_capacity(n_nodes);
    for i in 0..n_nodes {
        let e = rt
            .create_entity(
                &tok,
                "concept",
                None,
                &format!("cap894-edgenode-big-{i}"),
                None,
                None,
                vec![],
            )
            .await
            .unwrap_or_else(|e| panic!("create node {i} must succeed: {e}"));
        node_ids.push(e.id);
    }

    let mut created = 0usize;
    'seed: for i in 0..n_nodes {
        for j in 0..n_nodes {
            if i == j {
                continue;
            }
            rt.link(
                &tok,
                node_ids[i],
                node_ids[j],
                EdgeRelation::Extends,
                1.0,
                None,
            )
            .await
            .unwrap_or_else(|e| panic!("create edge ({i},{j}) must succeed: {e}"));
            created += 1;
            if created >= 1001 {
                break 'seed;
            }
        }
    }
    assert_eq!(
        created, 1001,
        "must have seeded exactly 1001 edges for this test's premise"
    );

    // Offset mode.
    let resp = pack
        .dispatch("list", json!({"kind": "edge", "limit": 1500}))
        .await
        .expect("#894: list edges must succeed even when the cap binds");
    let items = resp["items"].as_array().expect("items must be an array");
    assert_eq!(
        items.len(),
        1000,
        "must truncate to EDGE_LIST_MAX_LIMIT (1000) exactly"
    );
    assert_eq!(resp["requested_limit"], 1500);
    assert_eq!(resp["effective_limit"], 1000);
    assert_eq!(resp["limit_clamped"], true);

    // Cursor mode retains its cursor metadata and adds the limit metadata.
    let cursor_resp = pack
        .dispatch("list", json!({"kind": "edge", "after": "", "limit": 1500}))
        .await
        .expect("#894: cursor-mode list edges must succeed even when the cap binds");
    let cursor_edges = cursor_resp["edges"]
        .as_array()
        .expect("cursor mode keeps the {\"edges\": [...]} envelope");
    assert_eq!(
        cursor_edges.len(),
        1000,
        "cursor mode must also truncate to the cap"
    );
    assert!(
        cursor_resp["next_after"].is_string(),
        "truncated cursor page must still offer a next_after cursor: {cursor_resp}"
    );
    assert_eq!(cursor_resp["requested_limit"], 1500);
    assert_eq!(cursor_resp["effective_limit"], 1000);
    assert_eq!(cursor_resp["limit_clamped"], true);
}

/// `list(kind="event")` also keeps the bare-array response below its cap.
#[tokio::test]
async fn list_event_limit_under_cap_honored_exactly() {
    let f = pack_with_events();
    for i in 0..4u32 {
        f.dispatch(
            "create",
            json!({"kind": "concept", "name": format!("cap894-event-{i}")}),
        )
        .await
        .unwrap_or_else(|e| panic!("create entity {i} must succeed: {e}"));
    }

    let resp = f
        .dispatch("list", json!({"kind": "event", "limit": 2}))
        .await
        .expect("#894: list events under cap must succeed");
    let items = resp.as_array().expect("list must return an array");
    assert!(
        items.len() <= 2,
        "limit=2 must never return more than 2 rows; got {}",
        items.len()
    );
}

#[tokio::test]
async fn list_event_limit_over_cap_reports_effective_limit() {
    let f = pack_with_events();
    f.dispatch(
        "create",
        json!({"kind": "concept", "name": "cap894-event-over"}),
    )
    .await
    .expect("create must succeed");

    let response = f
        .dispatch("list", json!({"kind": "event", "limit": 1500}))
        .await
        .expect("over-cap event list must succeed");

    assert!(response["items"].is_array());
    assert_eq!(response["requested_limit"], 1500);
    assert_eq!(response["effective_limit"], 1000);
    assert_eq!(response["limit_clamped"], true);
}

#[tokio::test]
async fn list_proposal_limit_over_cap_reports_effective_limit() {
    let f = pack_with_events();
    f.dispatch(
        "propose",
        json!({
            "title": "cap894 proposal",
            "description": "Verify proposal list limit metadata",
            "changeset": changeset_add_entity(),
        }),
    )
    .await
    .expect("propose must succeed");

    let response = f
        .dispatch("list", json!({"kind": "proposal", "limit": 600}))
        .await
        .expect("over-cap proposal list must succeed");

    assert!(response["items"].is_array());
    assert_eq!(response["requested_limit"], 600);
    assert_eq!(response["effective_limit"], 500);
    assert_eq!(response["limit_clamped"], true);
}

// ── #806: `search_executed` event-plane emission ────────────────────────────
//
// Mirrors memory.recall's `#866` `recall_executed` regression
// (khive-pack-memory/src/handlers/recall.rs's
// `recall_emits_exactly_one_recall_executed_event`): `search` must append
// exactly one `SearchExecuted` event per served search, off the response
// path via `track_background_task`, so poll briefly instead of assuming it
// has landed by the time `search` returns.

async fn poll_search_executed_events(
    store: &std::sync::Arc<dyn khive_storage::EventStore>,
) -> Vec<khive_storage::Event> {
    for _ in 0..100 {
        let page = store
            .query_events(
                khive_storage::EventFilter {
                    kinds: vec![khive_types::EventKind::SearchExecuted],
                    ..Default::default()
                },
                khive_storage::types::PageRequest {
                    limit: 50,
                    offset: 0,
                },
            )
            .await
            .expect("query_events");
        if !page.items.is_empty() {
            return page.items;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    Vec::new()
}

async fn assert_search_projection(
    rt: &KhiveRuntime,
    store: &std::sync::Arc<dyn khive_storage::EventStore>,
    event: &khive_storage::Event,
    hits: &[Value],
    referent_kind: &str,
) {
    let expected_ids: Vec<String> = hits
        .iter()
        .map(|hit| {
            hit["id"]
                .as_str()
                .expect("search hit id must be a UUID string")
                .to_string()
        })
        .collect();
    assert_eq!(event.payload["candidates"], json!(expected_ids));
    assert_eq!(event.payload["selected"], json!(expected_ids));

    let mut reader = rt.sql().reader().await.expect("sql reader must open");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT entity_id, referent_kind, role, position \
                  FROM event_observations WHERE event_id = ?1 \
                  ORDER BY CASE role WHEN 'candidate' THEN 0 ELSE 1 END, position"
                .into(),
            params: vec![SqlValue::Text(event.id.to_string())],
            label: Some("search_observation_projection".into()),
        })
        .await
        .expect("projection query must succeed");

    let projected: Vec<(String, String, String, i64)> = rows
        .iter()
        .map(|row| {
            let text = |column| match row.get(column) {
                Some(SqlValue::Text(value)) => value.clone(),
                other => panic!("{column} must be text, got {other:?}"),
            };
            let position = match row.get("position") {
                Some(SqlValue::Integer(value)) => *value,
                other => panic!("position must be integer, got {other:?}"),
            };
            (
                text("entity_id"),
                text("referent_kind"),
                text("role"),
                position,
            )
        })
        .collect();
    let expected_projection: Vec<(String, String, String, i64)> = ["candidate", "selected"]
        .into_iter()
        .flat_map(|role| {
            expected_ids.iter().enumerate().map(move |(position, id)| {
                (
                    id.clone(),
                    referent_kind.to_string(),
                    role.to_string(),
                    position as i64,
                )
            })
        })
        .collect();
    assert_eq!(projected, expected_projection);

    let first_id = uuid::Uuid::parse_str(&expected_ids[0]).expect("search hit id must parse");
    for (filter_name, filter) in [
        (
            "observed",
            khive_storage::EventFilter {
                kinds: vec![khive_types::EventKind::SearchExecuted],
                observed: vec![first_id],
                ..Default::default()
            },
        ),
        (
            "selected",
            khive_storage::EventFilter {
                kinds: vec![khive_types::EventKind::SearchExecuted],
                selected: vec![first_id],
                ..Default::default()
            },
        ),
    ] {
        let page = store
            .query_events(
                filter,
                khive_storage::types::PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .unwrap_or_else(|error| panic!("{filter_name} filter failed: {error}"));
        assert_eq!(
            page.items.iter().map(|item| item.id).collect::<Vec<_>>(),
            vec![event.id],
            "{filter_name} must find the search event through its projection"
        );
    }
}

#[tokio::test]
async fn search_entity_emits_exactly_one_search_executed_event() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime must succeed");
    let ns = Namespace::local();
    let token = rt.authorize(ns.clone()).expect("authorize local");

    rt.create_entity(
        &token,
        "concept",
        None,
        "806 search executed entity",
        None,
        None,
        vec![],
    )
    .await
    .expect("create entity");

    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");

    let result = registry
        .dispatch(
            "search",
            json!({"kind": "entity", "query": "806 search executed entity"}),
        )
        .await
        .expect("search must succeed");
    let hits = result.as_array().expect("bare array result");
    assert!(!hits.is_empty(), "must find the seeded entity");

    let store = rt.events(&token).expect("event store for local namespace");
    let search_events = poll_search_executed_events(&store).await;

    assert_eq!(
        search_events.len(),
        1,
        "exactly one search_executed event per served search, got: {search_events:?}"
    );
    let event = &search_events[0];
    assert_eq!(event.kind, khive_types::EventKind::SearchExecuted);
    assert_eq!(event.verb, "search");
    assert_eq!(event.payload["result_kind"], json!("entity"));
    assert_eq!(event.payload["result_count"], json!(hits.len()));
    assert_eq!(event.payload["query"], json!("806 search executed entity"));
    assert_eq!(
        event.payload["served_by_profile_id"],
        Value::Null,
        "kg search has no profile resolution — the field must stay present but null"
    );
    assert!(
        event.payload["latency_us"]
            .as_i64()
            .is_some_and(|us| us >= 0),
        "latency_us must be a non-negative measured duration, got: {:?}",
        event.payload["latency_us"]
    );
    assert!(
        event.payload["actor"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "actor must be stamped, got: {:?}",
        event.payload["actor"]
    );
    let selected = event.payload["selected"]
        .as_array()
        .expect("selected must be a UUID array");
    assert_eq!(
        selected.len(),
        hits.len(),
        "selected must carry the full served result-ID list"
    );
    assert_search_projection(&rt, &store, event, hits, "entity").await;
}

#[tokio::test]
async fn search_note_emits_exactly_one_search_executed_event_with_note_result_kind() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime must succeed");
    let ns = Namespace::local();
    let token = rt.authorize(ns.clone()).expect("authorize local");

    rt.create_note(
        &token,
        "observation",
        None,
        "806 search executed note unique_marker_9931",
        None,
        None,
        vec![],
    )
    .await
    .expect("create note");

    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");

    let result = registry
        .dispatch(
            "search",
            json!({"kind": "note", "query": "unique_marker_9931"}),
        )
        .await
        .expect("search must succeed");
    let hits = result.as_array().expect("bare array result");
    assert!(!hits.is_empty(), "must find the seeded note");

    let store = rt.events(&token).expect("event store for local namespace");
    let search_events = poll_search_executed_events(&store).await;

    assert_eq!(
        search_events.len(),
        1,
        "exactly one search_executed event per served search, got: {search_events:?}"
    );
    let event = &search_events[0];
    assert_eq!(event.payload["result_kind"], json!("note"));
    assert_eq!(event.payload["result_count"], json!(hits.len()));
    assert_search_projection(&rt, &store, event, hits, "note").await;
}
