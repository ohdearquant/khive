use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use khive_pack_kg::KgPack;
use khive_pack_web::WebPack;
use khive_runtime::{KhiveRuntime, RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use khive_storage::types::{EdgeFilter, PageRequest};
use khive_storage::EntityFilter;
use khive_types::Namespace;
use serde_json::{json, Value};
use tempfile::TempDir;
use uuid::Uuid;

fn registry(runtime: KhiveRuntime) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(WebPack::new(runtime.clone()));
    let registry = builder.build().expect("web registry builds");
    runtime.install_edge_rules(registry.all_edge_rules());
    registry
}

fn memory_registry() -> VerbRegistry {
    registry(KhiveRuntime::memory().expect("memory runtime"))
}

fn runtime_at(path: &Path) -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        db_path: Some(path.to_path_buf()),
        packs: vec![],
        ..RuntimeConfig::no_embeddings()
    })
    .expect("map runtime opens")
}

fn copy_tree(source: &Path, target: &Path) {
    fs::create_dir_all(target).expect("fixture directory");
    for entry in fs::read_dir(source).expect("fixture entries") {
        let entry = entry.expect("fixture entry");
        let destination = target.join(entry.file_name());
        if entry.file_type().expect("fixture file type").is_dir() {
            copy_tree(&entry.path(), &destination);
        } else {
            fs::copy(entry.path(), destination).expect("copy fixture file");
        }
    }
}

fn fixture(name: &str) -> TempDir {
    let target = tempfile::tempdir().expect("temporary site tree");
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    copy_tree(&source, target.path());
    target
}

