//! Namespace attribution and deterministic web-identity collisions.

use std::path::Path;
use std::sync::Arc;

use khive_pack_kg::KgPack;
use khive_pack_web::identity;
use khive_pack_web::producer::{self, Capture};
use khive_pack_web::WebPack;
use khive_runtime::engine_config::WebSectionConfig;
use khive_runtime::pack::{VerbRegistry, VerbRegistryBuilder};
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};
use khive_storage::{
    Attachment, AttachmentSubstrate, ContentRef, Direction, EdgeFilter, EdgeRelation, Entity,
    EntityFilter, NewAttachment, PageRequest,
};
use serde_json::{json, Value};
use url::Url;
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

    // Namespaced derivation prevents ordinary collisions. Deliberately place
    // foreign attribution at beta's derived ID to retain the mutation guard.
    let beta_site = identity::site_id(
        beta.namespace(),
        &Url::parse("https://namespace.example.test").unwrap(),
    );
    let beta_document = identity::document_id(beta_site, "/index.html");
    let mut site = store.get_entity(site_id).await.unwrap().unwrap();
    site.id = beta_site;
    if caller_owns_site {
        site.namespace = "beta".into();
        let mut document = store.get_entity(document_id).await.unwrap().unwrap();
        document.id = beta_document;
        assert!(store.insert_entity_if_absent(document).await.unwrap());
    }
    assert!(store.insert_entity_if_absent(site).await.unwrap());
    let before = domain_snapshot(&runtime).await;
    std::fs::write(
        tree.path().join("index.html"),
        b"<html><body>replacement body</body></html>",
    )
    .unwrap();

    let error = registry
        .dispatch("web.ingest", ingest_args(tree.path(), "beta"))
        .await
        .expect_err("foreign deterministic identity must be refused before reusing its row");
    let refused_id = if caller_owns_site {
        beta_document
    } else {
        beta_site
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

fn address_ids(namespace: &str, url: &str) -> (Uuid, Uuid) {
    let canonical = identity::canonicalize(Url::parse(url).unwrap());
    let site = identity::site_id(&Namespace::parse(namespace).unwrap(), &canonical);
    (
        site,
        identity::document_id(site, &identity::path_and_query(&canonical)),
    )
}

fn body_blob_paths(root: &Path) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            paths.extend(body_blob_paths(&entry.path()));
        } else {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.len() == 64 && name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                paths.push(entry.path());
            }
        }
    }
    paths
}

#[tokio::test]
async fn same_url_captures_separate_namespace_rows_and_share_body_cas() {
    let tree = tempfile::tempdir().unwrap();
    let (data, runtime, _registry) = fixture(tree.path());
    let url = Url::parse("https://Example.test:443/article?z=9&a=2#section").unwrap();
    let body = b"<html><body>shared capture</body></html>".to_vec();
    let digest = ContentRef::from_digest_bytes(blake3::hash(&body).as_bytes());
    let mut documents = Vec::new();
    let mut alpha_before_beta = None;
    for namespace in ["alpha", "beta"] {
        let token = runtime
            .authorize(Namespace::parse(namespace).unwrap())
            .unwrap();
        let expected = address_ids(namespace, url.as_str());
        assert_eq!(
            producer::mint_resource(&runtime, &token, &url)
                .await
                .unwrap(),
            expected
        );
        let mut capture = Capture::get(url.clone(), 200, body.clone());
        capture.headers.content_type = Some("text/html".into());
        let reply = producer::store_capture(&runtime, &token, capture)
            .await
            .unwrap();
        assert_eq!(reply["id"], expected.1.to_string());
        assert_eq!(reply["content_ref"], digest.to_string());
        let entity = runtime
            .entities(&token)
            .unwrap()
            .get_entity(expected.1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entity.namespace, namespace);
        assert_eq!(entity.entity_type.as_deref(), Some("page"));
        let attachment = runtime
            .core()
            .attachments()
            .unwrap()
            .get_attachment(expected.1, "content")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(attachment.content_ref, digest);
        let receipt_id = Uuid::parse_str(reply["receipt_id"].as_str().unwrap()).unwrap();
        let receipt = runtime
            .notes(&token)
            .unwrap()
            .get_note(receipt_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt.namespace, namespace);
        assert_eq!(
            receipt.properties.unwrap()["request"]["body_entity_id"],
            expected.1.to_string()
        );
        let contained = runtime
            .neighbors(
                &token,
                expected.0,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Contains]),
            )
            .await
            .unwrap();
        assert_eq!(contained.len(), 1);
        assert_eq!(contained[0].node_id, expected.1);
        documents.push(expected.1);
        if namespace == "alpha" {
            // An independently authorized producer in one namespace converges
            // even when URL spelling and query order differ.
            let independent = runtime
                .authorize(Namespace::parse(namespace).unwrap())
                .unwrap();
            let equivalent = Url::parse("https://example.test/article?a=2&z=9").unwrap();
            assert_eq!(
                producer::mint_resource(&runtime, &independent, &equivalent)
                    .await
                    .unwrap(),
                expected
            );
            let mut repeated = Capture::get(equivalent, 200, body.clone());
            repeated.headers.content_type = Some("text/html".into());
            let reply = producer::store_capture(&runtime, &independent, repeated)
                .await
                .unwrap();
            assert_eq!(reply["id"], expected.1.to_string());
            alpha_before_beta =
                Some(serde_json::to_value(namespace_entities(&runtime, "alpha").await).unwrap());
        }
        assert_eq!(
            namespace_entities(&runtime, namespace).await.len(),
            2,
            "one site and one page per namespace"
        );
    }
    assert_ne!(documents[0], documents[1]);
    assert_eq!(
        serde_json::to_value(namespace_entities(&runtime, "alpha").await).unwrap(),
        alpha_before_beta.unwrap()
    );
    assert!(namespace_entities(&runtime, "local").await.is_empty());
    assert_eq!(
        body_blob_paths(&data.path().join("blobs")).len(),
        1,
        "both namespace attachments root the same physical CAS body"
    );
    assert_eq!(
        runtime
            .blob_store()
            .unwrap()
            .get_bounded_verified(&digest, body.len() as u64)
            .await
            .unwrap(),
        body
    );
}

