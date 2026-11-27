//! Keyed entity create and lookup through the kg verbs (ADR-221 section 1).

use khive_runtime::RuntimeError;
use serde_json::{json, Value};

async fn configured() -> (
    khive_runtime::KhiveRuntime,
    khive_runtime::NamespaceToken,
    crate::KgPack,
    khive_runtime::VerbRegistry,
) {
    use crate::KgPack;
    use khive_runtime::VerbRegistryBuilder;

    let rt = khive_runtime::KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let registry = builder.build().expect("kg registry builds");
    rt.install_edge_rules(registry.all_edge_rules());
    let token = rt.authorize(khive_runtime::Namespace::local()).unwrap();
    let pack = KgPack::new(rt.clone());
    (rt, token, pack, registry)
}

fn gated(
    gate: std::sync::Arc<dyn khive_runtime::Gate>,
) -> (
    khive_runtime::KhiveRuntime,
    khive_runtime::NamespaceToken,
    crate::KgPack,
    khive_runtime::VerbRegistry,
) {
    use crate::KgPack;
    use khive_runtime::VerbRegistryBuilder;

    let rt = khive_runtime::KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.with_gate(gate);
    builder.register(KgPack::new(rt.clone()));
    let registry = builder.build().expect("kg registry builds");
    rt.install_edge_rules(registry.all_edge_rules());
    let token = rt.authorize(khive_runtime::Namespace::local()).unwrap();
    let pack = KgPack::new(rt.clone());
    (rt, token, pack, registry)
}

/// Permits every verb except `list`, so a caller may restore but may not
/// learn who holds a key.
#[derive(Debug)]
struct DenyList;

impl khive_runtime::Gate for DenyList {
    fn check(
        &self,
        request: &khive_runtime::GateRequest,
    ) -> Result<khive_runtime::GateDecision, khive_runtime::GateError> {
        Ok(if request.verb == "list" {
            khive_runtime::GateDecision::deny("listing denied")
        } else {
            khive_runtime::GateDecision::allow()
        })
    }
}

/// Refuses `update` and `delete` once the arguments name an id, so a call by
/// key passes the dispatch check and is refused only when the handler judges
/// it again by the id the key resolved to.
#[derive(Debug)]
struct DenyWritesById;

impl khive_runtime::Gate for DenyWritesById {
    fn check(
        &self,
        request: &khive_runtime::GateRequest,
    ) -> Result<khive_runtime::GateDecision, khive_runtime::GateError> {
        let write = request.verb == "update" || request.verb == "delete";
        Ok(if write && request.args.get("id").is_some() {
            khive_runtime::GateDecision::deny("writes by id denied")
        } else {
            khive_runtime::GateDecision::allow()
        })
    }
}

fn details(error: &RuntimeError) -> (Option<String>, Option<String>) {
    let RuntimeError::Khive(error) = error else {
        panic!("expected a domain error, got {error:?}");
    };
    let details = error.details().expect("details");
    (
        details.get("reason").map(str::to_owned),
        details.get("existing_id").map(str::to_owned),
    )
}

fn keyed_create(name: &str) -> Value {
    json!({
        "kind": "concept",
        "name": name,
        "key": "paper:smith2024",
        "skip_dedup_check": true
    })
}

#[tokio::test]
async fn keyed_entity_create_replays_and_refuses_a_different_payload() {
    let (_rt, _token, _pack, registry) = configured().await;
    let first = registry
        .dispatch("create", keyed_create("Smith 2024"))
        .await
        .expect("fresh keyed create");
    assert_eq!(first["created"], json!(true), "{first}");
    let id = first["id"].as_str().expect("id").to_owned();

    let replay = registry
        .dispatch("create", keyed_create("Smith 2024"))
        .await
        .expect("replay");
    assert_eq!(replay, json!({"id": id, "created": false}));

    let error = registry
        .dispatch("create", keyed_create("Someone else"))
        .await
        .expect_err("a different payload under a held key");
    assert_eq!(
        details(&error),
        (Some("key_conflict".to_owned()), Some(id.clone()))
    );

    let listed = registry
        .dispatch("list", json!({"kind": "concept"}))
        .await
        .expect("list");
    let count = listed
        .get("items")
        .or_else(|| listed.get("results"))
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or_else(|| listed.as_array().map(Vec::len).unwrap_or_default());
    assert_eq!(count, 1, "replay and conflict write nothing: {listed}");
}

