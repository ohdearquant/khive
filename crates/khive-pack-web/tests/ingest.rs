#![cfg(any(target_os = "linux", target_os = "macos"))]

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
    let store = khive_db::stores::blob::FsBlobStore::new(blob_dir.path().join("blobs"), 0)
        .expect("fs blob store");
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(blob_dir.path().join("web.db")),
        actor_id: None,
        web: WebSectionConfig {
            read_roots: vec![read_root
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned()],
            ..WebSectionConfig::default()
        },
        ..RuntimeConfig::no_embeddings()
    })
    .expect("file-backed runtime");
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
        b"<html><a href=\"/about.html\">About</a><a href=\"/later.html\">Later</a></html>",
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
                "source": tree.path().canonicalize().unwrap().to_string_lossy(),
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

#[tokio::test]
async fn disk_ingest_extracts_two_links_without_an_extra_origin_resource() {
    let tree = tempfile::tempdir().unwrap();
    let (registry, _dir) = dispatch_fixture(tree.path()).await;
    write_served_tree(tree.path());
    for _ in 0..2 {
        let reply = registry
            .dispatch(
                "web.ingest",
                json!({
                    "source": tree.path().canonicalize().unwrap().to_string_lossy(),
                    "origin": "https://plain-html.example.test"
                }),
            )
            .await
            .unwrap();
        assert_eq!(reply["ingested"].as_array().unwrap().len(), 2);
        let listed = registry
            .dispatch("list", json!({"kind": "entity", "limit": 100}))
            .await
            .unwrap();
        let entities = listed["items"].as_array().unwrap();
        assert_eq!(
            entities.len(),
            6,
            "site, two pages, one linked resource, and two text resources"
        );
        let documents: Vec<_> = entities
            .iter()
            .filter(|entity| entity["kind"] == "document")
            .collect();
        assert_eq!(documents.len(), 5);
        assert!(!documents
            .iter()
            .any(|entity| entity["properties"]["url"] == "https://plain-html.example.test/"));
        let target = documents
            .iter()
            .find(|entity| {
                entity["properties"]["url"] == "https://plain-html.example.test/later.html"
            })
            .unwrap();
        assert_eq!(target["entity_type"], "resource");
        assert!(target["properties"]["status"].is_null());
        let edges = registry
            .dispatch("list", json!({"kind": "edge", "limit": 100}))
            .await
            .unwrap();
        assert_eq!(
            edges["items"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|edge| edge["relation"] == "links_to")
                .count(),
            2
        );
    }
}

// Must-FAIL control: restore links-only extraction in ingest_disk. The
// derived-text rows and XML entries asserted here then do not exist.
#[tokio::test]
async fn disk_ingest_extracts_applicable_text_sitemap_and_feed() {
    let tree = tempfile::tempdir().unwrap();
    let (registry, _dir) = dispatch_fixture(tree.path()).await;
    for (name, body) in [
        ("page.html", "<p>visible page text</p>"),
        ("plain.txt", "plain text"),
        (
            "sitemap.xml",
            "<urlset><url><loc>https://extract.example.test/from-sitemap</loc></url></urlset>",
        ),
        (
            "feed.xml",
            "<feed><entry><link href=\"https://extract.example.test/from-feed\" /></entry></feed>",
        ),
    ] {
        std::fs::write(tree.path().join(name), body).unwrap();
    }
    for _ in 0..2 {
        let reply = registry
            .dispatch(
                "web.ingest",
                json!({
                    "source": tree.path().canonicalize().unwrap().to_string_lossy(),
                    "origin": "https://extract.example.test",
                }),
            )
            .await
            .unwrap();
        assert_eq!(reply["ingested"].as_array().unwrap().len(), 4);
        let listed = registry
            .dispatch("list", json!({"kind": "entity", "limit": 100}))
            .await
            .unwrap();
        let entities = listed["items"].as_array().unwrap();
        assert_eq!(
            entities.len(),
            9,
            "site, four sources, two text resources, two XML entries"
        );
        let edges = registry
            .dispatch("list", json!({"kind": "edge", "limit": 100}))
            .await
            .unwrap();
        let edges = edges["items"].as_array().unwrap();
        for (path, size) in [("page.html", 17), ("plain.txt", 10)] {
            let source = entities
                .iter()
                .find(|row| {
                    row["properties"]["url"] == format!("https://extract.example.test/{path}")
                })
                .unwrap();
            let derived = entities
                .iter()
                .find(|row| row["properties"]["derived_from"] == source["id"])
                .unwrap();
            assert_eq!(derived["properties"]["content_type"], "text/plain");
            assert_eq!(derived["properties"]["size"], size);
            assert!(derived["properties"]["blob_ref"].is_string());
            assert!(edges.iter().any(|edge| edge["relation"] == "derived_from"
                && edge["source_id"] == derived["id"]
                && edge["target_id"] == source["id"]));
        }
        for path in ["from-sitemap", "from-feed"] {
            let entry = entities
                .iter()
                .find(|row| {
                    row["properties"]["url"] == format!("https://extract.example.test/{path}")
                })
                .unwrap();
            assert_eq!(entry["entity_type"], "resource");
            assert!(entry["properties"]["status"].is_null());
            assert!(edges.iter().any(|edge| edge["relation"] == "contains"
                && edge["source_id"] == reply["site"]
                && edge["target_id"] == entry["id"]));
        }
    }
}
