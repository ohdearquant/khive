//! Entity aliases through the kg verbs (ADR-221 section 2).

use khive_db::stores::entity::entity_alias_insert_statement;
use khive_runtime::atomic_prepare::prepare_update;
use khive_runtime::{AtomicOpPlan, KhiveRuntime, Namespace, RuntimeError, VerbRegistry};
use khive_storage::entity::EntityAliasOrigin;
use serde_json::{json, Value};

async fn configured() -> VerbRegistry {
    configured_with_runtime().await.0
}

async fn configured_with_runtime() -> (VerbRegistry, KhiveRuntime) {
    use crate::KgPack;
    use khive_runtime::VerbRegistryBuilder;

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let registry = builder.build().expect("kg registry builds");
    rt.install_edge_rules(registry.all_edge_rules());
    (registry, rt)
}

/// Store an alias row for entity `id` directly, as a rename or a merge
/// would write it.
async fn seed_alias(
    rt: &KhiveRuntime,
    registry: &VerbRegistry,
    id: &Value,
    alias: &str,
    origin: EntityAliasOrigin,
) {
    let got = registry
        .dispatch("get", json!({"id": id}))
        .await
        .expect("get");
    let namespace = got["namespace"].as_str().expect("namespace");
    let id = uuid::Uuid::parse_str(id.as_str().expect("id")).expect("uuid");
    rt.sql()
        .writer()
        .await
        .expect("writer")
        .execute(entity_alias_insert_statement(
            namespace, id, alias, origin, 1,
        ))
        .await
        .expect("seed alias");
}

/// A concept create; `aliases` is left out when empty.
fn concept(name: &str, aliases: &[&str]) -> Value {
    let mut args = json!({"kind": "concept", "name": name, "skip_dedup_check": true});
    if !aliases.is_empty() {
        args["aliases"] = json!(aliases);
    }
    args
}

fn keyed(mut args: Value, key: &str) -> Value {
    args["key"] = json!(key);
    args
}

async fn create(registry: &VerbRegistry, args: Value) -> Value {
    registry.dispatch("create", args).await.expect("create")
}

async fn update(registry: &VerbRegistry, args: Value) -> Value {
    registry.dispatch("update", args).await.expect("update")
}

/// The entity's `aliases` as `get` returns them; `Null` when the field is
/// absent.
async fn aliases_of(registry: &VerbRegistry, id: &Value) -> Value {
    let got = registry
        .dispatch("get", json!({"id": id}))
        .await
        .expect("get");
    got.get("aliases").cloned().unwrap_or(Value::Null)
}

fn caller(aliases: &[&str]) -> Value {
    Value::Array(
        aliases
            .iter()
            .map(|alias| json!({"alias": alias, "origin": "caller"}))
            .collect(),
    )
}

fn reason(error: &RuntimeError) -> Option<String> {
    match error {
        RuntimeError::Khive(error) => error
            .details()
            .and_then(|details| details.get("reason").map(str::to_owned)),
        _ => None,
    }
}

/// A credential-shaped value the runtime secret gate refuses, built at run
/// time so the source holds no such literal.
fn credential() -> String {
    format!("ghp_{}", "A".repeat(36))
}

#[tokio::test]
async fn keyed_replay_compares_caller_aliases_as_a_set() {
    let registry = configured().await;
    let created = create(&registry, keyed(concept("Smith 2024", &["b", "a"]), "k")).await;
    assert_eq!(created["created"], json!(true), "{created}");
    let id = created["id"].clone();

    let replayed = create(
        &registry,
        keyed(concept("Smith 2024", &["a", "b", "a"]), "k"),
    )
    .await;
    assert_eq!(replayed, json!({"id": id, "created": false}));

    for aliases in [&[][..], &["a"][..], &["a", "b", "c"][..]] {
        let error = registry
            .dispatch("create", keyed(concept("Smith 2024", aliases), "k"))
            .await
            .expect_err("a replay whose aliases differ");
        assert_eq!(
            reason(&error).as_deref(),
            Some("key_conflict"),
            "{aliases:?}: {error}"
        );
    }
    assert_eq!(aliases_of(&registry, &id).await, caller(&["a", "b"]));
}

