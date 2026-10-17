use crate::KgPack;
use khive_runtime::{KhiveRuntime, Namespace, NamespaceToken, VerbRegistry, VerbRegistryBuilder};
use serde_json::{json, Value};

async fn pair(
    source_kind: &str,
    target_kind: &str,
) -> (KgPack, NamespaceToken, VerbRegistry, Value) {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let source = runtime
        .create_entity_with_embedding_report(
            &token,
            source_kind,
            None,
            "source",
            None,
            None,
            vec![],
        )
        .await
        .unwrap()
        .0;
    let target = runtime
        .create_entity_with_embedding_report(
            &token,
            target_kind,
            None,
            "target",
            None,
            None,
            vec![],
        )
        .await
        .unwrap()
        .0;
    let pack = KgPack::new(runtime.clone());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    let registry = builder.build().unwrap();
    runtime.install_edge_rules(registry.all_edge_rules());
    (
        pack,
        token,
        registry,
        json!({"source_id": source.id.to_string(), "target_id": target.id.to_string(), "relation": "depends_on"}),
    )
}

#[tokio::test]
async fn every_inferred_dependency_kind_is_disclosed_without_persisting_the_hint() {
    for (source, target, expected) in [
        ("project", "project", "build"),
        ("service", "service", "runtime"),
        ("service", "dataset", "data"),
        ("service", "artifact", "artifact"),
        ("artifact", "project", "tooling"),
        ("artifact", "service", "tooling"),
        ("document", "document", "normative"),
    ] {
        let (pack, token, registry, params) = pair(source, target).await;
        let response = pack.handle_link(&token, params, &registry).await.unwrap();
        let hint = response["hint"].as_str().expect("inference hint");
        assert!(hint.contains(&format!("{expected:?}")), "{response}");
        assert_eq!(hint.contains("links_to"), expected == "normative");
        assert_eq!(response["metadata"]["dependency_kind"], expected);
        assert_eq!(response["mutation"], "created");
        let stored = pack
            .runtime
            .list_edges(&token, Default::default(), 10, 0)
            .await
            .unwrap();
        assert_eq!(stored.len(), 1);
        let edge = serde_json::to_value(&stored[0]).unwrap();
        assert_eq!(edge["metadata"]["dependency_kind"], expected);
        assert!(edge.get("hint").is_none());
        assert!(edge["metadata"].get("hint").is_none());
    }
}

#[tokio::test]
async fn explicit_qualifiers_suppress_hints_and_metadata_keeps_precedence() {
    let (pack, token, registry, base) = pair("document", "document").await;
    let inferred = pack
        .handle_link(&token, base.clone(), &registry)
        .await
        .unwrap();
    for (metadata, top_level, expected) in [
        (Value::Null, "normative", "normative"),
        (
            json!({"dependency_kind": "normative"}),
            "runtime",
            "normative",
        ),
        (
            json!({"dependency_kind": "data", "label": "keep"}),
            "runtime",
            "data",
        ),
    ] {
        let mut params = base.clone();
        params["metadata"] = metadata.clone();
        params["dependency_kind"] = json!(top_level);
        let response = pack.handle_link(&token, params, &registry).await.unwrap();
        assert!(response.get("hint").is_none(), "{response}");
        assert_eq!(response["metadata"]["dependency_kind"], expected);
        assert_eq!(response["id"], inferred["id"]);
        assert_eq!(response["mutation"], "updated");
        if metadata.get("label").is_some() {
            assert_eq!(response["metadata"]["label"], "keep");
        }
    }
}