#[tokio::test]
async fn disk_ingest_and_url_extract_use_effective_namespace_for_all_rows() {
    let tree = tempfile::tempdir().unwrap();
    let body = b"<html><body><p>namespace text</p><a href=\"https://target.example.test/item?z=9&a=2\">target</a></body></html>";
    std::fs::write(tree.path().join("index.html"), body).unwrap();
    let (_data, runtime, registry) = fixture(tree.path());
    let source_url = "https://namespace.example.test/index.html";
    let target_url = "https://target.example.test/item?a=2&z=9";
    let digest = ContentRef::from_digest_bytes(blake3::hash(body).as_bytes());
    let mut alpha_before_beta = None;
    for namespace in ["alpha", "beta"] {
        let token = runtime
            .authorize(Namespace::parse(namespace).unwrap())
            .unwrap();
        let source = address_ids(namespace, source_url);
        let target = address_ids(namespace, target_url);
        let text = identity::derived_text_id(source.1, digest.as_ref());
        let ingested = registry
            .dispatch("web.ingest", ingest_args(tree.path(), namespace))
            .await
            .unwrap();
        assert_eq!(ingested["site"], source.0.to_string());
        assert_eq!(ingested["ingested"], json!([source.1.to_string()]));
        let extracted = registry
            .dispatch(
                "web.extract",
                json!({
                    "url": format!("{source_url}#heading"),
                    "kinds": ["links", "text"],
                    "namespace": namespace,
                }),
            )
            .await
            .unwrap();
        assert_eq!(extracted["id"], source.1.to_string());
        assert_eq!(extracted["result"]["text"]["id"], text.to_string());
        assert_eq!(extracted["result"]["links"]["admitted_targets"], 1);
        let receipt_id = Uuid::parse_str(extracted["receipt_id"].as_str().unwrap()).unwrap();
        assert_eq!(
            runtime
                .notes(&token)
                .unwrap()
                .get_note(receipt_id)
                .await
                .unwrap()
                .unwrap()
                .namespace,
            namespace
        );
        let rows = namespace_entities(&runtime, namespace).await;
        let mut expected = vec![source.0, source.1, target.0, target.1, text];
        expected.sort_unstable();
        assert_eq!(
            rows.iter().map(|entity| entity.id).collect::<Vec<_>>(),
            expected
        );
        let source_row = rows.iter().find(|entity| entity.id == source.1).unwrap();
        assert_eq!(
            source_row.properties.as_ref().unwrap()["blob_ref"],
            digest.to_string()
        );
        for (from, relation, to) in [
            (source.0, EdgeRelation::Contains, source.1),
            (target.0, EdgeRelation::Contains, target.1),
            (source.1, EdgeRelation::LinksTo, target.1),
            (text, EdgeRelation::DerivedFrom, source.1),
        ] {
            let neighbors = runtime
                .neighbors(&token, from, Direction::Out, None, Some(vec![relation]))
                .await
                .unwrap();
            assert_eq!(neighbors.len(), 1);
            assert_eq!(neighbors[0].node_id, to);
        }
        if namespace == "alpha" {
            alpha_before_beta = Some(serde_json::to_value(rows).unwrap());
        }
    }
    assert_eq!(
        serde_json::to_value(namespace_entities(&runtime, "alpha").await).unwrap(),
        alpha_before_beta.unwrap()
    );
    assert!(namespace_entities(&runtime, "local").await.is_empty());
}

