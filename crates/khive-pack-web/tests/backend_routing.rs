//! ADR-191 A1.2/A1.3: routed records and their main-backend body roots.
//! The registry uses the production multi-backend factory composition seam.

use khive_db::{stores::blob::FsBlobStore, StorageBackend};
use khive_pack_kg::KgPack;
use khive_pack_web::WebPack;
use khive_runtime::engine_config::WebSectionConfig;
use khive_runtime::{
    BackendId, KhiveRuntime, PackRegistry, RuntimeConfig, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{EdgeFilter, Entity, EntityFilter, PageRequest};
use khive_types::Namespace;
use serde_json::json;
use std::{collections::HashMap, path::Path, sync::Arc};

fn config(root: &Path, backend: &str) -> RuntimeConfig {
    RuntimeConfig {
        db_path: None,
        actor_id: None,
        backend_id: BackendId::parse(backend).unwrap(),
        web: WebSectionConfig {
            read_roots: vec![root.canonicalize().unwrap().to_string_lossy().into_owned()],
            ..Default::default()
        },
        ..RuntimeConfig::no_embeddings()
    }
}

fn migrated_backend(path: &Path) -> Arc<StorageBackend> {
    let backend = StorageBackend::sqlite(path).unwrap();
    {
        let mut writer = backend.pool().try_writer().unwrap();
        khive_db::run_migrations(writer.conn_mut()).unwrap();
    }
    Arc::new(backend)
}

fn registry(main: &KhiveRuntime, routed: &KhiveRuntime) -> VerbRegistry {
    let _linked_packs = (KgPack::new(main.clone()), WebPack::new(routed.clone()));
    let mut builder = VerbRegistryBuilder::new();
    PackRegistry::register_packs_with_runtimes(
        &["kg".into(), "web".into()],
        &HashMap::from([("web".into(), routed.clone())]),
        main,
        &mut builder,
    )
    .unwrap();
    let registry = builder.build().unwrap();
    main.install_edge_rules(registry.all_edge_rules());
    routed.install_edge_rules(registry.all_edge_rules());
    registry
}

struct Fixture {
    _dir: tempfile::TempDir,
    tree: std::path::PathBuf,
    main_backend: Arc<StorageBackend>,
    routed_backend: Arc<StorageBackend>,
    main: KhiveRuntime,
    routed: KhiveRuntime,
    registry: VerbRegistry,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("served");
        std::fs::create_dir(&tree).unwrap();
        std::fs::write(
            tree.join("index.html"),
            b"<html><body>routed page</body></html>",
        )
        .unwrap();
        std::fs::write(tree.join("data.json"), b"{\"routed\":true}").unwrap();
        let main_path = dir.path().join("main.db");
        let routed_path = dir.path().join("web.db");
        let main_backend = migrated_backend(&main_path);
        let main = KhiveRuntime::from_backend(main_backend.clone(), config(&tree, "main"));
        let routed_backend = migrated_backend(&routed_path);
        let routed = KhiveRuntime::from_backend(routed_backend.clone(), config(&tree, "web-map"))
            .with_core_backend(main_backend.clone());
        let store = Arc::new(FsBlobStore::new(dir.path().join("blobs"), 0).unwrap());
        main.install_blob_store(store.clone()).unwrap();
        routed.install_blob_store(store).unwrap();
        let registry = registry(&main, &routed);
        Self {
            _dir: dir,
            tree,
            main_backend,
            routed_backend,
            main,
            routed,
            registry,
        }
    }

    async fn ingest(&self) {
        self.registry
            .dispatch(
                "web.ingest",
                json!({
                    "source": self.tree.to_string_lossy(),
                    "origin": "https://routed.example.test",
                }),
            )
            .await
            .unwrap();
    }
}

async fn entities(runtime: &KhiveRuntime) -> Vec<Entity> {
    let token = runtime.authorize(Namespace::local()).unwrap();
    runtime
        .entities(&token)
        .unwrap()
        .query_entities(
            "local",
            EntityFilter::default(),
            PageRequest {
                offset: 0,
                limit: 1_000,
            },
        )
        .await
        .unwrap()
        .items
}

async fn fetched_entities(runtime: &KhiveRuntime) -> Vec<Entity> {
    entities(runtime)
        .await
        .into_iter()
        .filter(|entity| {
            entity
                .properties
                .as_ref()
                .and_then(|properties| properties.get("blob_ref"))
                .is_some_and(|reference| reference.is_string())
        })
        .collect()
}

