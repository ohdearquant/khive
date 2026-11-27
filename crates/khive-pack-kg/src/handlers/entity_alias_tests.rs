//! Entity aliases through the kg verbs (ADR-221 section 2).

use khive_runtime::{RuntimeError, VerbRegistry};
use serde_json::{json, Value};

async fn configured() -> VerbRegistry {
    use crate::KgPack;
    use khive_runtime::VerbRegistryBuilder;

    let rt = khive_runtime::KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let registry = builder.build().expect("kg registry builds");
    rt.install_edge_rules(registry.all_edge_rules());
    registry
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
        caller(&["y"]),
        "a caller alias equal to the new name is removed"
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