#[tokio::test]
async fn on_key_update_replaces_the_caller_aliases() {
    let registry = configured().await;
    let created = create(&registry, keyed(concept("Smith 2024", &["a", "b"]), "k")).await;
    let id = created["id"].clone();

    let mut reload = keyed(concept("Smith 2024", &["c", "Smith 2024"]), "k");
    reload["on_key"] = json!("update");
    let updated = create(&registry, reload).await;
    assert_eq!(updated["updated"], json!(true), "{updated}");
    assert_eq!(
        updated["aliases_not_stored"],
        json!(["Smith 2024"]),
        "{updated}"
    );
    assert_eq!(aliases_of(&registry, &id).await, caller(&["c"]));

    let mut bare = keyed(concept("Smith 2024", &[]), "k");
    bare["on_key"] = json!("update");
    let cleared = create(&registry, bare).await;
    assert_eq!(cleared["updated"], json!(true), "{cleared}");
    assert_eq!(
        aliases_of(&registry, &id).await,
        Value::Null,
        "an absent aliases means none, and reads omit the empty field"
    );
}

#[tokio::test]
async fn a_refused_alias_is_refused_before_any_lookup() {
    let registry = configured().await;
    create(&registry, keyed(concept("Smith 2024", &[]), "k")).await;

    let mut refused = keyed(concept("Smith 2024", &[]), "k");
    refused["aliases"] = json!([credential()]);
    let error = registry
        .dispatch("create", refused)
        .await
        .expect_err("a credential alias under a held key");
    assert!(
        matches!(error, RuntimeError::SecretDetected(_)),
        "{error:?}"
    );

    // No entity holds this id, so a lookup first would answer not found.
    let missing = uuid::Uuid::new_v4().to_string();
    for field in ["add_aliases", "remove_aliases"] {
        let error = registry
            .dispatch(
                "update",
                json!({"id": missing, "kind": "entity", field: [credential()]}),
            )
            .await
            .expect_err("a credential alias on update");
        assert!(
            matches!(error, RuntimeError::SecretDetected(_)),
            "{field}: {error:?}"
        );
    }
}

#[tokio::test]
async fn resolve_by_alias_names_the_alias_and_text_search_finds_it() {
    let registry = configured().await;
    let paper = create(
        &registry,
        concept("Attention Is All You Need", &["Vaswani2017"]),
    )
    .await;
    let id = paper["id"].as_str().expect("id").to_owned();

    let resolved = registry
        .dispatch(
            "resolve",
            json!({"refs": ["Vaswani2017", "Attention Is All You Need"]}),
        )
        .await
        .expect("resolve");
    let by_alias = &resolved["results"][0];
    assert_eq!(by_alias["status"], json!("resolved"), "{resolved}");
    assert_eq!(by_alias["id"], json!(id), "{resolved}");
    assert_eq!(
        by_alias["matched_alias"],
        json!("Vaswani2017"),
        "{resolved}"
    );
    let by_name = &resolved["results"][1];
    assert_eq!(by_name["id"], json!(id), "{resolved}");
    assert!(by_name.get("matched_alias").is_none(), "{resolved}");

    let found = registry
        .dispatch("search", json!({"kind": "entity", "query": "Vaswani2017"}))
        .await
        .expect("search");
    assert!(found.to_string().contains(&id), "{found}");

    let other = create(&registry, concept("Other Paper", &["SharedAlias"])).await;
    let other_id = other["id"].as_str().expect("id").to_owned();
    update(&registry, json!({"id": id, "add_aliases": ["SharedAlias"]})).await;
    let shared = registry
        .dispatch("resolve", json!({"refs": ["SharedAlias"]}))
        .await
        .expect("resolve a shared alias");
    let answer = &shared["results"][0];
    assert_eq!(answer["status"], json!("ambiguous"), "{shared}");
    let mut ids: Vec<&str> = answer["candidates"]
        .as_array()
        .expect("candidates")
        .iter()
        .map(|candidate| candidate["id"].as_str().expect("candidate id"))
        .collect();
    ids.sort_unstable();
    let mut want = vec![id.as_str(), other_id.as_str()];
    want.sort_unstable();
    assert_eq!(ids, want, "{shared}");
}