// Must fail if the core pointer is absent, roots move to web, or records move to main.
#[tokio::test]
async fn a9_web_pack_scoped_backend_routes_records_and_attachments() {
    let fixture = Fixture::new();
    fixture.ingest().await;
    assert_ne!(fixture.main.backend_id(), fixture.routed.backend_id());
    assert_eq!(
        fixture.routed.core().backend_id(),
        fixture.main.backend_id()
    );
    let records = entities(&fixture.routed).await;
    assert!(!records.is_empty());
    assert!(entities(&fixture.main).await.is_empty());
    let token = fixture.main.authorize(Namespace::local()).unwrap();
    let notes = fixture
        .routed
        .notes(&token)
        .unwrap()
        .query_notes(
            "local",
            None,
            PageRequest {
                offset: 0,
                limit: 1_000,
            },
        )
        .await
        .unwrap()
        .items;
    assert!(!notes.is_empty());
    assert_eq!(
        fixture
            .main
            .notes(&token)
            .unwrap()
            .count_notes("local", None)
            .await
            .unwrap(),
        0
    );
    assert!(
        fixture
            .routed
            .graph(&token)
            .unwrap()
            .count_edges(EdgeFilter::default())
            .await
            .unwrap()
            > 0
    );
    assert_eq!(
        fixture
            .main
            .graph(&token)
            .unwrap()
            .count_edges(EdgeFilter::default())
            .await
            .unwrap(),
        0
    );
    let main_attachments = fixture.main.attachments().unwrap();
    let web_attachments = fixture.routed.backend().attachments().unwrap();
    let mut rooted = 0;
    for entity in records {
        let roots = main_attachments.list_attachments(entity.id).await.unwrap();
        assert!(web_attachments
            .list_attachments(entity.id)
            .await
            .unwrap()
            .is_empty());
        if let Some(reference) = entity
            .properties
            .as_ref()
            .and_then(|p| p["blob_ref"].as_str())
        {
            assert_eq!(roots.len(), 1);
            assert_eq!(roots[0].role, "content");
            assert_eq!(roots[0].content_ref.as_str(), reference);
            rooted += 1;
        } else {
            assert!(roots.is_empty());
        }
    }
    assert!(rooted >= 2, "page and resource each root their body");
    for note in notes {
        assert!(main_attachments
            .list_attachments(note.id)
            .await
            .unwrap()
            .is_empty());
        assert!(web_attachments
            .list_attachments(note.id)
            .await
            .unwrap()
            .is_empty());
    }
}

// Must fail if web.extract leaves the derived body unrooted, roots it in the
// routed database, or creates a second root when extraction is retried.
#[tokio::test]
async fn routed_extract_roots_derived_text_in_main_backend_once() {
    let fixture = Fixture::new();
    fixture.ingest().await;
    let page = fetched_entities(&fixture.routed)
        .await
        .into_iter()
        .find(|entity| entity.entity_type.as_deref() == Some("page"))
        .expect("ingest creates a fetched page");
    let token = fixture.main.authorize(Namespace::local()).unwrap();

    for _ in 0..2 {
        let reply = fixture
            .registry
            .dispatch("web.extract", json!({ "id": page.id, "kinds": ["text"] }))
            .await
            .expect("extract routed page text");
        let text_id = uuid::Uuid::parse_str(reply["result"]["text"]["id"].as_str().unwrap())
            .expect("derived text id");
        let derived = fixture
            .routed
            .entities(&token)
            .unwrap()
            .get_entity(text_id)
            .await
            .unwrap()
            .expect("derived text resource stays in web backend");
        assert_eq!(derived.entity_type.as_deref(), Some("resource"));
        assert!(fixture
            .main
            .entities(&token)
            .unwrap()
            .get_entity(text_id)
            .await
            .unwrap()
            .is_none());

        let roots = fixture
            .main
            .attachments()
            .unwrap()
            .list_attachments(text_id)
            .await
            .unwrap();
        assert_eq!(roots.len(), 1, "derived text has one durable root");
        assert_eq!(roots[0].role, "content");
        assert_eq!(roots[0].media_type.as_deref(), Some("text/plain"));
        assert_eq!(
            roots[0].content_ref.as_str(),
            derived.properties.as_ref().unwrap()["blob_ref"]
                .as_str()
                .unwrap()
        );
        assert!(fixture
            .routed
            .backend()
            .attachments()
            .unwrap()
            .list_attachments(text_id)
            .await
            .unwrap()
            .is_empty());
    }
}

