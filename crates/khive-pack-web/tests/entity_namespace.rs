//! Namespace attribution and deterministic web-identity collisions.

use std::path::Path;
use std::sync::Arc;

use khive_pack_kg::KgPack;
use khive_pack_web::WebPack;
use khive_runtime::engine_config::WebSectionConfig;
use khive_runtime::pack::{VerbRegistry, VerbRegistryBuilder};
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};
use khive_storage::{EdgeFilter, Entity, EntityFilter, PageRequest};
use serde_json::{json, Value};
use uuid::Uuid;

async fn namespace_entities(runtime: &KhiveRuntime, namespace: &str) -> Vec<Entity> {
    let token = runtime
        .authorize(Namespace::parse(namespace).unwrap())
        .unwrap();
    let mut entities = runtime
        .entities(&token)
        .unwrap()
        .query_entities(
            namespace,
            EntityFilter::default(),
            PageRequest {
                offset: 0,
                limit: 100,
            },
        )
        .await
        .unwrap()
        .items;
    entities.sort_by_key(|entity| entity.id);
    entities
}

#[tokio::test]
async fn runtime_by_id_is_global_and_duplicate_id_insert_preserves_namespace_attribution() {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        actor_id: None,
        brain_profile: None,
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let alpha = runtime
        .authorize(Namespace::parse("alpha").unwrap())
        .unwrap();
    let beta = runtime
        .authorize(Namespace::parse("beta").unwrap())
        .unwrap();
    let alpha_store = runtime.entities(&alpha).unwrap();
    let beta_store = runtime.entities(&beta).unwrap();
    let mut winner = Entity::new("alpha", "document", "first writer")
        .with_entity_type(Some("resource"))
        .with_properties(json!({"marker": "keep"}));
    winner.id = Uuid::from_u128(0x1910_0000_0000_5000_8000_0000_0000_0001);
    assert!(alpha_store
        .insert_entity_if_absent(winner.clone())
        .await
        .unwrap());

    // ADR-007: the token's namespace scopes multi-record queries, not by-ID access.
    let from_beta = beta_store.get_entity(winner.id).await.unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(&from_beta).unwrap(),
        serde_json::to_value(&winner).unwrap()
    );
    assert!(namespace_entities(&runtime, "beta").await.is_empty());

    let mut loser = winner.clone();
    loser.namespace = "beta".into();
    loser.name = "must not replace the first writer".into();
    loser.properties = Some(json!({"marker": "replace"}));
    assert!(!beta_store.insert_entity_if_absent(loser).await.unwrap());
    let after = beta_store.get_entity(winner.id).await.unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(&after).unwrap(),
        serde_json::to_value(&winner).unwrap()
    );
    assert_eq!(namespace_entities(&runtime, "alpha").await.len(), 1);
    assert!(namespace_entities(&runtime, "beta").await.is_empty());

    let distinct =
        Entity::new("beta", "document", "separate identity").with_entity_type(Some("resource"));
    assert!(beta_store.insert_entity_if_absent(distinct).await.unwrap());
    assert_eq!(namespace_entities(&runtime, "beta").await.len(), 1);
}

fn fixture(read_root: &Path) -> (tempfile::TempDir, KhiveRuntime, VerbRegistry) {
    let data = tempfile::tempdir().unwrap();
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(data.path().join("namespace.db")),
        actor_id: None,
        brain_profile: None,
        web: WebSectionConfig {
            read_roots: vec![read_root
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned()],
            ..Default::default()
        },
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let blobs = khive_db::stores::blob::FsBlobStore::new(data.path().join("blobs"), 0).unwrap();
    runtime.install_blob_store(Arc::new(blobs)).unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(WebPack::new(runtime.clone()));
    let registry = builder.build().unwrap();
    runtime.install_edge_rules(registry.all_edge_rules());
    (data, runtime, registry)
}

fn ingest_args(root: &Path, namespace: &str) -> Value {
    json!({
        "source": root.canonicalize().unwrap().to_string_lossy(),
        "origin": "https://namespace.example.test",
        "namespace": namespace,
    })
}