#[tokio::test]
async fn get_by_entity_key_reads_the_live_holder() {
    let (_rt, _token, _pack, registry) = configured().await;
    let created = registry
        .dispatch("create", keyed_create("Smith 2024"))
        .await
        .expect("keyed create");
    let got = registry
        .dispatch("get", json!({"kind": "concept", "key": "paper:smith2024"}))
        .await
        .expect("get by key");
    assert_eq!(got["id"], created["id"], "{got}");
    assert_eq!(got["name"], json!("Smith 2024"));

    registry
        .dispatch("get", json!({"kind": "concept", "key": "paper:absent"}))
        .await
        .expect_err("no holder");
    registry
        .dispatch("get", json!({"kind": "project", "key": "paper:smith2024"}))
        .await
        .expect_err("the key is scoped to its kind");
    let error = registry
        .dispatch("get", json!({"kind": "entity", "key": "paper:smith2024"}))
        .await
        .expect_err("an entity key lookup needs a specific kind");
    assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error:?}");
}

#[tokio::test]
async fn key_is_refused_on_a_substrate_without_keys() {
    let (_rt, _token, _pack, registry) = configured().await;
    let a = registry
        .dispatch("create", json!({"kind": "concept", "name": "A"}))
        .await
        .unwrap();
    let b = registry
        .dispatch("create", json!({"kind": "concept", "name": "B"}))
        .await
        .unwrap();
    let error = registry
        .dispatch(
            "create",
            json!({
                "kind": "edge",
                "source_id": a["id"],
                "target_id": b["id"],
                "relation": "extends",
                "key": "k"
            }),
        )
        .await
        .expect_err("edges hold no key");
    assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error:?}");
}

#[tokio::test]
async fn delete_releases_and_restore_refuses_a_taken_entity_key() {
    let (_rt, token, pack, registry) = configured().await;
    let first = registry
        .dispatch("create", keyed_create("First"))
        .await
        .expect("keyed create");
    let first_id = first["id"].as_str().unwrap().to_owned();
    registry
        .dispatch("delete", json!({"id": first_id}))
        .await
        .expect("soft delete");
    registry
        .dispatch("get", json!({"kind": "concept", "key": "paper:smith2024"}))
        .await
        .expect_err("a deleted entity holds no key");

    let second = registry
        .dispatch("create", keyed_create("Second"))
        .await
        .expect("the released key is free");
    assert_eq!(second["created"], json!(true));
    let second_id = second["id"].as_str().unwrap().to_owned();
    assert_ne!(second_id, first_id);

    let conflict = pack
        .handle_restore(&token, json!({"id": first_id}), &registry)
        .await
        .expect_err("restore must refuse a taken key");
    let text = format!("{conflict}");
    assert!(text.contains("restore_key_conflict"), "{text}");
    assert!(text.contains("paper:smith2024"), "{text}");
    assert!(text.contains(&second_id), "{text}");

    let tombstone = registry
        .dispatch("get", json!({"id": first_id, "include_deleted": true}))
        .await
        .expect("the refused restore leaves the tombstone");
    assert!(tombstone["deleted_at"].is_string(), "{tombstone}");
    let holder = registry
        .dispatch("get", json!({"kind": "concept", "key": "paper:smith2024"}))
        .await
        .expect("the second entity still holds the key");
    assert_eq!(holder["id"], json!(second_id));
}

#[tokio::test]
async fn restore_key_conflict_names_no_holder_the_caller_may_not_list() {
    let (_rt, token, pack, registry) = gated(std::sync::Arc::new(DenyList));

    let first = registry
        .dispatch("create", keyed_create("First"))
        .await
        .expect("keyed create");
    let first_id = first["id"].as_str().unwrap().to_owned();
    registry
        .dispatch("delete", json!({"id": first_id}))
        .await
        .expect("soft delete");
    let second = registry
        .dispatch("create", keyed_create("Second"))
        .await
        .expect("the released key is free");
    let second_id = second["id"].as_str().unwrap().to_owned();

    let conflict = pack
        .handle_restore(&token, json!({"id": first_id}), &registry)
        .await
        .expect_err("restore must refuse a taken key");
    let text = format!("{conflict}");
    assert!(text.contains("restore_key_conflict"), "{text}");
    assert!(text.contains("paper:smith2024"), "{text}");
    assert!(!text.contains(&second_id), "{text}");
    let RuntimeError::Khive(error) = &conflict else {
        panic!("expected a domain error, got {conflict:?}");
    };
    let details = error.details().expect("details");
    assert_eq!(details.get("reason"), Some("restore_key_conflict"));
    let taken: Value =
        serde_json::from_str(details.get("taken_keys").expect("taken keys")).unwrap();
    assert_eq!(
        taken,
        json!([{"kind": "concept", "key": "paper:smith2024"}]),
        "{taken}"
    );
}

