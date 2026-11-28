//! `search(follow=...)`: each hit carries the targets of one named relation.

use khive_runtime::{KhiveRuntime, VerbRegistry};
use serde_json::{json, Value};

fn configured() -> VerbRegistry {
    use crate::KgPack;
    use khive_runtime::VerbRegistryBuilder;

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let registry = builder.build().expect("kg registry builds");
    rt.install_edge_rules(registry.all_edge_rules());
    registry
}

async fn concept(registry: &VerbRegistry, name: &str) -> String {
    let created = registry
        .dispatch(
            "create",
            json!({"kind": "concept", "name": name, "skip_dedup_check": true}),
        )
        .await
        .expect("create");
    created["id"].as_str().expect("id").to_owned()
}

async fn link(registry: &VerbRegistry, source: &str, target: &str, relation: &str) {
    registry
        .dispatch(
            "link",
            json!({"source_id": source, "target_id": target, "relation": relation}),
        )
        .await
        .expect("link");
}

fn hit<'a>(rows: &'a Value, id: &str) -> &'a Value {
    rows.as_array()
        .expect("search returns rows")
        .iter()
        .find(|row| row["id"] == json!(id))
        .unwrap_or_else(|| panic!("no hit {id} in {rows}"))
}

#[tokio::test]
async fn follow_returns_the_targets_of_the_named_outgoing_relation() {
    let registry = configured();
    let wording = concept(&registry, "printed wording sample").await;
    let named = concept(&registry, "Named Concept").await;
    let variant = concept(&registry, "Variant Concept").await;
    let upstream = concept(&registry, "Upstream Concept").await;
    link(&registry, &wording, &named, "extends").await;
    // A directed relation, so the edge always leaves the hit whatever the ids.
    link(&registry, &wording, &variant, "variant_of").await;
    link(&registry, &upstream, &wording, "extends").await;

    let plain = registry
        .dispatch(
            "search",
            json!({"kind": "entity", "query": "printed wording"}),
        )
        .await
        .expect("search");
    let row = hit(&plain, &wording);
    assert!(row.get("followed").is_none(), "{row}");
    assert!(row.get("followed_truncated").is_none(), "{row}");

    let found = registry
        .dispatch(
            "search",
            json!({"kind": "entity", "query": "printed wording", "follow": "extends"}),
        )
        .await
        .expect("search with follow");
    let row = hit(&found, &wording);
    let followed = row["followed"].as_array().expect("followed rows");
    let ids: Vec<&str> = followed
        .iter()
        .map(|target| target["id"].as_str().expect("target id"))
        .collect();
    // Only the outgoing edge of the named relation: the variant is another
    // relation and the upstream concept reaches the hit, not from it.
    assert_eq!(ids, vec![named.as_str()], "{row}");
    assert_eq!(followed[0]["relation"], json!("extends"), "{row}");
    assert_eq!(followed[0]["name"], json!("Named Concept"), "{row}");
    assert_eq!(row["followed_truncated"], json!(false), "{row}");
}

#[tokio::test]
async fn follow_on_a_note_search_returns_the_annotated_targets() {
    let registry = configured();
    let named = concept(&registry, "Named Concept").await;
    let other = concept(&registry, "Other Concept").await;
    let created = registry
        .dispatch(
            "create",
            json!({"kind": "observation", "content": "printed wording observation", "skip_dedup_check": true}),
        )
        .await
        .expect("create note");
    let note = created["id"].as_str().expect("note id").to_owned();
    link(&registry, &note, &named, "annotates").await;
    link(&registry, &other, &named, "extends").await;

    let found = registry
        .dispatch(
            "search",
            json!({"kind": "note", "query": "printed wording", "follow": "annotates"}),
        )
        .await
        .expect("note search with follow");
    let row = hit(&found, &note);
    let ids: Vec<&str> = row["followed"]
        .as_array()
        .expect("followed rows")
        .iter()
        .map(|target| target["id"].as_str().expect("target id"))
        .collect();
    assert_eq!(ids, vec![named.as_str()], "{row}");
    assert_eq!(row["followed_truncated"], json!(false), "{row}");
}

#[tokio::test]
async fn follow_refuses_a_relation_outside_the_edge_set() {
    let registry = configured();
    concept(&registry, "printed wording sample").await;
    let error = registry
        .dispatch(
            "search",
            json!({"kind": "entity", "query": "printed wording", "follow": "names"}),
        )
        .await
        .expect_err("an unknown relation is refused");
    assert!(
        error.to_string().contains("unknown relation \"names\""),
        "{error}"
    );
}