#[tokio::test]
async fn an_alias_equal_to_the_name_is_not_stored() {
    let registry = configured().await;
    let created = create(&registry, concept("N", &["N", "x"])).await;
    assert_eq!(created["aliases_not_stored"], json!(["N"]), "{created}");
    let id = created["id"].clone();
    assert_eq!(aliases_of(&registry, &id).await, caller(&["x"]));

    let updated = update(&registry, json!({"id": id, "add_aliases": ["N", "y"]})).await;
    assert_eq!(updated["aliases_not_stored"], json!(["N"]), "{updated}");
    assert_eq!(aliases_of(&registry, &id).await, caller(&["x", "y"]));

    let renamed = update(&registry, json!({"id": id, "name": "x"})).await;
    assert!(renamed.get("aliases_not_stored").is_none(), "{renamed}");
    assert_eq!(
        aliases_of(&registry, &id).await,
        json!([
            {"alias": "N", "origin": "rename"},
            {"alias": "y", "origin": "caller"}
        ]),
        "a caller alias equal to the new name is removed and the old name is kept"
    );

    for atomic in [true, false] {
        let name = format!("B{atomic}");
        let bulk = create(
            &registry,
            json!({"atomic": atomic, "items": [{"kind": "concept", "name": name, "aliases": [name, "z"]}]}),
        )
        .await;
        let item = &bulk["results"][0]["result"];
        assert_eq!(item["aliases_not_stored"], json!([name]), "{bulk}");
        assert_eq!(aliases_of(&registry, &item["id"]).await, caller(&["z"]));
    }
}

#[tokio::test]
async fn remove_aliases_lists_what_it_did_not_remove() {
    let registry = configured().await;
    let created = create(&registry, concept("R", &["a", "b"])).await;
    let id = created["id"].clone();

    let updated = update(&registry, json!({"id": id, "remove_aliases": ["a", "zz"]})).await;
    assert_eq!(updated["aliases_not_removed"], json!(["zz"]), "{updated}");
    assert_eq!(aliases_of(&registry, &id).await, caller(&["b"]));

    let updated = update(&registry, json!({"id": id, "remove_aliases": ["b"]})).await;
    assert!(updated.get("aliases_not_removed").is_none(), "{updated}");
    assert_eq!(aliases_of(&registry, &id).await, Value::Null);

    let error = registry
        .dispatch(
            "update",
            json!({"id": id, "add_aliases": ["q"], "remove_aliases": ["q"]}),
        )
        .await
        .expect_err("one alias both added and removed");
    assert!(
        format!("{error}").contains("both added and removed"),
        "{error}"
    );
}

#[tokio::test]
async fn keyed_replay_leaves_out_aliases_held_under_another_origin() {
    let (registry, rt) = configured_with_runtime().await;
    let created = create(&registry, keyed(concept("N", &["a"]), "k")).await;
    let id = created["id"].clone();
    seed_alias(&rt, &registry, &id, "x", EntityAliasOrigin::Rename).await;
    seed_alias(&rt, &registry, &id, "y", EntityAliasOrigin::Merge).await;
    let held = registry
        .dispatch("get", json!({"id": id}))
        .await
        .expect("get");

    for aliases in [&["a", "x"][..], &["a", "x", "y"][..], &["y", "a"][..]] {
        let replayed = create(&registry, keyed(concept("N", aliases), "k")).await;
        assert_eq!(replayed, json!({"id": id, "created": false}), "{aliases:?}");
        let mut reload = keyed(concept("N", aliases), "k");
        reload["on_key"] = json!("update");
        let replayed = create(&registry, reload).await;
        assert_eq!(
            replayed,
            json!({"id": id, "created": false}),
            "on_key {aliases:?}"
        );
        for on_key in [false, true] {
            let mut item = json!({"kind": "concept", "name": "N", "key": "k", "aliases": aliases});
            if on_key {
                item["on_key"] = json!("update");
            }
            let bulk = create(&registry, json!({"items": [item]})).await;
            let result = &bulk["results"][0]["result"];
            assert_eq!(result["created"], json!(false), "{aliases:?}: {bulk}");
            assert!(result.get("updated").is_none(), "{aliases:?}: {bulk}");
        }
    }
    let again = registry
        .dispatch("get", json!({"id": id}))
        .await
        .expect("get");
    assert_eq!(again, held, "a replay writes nothing");

    // A caller alias the request leaves out still differs.
    let error = registry
        .dispatch("create", keyed(concept("N", &["x"]), "k"))
        .await
        .expect_err("the caller alias a is missing");
    assert_eq!(reason(&error).as_deref(), Some("key_conflict"), "{error}");
}