#[tokio::test]
async fn update_and_delete_by_key_reach_the_live_holder() {
    let (_rt, _token, _pack, registry) = configured().await;
    let created = registry
        .dispatch("create", keyed_create("Smith 2024"))
        .await
        .expect("keyed create");
    let id = created["id"].clone();

    registry
        .dispatch(
            "update",
            json!({"kind": "concept", "key": "paper:smith2024", "description": "by key"}),
        )
        .await
        .expect("update by key");
    let got = registry
        .dispatch("get", json!({"id": id}))
        .await
        .expect("get by id");
    assert_eq!(got["description"], json!("by key"), "{got}");

    registry
        .dispatch(
            "delete",
            json!({"kind": "concept", "key": "paper:smith2024"}),
        )
        .await
        .expect("delete by key");
    let tombstone = registry
        .dispatch("get", json!({"id": id, "include_deleted": true}))
        .await
        .expect("the tombstone stays reachable by id");
    assert!(tombstone["deleted_at"].is_string(), "{tombstone}");

    for verb in ["update", "delete"] {
        let mut args = json!({"kind": "concept", "key": "paper:smith2024"});
        if verb == "update" {
            args["description"] = json!("again");
        }
        let error = registry
            .dispatch(verb, args)
            .await
            .expect_err("a deleted entity holds no key");
        assert!(
            format!("{error}").contains("paper:smith2024"),
            "{verb}: {error}"
        );
    }
}

#[tokio::test]
async fn update_and_delete_by_key_refuse_malformed_targets() {
    let (_rt, _token, _pack, registry) = configured().await;
    let created = registry
        .dispatch("create", keyed_create("Smith 2024"))
        .await
        .expect("keyed create");
    for verb in ["update", "delete"] {
        for args in [
            json!({"kind": "concept", "key": "paper:smith2024", "id": created["id"]}),
            json!({"key": "paper:smith2024"}),
            json!({"kind": "entity", "key": "paper:smith2024"}),
            json!({"kind": "note", "key": "paper:smith2024"}),
            json!({"kind": "concept", "key": 7}),
        ] {
            let error = registry
                .dispatch(verb, args.clone())
                .await
                .expect_err("malformed by-key target");
            assert!(
                matches!(error, RuntimeError::InvalidInput(_)),
                "{verb} {args}: {error:?}"
            );
        }
    }
    let got = registry
        .dispatch("get", json!({"id": created["id"]}))
        .await
        .expect("the holder is untouched");
    assert!(got["deleted_at"].is_null(), "{got}");
}

#[tokio::test]
async fn update_and_delete_by_key_are_judged_by_the_resolved_id() {
    let (_rt, _token, _pack, registry) = gated(std::sync::Arc::new(DenyWritesById));
    let created = registry
        .dispatch("create", keyed_create("Smith 2024"))
        .await
        .expect("keyed create");
    for (verb, args) in [
        (
            "update",
            json!({"kind": "concept", "key": "paper:smith2024", "description": "x"}),
        ),
        (
            "delete",
            json!({"kind": "concept", "key": "paper:smith2024"}),
        ),
    ] {
        let error = registry
            .dispatch(verb, args)
            .await
            .expect_err("the second check sees the id");
        assert!(
            format!("{error}").contains("writes by id denied"),
            "{verb}: {error}"
        );
    }
    let got = registry
        .dispatch("get", json!({"id": created["id"]}))
        .await
        .expect("get by id");
    assert!(got["description"].is_null(), "{got}");
    assert!(got["deleted_at"].is_null(), "{got}");
}

/// Refuses every `update`, so a caller may create under a key but may not
/// replace the entity that holds it.
#[derive(Debug)]
struct DenyUpdate;

