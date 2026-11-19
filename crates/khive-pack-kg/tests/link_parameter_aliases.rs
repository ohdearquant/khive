use khive_runtime::{EdgeListFilter, KhiveRuntime, Namespace, VerbRegistry, VerbRegistryBuilder};
use khive_storage::EdgeRelation;
use serde_json::{json, Value};
use uuid::Uuid;

async fn fixture() -> (VerbRegistry, KhiveRuntime, Value) {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let mut ids = Vec::new();
    for name in ["source concept", "target concept"] {
        let (entity, _) = runtime
            .create_entity_with_embedding_report(&token, "concept", None, name, None, None, vec![])
            .await
            .unwrap();
        ids.push(entity.id);
    }
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    let registry = builder.build().unwrap();
    runtime.install_edge_rules(registry.all_edge_rules());
    let args = json!({
        "source_id": ids[0], "target_id": ids[1], "relation": "contains",
        "weight": 0.75, "metadata": {"evidence": "caller spellings"}
    });
    (registry, runtime, args)
}

async fn stored(runtime: &KhiveRuntime, id: &Value) -> Value {
    let token = runtime.authorize(Namespace::local()).unwrap();
    let edge = runtime
        .get_edge(&token, id.as_str().unwrap().parse().unwrap())
        .await
        .unwrap()
        .unwrap();
    serde_json::to_value(edge).unwrap()
}

fn spelling(mut args: Value, canonical: &str, alias: &str) -> Value {
    let map = args.as_object_mut().unwrap();
    let value = map.remove(canonical).unwrap();
    map.insert(alias.into(), value);
    args
}

#[tokio::test]
async fn each_link_alias_resolves_to_the_same_stored_natural_edge() {
    let (registry, runtime, args) = fixture().await;
    let canonical = registry.dispatch("link", args.clone()).await.unwrap();
    let expected = stored(&runtime, &canonical["id"]).await;
    for (name, alias) in [
        ("source_id", "source"),
        ("target_id", "target"),
        ("relation", "kind"),
    ] {
        let result = registry
            .dispatch("link", spelling(args.clone(), name, alias))
            .await
            .unwrap();
        assert_eq!(result["id"], canonical["id"]);
        let actual = stored(&runtime, &result["id"]).await;
        for key in ["source_id", "target_id", "relation", "weight", "metadata"] {
            assert_eq!(actual[key], expected[key], "{alias}: {key}");
        }
    }
    let aliases = spelling(
        spelling(
            spelling(args.clone(), "source_id", "source"),
            "target_id",
            "target",
        ),
        "relation",
        "kind",
    );
    let result = registry.dispatch("link", aliases).await.unwrap();
    assert_eq!(result["id"], canonical["id"]);
    let token = runtime.authorize(Namespace::local()).unwrap();
    let edges = runtime
        .list_edges(
            &token,
            EdgeListFilter {
                relations: vec![EdgeRelation::Contains],
                ..Default::default()
            },
            10,
            0,
        )
        .await
        .unwrap();
    assert_eq!(edges.len(), 1);
}

#[tokio::test]
async fn link_both_spellings_refuse_even_equal_or_null_before_updating_the_edge() {
    let (registry, runtime, args) = fixture().await;
    let canonical = registry.dispatch("link", args.clone()).await.unwrap();
    let before = stored(&runtime, &canonical["id"]).await;
    for (name, alias) in [
        ("source_id", "source"),
        ("target_id", "target"),
        ("relation", "kind"),
    ] {
        for (canonical_value, alias_value) in [
            (args[name].clone(), args[name].clone()),
            (args[name].clone(), json!("different")),
            (Value::Null, args[name].clone()),
            (args[name].clone(), Value::Null),
            (Value::Null, Value::Null),
        ] {
            let mut conflicting = args.clone();
            conflicting[name] = canonical_value;
            conflicting[alias] = alias_value;
            conflicting["weight"] = json!(0.1);
            let error = registry
                .dispatch("link", conflicting)
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(&format!("`{alias}` is an alias for `{name}`")),
                "{error}"
            );
            assert!(error.contains("supply only one"), "{error}");
            assert_eq!(stored(&runtime, &canonical["id"]).await, before);
        }
    }
}

#[tokio::test]
async fn link_alias_help_and_unknown_fields_keep_the_canonical_contract() {
    let (registry, runtime, args) = fixture().await;
    let canonical = registry.dispatch("link", args.clone()).await.unwrap();
    let before = stored(&runtime, &canonical["id"]).await;
    let help = registry
        .dispatch("link", json!({"help": true}))
        .await
        .unwrap();
    let params = help["params"].as_array().unwrap();
    for (name, alias) in [
        ("source_id", "source"),
        ("target_id", "target"),
        ("relation", "kind"),
    ] {
        assert!(!params.iter().any(|param| param["name"] == alias));
        let param = params.iter().find(|param| param["name"] == name).unwrap();
        assert!(param["description"]
            .as_str()
            .unwrap()
            .contains(&format!("`{alias}` is accepted as an alias for `{name}`")));
        let mut canonical_typo = args.clone();
        canonical_typo["misspelled"] = json!(true);
        let expected = registry
            .dispatch("link", canonical_typo.clone())
            .await
            .unwrap_err()
            .to_string();
        let actual = registry
            .dispatch("link", spelling(canonical_typo, name, alias))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(actual, expected);
        for field in [
            "misspelled",
            "source_id",
            "target_id",
            "relation",
            "links",
            "atomic",
        ] {
            assert!(actual.contains(&format!("`{field}`")), "{actual}");
        }
        assert_eq!(stored(&runtime, &canonical["id"]).await, before);
    }
}

#[tokio::test]
async fn singleton_aliases_do_not_change_bulk_entry_validation_or_endpoint_rules() {
    let (registry, runtime, args) = fixture().await;
    let token = runtime.authorize(Namespace::local()).unwrap();
    for atomic in [false, true] {
        for (canonical, alias) in [
            ("source_id", "source"),
            ("target_id", "target"),
            ("relation", "kind"),
        ] {
            let entry = spelling(args.clone(), canonical, alias);
            let error = registry
                .dispatch("link", json!({"links": [entry], "atomic": atomic}))
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(&format!("unknown field `{alias}`")),
                "{error}"
            );
            assert!(error.contains(&format!("`{canonical}`")), "{error}");
        }
    }
    let mut invalid_relation = spelling(args, "relation", "kind");
    invalid_relation["kind"] = json!("made_up_relation");
    assert!(registry.dispatch("link", invalid_relation).await.is_err());
    assert!(runtime
        .list_edges(&token, EdgeListFilter::default(), 10, 0)
        .await
        .unwrap()
        .is_empty());
    let missing = json!({"source": Uuid::new_v4(), "target": Uuid::new_v4(), "kind": "contains"});
    assert!(registry.dispatch("link", missing).await.is_err());
}
