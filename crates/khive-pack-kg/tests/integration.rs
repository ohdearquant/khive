//! Integration tests for khive-pack-kg.
//!
//! Tests exercise the full dispatch path through KgPack: params deserialize,
//! validation, runtime call, and JSON response. All tests use an in-memory
//! runtime so there is no I/O dependency.

use async_trait::async_trait;
use khive_pack_kg::KgPack;
use khive_runtime::pack::{PackRuntime, VerbDef};
use khive_runtime::{KhiveRuntime, RuntimeError, VerbRegistry, VerbRegistryBuilder};
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

    fn verbs(&self) -> Vec<&'static VerbDef> {
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

fn pack_with_events() -> Fixture {
    let rt = KhiveRuntime::memory().expect("in-memory runtime must succeed");
    let event_store = rt.events(None).expect("event store must be available");
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
    const VERBS: &'static [VerbDef] = &[];
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

    fn verbs(&self) -> &'static [VerbDef] {
        FakeMemoryPack::VERBS
    }

    fn requires(&self) -> &'static [&'static str] {
        FakeMemoryPack::REQUIRES
    }

    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
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

    assert_eq!(
        get_result.get("kind").and_then(Value::as_str),
        Some("event"),
        "get wrapper must have kind=event"
    );
    let data = get_result.get("data").expect("get must have data field");
    assert_eq!(
        data.get("id").and_then(Value::as_str),
        Some(event_id.as_str()),
        "data.id must match the requested event UUID"
    );
    assert_eq!(
        data.get("verb").and_then(Value::as_str),
        Some("create"),
        "data.verb must be create"
    );
    assert_eq!(
        data.get("outcome").and_then(Value::as_str),
        Some("success"),
        "data.outcome must be success"
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
            json!({"id": event_id, "name": "should-not-apply"}),
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
        .dispatch("delete", json!({"id": event_id}))
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