impl khive_runtime::Gate for DenyUpdate {
    fn check(
        &self,
        request: &khive_runtime::GateRequest,
    ) -> Result<khive_runtime::GateDecision, khive_runtime::GateError> {
        Ok(if request.verb == "update" {
            khive_runtime::GateDecision::deny("updates denied")
        } else {
            khive_runtime::GateDecision::allow()
        })
    }
}

fn on_key_create(name: &str, extra: Value) -> Value {
    let mut args = keyed_create(name);
    args["on_key"] = json!("update");
    if let (Some(args), Value::Object(extra)) = (args.as_object_mut(), extra) {
        args.extend(extra);
    }
    args
}

async fn concept_count(registry: &khive_runtime::VerbRegistry) -> usize {
    let listed = registry
        .dispatch("list", json!({"kind": "concept"}))
        .await
        .expect("list");
    listed
        .get("items")
        .or_else(|| listed.get("results"))
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or_else(|| listed.as_array().map(Vec::len).unwrap_or_default())
}

#[tokio::test]
async fn on_key_update_replaces_a_differing_holder_and_replays_an_identical_one() {
    let (_rt, _token, _pack, registry) = configured().await;
    let mut first = keyed_create("Smith 2024");
    first["description"] = json!("first");
    first["properties"] = json!({"a": 1, "b": 2});
    first["tags"] = json!(["x"]);
    let created = registry
        .dispatch("create", first)
        .await
        .expect("keyed create");
    let id = created["id"].clone();

    let reload = on_key_create("Smith et al. 2024", json!({"properties": {"a": 3}}));
    let updated = registry
        .dispatch("create", reload.clone())
        .await
        .expect("on_key update of a differing holder");
    assert_eq!(updated["id"], id, "{updated}");
    assert_eq!(updated["created"], json!(false), "{updated}");
    assert_eq!(updated["updated"], json!(true), "{updated}");

    let got = registry
        .dispatch("get", json!({"id": id}))
        .await
        .expect("get by id");
    assert_eq!(got["name"], json!("Smith et al. 2024"), "{got}");
    assert!(
        got["description"].is_null(),
        "an omitted description is cleared: {got}"
    );
    assert_eq!(got["properties"]["a"], json!(3), "{got}");
    assert!(
        got["properties"].get("b").is_none(),
        "an omitted property is cleared: {got}"
    );
    assert!(
        got["tags"].as_array().is_none_or(Vec::is_empty),
        "omitted tags are cleared: {got}"
    );

    let replay = registry
        .dispatch("create", reload)
        .await
        .expect("identical on_key reload");
    assert_eq!(replay, json!({"id": id, "created": false}));
    let again = registry
        .dispatch("get", json!({"id": id}))
        .await
        .expect("get by id");
    assert_eq!(again, got, "an identical reload writes nothing");
    assert_eq!(concept_count(&registry).await, 1);
}

#[tokio::test]
async fn on_key_asks_the_update_gate_only_when_the_payload_differs() {
    let (_rt, _token, _pack, registry) = gated(std::sync::Arc::new(DenyUpdate));
    let created = registry
        .dispatch("create", keyed_create("Smith 2024"))
        .await
        .expect("keyed create");
    let id = created["id"].as_str().unwrap().to_owned();

    let replay = registry
        .dispatch("create", on_key_create("Smith 2024", json!({})))
        .await
        .expect("an identical reload needs no update permission");
    assert_eq!(replay, json!({"id": id, "created": false}));

    let error = registry
        .dispatch("create", on_key_create("Someone else", json!({})))
        .await
        .expect_err("a differing reload needs update permission");
    assert_eq!(
        details(&error),
        (Some("key_conflict".to_owned()), Some(id.clone()))
    );
    let got = registry
        .dispatch("get", json!({"id": id}))
        .await
        .expect("get by id");
    assert_eq!(got["name"], json!("Smith 2024"), "{got}");
}

