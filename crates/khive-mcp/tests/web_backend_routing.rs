//! Pack-scoped backend routing driven through the real serving composition:
//! a `[[backends]]` declaration plus a `[packs.web]` assignment, resolved by
//! `resolve_pack_backend_config` inside `build_registry_for_multi_backend`.
//! The property under test is that `web.*` writes land only in the backend
//! the configuration routes the pack to; the control shows that without an
//! assignment the pack shares the `main` backend.

use khive_pack_kg as _;
use khive_pack_web as _;

use std::collections::HashMap;
use std::sync::Arc;

use khive_db::stores::blob::FsBlobStore;
use khive_mcp::serve::build_registry_for_multi_backend;
use khive_runtime::engine_config::{KhiveConfig, PackConfig, WebSectionConfig};
use khive_runtime::{
    AllowAllGate, BackendConfig, BackendId, BackendKind, KhiveRuntime, Namespace, RuntimeConfig,
};
use khive_storage::{EntityFilter, PageRequest};
use serde_json::json;

fn memory_backend(name: &str) -> BackendConfig {
    BackendConfig {
        name: name.to_string(),
        kind: BackendKind::Memory,
        path: None,
        cache_mb: None,
        journal_mode: None,
        served_kinds: None,
        read_only: false,
    }
}

fn base_config(read_root: &std::path::Path) -> RuntimeConfig {
    RuntimeConfig {
        db_path: khive_runtime::resolve_db_anchor(None),
        gate: Arc::new(AllowAllGate),
        default_namespace: Namespace::parse("local").expect("namespace"),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string(), "web".to_string()],
        backend_id: BackendId::main(),
        web: WebSectionConfig {
            read_roots: vec![read_root.to_string_lossy().into_owned()],
            ..WebSectionConfig::default()
        },
        ..RuntimeConfig::default()
    }
}

async fn entity_count(runtime: &KhiveRuntime) -> usize {
    let token = runtime
        .authorize(Namespace::local())
        .expect("authorize local namespace");
    runtime
        .entities(&token)
        .expect("entity store capability")
        .query_entities(
            "local",
            EntityFilter::default(),
            PageRequest {
                offset: 0,
                limit: 1_000,
            },
        )
        .await
        .expect("query entities")
        .items
        .len()
}

fn write_tree(dir: &std::path::Path) {
    std::fs::write(dir.join("index.html"), b"<html><body>root</body></html>").expect("write");
    std::fs::write(dir.join("about.html"), b"<html><body>about</body></html>").expect("write");
}

async fn ingest_tree(
    multi: &khive_mcp::serve::MultiBackendRegistry,
    tree: &std::path::Path,
    blobs: &std::path::Path,
) {
    let web_runtime = multi
        .per_pack_runtimes
        .get("web")
        .expect("web pack runtime is registered");
    let store = FsBlobStore::new(blobs.to_path_buf(), 0).expect("fs blob store");
    web_runtime
        .install_blob_store(Arc::new(store))
        .expect("install blob store on the web runtime");
    let reply = multi
        .registry
        .dispatch(
            "web.ingest",
            json!({
                "source": tree.to_string_lossy(),
                "origin": "https://routed.example.test",
            }),
        )
        .await
        .expect("web.ingest dispatches through the composed registry");
    assert_eq!(
        reply["ingested"].as_array().map(Vec::len),
        Some(2),
        "two files ingested: {reply}"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn web_pack_assigned_to_a_declared_backend_writes_there_only() {
    let dir = tempfile::tempdir().expect("temp dir");
    let tree = dir.path().join("tree");
    std::fs::create_dir(&tree).expect("tree dir");
    write_tree(&tree);

    let mut packs = HashMap::new();
    packs.insert(
        "web".to_string(),
        PackConfig {
            backend: "web".to_string(),
            no_embed: false,
        },
    );
    let khive_cfg = KhiveConfig {
        backends: vec![memory_backend("main"), memory_backend("web")],
        packs,
        ..KhiveConfig::default()
    };

    let multi = build_registry_for_multi_backend(base_config(&tree), &khive_cfg, None)
        .await
        .expect("multi-backend registry build must succeed");
    ingest_tree(&multi, &tree, &dir.path().join("blobs")).await;

    let web_runtime = multi.per_pack_runtimes.get("web").expect("web runtime");
    let kg_runtime = multi.per_pack_runtimes.get("kg").expect("kg runtime");
    assert!(
        entity_count(web_runtime).await > 0,
        "the site and its pages are rows in the backend the pack is routed to"
    );
    assert_eq!(
        entity_count(kg_runtime).await,
        0,
        "the main backend holds none of the web pack's rows"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn web_pack_without_an_assignment_shares_the_main_backend() {
    let dir = tempfile::tempdir().expect("temp dir");
    let tree = dir.path().join("tree");
    std::fs::create_dir(&tree).expect("tree dir");
    write_tree(&tree);

    let khive_cfg = KhiveConfig {
        backends: vec![memory_backend("main"), memory_backend("web")],
        ..KhiveConfig::default()
    };

    let multi = build_registry_for_multi_backend(base_config(&tree), &khive_cfg, None)
        .await
        .expect("multi-backend registry build must succeed");
    ingest_tree(&multi, &tree, &dir.path().join("blobs")).await;

    let kg_runtime = multi.per_pack_runtimes.get("kg").expect("kg runtime");
    assert!(
        entity_count(kg_runtime).await > 0,
        "with no [packs.web] assignment the web rows are visible on the main backend"
    );
}