#[tokio::test]
async fn absent_metadata_infers_but_invalid_qualifiers_still_refuse() {
    let (pack, token, registry, base) = pair("document", "document").await;
    for metadata in [Value::Null, json!({}), json!({"label": "keep"})] {
        let mut params = base.clone();
        params["metadata"] = metadata.clone();
        let response = pack.handle_link(&token, params, &registry).await.unwrap();
        assert!(response["hint"].as_str().unwrap().contains("normative"));
        if metadata.get("label").is_some() {
            assert_eq!(response["metadata"]["label"], "keep");
        }
    }
    for metadata in [
        json!({"dependency_kind": null}),
        json!({"dependency_kind": "unknown-kind"}),
        json!("not an object"),
    ] {
        let mut params = base.clone();
        params["metadata"] = metadata;
        assert!(pack.handle_link(&token, params, &registry).await.is_err());
    }
}

#[tokio::test]
async fn reference_relations_and_pairs_without_defaults_do_not_gain_hints() {
    let (pack, token, registry, params) = pair("service", "project").await;
    let response = pack.handle_link(&token, params, &registry).await.unwrap();
    assert!(response.get("hint").is_none());
    assert!(response["metadata"].get("dependency_kind").is_none());
    let (pack, token, registry, mut params) = pair("document", "document").await;
    params["relation"] = json!("links_to");
    let response = pack.handle_link(&token, params, &registry).await.unwrap();
    assert!(response.get("hint").is_none());
    assert!(response["metadata"].get("dependency_kind").is_none());
}

#[tokio::test]
async fn bulk_hints_preserve_caller_indices_skips_failures_and_verbosity() {
    for atomic in [true, false] {
        for verbose in [true, false] {
            let (pack, token, registry, base) = pair("document", "document").await;
            let third = pack
                .runtime
                .create_entity_with_embedding_report(
                    &token,
                    "document",
                    None,
                    "third",
                    None,
                    None,
                    vec![],
                )
                .await
                .unwrap()
                .0;
            let mut explicit = base.clone();
            explicit["target_id"] = json!(third.id.to_string());
            explicit["dependency_kind"] = json!("normative");
            let mut inferred = base.clone();
            inferred["source_id"] = base["target_id"].clone();
            inferred["target_id"] = json!(third.id.to_string());
            let mut entries = vec![base.clone(), base.clone(), explicit, inferred];
            if !atomic {
                let mut failed = base.clone();
                failed["relation"] = json!("not-a-relation");
                entries.push(failed);
            }
            let response = pack
                .handle_link(
                    &token,
                    json!({"links": entries.clone(), "atomic": atomic, "verbose": verbose}),
                    &registry,
                )
                .await
                .unwrap();
            assert_eq!(response["created"], 3, "{response}");
            assert_eq!(response["skipped"], 1, "{response}");
            assert_eq!(response["failed"], if atomic { 0 } else { 1 });
            let hints = response["hints"].as_array().unwrap();
            assert_eq!(hints.len(), 2);
            assert_eq!(hints[0]["index"], 0);
            assert_eq!(hints[1]["index"], 3);
            for hint in hints {
                assert!(hint["hint"].as_str().unwrap().contains("links_to"));
            }
            if verbose {
                let edges = response["edges"].as_array().unwrap();
                assert_eq!(edges.len(), 3);
                assert_eq!(edges[0]["hint"], hints[0]["hint"]);
                assert!(edges[1].get("hint").is_none());
                assert_eq!(edges[2]["hint"], hints[1]["hint"]);
            } else {
                assert!(response.get("edges").is_none());
            }
            if !atomic {
                assert_eq!(response["errors"][0]["index"], 4);
                entries.pop();
            }
            for entry in &mut entries {
                entry["dependency_kind"] = json!("normative");
            }
            let explicit = pack
                .handle_link(
                    &token,
                    json!({"links": entries, "atomic": atomic, "verbose": verbose}),
                    &registry,
                )
                .await
                .unwrap();
            assert_eq!(explicit["updated"], 3, "{explicit}");
            assert_eq!(explicit["skipped"], 1, "{explicit}");
            assert!(explicit.get("hints").is_none(), "{explicit}");
            if verbose {
                assert!(explicit["edges"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|edge| edge.get("hint").is_none()));
            }
        }
    }
}