#[tokio::test]
async fn a_rename_onto_an_alias_of_any_origin_removes_that_row() {
    let (registry, rt) = configured_with_runtime().await;
    let created = create(&registry, concept("N", &["c"])).await;
    let id = created["id"].clone();
    seed_alias(&rt, &registry, &id, "r", EntityAliasOrigin::Rename).await;
    seed_alias(&rt, &registry, &id, "m", EntityAliasOrigin::Merge).await;

    let renamed = update(
        &registry,
        json!({"id": id, "name": "r", "keep_old_name": false}),
    )
    .await;
    assert_eq!(renamed["name"], json!("r"), "{renamed}");
    assert_eq!(
        aliases_of(&registry, &id).await,
        json!([
            {"alias": "c", "origin": "caller"},
            {"alias": "m", "origin": "merge"}
        ]),
        "a rename-origin alias equal to the new name is removed"
    );

    update(
        &registry,
        json!({"id": id, "name": "m", "keep_old_name": false}),
    )
    .await;
    assert_eq!(
        aliases_of(&registry, &id).await,
        caller(&["c"]),
        "a merge-origin alias equal to the new name is removed"
    );

    // A caller removal of the new name reads as done, not as not removed.
    seed_alias(&rt, &registry, &id, "s", EntityAliasOrigin::Merge).await;
    let renamed = update(
        &registry,
        json!({"id": id, "name": "s", "remove_aliases": ["s"], "keep_old_name": false}),
    )
    .await;
    assert!(renamed.get("aliases_not_removed").is_none(), "{renamed}");
    assert_eq!(aliases_of(&registry, &id).await, caller(&["c"]));
}

async fn resolve_one(registry: &VerbRegistry, name: &str) -> Value {
    let resolved = registry
        .dispatch("resolve", json!({"refs": [name]}))
        .await
        .expect("resolve");
    resolved["results"][0].clone()
}

/// ADR-221 acceptance 8: a rename keeps the old name as a `rename` alias
/// and resolve finds the entity by it.
#[tokio::test]
async fn a_rename_keeps_the_old_name_and_resolve_finds_it() {
    let registry = configured().await;
    let created = create(&registry, concept("Old Name", &[])).await;
    let id = created["id"].clone();
    update(&registry, json!({"id": id, "name": "New Name"})).await;
    assert_eq!(
        aliases_of(&registry, &id).await,
        json!([{"alias": "Old Name", "origin": "rename"}])
    );
    let by_old = resolve_one(&registry, "Old Name").await;
    assert_eq!(by_old["status"], json!("resolved"), "{by_old}");
    assert_eq!(by_old["id"], id, "{by_old}");
    assert_eq!(by_old["matched_alias"], json!("Old Name"), "{by_old}");

    // A caller cannot remove a rename alias.
    let answer = update(&registry, json!({"id": id, "remove_aliases": ["Old Name"]})).await;
    assert_eq!(
        answer["aliases_not_removed"],
        json!(["Old Name"]),
        "{answer}"
    );
}