fn rewrite_manifest(source: &Path, change: impl FnOnce(&mut Value)) {
    let path = source.join(".well-known/arw.json");
    let mut manifest = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    change(&mut manifest);
    fs::write(path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
}

async fn map_rows(runtime: &KhiveRuntime) -> Value {
    let token = runtime.authorize(Namespace::local()).unwrap();
    let page = PageRequest {
        offset: 0,
        limit: 1_000,
    };
    let mut entities = runtime
        .entities(&token)
        .unwrap()
        .query_entities("local", EntityFilter::default(), page.clone())
        .await
        .unwrap()
        .items;
    let mut edges = runtime
        .graph(&token)
        .unwrap()
        .query_edges(EdgeFilter::default(), vec![], page)
        .await
        .unwrap()
        .items;
    entities.sort_by_key(|entity| entity.id);
    edges.sort_by_key(|edge| Uuid::from(edge.id));
    json!({"entities": entities, "edges": edges})
}

fn entities(rows: &Value) -> &[Value] {
    rows["entities"].as_array().expect("entity records")
}

fn edges(rows: &Value) -> &[Value] {
    rows["edges"].as_array().expect("edge records")
}

fn entity_counts(site: usize, page: usize, view: usize, tool: usize, skill: usize) -> Value {
    json!({
        "site": site,
        "page": page,
        "machine_view": view,
        "agent_tool": tool,
        "agent_skill": skill,
    })
}

fn relation_counts(contains: usize, derived_from: usize, depends_on: usize) -> Value {
    json!({
        "contains": contains,
        "derived_from": derived_from,
        "depends_on": depends_on,
        "implements": 0,
    })
}

fn expected_plants_id() -> Uuid {
    let namespace = Uuid::parse_str("71c1a6f3-8b91-5c7a-a027-9f8f868644a9").unwrap();
    Uuid::new_v5(&namespace, br#"["page","meadow.invalid","/plants"]"#)
}

fn plants_page(rows: &Value) -> &Value {
    entities(rows)
        .iter()
        .find(|entity| entity["id"] == expected_plants_id().to_string())
        .expect("the /plants page has its independently keyed UUIDv5")
}

#[tokio::test]
async fn complete_site_has_exact_counts_and_emits_view_to_page_derivation() {
    let expected_entities = entity_counts(1, 2, 2, 2, 1);
    let expected_relations = relation_counts(5, 2, 2);
    let site = fixture("meadow");
    let report = memory_registry()
        .dispatch("web.ingest", json!({"source": site.path()}))
        .await
        .expect("complete site ingests");

    assert_eq!(report["entity_counts"], expected_entities);
    assert_eq!(report["relation_counts"], expected_relations);
    assert_eq!(report["views_missing"], 0);
    assert_eq!(report["quarantined"], json!([]));
    assert_eq!(report["ignored_keys"], 0);
    assert_eq!(report["include_views"], true);
    let raw_manifest = fs::read(site.path().join(".well-known/arw.json")).unwrap();
    assert_eq!(
        report["manifest_digest"],
        blake3::hash(&raw_manifest).to_hex().to_string()
    );
    assert_eq!(
        Path::new(report["source"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        site.path().canonicalize().unwrap()
    );
    let db = site.path().join(".khive/web-map.db");
    assert_eq!(
        Path::new(report["db_path"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        db.canonicalize().unwrap()
    );
    let rows = map_rows(&runtime_at(&db)).await;
    assert_eq!(entities(&rows).len(), 8);
    assert_eq!(edges(&rows).len(), 9);
    let types: BTreeMap<&str, &str> = entities(&rows)
        .iter()
        .map(|entity| {
            (
                entity["id"].as_str().unwrap(),
                entity["entity_type"].as_str().unwrap(),
            )
        })
        .collect();
    let derivations: Vec<&Value> = edges(&rows)
        .iter()
        .filter(|edge| edge["relation"] == "derived_from")
        .collect();
    assert_eq!(derivations.len(), 2);
    for edge in derivations {
        assert_eq!(types[edge["source_id"].as_str().unwrap()], "machine_view");
        assert_eq!(types[edge["target_id"].as_str().unwrap()], "page");
    }
    assert_eq!(plants_page(&rows)["tags"], json!(["botany", "plants"]));
}

#[tokio::test]
async fn canonical_origin_and_path_spellings_preserve_entity_and_edge_ids() {
    let site = fixture("meadow");
    fs::remove_file(site.path().join("llms.txt")).unwrap();
    let db = site.path().join(".khive/web-map.db");
    let registry = memory_registry();
    registry
        .dispatch("web.ingest", json!({"source": site.path()}))
        .await
        .unwrap();
    let first = map_rows(&runtime_at(&db)).await;
    rewrite_manifest(site.path(), |manifest| {
        manifest["site"]["homepage"] = json!("http://MEADOW.invalid:8080/");
        manifest["content"][0]["url"] = json!("plants/");
        manifest["content"][0]["markdown_url"] = json!("///plants.md/");
    });
    let report = registry
        .dispatch("web.ingest", json!({"source": site.path()}))
        .await
        .unwrap();
    assert_eq!(report["quarantined"], json!([]));
    let second = map_rows(&runtime_at(&db)).await;
    for collection in ["entities", "edges"] {
        let ids = |rows: &Value| {
            rows[collection]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["id"].clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&first), ids(&second));
    }
}

#[tokio::test]
async fn shared_machine_view_keeps_both_declared_page_derivations() {
    let site = fixture("meadow");
    fs::remove_file(site.path().join("llms.txt")).unwrap();
    rewrite_manifest(site.path(), |manifest| {
        manifest["content"][1]["markdown_url"] = json!("/plants.md/");
    });
    let report = memory_registry()
        .dispatch("web.ingest", json!({"source": site.path()}))
        .await
        .unwrap();
    assert_eq!(report["entity_counts"], entity_counts(1, 2, 1, 2, 1));
    assert_eq!(report["relation_counts"], relation_counts(5, 2, 2));
    assert_eq!(report["quarantined"], json!([]));
    let rows = map_rows(&runtime_at(&site.path().join(".khive/web-map.db"))).await;
    let derivations: Vec<_> = edges(&rows)
        .iter()
        .filter(|edge| edge["relation"] == "derived_from")
        .collect();
    assert_eq!(derivations.len(), 2);
    assert_eq!(derivations[0]["source_id"], derivations[1]["source_id"]);
    assert_ne!(derivations[0]["target_id"], derivations[1]["target_id"]);
}

#[tokio::test]
async fn reingest_preserves_every_map_row_and_updates_description_in_place() {
    let expected_entities = entity_counts(1, 2, 2, 2, 1);
    let expected_relations = relation_counts(5, 2, 2);
    let site = fixture("meadow");
    let db = site.path().join("separate-map.db");
    let registry = memory_registry();
    let args = json!({"source": site.path(), "db": db});
    let first_report = registry.dispatch("web.ingest", args.clone()).await.unwrap();
    assert_eq!(first_report["entity_counts"], expected_entities);
    assert_eq!(first_report["relation_counts"], expected_relations);
    let first = map_rows(&runtime_at(&db)).await;
    let original_page = plants_page(&first).clone();

    let second_report = registry.dispatch("web.ingest", args.clone()).await.unwrap();
    assert_eq!(second_report["entity_counts"], expected_entities);
    assert_eq!(second_report["relation_counts"], expected_relations);
    let second = map_rows(&runtime_at(&db)).await;
    assert_eq!(
        serde_json::to_vec(&first).unwrap(),
        serde_json::to_vec(&second).unwrap(),
        "unchanged declarations must preserve all entity and edge fields, including timestamps"
    );

    let updated_description = "Browse the revised fictional collection of meadow plants.";
    rewrite_manifest(site.path(), |manifest| {
        manifest["content"][0]["description"] = json!(updated_description);
    });
    let third_report = registry.dispatch("web.ingest", args).await.unwrap();
    assert_eq!(third_report["entity_counts"], expected_entities);
    assert_eq!(third_report["relation_counts"], expected_relations);
    let third = map_rows(&runtime_at(&db)).await;
    assert_eq!(entities(&third).len(), 8);
    assert_eq!(edges(&third).len(), 9);
    assert_eq!(plants_page(&third)["id"], original_page["id"]);
    assert_eq!(plants_page(&third)["description"], updated_description);
    assert_eq!(
        plants_page(&third)["created_at"],
        original_page["created_at"]
    );
    assert_ne!(
        third_report["manifest_digest"],
        first_report["manifest_digest"]
    );
}

#[tokio::test]
async fn manifest_only_site_has_pages_without_views_tools_or_skills() {
    let expected_entities = entity_counts(1, 1, 0, 0, 0);
    let expected_relations = relation_counts(1, 0, 0);
    let site = fixture("fern");
    let report = memory_registry()
        .dispatch("web.ingest", json!({"source": site.path()}))
        .await
        .unwrap();
    assert_eq!(report["entity_counts"], expected_entities);
    assert_eq!(report["relation_counts"], expected_relations);
    assert_eq!(report["views_missing"], 0);
    assert_eq!(report["quarantined"], json!([]));
    let rows = map_rows(&runtime_at(&site.path().join(".khive/web-map.db"))).await;
    assert_eq!(entities(&rows).len(), 2);
    assert_eq!(edges(&rows).len(), 1);
}

#[tokio::test]
async fn include_views_false_skips_view_files_and_preserves_page_counts() {
    let expected_entities = entity_counts(1, 2, 0, 2, 1);
    let expected_relations = relation_counts(5, 0, 2);
    let site = fixture("meadow");
    fs::write(site.path().join("plants.md"), [0xff, 0xfe]).unwrap();
    fs::remove_file(site.path().join("seasons.md")).unwrap();
    let report = memory_registry()
        .dispatch(
            "web.ingest",
            json!({"source": site.path(), "include_views": false}),
        )
        .await
        .unwrap();
    assert_eq!(report["entity_counts"], expected_entities);
    assert_eq!(report["relation_counts"], expected_relations);
    assert_eq!(report["include_views"], false);
    assert_eq!(report["views_missing"], 0);
    assert_eq!(report["quarantined"], json!([]));
    let rows = map_rows(&runtime_at(&site.path().join(".khive/web-map.db"))).await;
    assert_eq!(entities(&rows).len(), 6);
    assert_eq!(edges(&rows).len(), 7);
    assert!(entities(&rows)
        .iter()
        .all(|entity| entity["entity_type"] != "machine_view"));
}

#[tokio::test]
async fn missing_view_keeps_its_page_without_quarantine() {
    let expected_entities = entity_counts(1, 2, 1, 2, 1);
    let expected_relations = relation_counts(5, 1, 2);
    let site = fixture("meadow");
    fs::remove_file(site.path().join("plants.md")).unwrap();
    let report = memory_registry()
        .dispatch("web.ingest", json!({"source": site.path()}))
        .await
        .unwrap();
    assert_eq!(report["entity_counts"], expected_entities);
    assert_eq!(report["relation_counts"], expected_relations);
    assert_eq!(report["views_missing"], 1);
    assert_eq!(report["quarantined"], json!([]));
    let rows = map_rows(&runtime_at(&site.path().join(".khive/web-map.db"))).await;
    assert_eq!(entities(&rows).len(), 7);
    assert_eq!(edges(&rows).len(), 8);
    assert_eq!(plants_page(&rows)["entity_type"], "page");
}

#[tokio::test]
async fn malformed_manifest_refuses_site_without_writing_map_rows() {
    let site = fixture("malformed");
    let db = site.path().join("refused-map.db");
    let error = memory_registry()
        .dispatch("web.ingest", json!({"source": site.path(), "db": db}))
        .await
        .expect_err("malformed JSON must refuse the site");
    assert!(error.to_string().contains("manifest_malformed"), "{error}");
    let rows = map_rows(&runtime_at(&db)).await;
    assert!(entities(&rows).is_empty());
    assert!(edges(&rows).is_empty());
}

#[tokio::test]
async fn missing_manifest_refuses_site_without_writing_map_rows() {
    let site = tempfile::tempdir().unwrap();
    let db = site.path().join("refused-map.db");
    let error = memory_registry()
        .dispatch("web.ingest", json!({"source": site.path(), "db": db}))
        .await
        .expect_err("missing manifest must refuse the site");
    assert!(error.to_string().contains("manifest_missing"), "{error}");
    let rows = map_rows(&runtime_at(&db)).await;
    assert!(entities(&rows).is_empty());
    assert!(edges(&rows).is_empty());
}

#[tokio::test]
async fn llms_disagreement_quarantines_named_field_and_keeps_manifest_value() {
    let site = fixture("meadow");
    fs::write(
        site.path().join("llms.txt"),
        "# Meadow notes\n\n```yaml\nsite:\n  name: Different Meadow\n```\n",
    )
    .unwrap();
    let report = memory_registry()
        .dispatch("web.ingest", json!({"source": site.path()}))
        .await
        .unwrap();
    let quarantined = report["quarantined"].as_array().unwrap();
    assert_eq!(quarantined.len(), 1);
    assert_eq!(quarantined[0]["field"], "site.name");
    assert!(!quarantined[0]["reason"].as_str().unwrap().is_empty());
    let rows = map_rows(&runtime_at(&site.path().join(".khive/web-map.db"))).await;
    let site_entity = entities(&rows)
        .iter()
        .find(|entity| entity["entity_type"] == "site")
        .unwrap();
    assert_eq!(site_entity["name"], "Meadow Archive");
}

#[tokio::test]
async fn production_database_is_refused_before_any_write() {
    let site = fixture("meadow");
    let daemon_directory = tempfile::tempdir().unwrap();
    let production = daemon_directory.path().join("production.db");
    let runtime = runtime_at(&production);
    let registry = registry(runtime.clone());
    registry
        .dispatch("create", json!({"kind": "concept", "name": "Botany"}))
        .await
        .unwrap();
    let before = map_rows(&runtime).await;
    let error = registry
        .dispatch(
            "web.ingest",
            json!({"source": site.path(), "db": production}),
        )
        .await
        .expect_err("the calling runtime's database is never an ingest target");
    assert!(error.to_string().contains("production database"), "{error}");
    assert_eq!(map_rows(&runtime).await, before);
    assert!(!site.path().join(".khive/web-map.db").exists());
}

#[tokio::test]
async fn unknown_parameters_are_rejected_without_writing_a_map() {
    let site = fixture("fern");
    let error = memory_registry()
        .dispatch(
            "web.ingest",
            json!({"source": site.path(), "allow_production": true}),
        )
        .await
        .expect_err("unknown parameters must not be silently ignored");
    assert!(error.to_string().contains("allow_production"), "{error}");
    assert!(!site.path().join(".khive/web-map.db").exists());
}

#[tokio::test]
async fn unconsumed_manifest_keys_are_counted_without_inferring_protocols() {
    let expected_entities = entity_counts(1, 1, 0, 0, 0);
    let expected_relations = relation_counts(1, 0, 0);
    let site = fixture("fern");
    rewrite_manifest(site.path(), |manifest| {
        manifest["computer_use"] = json!(false);
        manifest["policies"] = json!({"attribution": {"required": true}});
        manifest["integrations"] = json!([{
            "name": "Fern catalog",
            "type": "mcp",
            "endpoint": "/mcp"
        }]);
    });
    let report = memory_registry()
        .dispatch("web.ingest", json!({"source": site.path()}))
        .await
        .unwrap();
    assert_eq!(report["ignored_keys"], 3);
    assert_eq!(report["entity_counts"], expected_entities);
    assert_eq!(report["relation_counts"], expected_relations);
    assert_eq!(report["quarantined"], json!([]));
    let rows = map_rows(&runtime_at(&site.path().join(".khive/web-map.db"))).await;
    assert_eq!(entities(&rows).len(), 2);
    assert!(entities(&rows)
        .iter()
        .all(|entity| entity["entity_type"] != "interface"));
}