#[tokio::test]
async fn feed_and_sitemap_entries_use_the_effective_namespace_and_publishing_site() {
    let tree = tempfile::tempdir().unwrap();
    let (_data, runtime, registry) = fixture(tree.path());
    let cases = [
        ("sitemap", "https://publisher.example.test/map.xml", "https://entries.example.test/map-item", "<urlset><url><loc>https://entries.example.test/map-item</loc></url></urlset>"),
        ("feed", "https://publisher.example.test/feed.xml", "https://entries.example.test/feed-item", "<rss><channel><item><link>https://entries.example.test/feed-item</link></item></channel></rss>"),
    ];
    for namespace in ["alpha", "beta"] {
        let token = runtime
            .authorize(Namespace::parse(namespace).unwrap())
            .unwrap();
        let mut expected = Vec::new();
        for (kind, source_url, target_url, body) in cases {
            let source = address_ids(namespace, source_url);
            let target = address_ids(namespace, target_url);
            let mut capture = Capture::get(
                Url::parse(source_url).unwrap(),
                200,
                body.as_bytes().to_vec(),
            );
            capture.headers.content_type = Some("application/xml".into());
            producer::store_capture(&runtime, &token, capture)
                .await
                .unwrap();
            let extracted = registry
                .dispatch(
                    "web.extract",
                    json!({"url": source_url, "kinds": [kind], "namespace": namespace}),
                )
                .await
                .unwrap();
            assert_eq!(extracted["result"][kind]["entries"], 1);
            for site in [source.0, target.0] {
                let contained = runtime
                    .neighbors(
                        &token,
                        site,
                        Direction::Out,
                        None,
                        Some(vec![EdgeRelation::Contains]),
                    )
                    .await
                    .unwrap();
                assert!(contained
                    .iter()
                    .any(|neighbor| neighbor.node_id == target.1));
            }
            expected.extend([source.0, source.1, target.0, target.1]);
        }
        expected.sort_unstable();
        expected.dedup();
        assert_eq!(
            namespace_entities(&runtime, namespace)
                .await
                .iter()
                .map(|entity| entity.id)
                .collect::<Vec<_>>(),
            expected
        );
    }
    assert!(namespace_entities(&runtime, "local").await.is_empty());
}

#[tokio::test]
async fn namespaced_capture_preserves_legacy_url_only_rows() {
    let tree = tempfile::tempdir().unwrap();
    let (_data, runtime, _registry) = fixture(tree.path());
    let token = runtime
        .authorize(Namespace::parse("alpha").unwrap())
        .unwrap();
    let url = Url::parse("https://legacy.example.test/page").unwrap();
    let legacy_site = Uuid::new_v5(
        &identity::WEB_NAMESPACE,
        format!("site|{}", identity::site_key(&url)).as_bytes(),
    );
    let legacy_document = identity::document_id(legacy_site, "/page");
    let mut legacy = Entity::new("alpha", "document", "legacy row")
        .with_entity_type(Some("page"))
        .with_properties(json!({"url": url.as_str(), "marker": "retain"}));
    legacy.id = legacy_document;
    let store = runtime.entities(&token).unwrap();
    assert!(store.insert_entity_if_absent(legacy.clone()).await.unwrap());
    let mut capture = Capture::get(url.clone(), 200, b"new namespaced body".to_vec());
    capture.headers.content_type = Some("text/html".into());
    let reply = producer::store_capture(&runtime, &token, capture)
        .await
        .unwrap();
    assert_eq!(
        reply["id"],
        address_ids("alpha", url.as_str()).1.to_string()
    );
    assert_ne!(reply["id"], legacy_document.to_string());
    assert_eq!(
        serde_json::to_value(store.get_entity(legacy_document).await.unwrap().unwrap()).unwrap(),
        serde_json::to_value(legacy).unwrap()
    );
    assert_eq!(namespace_entities(&runtime, "alpha").await.len(), 3);
}