#[tokio::test]
async fn on_key_shape_refusals_hold_whether_or_not_the_key_is_held() {
    for held in [false, true] {
        let (_rt, _token, _pack, registry) = configured().await;
        let target = registry
            .dispatch("create", json!({"kind": "project", "name": "Target"}))
            .await
            .expect("edge target");
        if held {
            registry
                .dispatch("create", keyed_create("Smith 2024"))
                .await
                .expect("keyed create");
        }
        let cases = [
            (
                on_key_create("N", json!({"on_key": "replace"})),
                "on_key accepts only",
            ),
            (
                json!({"kind": "concept", "name": "N", "on_key": "update"}),
                "keyed entity create",
            ),
            (
                json!({"kind": "note", "content": "c", "key": "paper:smith2024", "on_key": "update"}),
                "keyed entity create",
            ),
            (
                on_key_create(
                    "N",
                    json!({"edges": [{"target_id": target["id"], "relation": "extends"}]}),
                ),
                "edges, embed or fence",
            ),
            (on_key_create("N", json!({"embed": true})), "embed"),
            (
                json!({"items": [{"kind": "concept", "name": "N"}], "on_key": "update"}),
                "set it on each entity item",
            ),
        ];
        for (args, expected) in cases {
            let error = registry
                .dispatch("create", args.clone())
                .await
                .expect_err("on_key shape refusal");
            assert!(
                matches!(error, RuntimeError::InvalidInput(_)),
                "held={held} {args}: {error:?}"
            );
            assert!(
                format!("{error}").contains(expected),
                "held={held} {args}: {error}"
            );
        }
        assert_eq!(concept_count(&registry).await, usize::from(held));
        if held {
            let holder = registry
                .dispatch("get", json!({"kind": "concept", "key": "paper:smith2024"}))
                .await
                .expect("the holder stays");
            assert_eq!(holder["name"], json!("Smith 2024"), "{holder}");
        }
    }
}

fn bulk(items: Vec<Value>, atomic: bool) -> Value {
    json!({"items": items, "atomic": atomic})
}

fn keyed_item(name: &str, key: &str) -> Value {
    json!({"kind": "concept", "name": name, "key": key})
}

fn on_key_item(name: &str, key: &str) -> Value {
    let mut item = keyed_item(name, key);
    item["on_key"] = json!("update");
    item
}

/// The live holder of `key`, or `None` when the read answers not-found.
/// Any other failure fails the test, so a read that breaks for another
/// reason cannot pass for an absent key.
async fn key_holder(registry: &khive_runtime::VerbRegistry, key: &str) -> Option<Value> {
    match registry
        .dispatch("get", json!({"kind": "concept", "key": key}))
        .await
    {
        Ok(holder) => Some(holder),
        Err(RuntimeError::Khive(error)) if error.kind() == khive_types::ErrorKind::NotFound => None,
        Err(error) => panic!("get by key {key:?} failed: {error:?}"),
    }
}

/// A credential-shaped value the runtime secret gate refuses, built at run
/// time so the source holds no such literal.
fn credential() -> String {
    format!("ghp_{}", "A".repeat(36))
}

#[tokio::test]
async fn bulk_items_under_one_key_are_judged_against_the_earlier_item() {
    let (_rt, _token, _pack, registry) = configured().await;
    let same = registry
        .dispatch(
            "create",
            bulk(
                vec![keyed_item("Smith 2024", "k"), keyed_item("Smith 2024", "k")],
                true,
            ),
        )
        .await
        .expect("two identical keyed items");
    let (first, second) = (&same["results"][0]["result"], &same["results"][1]["result"]);
    assert_eq!(first["created"], json!(true), "{same}");
    assert_eq!(second["created"], json!(false), "{same}");
    assert_eq!(second["id"], first["id"], "{same}");
    assert_eq!(same["created"], json!(1), "{same}");
    assert!(same.get("updated").is_none(), "{same}");
    assert_eq!(concept_count(&registry).await, 1);

    let error = registry
        .dispatch(
            "create",
            bulk(
                vec![
                    keyed_item("Jones 2025", "j"),
                    keyed_item("Someone else", "j"),
                ],
                true,
            ),
        )
        .await
        .expect_err("a differing second item under the same key");
    assert_eq!(
        details(&error),
        (Some("key_conflict".to_owned()), None),
        "the earlier item's row is never written, so no id is named"
    );
    assert_eq!(concept_count(&registry).await, 1);
    assert!(key_holder(&registry, "j").await.is_none());
}