async fn namespace_notes(runtime: &KhiveRuntime, namespace: &str) -> Value {
    let token = runtime
        .authorize(Namespace::parse(namespace).unwrap())
        .unwrap();
    let mut notes = runtime
        .notes(&token)
        .unwrap()
        .query_notes(
            namespace,
            None,
            PageRequest {
                offset: 0,
                limit: 100,
            },
        )
        .await
        .unwrap()
        .items;
    notes.sort_by_key(|note| note.id);
    serde_json::to_value(notes).unwrap()
}

async fn domain_snapshot(runtime: &KhiveRuntime) -> Value {
    let token = runtime
        .authorize(Namespace::parse("alpha").unwrap())
        .unwrap();
    let mut edges = runtime
        .graph(&token)
        .unwrap()
        .query_edges(
            EdgeFilter::default(),
            Vec::new(),
            PageRequest {
                offset: 0,
                limit: 100,
            },
        )
        .await
        .unwrap()
        .items;
    edges.sort_by_key(|edge| edge.id.0);
    json!({
        "alpha": namespace_entities(runtime, "alpha").await,
        "beta": namespace_entities(runtime, "beta").await,
        "alpha_notes": namespace_notes(runtime, "alpha").await,
        "beta_notes": namespace_notes(runtime, "beta").await,
        "edges": edges,
    })
}

async fn assert_foreign_identity_refused(caller_owns_site: bool) {
    let tree = tempfile::tempdir().unwrap();
    std::fs::write(
        tree.path().join("index.html"),
        b"<html><body>first body</body></html>",
    )
    .unwrap();
    let (_data, runtime, registry) = fixture(tree.path());
    let first = registry
        .dispatch("web.ingest", ingest_args(tree.path(), "alpha"))
        .await
        .unwrap();
    let repeated = registry
        .dispatch("web.ingest", ingest_args(tree.path(), "alpha"))
        .await
        .unwrap();
    assert_eq!(repeated["site"], first["site"]);
    assert_eq!(
        repeated["ingested"], first["ingested"],
        "same-namespace address identity remains idempotent"
    );
    let site_id = Uuid::parse_str(first["site"].as_str().unwrap()).unwrap();
    let document_id = Uuid::parse_str(first["ingested"][0].as_str().unwrap()).unwrap();
    let beta = runtime
        .authorize(Namespace::parse("beta").unwrap())
        .unwrap();
    let store = runtime.entities(&beta).unwrap();
    assert_eq!(
        store
            .get_entity(document_id)
            .await
            .unwrap()
            .unwrap()
            .namespace,
        "alpha"
    );

    if caller_owns_site {
        // Deliberately seed mixed attribution to reach the document reuse arm.
        // Ordinary by-ID access remains global; this is fixture setup, not a web policy.
        let mut site = store.get_entity(site_id).await.unwrap().unwrap();
        site.namespace = "beta".into();
        store.upsert_entity(site).await.unwrap();
    }
    let before = domain_snapshot(&runtime).await;
    std::fs::write(
        tree.path().join("index.html"),
        b"<html><body>replacement body</body></html>",
    )
    .unwrap();

    // Must fail without a web-specific collision refusal: the baseline reuses
    // the foreign ID and patches the other namespace's document successfully.
    let error = registry
        .dispatch("web.ingest", ingest_args(tree.path(), "beta"))
        .await
        .expect_err("foreign deterministic identity must be refused before reusing its row");
    let refused_id = if caller_owns_site {
        document_id
    } else {
        site_id
    };
    let projected =
        khive_runtime::runtime_error_value(error, khive_runtime::DomainDisposition::Unknown);
    assert_eq!(projected["kind"], "not_found");
    assert_eq!(
        projected["message"],
        format!("web entity not found: {refused_id}")
    );
    assert!(projected["details"].is_null());
    assert!(projected["code"].is_null());
    assert_eq!(
        domain_snapshot(&runtime).await,
        before,
        "refusal must not change either namespace's entities, edges, or receipts"
    );
}

#[tokio::test]
async fn web_ingest_refuses_foreign_deterministic_site_without_changing_entities() {
    assert_foreign_identity_refused(false).await;
}

#[tokio::test]
async fn web_ingest_refuses_foreign_deterministic_document_under_caller_owned_site() {
    assert_foreign_identity_refused(true).await;
}
