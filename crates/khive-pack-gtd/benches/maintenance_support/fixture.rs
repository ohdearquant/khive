use khive_pack_gtd as _;
use khive_pack_kg as _;
use khive_runtime::{
    IngestAuditStore, KhiveRuntime, Namespace, PackRegistry, RuntimeConfig, VerbRegistry,
    WalCeilingSource,
};
use khive_storage::{Note, SqlStatement, SqlValue};
use serde_json::json;
use uuid::Uuid;

pub const ACTOR: &str = "benchmark:gtd";
pub const MICROS: i64 = 1_772_323_200_000_000;
pub const SECONDS: i64 = 1_772_323_200;
pub const MILLIS: i64 = 1_772_323_200_000;

pub struct Fixture {
    pub runtime: KhiveRuntime,
    pub registry: VerbRegistry,
}

impl Fixture {
    pub fn new() -> Self {
        let mut config = RuntimeConfig::no_embeddings();
        config.db_path = None;
        config.wal_ceiling_bytes = 0;
        config.wal_ceiling_configured_bytes = 0;
        config.wal_ceiling_source = WalCeilingSource::Default;
        config.wal_ceiling_env_raw = None;
        config.disk_guard_environment = Default::default();
        config.disk_guard_config = None;
        config.volume_lock_dir = None;
        config.credentials.clear();
        config.visibility_receipts = None;
        config.mounts.clear();
        config.events_split = None;
        config.actor_id = Some(ACTOR.into());
        config.default_namespace = Namespace::local();
        config.visible_namespaces.clear();
        config.allowed_outbound_namespaces.clear();
        config.brain_profile = None;
        config.brain = Default::default();
        config.packs = vec!["kg".into(), "gtd".into()];
        let runtime = KhiveRuntime::new(config).expect("private memory runtime");
        assert!(runtime.backend_data_dir().is_none());
        assert!(runtime.backend_ann_root().is_none());
        assert!(!runtime.backend().pool().write_queue_active());
        assert!(runtime.registered_embedding_model_names().is_empty());
        let registry = PackRegistry::build_ingest_registry(&runtime, IngestAuditStore::Attach)
            .expect("configured KG + GTD registry and audit sink");
        Self { runtime, registry }
    }

    pub async fn seed(&self, count: usize, historical: bool) -> Vec<Note> {
        let token = self
            .runtime
            .authorize(Namespace::local())
            .expect("local token");
        let store = self.runtime.notes(&token).expect("notes store");
        let mut originals = Vec::with_capacity(count);
        for index in 0..count {
            let mut note = Note::new("local", "task", format!("Generated task {index}"));
            note.id = Uuid::from_u128(0x5659_0000 + index as u128);
            note.created_at = if historical { SECONDS } else { MICROS };
            note.updated_at = if historical { MILLIS } else { MICROS };
            note.properties = Some(json!({
                "title":format!("Generated task {index}"),
                "status":if historical { "archived" } else { "next" },
                "assignee":ACTOR,
            }));
            store
                .upsert_note(note.clone())
                .await
                .expect("generated historical-capable seed");
            let stored = self.stored(note.id).await;
            assert_eq!(
                stored, note,
                "fixture timestamp/status preimage must be exact"
            );
            originals.push(note);
        }
        originals
    }

    pub async fn stored(&self, id: Uuid) -> Note {
        let token = self
            .runtime
            .authorize(Namespace::local())
            .expect("local token");
        self.runtime
            .notes(&token)
            .expect("notes")
            .get_note_including_deleted(id)
            .await
            .expect("stored note")
            .expect("note exists")
    }

    pub async fn audit_rows(&self) -> Vec<khive_storage::SqlRow> {
        self.runtime
            .sql()
            .reader()
            .await
            .expect("audit reader")
            .query_all(SqlStatement {
                sql:
                    "SELECT note_id, from_state, to_state, note, namespace FROM gtd_lifecycle_audit"
                        .into(),
                params: Vec::new(),
                label: Some("benchmark-gtd-audit-check".into()),
            })
            .await
            .expect("mandatory lifecycle audit rows")
    }

    pub async fn close(self) {
        self.registry
            .shutdown_audit_batch()
            .await
            .expect("drain and join audit supervisor");
        assert!(!self.runtime.backend().pool().write_queue_active());
        drop(self.registry);
        drop(self.runtime);
    }
}

pub fn text<'a>(row: &'a khive_storage::SqlRow, field: &str) -> &'a str {
    match row.get(field) {
        Some(SqlValue::Text(value)) => value,
        _ => panic!("missing text audit field {field}"),
    }
}
