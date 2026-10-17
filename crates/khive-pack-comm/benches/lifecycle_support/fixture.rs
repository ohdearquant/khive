use khive_pack_comm as _; // Inventory registration, including the trusted ingest grant.
use khive_pack_kg as _;
use khive_runtime::{
    IngestAuditStore, KhiveRuntime, Namespace, PackRegistry, RuntimeConfig, VerbRegistry,
    WalCeilingSource,
};
use khive_storage::Note;
use serde_json::{json, Value};
use uuid::Uuid;

pub const ACTOR: &str = "benchmark:comm";
pub const CHANNEL: &str = "benchmark";
pub const SLUG: &str = "generated-5658";

pub struct Fixture {
    pub registry: VerbRegistry,
    pub runtime: KhiveRuntime,
}

impl Fixture {
    pub fn new() -> Self {
        // No ambient DB, model, namespace routing, credentials or event sidecar.
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
        config.packs = vec!["kg".into(), "comm".into()];
        let runtime = KhiveRuntime::new(config).expect("private memory runtime");
        assert!(runtime.backend_data_dir().is_none());
        assert!(runtime.backend_ann_root().is_none());
        assert!(!runtime.backend().pool().write_queue_active());
        assert!(runtime.registered_embedding_model_names().is_empty());
        let registry = PackRegistry::build_ingest_registry(&runtime, IngestAuditStore::Attach)
            .expect("KG + comm registry with trusted ingest grant and audit sink");
        Self { registry, runtime }
    }

    pub async fn stored(&self, id: Uuid) -> Option<Note> {
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
    }

    pub async fn seed(&self, count: usize, quarantine: bool) -> (Vec<Uuid>, Option<i64>) {
        let mut ids = Vec::with_capacity(count);
        let mut last_expiry = None;
        for index in 0..count {
            let receipt = self
                .registry
                .dispatch("comm.ingest", ingest_params(index, quarantine))
                .await
                .expect("seed real inbound note");
            let id = check_ingest_receipt(&receipt);
            let note = self.stored(id).await.expect("seed persisted");
            check_inbound(&note, index, false);
            if quarantine {
                assert_eq!(note.properties.as_ref().unwrap()["quarantined"], true);
                let expiry = note
                    .expires_at
                    .expect("quarantine has actual retention deadline");
                last_expiry =
                    Some(last_expiry.map_or(expiry, |previous: i64| previous.max(expiry)));
            } else {
                assert!(note.expires_at.is_none());
            }
            ids.push(id);
        }
        assert_eq!(
            ids.iter().collect::<std::collections::BTreeSet<_>>().len(),
            count
        );
        (ids, last_expiry)
    }

    pub async fn close(self) {
        let Self { registry, runtime } = self;
        registry
            .shutdown_audit_batch()
            .await
            .expect("drain and join dispatch audit work");
        // Memory storage has no queued writer or file/ANN workers to join.
        assert!(!runtime.backend().pool().write_queue_active());
        drop(registry);
        drop(runtime);
    }
}

pub fn ingest_params(index: usize, quarantine: bool) -> Value {
    let mut params = json!({
        "from":"benchmark:sender", "to":ACTOR,
        "content":format!("Generated inbound message number {index}"),
        "subject":format!("Generated subject {index}"),
        "channel_kind":CHANNEL, "channel_slug":SLUG,
        "external_id":format!("generated:5658:{index}"),
        "sent_at":"2026-10-17T00:00:00Z",
    });
    if quarantine {
        params["metadata"] = json!({"quarantined":true});
    }
    params
}

pub fn check_ingest_receipt(receipt: &Value) -> Uuid {
    assert_eq!(receipt["deduplicated"], false, "{receipt}");
    assert!(
        receipt.get("post_commit_degradations").is_none(),
        "{receipt}"
    );
    let raw = receipt["full_id"].as_str().expect("full inbound UUID");
    let id = Uuid::parse_str(raw).expect("UUID receipt");
    assert_eq!(id.to_string(), raw);
    id
}

pub fn check_inbound(note: &Note, index: usize, read: bool) {
    assert_eq!(note.kind, "message");
    assert_eq!(note.namespace, "local");
    assert!(note.deleted_at.is_none());
    assert_eq!(
        note.content,
        format!("Generated inbound message number {index}")
    );
    let p = note.properties.as_ref().expect("message properties");
    assert_eq!(p["direction"], "inbound");
    assert_eq!(p["to_actor"], ACTOR);
    assert_eq!(p["read"], read);
    assert_eq!(p["channel_kind"], CHANNEL);
    assert_eq!(p["channel_slug"], SLUG);
}