#[tokio::test]
async fn bulk_on_key_rewrites_the_row_an_earlier_item_creates() {
    let (_rt, _token, _pack, registry) = configured().await;
    let mut first = keyed_item("Smith 2024", "k");
    first["properties"] = json!({"a": 1});
    let mut second = on_key_item("Smith et al. 2024", "k");
    second["properties"] = json!({"b": 2});
    let response = registry
        .dispatch("create", bulk(vec![first, second], true))
        .await
        .expect("a later item rewrites the earlier item's row");
    let (a, b) = (
        &response["results"][0]["result"],
        &response["results"][1]["result"],
    );
    assert_eq!(a["created"], json!(true), "{response}");
    assert_eq!(b["created"], json!(false), "{response}");
    assert_eq!(b["updated"], json!(true), "{response}");
    assert_eq!(
        a["id"], b["id"],
        "both items answer for one row: {response}"
    );
    assert_eq!(response["created"], json!(1), "{response}");
    assert_eq!(response["updated"], json!(1), "{response}");
    let holder = key_holder(&registry, "k")
        .await
        .expect("the key names the row");
    assert_eq!(holder["id"], a["id"], "{holder}");
    assert_eq!(holder["name"], json!("Smith et al. 2024"), "{holder}");
    assert_eq!(holder["properties"], json!({"b": 2}), "{holder}");
    assert_eq!(concept_count(&registry).await, 1);
}

#[tokio::test]
async fn bulk_on_key_updates_a_stored_holder_in_the_unit() {
    let (_rt, _token, _pack, registry) = configured().await;
    let mut stored = keyed_create("Smith 2024");
    stored["properties"] = json!({"a": 1});
    stored["tags"] = json!(["x"]);
    let created = registry
        .dispatch("create", stored)
        .await
        .expect("keyed create");
    let before = key_holder(&registry, "paper:smith2024")
        .await
        .expect("holder");

    let response = registry
        .dispatch(
            "create",
            bulk(
                vec![
                    on_key_item("Smith et al. 2024", "paper:smith2024"),
                    json!({"kind": "concept", "name": "Unkeyed"}),
                ],
                true,
            ),
        )
        .await
        .expect("the stored holder is reloaded in the unit");
    let reload = &response["results"][0]["result"];
    assert_eq!(reload["id"], created["id"], "{response}");
    assert_eq!(reload["created"], json!(false), "{response}");
    assert_eq!(reload["updated"], json!(true), "{response}");
    assert!(reload.get("reindex_error").is_none(), "{response}");
    assert_eq!(response["created"], json!(1), "{response}");
    assert_eq!(response["updated"], json!(1), "{response}");
    let after = key_holder(&registry, "paper:smith2024")
        .await
        .expect("holder");
    assert_eq!(after["name"], json!("Smith et al. 2024"), "{after}");
    assert!(
        after["properties"].get("a").is_none(),
        "an omitted property is cleared: {after}"
    );
    assert!(
        after["tags"].as_array().is_none_or(Vec::is_empty),
        "omitted tags are cleared: {after}"
    );
    assert_eq!(
        after["version"].as_i64(),
        before["version"].as_i64().map(|version| version + 1),
        "{after}"
    );
    assert_eq!(concept_count(&registry).await, 2);

    let replay = registry
        .dispatch(
            "create",
            bulk(
                vec![on_key_item("Smith et al. 2024", "paper:smith2024")],
                true,
            ),
        )
        .await
        .expect("an identical reload writes nothing");
    let replayed = &replay["results"][0]["result"];
    assert_eq!(replayed["created"], json!(false), "{replay}");
    assert!(replayed.get("updated").is_none(), "{replay}");
    assert!(replay.get("updated").is_none(), "{replay}");
    let again = key_holder(&registry, "paper:smith2024")
        .await
        .expect("holder");
    assert_eq!(again["version"], after["version"], "{again}");
}

#[tokio::test]
async fn a_bulk_reload_reindexes_the_holder_text() {
    let (_rt, _token, _pack, registry) = configured().await;
    let created = registry
        .dispatch("create", keyed_create("Smith 2024"))
        .await
        .expect("keyed create");
    registry
        .dispatch(
            "create",
            bulk(
                vec![on_key_item("Zebrafish atlas", "paper:smith2024")],
                true,
            ),
        )
        .await
        .expect("bulk reload");
    let found = |hits: &Value| -> Vec<Value> {
        hits.as_array()
            .expect("search rows")
            .iter()
            .map(|row| row["id"].clone())
            .collect()
    };
    let new_name = registry
        .dispatch(
            "search",
            json!({"kind": "entity", "query": "zebrafish", "limit": 10}),
        )
        .await
        .expect("search by the new name");
    assert_eq!(found(&new_name), vec![created["id"].clone()], "{new_name}");
    let old_name = registry
        .dispatch(
            "search",
            json!({"kind": "entity", "query": "smith", "limit": 10}),
        )
        .await
        .expect("search by the old name");
    assert!(found(&old_name).is_empty(), "{old_name}");
}