/// ADR-221 acceptance 9: a split that hands the old name to the parent
/// renames the child with `keep_old_name: false`, so the old name resolves
/// to the parent only.
#[tokio::test]
async fn keep_old_name_false_leaves_the_old_name_to_another_entity() {
    let registry = configured().await;
    let child = create(&registry, concept("Shared Name", &[])).await;
    let child_id = child["id"].clone();
    update(
        &registry,
        json!({"id": child_id, "name": "Child Name", "keep_old_name": false}),
    )
    .await;
    assert_eq!(aliases_of(&registry, &child_id).await, Value::Null);
    let parent = create(&registry, concept("Shared Name", &[])).await;
    let by_old = resolve_one(&registry, "Shared Name").await;
    assert_eq!(by_old["status"], json!("resolved"), "{by_old}");
    assert_eq!(by_old["id"], parent["id"], "{by_old}");
}

#[tokio::test]
async fn keep_old_name_is_refused_off_entities_and_must_be_a_boolean() {
    let registry = configured().await;
    let note = registry
        .dispatch(
            "create",
            json!({"kind": "observation", "name": "n", "content": "c"}),
        )
        .await
        .expect("create note");
    let error = registry
        .dispatch(
            "update",
            json!({"id": note["id"], "name": "m", "keep_old_name": false}),
        )
        .await
        .expect_err("keep_old_name on a note");
    assert!(error.to_string().contains("keep_old_name"), "{error}");

    let entity = create(&registry, concept("E", &[])).await;
    registry
        .dispatch(
            "update",
            json!({"id": entity["id"], "name": "F", "keep_old_name": "no"}),
        )
        .await
        .expect_err("a string keep_old_name");
}

/// A keyed `on_key: update` reload that changes the holder's name is an
/// update of the held entity, so it keeps the old name as an alias.
#[tokio::test]
async fn a_keyed_reload_that_renames_keeps_the_old_name() {
    let registry = configured().await;
    let created = create(&registry, keyed(concept("Before", &["c"]), "k")).await;
    let id = created["id"].clone();
    let mut reload = keyed(concept("After", &["c"]), "k");
    reload["on_key"] = json!("update");
    let reloaded = create(&registry, reload).await;
    assert_eq!(reloaded["id"], id, "{reloaded}");
    assert_eq!(
        aliases_of(&registry, &id).await,
        json!([
            {"alias": "Before", "origin": "rename"},
            {"alias": "c", "origin": "caller"}
        ])
    );
    let by_old = resolve_one(&registry, "Before").await;
    assert_eq!(by_old["id"], id, "{by_old}");
}

#[tokio::test]
async fn the_atomic_update_route_refuses_a_bad_alias_before_any_lookup() {
    let (_registry, rt) = configured_with_runtime().await;
    let token = rt.authorize(Namespace::local()).expect("authorize");
    // No entity holds this id, so a lookup first would answer not found.
    let missing = uuid::Uuid::new_v4();
    for field in ["add_aliases", "remove_aliases"] {
        let error = prepare_update(
            &rt,
            &token,
            &json!({"id": missing, field: [credential()]}),
            None,
        )
        .await
        .expect_err("a credential alias on the atomic route");
        assert!(
            matches!(error, RuntimeError::SecretDetected(_)),
            "{field}: {error:?}"
        );
    }
    let error = prepare_update(
        &rt,
        &token,
        &json!({"id": missing, "add_aliases": ["fine"]}),
        None,
    )
    .await
    .expect_err("a valid alias on a missing id");
    assert!(matches!(error, RuntimeError::NotFound(_)), "{error:?}");
}

