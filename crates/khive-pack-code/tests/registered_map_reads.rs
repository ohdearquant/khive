//! Read registered dependency maps produced by the real L1 manifest ingester.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use chrono::Utc;
use khive_pack_code::source_ingest::run_code_ingest;
use khive_pack_code::{CodePack, CodeSourceIngestOptions};
use khive_pack_kg::KgPack;
use khive_runtime::{CodeMapConfig, KhiveRuntime, Namespace, RuntimeConfig, VerbRegistryBuilder};
use serde_json::{json, Value};

fn physical_tempdir() -> std::io::Result<tempfile::TempDir> {
    tempfile::tempdir_in(std::env::temp_dir().canonicalize()?)
}

fn write_crate(root: &Path, directory: &str, name: &str, dependencies: &str) {
    let path = root.join(directory);
    std::fs::create_dir_all(path.join("src")).unwrap();
    std::fs::write(
        path.join("Cargo.toml"),
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n{dependencies}\n"),
    )
    .unwrap();
    std::fs::write(path.join("src/lib.rs"), "pub fn marker() {}\n").unwrap();
}

fn names(value: &Value) -> BTreeSet<String> {
    value["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["name"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn l1_ingested_projects_support_registered_dependency_reads() {
    let fixture = physical_tempdir().unwrap();
    let source = fixture.path().join("source");
    let maps = fixture.path().join("maps");
    let locks = fixture.path().join("volume-locks");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir(&maps).unwrap();
    std::fs::write(
        source.join("Cargo.toml"),
        "[workspace]\nmembers = [\"a\", \"b\", \"c\", \"dev\", \"build_dep\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    write_crate(&source, "a", "crate_a", "");
    write_crate(
        &source,
        "b",
        "crate_b",
        "[dependencies]\ncrate_a = { path = \"../a\" }",
    );
    write_crate(
        &source,
        "c",
        "crate_c",
        "[dependencies]\ncrate_b = { path = \"../b\" }",
    );
    write_crate(
        &source,
        "dev",
        "crate_dev",
        "[dev-dependencies]\ncrate_a = { path = \"../a\" }",
    );
    write_crate(
        &source,
        "build_dep",
        "crate_build",
        "[build-dependencies]\ncrate_a = { path = \"../a\" }",
    );
    let path = maps.join("workspace.db");
    std::fs::File::create(&path).unwrap();
    {
        let writer = KhiveRuntime::new_code_map(
            RuntimeConfig {
                db_path: Some(path.clone()),
                packs: vec![],
                volume_lock_dir: Some(locks),
                ..RuntimeConfig::no_embeddings()
            },
            vec![],
            vec![],
        )
        .unwrap();
        let token = writer.authorize(Namespace::local()).unwrap();
        let report = run_code_ingest(
            &writer,
            &token,
            CodeSourceIngestOptions {
                path: &source,
                languages: ["rust"].into_iter().collect(),
                sweep_time: Utc::now(),
                enable_l1: true,
                enable_l1_5: false,
                enable_l2: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(report.projects_created, 5);
        // The fixture is not a git checkout, so ingest records its source as unversioned; any other
        // warning fails.
        assert!(
            report
                .warnings
                .iter()
                .all(|warning| warning.contains("git metadata unavailable")),
            "{:?}",
            report.warnings
        );
    }
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        packs: vec![],
        code_map_root: Some(maps.display().to_string()),
        code_maps: vec![CodeMapConfig {
            name: Some("workspace".into()),
            path: Some(path.display().to_string()),
            source_root: Some(source.display().to_string()),
            ..CodeMapConfig::default()
        }],
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    runtime.code_map_registry().resolve("workspace").unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(CodePack::new(runtime.clone()));
    let registry = builder.build().unwrap();
    runtime.install_edge_rules(registry.all_edge_rules());
    let listed = registry
        .dispatch(
            "code.list",
            json!({"map": "workspace", "kind": "project", "limit": 500}),
        )
        .await
        .unwrap();
    let projects: BTreeMap<String, String> = listed["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            assert_eq!(item["kind"], "project");
            assert!(item["project_id"].is_null());
            (
                item["name"].as_str().unwrap().to_owned(),
                item["id"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        projects.keys().map(String::as_str).collect::<Vec<_>>(),
        ["crate_a", "crate_b", "crate_build", "crate_c", "crate_dev"]
    );
    assert_eq!(listed["items"].as_array().unwrap().len(), 5);
    assert!(listed["next_cursor"].is_null());
    let direct = registry.dispatch("code.deps", json!({"map": "workspace", "id": projects["crate_a"], "direction": "incoming", "max_depth": 1})).await.unwrap();
    assert_eq!(
        names(&direct),
        ["crate_a", "crate_b", "crate_build", "crate_dev"]
            .map(str::to_owned)
            .into_iter()
            .collect()
    );
    assert_eq!(direct["truncated"], false);
    let deeper = registry.dispatch("code.deps", json!({"map": "workspace", "id": projects["crate_a"], "direction": "incoming", "max_depth": 2})).await.unwrap();
    assert_eq!(names(&deeper), projects.keys().cloned().collect());
    assert_eq!(deeper["truncated"], false);
    let limited = registry.dispatch("code.deps", json!({"map": "workspace", "id": projects["crate_a"], "direction": "incoming", "max_depth": 2, "limit": 2})).await.unwrap();
    assert_eq!(limited["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(limited["nodes"][0]["id"], projects["crate_a"]);
    assert_eq!(limited["truncated"], true);
    for response in [&listed, &direct, &deeper, &limited] {
        assert_eq!(response["map"], "workspace");
        let encoded = response.to_string();
        assert!(!encoded.contains(path.to_str().unwrap()));
        assert!(!encoded.contains(source.to_str().unwrap()));
    }
}
