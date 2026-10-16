use khive_pack_gtd::GtdPack;
use khive_pack_kg::KgPack;
use khive_runtime::{
    KhiveRuntime, Namespace, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::Note;
use serde_json::{json, Value};
use uuid::Uuid;

struct Fixture {
    runtime: KhiveRuntime,
    registry: VerbRegistry,
}

impl Fixture {
    fn new() -> Self {
        let config = RuntimeConfig {
            db_path: None,
            embedding_model: None,
            additional_embedding_models: Vec::new(),
            wal_ceiling_bytes: 0,
            wal_ceiling_configured_bytes: 0,
            wal_ceiling_source: Default::default(),
            wal_ceiling_env_raw: None,
            disk_guard_environment: Default::default(),
            disk_guard_config: None,
            volume_lock_dir: None,
            credentials: Vec::new(),
            visibility_receipts: None,
            default_namespace: Namespace::local(),
            visible_namespaces: Vec::new(),
            allowed_outbound_namespaces: Vec::new(),
            packs: vec!["kg".into(), "gtd".into()],
            actor_id: None,
            brain_profile: None,
            brain: Default::default(),
            blob: Default::default(),
            mounts: Vec::new(),
            events_split: None,
            display_timezone: chrono_tz::UTC,
            ..RuntimeConfig::no_embeddings()
        };
        let runtime = KhiveRuntime::new(config).expect("private memory runtime");
        assert!(runtime.config().db_path.is_none());
        assert!(runtime.config().embedding_model.is_none());
        assert!(runtime.config().additional_embedding_models.is_empty());
        assert!(!runtime.backend().is_file_backed());
        assert!(runtime.backend_data_dir().is_none());
        assert!(runtime.backend_ann_root().is_none());
        assert!(runtime.backend().pool().canonical_path().is_none());
        assert!(runtime.default_embedder_name().is_empty());
        assert!(runtime.blob_store().is_none());
        let mut builder = VerbRegistryBuilder::new();
        builder.with_actor_id(Some("due-zone-alias-fixture".into()));
        builder.with_default_namespace("local");
        builder.with_visible_namespaces(Vec::new());
        builder.register(KgPack::new(runtime.clone()));
        builder.register(GtdPack::new(runtime.clone()));
        let registry = builder.build().expect("registry");
        registry.apply_schema_plans(runtime.backend());
        runtime.install_edge_rules(registry.all_edge_rules());
        Self { runtime, registry }
    }

    async fn seed(&self, zone: Option<&str>) -> Uuid {
        let mut args = json!({
            "title": "Review deadline", "description": "Preserve this task body", "priority": "p2"
        });
        if let Some(zone) = zone {
            args["due"] = json!("2026-10-01");
            args["timezone"] = json!(zone);
        }
        let created = self
            .registry
            .dispatch("gtd.assign", args)
            .await
            .expect("seed task");
        created["full_id"].as_str().unwrap().parse().unwrap()
    }

    async fn stored(&self, id: Uuid) -> Note {
        let token = self.runtime.authorize(Namespace::local()).unwrap();
        self.runtime
            .notes(&token)
            .unwrap()
            .get_note(id)
            .await
            .unwrap()
            .expect("stored task")
    }

    async fn update(&self, id: Uuid, properties: Value) -> Result<Value, RuntimeError> {
        self.registry
            .dispatch("update", json!({"id": id, "properties": properties}))
            .await
    }

    async fn refuses_without_changing_task(&self, id: Uuid, properties: Value) {
        let before = self.stored(id).await;
        let error = self
            .update(id, properties.clone())
            .await
            .expect_err("malformed zone must refuse");
        assert!(
            matches!(error, RuntimeError::InvalidInput(_)),
            "{properties}: {error:?}"
        );
        assert!(error.to_string().contains("IANA"), "{properties}: {error}");
        assert_eq!(
            self.stored(id).await,
            before,
            "refused patch changed task: {properties}"
        );
    }
}

#[tokio::test]
async fn malformed_deadline_zone_aliases_refuse_the_entire_task_update() {
    let fixture = Fixture::new();
    let id = fixture.seed(Some("America/New_York")).await;
    for invalid in [
        json!(7),
        json!(true),
        json!([]),
        json!({}),
        json!(""),
        json!(" "),
        json!("Mars/Olympus"),
    ] {
        fixture.refuses_without_changing_task(id, json!({
            "due": "2026-12-25", "due_timezone": "UTC", "timezone": invalid, "priority": "p1"
        })).await;
        fixture.refuses_without_changing_task(id, json!({
            "due": "2026-12-25", "due_timezone": invalid, "timezone": "UTC", "priority": "p1"
        })).await;
        fixture
            .refuses_without_changing_task(
                id,
                json!({
                    "due": "2026-12-25", "due_timezone": null, "timezone": invalid, "priority": "p1"
                }),
            )
            .await;
        fixture
            .refuses_without_changing_task(
                id,
                json!({
                    "due": "2026-12-25", "timezone": invalid, "priority": "p1"
                }),
            )
            .await;
    }
}

#[tokio::test]
async fn valid_aliases_keep_precedence_and_null_or_absent_aliases_keep_fallbacks() {
    let fixture = Fixture::new();
    for (stored_zone, mut properties, expected_zone, expected_due) in [
        (
            Some("America/New_York"),
            json!({"due_timezone": "UTC", "timezone": "Asia/Tokyo"}),
            "UTC",
            "2026-12-25T00:00:00+00:00",
        ),
        (
            Some("America/New_York"),
            json!({"due_timezone": "Asia/Tokyo", "timezone": "UTC"}),
            "Asia/Tokyo",
            "2026-12-25T00:00:00+09:00",
        ),
        (
            Some("America/New_York"),
            json!({"due_timezone": null, "timezone": "Asia/Tokyo"}),
            "Asia/Tokyo",
            "2026-12-25T00:00:00+09:00",
        ),
        (
            Some("America/New_York"),
            json!({"timezone": "Asia/Tokyo"}),
            "Asia/Tokyo",
            "2026-12-25T00:00:00+09:00",
        ),
        (
            Some("America/New_York"),
            json!({"due_timezone": "UTC", "timezone": null}),
            "UTC",
            "2026-12-25T00:00:00+00:00",
        ),
        (
            Some("America/New_York"),
            json!({}),
            "America/New_York",
            "2026-12-25T00:00:00-05:00",
        ),
        (
            Some("America/New_York"),
            json!({"due_timezone": null, "timezone": null}),
            "America/New_York",
            "2026-12-25T00:00:00-05:00",
        ),
        (None, json!({}), "UTC", "2026-12-25T00:00:00+00:00"),
        (
            None,
            json!({"due_timezone": null, "timezone": null}),
            "UTC",
            "2026-12-25T00:00:00+00:00",
        ),
    ] {
        let id = fixture.seed(stored_zone).await;
        let before = fixture.stored(id).await;
        properties["due"] = json!("2026-12-25");
        fixture
            .update(id, properties)
            .await
            .expect("valid zone aliases");
        let after = fixture.stored(id).await;
        assert!(after.version > before.version, "valid patch must persist");
        assert_eq!(after.name, before.name);
        assert_eq!(after.content, before.content);
        let properties = after.properties.unwrap();
        assert_eq!(properties["due"], expected_due);
        assert_eq!(properties["due_timezone"], expected_zone);
        assert!(
            properties.get("timezone").is_none(),
            "alias must be consumed"
        );
        assert_eq!(properties["priority"], "p2");
    }
}

#[tokio::test]
async fn absent_deadline_keeps_its_anchor_and_null_deadline_still_clears_it() {
    let fixture = Fixture::new();
    let id = fixture.seed(Some("America/New_York")).await;
    let before = fixture.stored(id).await;
    fixture
        .update(id, json!({"priority": "p1"}))
        .await
        .expect("priority-only update");
    let unchanged_due = fixture.stored(id).await;
    assert!(unchanged_due.version > before.version);
    assert_eq!(unchanged_due.content, before.content);
    let old_properties = before.properties.unwrap();
    let properties = unchanged_due.properties.unwrap();
    assert_eq!(properties["due"], old_properties["due"]);
    assert_eq!(properties["due_timezone"], old_properties["due_timezone"]);
    assert_eq!(properties["priority"], "p1");

    fixture
        .update(id, json!({"due": null}))
        .await
        .expect("clear deadline");
    let cleared = fixture.stored(id).await;
    assert!(cleared.version > unchanged_due.version);
    let properties = cleared.properties.unwrap();
    assert!(properties["due"].is_null());
    assert!(properties["due_timezone"].is_null());
    assert_eq!(properties["priority"], "p1");
}