#[tokio::test]
async fn an_alias_only_update_that_changes_nothing_writes_nothing() {
    let (registry, rt) = configured_with_runtime().await;
    let created = create(&registry, concept("R", &["a"])).await;
    let id = created["id"].clone();
    let before = registry
        .dispatch("get", json!({"id": id}))
        .await
        .expect("get");
    assert!(before.get("version").is_some(), "{before}");

    let answer = update(&registry, json!({"id": id, "remove_aliases": ["zz"]})).await;
    assert_eq!(answer["aliases_not_removed"], json!(["zz"]), "{answer}");
    let answer = update(&registry, json!({"id": id, "add_aliases": ["R"]})).await;
    assert_eq!(answer["aliases_not_stored"], json!(["R"]), "{answer}");
    for args in [
        json!({"id": id, "remove_aliases": ["zz"]}),
        json!({"id": id, "add_aliases": ["a"]}),
        json!({"id": id, "add_aliases": ["R"]}),
    ] {
        let answer = update(&registry, args.clone()).await;
        assert_eq!(answer["version"], before["version"], "{args}: {answer}");
        assert_eq!(
            answer["updated_at"], before["updated_at"],
            "{args}: {answer}"
        );
    }
    let after = registry
        .dispatch("get", json!({"id": id}))
        .await
        .expect("get");
    assert_eq!(after, before, "an unchanged alias update writes nothing");

    let token = rt.authorize(Namespace::local()).expect("authorize");
    let noop = |plan: &AtomicOpPlan| matches!(plan, AtomicOpPlan::Update(update) if update.is_idempotent_noop());
    let plan = prepare_update(
        &rt,
        &token,
        &json!({"id": id, "remove_aliases": ["zz"]}),
        None,
    )
    .await
    .expect("atomic alias-only update");
    assert!(noop(&plan), "an unchanged alias update plans no write");

    // Controls: an alias change still writes on both routes.
    let plan = prepare_update(&rt, &token, &json!({"id": id, "add_aliases": ["b"]}), None)
        .await
        .expect("atomic alias change");
    assert!(!noop(&plan), "an alias change plans a write");
    let changed = update(&registry, json!({"id": id, "add_aliases": ["b"]})).await;
    assert_ne!(changed["version"], before["version"], "{changed}");
}

#[tokio::test]
async fn aliases_on_a_note_are_refused() {
    let registry = configured().await;
    let error = registry
        .dispatch(
            "create",
            json!({"kind": "observation", "content": "c", "aliases": ["a"]}),
        )
        .await
        .expect_err("aliases on a note create");
    assert!(
        format!("{error}").contains("aliases apply only to entities"),
        "{error}"
    );

    let error = registry
        .dispatch(
            "create",
            json!({"items": [{"kind": "concept", "name": "I"}], "aliases": ["a"]}),
        )
        .await
        .expect_err("top-level aliases beside items");
    assert!(
        format!("{error}").contains("an entity item carries its own key and aliases"),
        "{error}"
    );

    let note = create(&registry, json!({"kind": "observation", "content": "c"})).await;
    for field in ["add_aliases", "remove_aliases"] {
        let error = registry
            .dispatch("update", json!({"id": note["id"], field: ["a"]}))
            .await
            .expect_err("aliases on a note update");
        assert!(format!("{error}").contains(field), "{field}: {error}");
    }
}

fn forced_merge(into: &Value, from: &Value, strategy: &str) -> Value {
    json!({
        "kind": "entity",
        "into_id": into,
        "from_id": from,
        "strategy": strategy,
        "force": true
    })
}

/// `aliases` sorted by alias, so a comparison does not depend on row order.
fn sorted(aliases: Value) -> Value {
    let mut rows = aliases.as_array().cloned().unwrap_or_default();
    rows.sort_by(|a, b| a["alias"].as_str().cmp(&b["alias"].as_str()));
    Value::Array(rows)
}