#[tokio::test]
async fn a9_control_web_pack_without_binding_writes_to_default_backend() {
    let fixture = Fixture::new();
    let shared = &fixture.main;
    let registry = registry(shared, shared);
    registry
        .dispatch(
            "web.ingest",
            json!({
                "source": fixture.tree.to_string_lossy(),
                "origin": "https://default.example.test",
            }),
        )
        .await
        .unwrap();
    let fetched = fetched_entities(shared).await;
    assert!(!fetched.is_empty());
    for entity in fetched {
        assert_eq!(
            shared
                .attachments()
                .unwrap()
                .list_attachments(entity.id)
                .await
                .unwrap()
                .len(),
            1
        );
        registry
            .dispatch("delete", json!({ "id": entity.id, "hard": true }))
            .await
            .unwrap();
        assert!(shared
            .attachments()
            .unwrap()
            .list_attachments(entity.id)
            .await
            .unwrap()
            .is_empty());
    }
    assert!(!entities(shared).await.is_empty());
    assert!(entities(&fixture.routed).await.is_empty());
}

// Must fail if cleanup is omitted, soft deletion unroots the body, or routing stays on main.
#[tokio::test]
async fn routed_hard_delete_removes_main_attachments_after_record_commit() {
    let fixture = Fixture::new();
    fixture.ingest().await;
    let token = fixture.main.authorize(Namespace::local()).unwrap();
    let documents = fetched_entities(&fixture.routed).await;
    assert!(documents
        .iter()
        .any(|entity| entity.entity_type.as_deref() == Some("page")));
    assert!(documents
        .iter()
        .any(|entity| entity.entity_type.as_deref() == Some("resource")));
    for (index, entity) in documents.into_iter().enumerate() {
        let roots = fixture.main.attachments().unwrap();
        assert_eq!(roots.list_attachments(entity.id).await.unwrap().len(), 1);
        let id = if index == 0 {
            entity.id.simple().to_string()[..12].to_string()
        } else {
            entity.id.to_string()
        };
        fixture
            .registry
            .dispatch("delete", json!({ "id": id }))
            .await
            .unwrap();
        assert_eq!(roots.list_attachments(entity.id).await.unwrap().len(), 1);
        let reply = fixture
            .registry
            .dispatch("delete", json!({ "id": id, "hard": true }))
            .await
            .unwrap();
        assert_eq!(reply["deleted"], true);
        assert!(fixture
            .routed
            .entities(&token)
            .unwrap()
            .get_entity_including_deleted(entity.id)
            .await
            .unwrap()
            .is_none());
        assert!(roots.list_attachments(entity.id).await.unwrap().is_empty());
        assert!(
            !fixture
                .registry
                .cleanup_deleted_entity_attachments(&fixture.main, &token, entity.id)
                .await
                .unwrap(),
            "repeating the second commit succeeds without removing more rows"
        );
    }
}

// Abort the selected DELETE: web is the first commit, core cleanup is the second.
#[tokio::test]
async fn routed_hard_delete_interrupted_cleanup_leaves_only_orphan_attachments() {
    for fail_core in [false, true] {
        let fixture = Fixture::new();
        fixture.ingest().await;
        let entity = fetched_entities(&fixture.routed)
            .await
            .into_iter()
            .find(|entity| entity.entity_type.as_deref() == Some("page"))
            .unwrap();
        let (fault_backend, trigger, fault_marker) = if fail_core {
            (
                &fixture.main_backend,
                format!(
                    "CREATE TRIGGER i3135_delete_fault BEFORE DELETE ON attachments \
                     WHEN OLD.record_uuid = '{}' AND OLD.substrate = 'entity' \
                     BEGIN SELECT RAISE(ABORT, 'i3135-injected-core-attachment-delete'); END;",
                    entity.id
                ),
                "i3135-injected-core-attachment-delete",
            )
        } else {
            (
                &fixture.routed_backend,
                format!(
                    "CREATE TRIGGER i3135_delete_fault BEFORE DELETE ON entities \
                     WHEN OLD.id = '{}' \
                     BEGIN SELECT RAISE(ABORT, 'i3135-injected-routed-entity-delete'); END;",
                    entity.id
                ),
                "i3135-injected-routed-entity-delete",
            )
        };
        // This trigger belongs only to the fixture's TempDir database. A SQL
        // TEMP trigger would be connection-local and miss the runtime's queued
        // or standalone SqlAccess writer. Release the pooled writer before dispatch.
        {
            let writer = fault_backend.pool().try_writer().unwrap();
            writer.conn().execute_batch(&trigger).unwrap();
        }
        let error = fixture
            .registry
            .dispatch("delete", json!({ "id": entity.id, "hard": true }))
            .await
            .expect_err("selected backend refuses its injected DELETE");
        assert!(
            error.to_string().contains(fault_marker),
            "delete must fail at the selected injected transaction ({fault_marker}): {error}"
        );
        let token = fixture.main.authorize(Namespace::local()).unwrap();
        let remaining = fixture
            .routed
            .entities(&token)
            .unwrap()
            .get_entity_including_deleted(entity.id)
            .await
            .unwrap();
        assert_eq!(
            remaining.is_none(),
            fail_core,
            "only the second-commit fault may leave the record deleted"
        );
        assert_eq!(
            fixture
                .main
                .attachments()
                .unwrap()
                .list_attachments(entity.id)
                .await
                .unwrap()
                .len(),
            1,
            "failed first commit keeps a live root; failed second commit keeps only an orphan root"
        );
        {
            let writer = fault_backend.pool().try_writer().unwrap();
            writer
                .conn()
                .execute_batch("DROP TRIGGER i3135_delete_fault")
                .unwrap();
        }
        let retry = fixture
            .registry
            .dispatch("delete", json!({ "id": entity.id, "hard": true }))
            .await
            .unwrap();
        assert_eq!(retry["deleted"], true);
        if fail_core {
            assert_eq!(retry["attachment_cleanup"], true);
        }
        assert!(fixture
            .routed
            .entities(&token)
            .unwrap()
            .get_entity_including_deleted(entity.id)
            .await
            .unwrap()
            .is_none());
        assert!(fixture
            .main
            .attachments()
            .unwrap()
            .list_attachments(entity.id)
            .await
            .unwrap()
            .is_empty());
    }
}