#[tokio::test]
async fn bulk_on_key_needs_update_permission_for_a_stored_or_earlier_holder() {
    let (_rt, _token, _pack, registry) = gated(std::sync::Arc::new(DenyUpdate));
    let created = registry
        .dispatch("create", keyed_create("Smith 2024"))
        .await
        .expect("keyed create");
    let id = created["id"].as_str().unwrap().to_owned();

    let error = registry
        .dispatch(
            "create",
            bulk(vec![on_key_item("Someone else", "paper:smith2024")], true),
        )
        .await
        .expect_err("a stored holder the caller may not update");
    assert_eq!(
        details(&error),
        (Some("key_conflict".to_owned()), Some(id.clone()))
    );
    let holder = key_holder(&registry, "paper:smith2024")
        .await
        .expect("holder");
    assert_eq!(holder["name"], json!("Smith 2024"), "{holder}");

    let replay = registry
        .dispatch(
            "create",
            bulk(vec![on_key_item("Smith 2024", "paper:smith2024")], true),
        )
        .await
        .expect("an identical item needs no update permission");
    assert_eq!(replay["results"][0]["result"]["id"], json!(id), "{replay}");

    let error = registry
        .dispatch(
            "create",
            bulk(
                vec![
                    keyed_item("Jones 2025", "j"),
                    on_key_item("Jones et al. 2025", "j"),
                ],
                true,
            ),
        )
        .await
        .expect_err("an earlier item's row the caller may not update");
    assert_eq!(details(&error), (Some("key_conflict".to_owned()), None));
    assert!(key_holder(&registry, "j").await.is_none());
    assert_eq!(concept_count(&registry).await, 1);
}

#[tokio::test]
async fn an_atomic_bulk_whose_later_item_fails_leaves_no_key_row() {
    let (_rt, _token, _pack, registry) = configured().await;
    registry
        .dispatch("create", keyed_create("Smith 2024"))
        .await
        .expect("keyed create");
    let error = registry
        .dispatch(
            "create",
            bulk(
                vec![
                    keyed_item("Fresh", "fresh"),
                    keyed_item("Someone else", "paper:smith2024"),
                ],
                true,
            ),
        )
        .await
        .expect_err("the later item conflicts");
    assert_eq!(details(&error).0, Some("key_conflict".to_owned()));
    assert!(key_holder(&registry, "fresh").await.is_none());
    let retaken = registry
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "Other", "key": "fresh", "skip_dedup_check": true}),
        )
        .await
        .expect("the earlier item never claimed the key");
    assert_eq!(retaken["created"], json!(true), "{retaken}");
    assert_eq!(concept_count(&registry).await, 2);
}

#[tokio::test]
async fn best_effort_bulk_judges_each_keyed_item_after_the_one_before() {
    let (_rt, _token, _pack, registry) = configured().await;
    let response = registry
        .dispatch(
            "create",
            bulk(
                vec![
                    keyed_item("Smith 2024", "k"),
                    keyed_item("Someone else", "k"),
                    keyed_item("Smith 2024", "k"),
                    on_key_item("Smith et al. 2024", "k"),
                    json!({"kind": "note", "content": "c", "key": "k"}),
                ],
                false,
            ),
        )
        .await
        .expect("best-effort bulk");
    let result = |index: usize| response["results"][index].clone();
    let id = result(0)["result"]["id"].clone();
    assert_eq!(result(0)["result"]["created"], json!(true), "{response}");
    assert_eq!(result(1)["ok"], json!(false), "{response}");
    assert!(
        result(1)["error"].to_string().contains("key_conflict"),
        "{response}"
    );
    assert_eq!(result(2)["result"]["created"], json!(false), "{response}");
    assert_eq!(result(2)["result"]["id"], id, "{response}");
    assert_eq!(result(3)["result"]["updated"], json!(true), "{response}");
    assert_eq!(result(3)["result"]["id"], id, "{response}");
    assert_eq!(result(4)["ok"], json!(false), "{response}");
    assert!(
        result(4)["error"].to_string().contains("entity items"),
        "{response}"
    );
    assert_eq!(response["created"], json!(1), "{response}");
    assert_eq!(response["updated"], json!(1), "{response}");
    assert_eq!(response["failed"], json!(2), "{response}");
    let holder = key_holder(&registry, "k").await.expect("holder");
    assert_eq!(holder["name"], json!("Smith et al. 2024"), "{holder}");
}