#[tokio::test]
async fn legacy_feed_and_sitemap_by_id_mint_publishing_site_and_preserve_source_rows() {
    let cases = [
        ("sitemap", "https://legacy-publisher.example.test/map.xml", "https://entries.example.test/map-item", "<urlset><url><loc>https://entries.example.test/map-item</loc></url></urlset>"),
        ("feed", "https://legacy-publisher.example.test/feed.xml", "https://entries.example.test/feed-item", "<rss><channel><item><link>https://entries.example.test/feed-item</link></item></channel></rss>"),
    ];
    for (kind, source_url, target_url, body) in cases {
        let tree = tempfile::tempdir().unwrap();
        let (_data, runtime, registry) = fixture(tree.path());
        let token = runtime
            .authorize(Namespace::parse("alpha").unwrap())
            .unwrap();
        let canonical = identity::canonicalize(Url::parse(source_url).unwrap());
        let legacy_site = Uuid::new_v5(
            &identity::WEB_NAMESPACE,
            format!("site|{}", identity::site_key(&canonical)).as_bytes(),
        );
        let legacy_document =
            identity::document_id(legacy_site, &identity::path_and_query(&canonical));
        let mut site = Entity::new("alpha", "service", "legacy publishing site")
            .with_entity_type(Some("site"))
            .with_properties(json!({"marker": "retain site"}));
        site.id = legacy_site;
        let content_ref = runtime
            .blob_store()
            .unwrap()
            .put(body.as_bytes().to_vec())
            .await
            .unwrap();
        let mut document = Entity::new("alpha", "document", "legacy XML document")
            .with_entity_type(Some("resource"))
            .with_properties(json!({
                "url": source_url,
                "content_type": "application/xml",
                "blob_ref": content_ref.as_str(),
                "marker": "retain document",
            }));
        document.id = legacy_document;
        let store = runtime.entities(&token).unwrap();
        assert!(store.insert_entity_if_absent(site).await.unwrap());
        assert!(store.insert_entity_if_absent(document).await.unwrap());
        let attachment = Attachment::from_new(
            legacy_document,
            AttachmentSubstrate::Entity,
            NewAttachment {
                role: "content".into(),
                content_ref,
                media_type: Some("application/xml".into()),
                size_bytes: Some(body.len() as u64),
            },
            chrono::Utc::now().timestamp_micros(),
        );
        let attachments = runtime.core().attachments().unwrap();
        attachments
            .upsert_attachment(attachment.clone())
            .await
            .unwrap();
        runtime
            .link(
                &token,
                legacy_site,
                legacy_document,
                EdgeRelation::Contains,
                1.0,
                None,
            )
            .await
            .unwrap();
        let mut legacy_before = Vec::new();
        for id in [legacy_site, legacy_document] {
            legacy_before
                .push(serde_json::to_value(store.get_entity(id).await.unwrap().unwrap()).unwrap());
        }
        let publishing = address_ids("alpha", source_url);
        let target = address_ids("alpha", target_url);
        assert_ne!(publishing.0, legacy_site);
        assert!(store.get_entity(publishing.0).await.unwrap().is_none());
        let extracted = registry
            .dispatch(
                "web.extract",
                json!({"id": legacy_document, "namespace": "alpha"}),
            )
            .await
            .unwrap();
        assert_eq!(extracted["id"], legacy_document.to_string());
        assert_eq!(extracted["kinds"], json!(["sitemap", "feed"]));
        assert_eq!(extracted["result"][kind]["entries"], 1);
        assert_eq!(extracted["status"], "complete");
        for site_id in [publishing.0, target.0] {
            assert_eq!(
                store.get_entity(site_id).await.unwrap().unwrap().namespace,
                "alpha"
            );
            let contained = runtime
                .neighbors(
                    &token,
                    site_id,
                    Direction::Out,
                    None,
                    Some(vec![EdgeRelation::Contains]),
                )
                .await
                .unwrap();
            assert_eq!(contained.len(), 1);
            assert_eq!(contained[0].node_id, target.1);
        }
        assert!(store.get_entity(publishing.1).await.unwrap().is_none());
        let mut legacy_after = Vec::new();
        for id in [legacy_site, legacy_document] {
            legacy_after
                .push(serde_json::to_value(store.get_entity(id).await.unwrap().unwrap()).unwrap());
        }
        assert_eq!(legacy_after, legacy_before);
        assert_eq!(
            attachments
                .get_attachment(legacy_document, "content")
                .await
                .unwrap()
                .unwrap(),
            attachment
        );
        let legacy_contains = runtime
            .neighbors(
                &token,
                legacy_site,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Contains]),
            )
            .await
            .unwrap();
        assert_eq!(legacy_contains.len(), 1);
        assert_eq!(legacy_contains[0].node_id, legacy_document);
        assert!(namespace_entities(&runtime, "local").await.is_empty());
    }
}
