//! A9 (ADR-191): pack-scoped backend routing. At serve time, `[[backends]]` +
//! `[packs.web].backend = "web-map"` makes `khive-mcp/src/serve.rs`'s
//! `build_registry_for_multi_backend_inner` construct a `KhiveRuntime` bound to
//! that backend and hand it to `WebPackFactory::create` (`build_pack_runtime`,
//! `resolve_pack_backend_config`, `PackRegistry::register_packs_with_runtimes`).
//! The web pack itself holds no backend-selection logic — every handler threads
//! `&self.runtime` (`fetch.rs`/`extract.rs`/`ingest.rs`/`search.rs`/`refresh.rs`)
//! into the write path, so it writes through whichever runtime it was
//! constructed with. This test reconstructs that same shape directly at the
//! `KhiveRuntime`/`VerbRegistryBuilder` level rather than exercising the real
//! `[[backends]]` TOML path end to end: that path's own composition function,
//! `khive_mcp::serve::resolve_pack_backend_config`, is real and `pub`, but
//! reaching it from this crate's test suite would need `khive-mcp` as a dev
//! dependency here — and `khive-mcp` already depends on `khive-pack-web` as an
//! ordinary dependency, so that edge is a dependency cycle this pass did not
//! have the means to verify resolves (no `cargo` access in this pass). The
//! gap this leaves: whether a real `[[backends]]` + `[packs.web]` TOML
//! document actually resolves to the runtime this test hand-picks is asserted
//! by construction here, not exercised — `khive-mcp`'s own test suite is
//! where `resolve_pack_backend_config`/`build_registry_for_multi_backend` can
//! be driven directly, since the dependency runs the other way from there.

use khive_db::stores::blob::FsBlobStore;
use khive_pack_kg::KgPack;
use khive_pack_web::WebPack;
use khive_runtime::engine_config::WebSectionConfig;
use khive_runtime::pack::VerbRegistryBuilder;
use khive_runtime::{KhiveRuntime, RuntimeConfig};
use khive_storage::{EntityFilter, PageRequest};
use khive_types::Namespace;
use serde_json::json;
use std::sync::Arc;

fn write_served_tree(root: &std::path::Path) {
    std::fs::write(
        root.join("index.html"),
        b"<html><body>routed page</body></html>",
    )
    .unwrap();
}

/// An in-memory runtime whose `[web] read_roots` allows disk ingest from
/// `root` — plain `KhiveRuntime::memory()` leaves `read_roots` empty, and
/// disk ingest refuses unconditionally against that (`ingest.rs`'s
/// `confine_to_read_roots`).
fn memory_runtime_with_read_root(root: &std::path::Path) -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        actor_id: None,
        web: WebSectionConfig {
            read_roots: vec![root.to_string_lossy().into_owned()],
            ..Default::default()
        },
        ..RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime")
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

// A9: with a pack-scoped backend, web writes land in the configured backend and
// leave the default/main backend untouched.
#[tokio::test]
async fn a9_web_pack_scoped_backend_routes_writes_to_configured_backend_only() {
    let tree = tempfile::tempdir().expect("served tree");
    write_served_tree(tree.path());

    let default_runtime = KhiveRuntime::memory().expect("default/main runtime");
    let routed_runtime = memory_runtime_with_read_root(tree.path());
    let blob_dir = tempfile::tempdir().expect("blob dir");
    let store = FsBlobStore::new(blob_dir.path().to_path_buf(), 0).expect("fs blob store");
    routed_runtime
        .install_blob_store(Arc::new(store))
        .expect("install blob store on the routed runtime only");

    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(default_runtime.clone()));
    builder.register(WebPack::new(routed_runtime.clone()));
    let registry = builder
        .build()
        .expect("registry builds with web bound to its own runtime");
    // Mirrors `build_registry_for_multi_backend_inner`: edge rules are
    // installed on every per-pack runtime AND the default runtime.
    default_runtime.install_edge_rules(registry.all_edge_rules());
    routed_runtime.install_edge_rules(registry.all_edge_rules());

    registry
        .dispatch(
            "web.ingest",
            json!({
                "source": tree.path().to_string_lossy(),
                "origin": "https://routed.example.test",
            }),
        )
        .await
        .expect("web.ingest dispatches through the runtime it was constructed with");

    let routed_count = entity_count(&routed_runtime).await;
    let default_count = entity_count(&default_runtime).await;
    assert!(
        routed_count > 0,
        "the web pack's writes must land in the runtime it was handed"
    );
    assert_eq!(
        default_count, 0,
        "a pack-scoped backend must leave the default/main backend untouched"
    );
}

// Control: without a distinct `[packs.web]` binding — the same runtime backs
// every pack, matching `resolve_pack_backend_config`'s `None` arm
// (`BackendId::MAIN`) — web writes land in that one, shared, default backend.
#[tokio::test]
async fn a9_control_web_pack_without_binding_writes_to_default_backend() {
    let tree = tempfile::tempdir().expect("served tree");
    write_served_tree(tree.path());

    let shared_runtime = memory_runtime_with_read_root(tree.path());
    let blob_dir = tempfile::tempdir().expect("blob dir");
    let store = FsBlobStore::new(blob_dir.path().to_path_buf(), 0).expect("fs blob store");
    shared_runtime
        .install_blob_store(Arc::new(store))
        .expect("install blob store");

    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(shared_runtime.clone()));
    builder.register(WebPack::new(shared_runtime.clone()));
    let registry = builder
        .build()
        .expect("registry builds with every pack on the default runtime");
    shared_runtime.install_edge_rules(registry.all_edge_rules());

    registry
        .dispatch(
            "web.ingest",
            json!({
                "source": tree.path().to_string_lossy(),
                "origin": "https://default.example.test",
            }),
        )
        .await
        .expect("web.ingest dispatches");

    assert!(
        entity_count(&shared_runtime).await > 0,
        "with no pack-scoped binding, writes land in the one default backend"
    );
}