#[tokio::test]
async fn bulk_item_key_shape_refusals_write_nothing() {
    let (_rt, _token, _pack, registry) = configured().await;
    let mut no_key = json!({"kind": "concept", "name": "N", "on_key": "update"});
    let mut replace = on_key_item("N", "k");
    replace["on_key"] = json!("replace");
    for (item, expected) in [
        (
            json!({"kind": "note", "content": "c", "key": "k"}),
            "entity items",
        ),
        (no_key.take(), "keyed entity item"),
        (replace, "accepts only"),
    ] {
        let error = registry
            .dispatch(
                "create",
                bulk(vec![keyed_item("Fresh", "fresh"), item.clone()], true),
            )
            .await
            .expect_err("a malformed keyed item");
        assert!(
            matches!(error, RuntimeError::InvalidInput(_)),
            "{item}: {error:?}"
        );
        assert!(format!("{error}").contains(expected), "{item}: {error}");
    }
    assert_eq!(concept_count(&registry).await, 0);
    assert!(key_holder(&registry, "fresh").await.is_none());
}

#[tokio::test]
async fn a_bulk_item_whose_payload_a_later_item_replaces_is_still_admitted() {
    let (_rt, _token, _pack, registry) = configured().await;
    let mut refused = keyed_item("Smith 2024", "k");
    refused["description"] = json!(credential());
    let items = || vec![refused.clone(), on_key_item("Smith et al. 2024", "k")];

    let error = registry
        .dispatch("create", bulk(items(), true))
        .await
        .expect_err("the first item's own payload is refused");
    assert!(format!("{error}").contains("items[0]"), "{error}");
    assert!(key_holder(&registry, "k").await.is_none());
    assert_eq!(concept_count(&registry).await, 0);

    // Sent one by one in order, the first is refused and the second then
    // finds the key free.
    let response = registry
        .dispatch("create", bulk(items(), false))
        .await
        .expect("best-effort bulk");
    assert_eq!(response["results"][0]["ok"], json!(false), "{response}");
    assert_eq!(
        response["results"][1]["result"]["created"],
        json!(true),
        "{response}"
    );
    let holder = key_holder(&registry, "k").await.expect("holder");
    assert_eq!(holder["name"], json!("Smith et al. 2024"), "{holder}");
    assert_eq!(concept_count(&registry).await, 1);
}

#[tokio::test]
async fn a_reload_reindex_error_reaches_every_answer_that_names_the_row() {
    let (rt, token, _pack, registry) = configured().await;
    let created = registry
        .dispatch("create", keyed_create("Smith 2024"))
        .await
        .expect("keyed create");
    rt.text(&token).unwrap();
    let mut writer = rt.sql().writer().await.unwrap();
    writer
        .execute_script(
            "CREATE TRIGGER fail_reload_fts BEFORE INSERT ON fts_entities_rowids \
             BEGIN SELECT RAISE(ABORT, 'forced FTS map failure'); END;"
                .into(),
        )
        .await
        .unwrap();
    drop(writer);
    let response = registry
        .dispatch(
            "create",
            bulk(
                vec![
                    on_key_item("Smith et al. 2024", "paper:smith2024"),
                    on_key_item("Smith and Jones 2024", "paper:smith2024"),
                    keyed_item("Smith and Jones 2024", "paper:smith2024"),
                ],
                true,
            ),
        )
        .await
        .expect("the unit commits before its reindex");
    for index in 0..3 {
        let result = &response["results"][index]["result"];
        assert_eq!(result["id"], created["id"], "{response}");
        assert!(
            result["reindex_error"].is_string(),
            "items[{index}]: {response}"
        );
    }
    let holder = key_holder(&registry, "paper:smith2024")
        .await
        .expect("holder");
    assert_eq!(holder["name"], json!("Smith and Jones 2024"), "{holder}");
}