// Deletion must retain kind guards and attribute the caller without filtering record ownership.
#[tokio::test]
async fn routed_delete_preserves_kind_guards_and_namespace_agnostic_lookup() {
    let fixture = Fixture::new();
    fixture.ingest().await;
    let entity = fetched_entities(&fixture.routed)
        .await
        .into_iter()
        .next()
        .unwrap();
    let error = fixture
        .registry
        .dispatch(
            "delete",
            json!({
                "id": entity.id, "kind": "service", "hard": true,
            }),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("kind mismatch"));
    let token = fixture.main.authorize(Namespace::local()).unwrap();
    assert!(
        !fixture
            .registry
            .cleanup_deleted_entity_attachments(&fixture.main, &token, entity.id)
            .await
            .unwrap(),
        "cleanup retries must refuse to unroot a live routed entity"
    );
    assert_eq!(
        fixture
            .main
            .attachments()
            .unwrap()
            .list_attachments(entity.id)
            .await
            .unwrap()
            .len(),
        1
    );
    let identity = khive_runtime::RequestIdentity {
        namespace: "other".into(),
        ..Default::default()
    };
    // Identity alone changes gate attribution; the explicit argument selects
    // the storage token's namespace. Prove this fixture crosses that boundary.
    let caller = fixture
        .registry
        .dispatch_with_identity(
            "whoami",
            json!({ "namespace": "other" }),
            Some(identity.clone()),
        )
        .await
        .unwrap();
    assert_eq!(caller["namespace"], "other");
    assert_ne!(
        entity.namespace,
        caller["namespace"].as_str().unwrap(),
        "routed deletion fixture must use a different storage namespace"
    );
    let reply = fixture
        .registry
        .dispatch_with_identity(
            "delete",
            json!({ "id": entity.id, "hard": true, "namespace": "other" }),
            Some(identity),
        )
        .await
        .expect("routed by-ID delete must cross the explicit namespace");
    assert_eq!(reply["deleted"], true);
    assert!(fixture
        .main
        .attachments()
        .unwrap()
        .list_attachments(entity.id)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn routed_delete_refuses_duplicate_entity_ids_without_removing_body_roots() {
    let fixture = Fixture::new();
    fixture.ingest().await;
    let entity = fetched_entities(&fixture.routed)
        .await
        .into_iter()
        .next()
        .unwrap();
    let token = fixture.main.authorize(Namespace::local()).unwrap();
    fixture
        .main
        .entities(&token)
        .unwrap()
        .upsert_entity(entity.clone())
        .await
        .unwrap();
    let error = fixture
        .registry
        .dispatch("delete", json!({ "id": entity.id, "hard": true }))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("multiple backends"));
    for runtime in [&fixture.main, &fixture.routed] {
        assert!(runtime
            .entities(&token)
            .unwrap()
            .get_entity(entity.id)
            .await
            .unwrap()
            .is_some());
    }
    assert_eq!(
        fixture
            .main
            .attachments()
            .unwrap()
            .list_attachments(entity.id)
            .await
            .unwrap()
            .len(),
        1
    );
}
