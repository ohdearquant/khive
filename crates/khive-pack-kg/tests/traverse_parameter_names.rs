//! Public traverse metadata and backwards-compatible root spellings.

use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, Namespace, VerbRegistry, VerbRegistryBuilder};
use serde_json::{json, Value};
use uuid::Uuid;

fn fixture() -> (KhiveRuntime, VerbRegistry) {
    let runtime = KhiveRuntime::memory().expect("private memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    let registry = builder.build().expect("KG registry");
    runtime.install_edge_rules(registry.all_edge_rules());
    (runtime, registry)
}

async fn stored_edge(runtime: &KhiveRuntime, id: Uuid) -> Value {
    let token = runtime.authorize(Namespace::local()).unwrap();
    let edge = runtime
        .get_edge(&token, id)
        .await
        .expect("stored edge lookup")
        .expect("actual link must persist its edge");
    serde_json::to_value(edge).unwrap()
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn traverse_help_prefers_required_ids_and_documents_legacy_aliases() {
    let (_runtime, registry) = fixture();
    let help = registry
        .dispatch("traverse", json!({"help": true}))
        .await
        .expect("public traverse help");
    assert_eq!(help["verb"], "traverse");
    assert_eq!(help["pack"], "kg");
    let params = help["params"].as_array().expect("help parameter array");
    assert_eq!(
        params.iter().filter(|param| param["name"] == "ids").count(),
        1
    );
    let ids = params
        .iter()
        .find(|param| param["name"] == "ids")
        .expect("public help must name the primary identifiers ids");
    assert_eq!(ids["type"], "array of uuid");
    assert_eq!(ids["required"], true);
    for alias in ["roots", "start_ids"] {
        assert!(!params.iter().any(|param| param["name"] == alias));
        assert!(
            ids["description"].as_str().unwrap().contains(alias),
            "canonical ids help must retain the accepted {alias} alias"
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn traverse_ids_and_legacy_aliases_return_the_same_nonempty_stored_graph() {
    let (runtime, registry) = fixture();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let mut ids = Vec::new();
    for name in ["Traversal alias root", "Traversal alias child"] {
        let (entity, _) = runtime
            .create_entity_with_embedding_report(&token, "concept", None, name, None, None, vec![])
            .await
            .expect("real concept fixture");
        ids.push(entity.id);
    }
    let root = ids[0];
    let child = ids[1];
    let linked = registry
        .dispatch(
            "link",
            json!({
                "source_id": root,
                "target_id": child,
                "relation": "contains",
                "weight": 0.75,
                "metadata": {"evidence": "traverse aliases"}
            }),
        )
        .await
        .expect("actual public link");
    let edge_id: Uuid = linked["id"].as_str().unwrap().parse().unwrap();
    let before = stored_edge(&runtime, edge_id).await;
    assert_eq!(before["source_id"], json!(root));
    assert_eq!(before["target_id"], json!(child));
    assert_eq!(before["relation"], "contains");
    assert_eq!(before["weight"], json!(0.75));
    assert_eq!(before["metadata"]["evidence"], "traverse aliases");

    let request = |spelling: &str| {
        let mut args = json!({
            "max_depth": 1,
            "direction": "out",
            "relations": ["contains"],
            "include_roots": true,
            "limit": 10
        });
        args[spelling] = json!([root]);
        args
    };
    let canonical = registry
        .dispatch("traverse", request("ids"))
        .await
        .expect("canonical ids traversal");
    let paths = canonical.as_array().expect("traversal path array");
    assert_eq!(paths.len(), 1, "one requested root must yield one path");
    let nodes = paths[0]["nodes"].as_array().expect("path nodes");
    assert_eq!(
        nodes.len(),
        2,
        "the actual contains edge must reach the child"
    );
    let node_ids: Vec<&str> = nodes
        .iter()
        .map(|node| node["id"].as_str().unwrap())
        .collect();
    let root_id = root.to_string();
    let child_id = child.to_string();
    assert_eq!(node_ids, [root_id.as_str(), child_id.as_str()]);
    assert_eq!(nodes[0]["name"], "Traversal alias root");
    assert_eq!(nodes[1]["name"], "Traversal alias child");
    assert!(nodes.iter().all(|node| node["kind"] == "concept"));

    for alias in ["roots", "start_ids"] {
        let actual = registry
            .dispatch("traverse", request(alias))
            .await
            .expect("legacy alias traversal");
        assert_eq!(
            actual, canonical,
            "{alias} must preserve the complete traversal result"
        );
        assert_eq!(stored_edge(&runtime, edge_id).await, before);
    }
}
