//! Black-box `web.ingest` test through the real dispatch surface
//! (`VerbRegistry::dispatch`, exactly as an MCP caller would reach it),
//! against a served-tree directory fixture in plain HTML — no
//! `.well-known` application manifest, matching ADR-191 (the manifest-bound
//! ADR-175 shape this pack supersedes had one; this ontology does not).

use khive_pack_kg::KgPack;
use khive_pack_web::WebPack;
use khive_runtime::engine_config::WebSectionConfig;
use khive_runtime::pack::VerbRegistryBuilder;
use khive_runtime::{KhiveRuntime, RuntimeConfig};
use serde_json::json;
use std::sync::Arc;

/// A registry whose web pack may read `read_root` from disk (`[web] read_roots`);
/// disk ingest of any other directory is refused.
async fn dispatch_fixture(
    read_root: &std::path::Path,
) -> (khive_runtime::pack::VerbRegistry, tempfile::TempDir) {
    let blob_dir = tempfile::tempdir().expect("blob dir");
    let store = khive_db::stores::blob::FsBlobStore::new(blob_dir.path().to_path_buf(), 0)
        .expect("fs blob store");
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        actor_id: None,
        web: WebSectionConfig {
            read_roots: vec![read_root.to_string_lossy().into_owned()],
            ..WebSectionConfig::default()
        },
        ..RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime");
    runtime
        .install_blob_store(Arc::new(store))
        .expect("install blob store");

    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(WebPack::new(runtime.clone()));
    let registry = builder
        .build()
        .expect("registry builds with its kg dependency present");
    // The web pack's own EDGE_RULES (`site contains page|resource`) are
    // declared on WebPack but never installed on the runtime just by
    // registering it — dispatch validates edges against whatever the
    // runtime was told via `install_edge_rules`, not against the registry.
    runtime.install_edge_rules(registry.all_edge_rules());
    (registry, blob_dir)
}

fn write_served_tree(root: &std::path::Path) {
    std::fs::write(
        root.join("index.html"),
        b"<html><body><a href=\"/about.html\">About</a></body></html>",
    )
    .unwrap();
    std::fs::write(
        root.join("about.html"),
        b"<html><body>plain html, no application manifest</body></html>",
    )
    .unwrap();
}

// A5 (black-box half): a served tree on disk under a declared origin, run
// through the real dispatch surface, mints one document entity per file
// under a `site` entity for the declared origin — the graph a live HTTP
// ingest of the same tree would also produce, per D1's address-based
// identity (checked in-crate against `identity::document_id` directly;
// this test checks the same property from outside the crate).
#[tokio::test]
async fn a5_ingest_served_tree_via_public_dispatch_mints_one_document_per_file() {
    let tree = tempfile::tempdir().expect("served tree");
    let (registry, _blob_dir) = dispatch_fixture(tree.path()).await;
    write_served_tree(tree.path());

    let reply = registry
        .dispatch(
            "web.ingest",
            json!({
                "source": tree.path().to_string_lossy(),
                "origin": "https://plain-html.example.test",
            }),
        )
        .await
        .expect("web.ingest dispatches through the real registry");

    let ingested = reply["ingested"].as_array().expect("ingested array");
    assert_eq!(ingested.len(), 2, "index.html and about.html");
    let site_id = reply["site"].as_str().expect("site id");

    for entry in ingested {
        let id = entry.as_str().expect("entity id string");
        let entity = registry
            .dispatch("get", json!({ "id": id }))
            .await
            .expect("the minted entity is readable back through the kg pack's own get verb");
        assert_eq!(
            entity["entity_type"], "page",
            "plain HTML files ingest as page"
        );
        assert_eq!(entity["properties"]["content_type"], "text/html");
    }

    let site = registry
        .dispatch("get", json!({ "id": site_id }))
        .await
        .expect("the site entity is readable back");
    assert_eq!(site["entity_type"], "site");
}