/// ADR-221 acceptance 10 and 14: a merge gives the kept entity the
/// merged-away entity's name and aliases as `merge` aliases, skips a name
/// the kept entity already holds or bears, and leaves the merged-away
/// entity no alias rows.
#[tokio::test]
async fn a_merge_moves_the_names_and_stores_none_equal_to_the_kept_name() {
    let registry = configured().await;
    let into = create(&registry, concept("Alpha", &["a1", "shared"])).await["id"].clone();
    let from = create(&registry, concept("Beta", &["bee1", "shared", "Alpha"])).await["id"].clone();

    let mut dry = forced_merge(&into, &from, "prefer_into");
    dry["dry_run"] = json!(true);
    let preview = registry.dispatch("merge", dry).await.expect("dry run");
    assert_eq!(
        preview["aliases_added"],
        json!(["Beta", "bee1"]),
        "{preview}"
    );
    assert_eq!(
        aliases_of(&registry, &into).await,
        caller(&["a1", "shared"]),
        "a dry run writes nothing"
    );

    let merged = registry
        .dispatch("merge", forced_merge(&into, &from, "prefer_into"))
        .await
        .expect("merge");
    assert_eq!(merged["aliases_added"], json!(["Beta", "bee1"]), "{merged}");
    assert_eq!(
        sorted(aliases_of(&registry, &into).await),
        json!([
            {"alias": "Beta", "origin": "merge"},
            {"alias": "a1", "origin": "caller"},
            {"alias": "bee1", "origin": "merge"},
            {"alias": "shared", "origin": "caller"}
        ])
    );
    let tombstone = registry
        .dispatch("get", json!({"id": from, "include_deleted": true}))
        .await
        .expect("tombstone");
    assert!(
        tombstone
            .get("aliases")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty),
        "the merged-away entity keeps no alias rows: {tombstone}"
    );

    let by_old = resolve_one(&registry, "Beta").await;
    assert_eq!(by_old["status"], json!("resolved"), "{by_old}");
    assert_eq!(by_old["id"], into, "{by_old}");
    let by_moved = resolve_one(&registry, "bee1").await;
    assert_eq!(by_moved["status"], json!("resolved"), "{by_moved}");
    assert_eq!(by_moved["id"], into, "{by_moved}");
    assert_eq!(by_moved["matched_alias"], json!("bee1"), "{by_moved}");
    let by_name = resolve_one(&registry, "Alpha").await;
    assert_eq!(by_name["status"], json!("resolved"), "{by_name}");
    assert_eq!(by_name["id"], into, "{by_name}");
    let found = registry
        .dispatch(
            "search",
            json!({"kind": "entity", "query": "bee1", "limit": 10}),
        )
        .await
        .expect("search by a moved alias");
    let ids: Vec<Value> = found
        .as_array()
        .expect("search rows")
        .iter()
        .map(|row| row["id"].clone())
        .collect();
    assert_eq!(ids, vec![into.clone()], "{found}");
}

/// ADR-221 acceptance 14: a merge whose strategy takes the merged-away
/// entity's name drops the kept entity's alias equal to that name and keeps
/// the kept entity's previous name as a `merge` alias.
#[tokio::test]
async fn a_merge_that_takes_the_other_name_keeps_the_previous_name() {
    let registry = configured().await;
    let into = create(&registry, concept("Gamma", &["Delta"])).await["id"].clone();
    let from = create(&registry, concept("Delta", &["d1"])).await["id"].clone();

    let merged = registry
        .dispatch("merge", forced_merge(&into, &from, "prefer_from"))
        .await
        .expect("merge");
    assert_eq!(merged["aliases_added"], json!(["d1", "Gamma"]), "{merged}");
    let kept = registry
        .dispatch("get", json!({"id": into}))
        .await
        .expect("kept entity");
    assert_eq!(kept["name"], json!("Delta"), "{kept}");
    assert_eq!(
        sorted(aliases_of(&registry, &into).await),
        json!([
            {"alias": "Gamma", "origin": "merge"},
            {"alias": "d1", "origin": "merge"}
        ])
    );
    let by_name = resolve_one(&registry, "Delta").await;
    assert_eq!(by_name["status"], json!("resolved"), "{by_name}");
    assert_eq!(by_name["id"], into, "{by_name}");
    let by_previous = resolve_one(&registry, "Gamma").await;
    assert_eq!(by_previous["id"], into, "{by_previous}");
    assert_eq!(
        by_previous["matched_alias"],
        json!("Gamma"),
        "{by_previous}"
    );
}
