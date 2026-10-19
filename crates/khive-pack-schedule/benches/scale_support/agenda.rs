use chrono::{DateTime, Duration, SecondsFormat, Utc};
use khive_pack_comm::CommPack;
use khive_pack_kg::KgPack;
use khive_pack_schedule::SchedulePack;
use khive_runtime::{
    KhiveRuntime, Namespace, RuntimeConfig, VerbRegistry, VerbRegistryBuilder, WalCeilingSource,
};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use uuid::Uuid;

const EVENTS: usize = 1_000;
const ACTOR: &str = "benchmark:schedule";

struct Expected {
    id: String,
    at: String,
    content: String,
}

pub struct Fixture {
    pub registry: VerbRegistry,
    pub rt: tokio::runtime::Runtime,
    runtime: KhiveRuntime,
    expected: Vec<Expected>,
}

impl Fixture {
    pub fn new() -> Self {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(2)
            .build()
            .expect("bounded benchmark executor");
        let entered = rt.enter();
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
        config.packs = vec!["kg".into(), "comm".into(), "schedule".into()];
        assert!(config.embedding_model.is_none());
        assert!(config.additional_embedding_models.is_empty());
        let runtime = KhiveRuntime::new(config).expect("isolated memory runtime");
        assert!(runtime.backend_data_dir().is_none());
        assert!(runtime.backend_ann_root().is_none());
        assert!(!runtime.backend().pool().write_queue_active());
        assert!(runtime.registered_embedding_model_names().is_empty());
        let mut builder = VerbRegistryBuilder::new();
        builder.with_actor_id(Some(ACTOR.into()));
        builder.register(KgPack::new(runtime.clone()));
        builder.register(CommPack::new(runtime.clone()));
        builder.register(SchedulePack::new(runtime.clone()));
        let registry = builder
            .build()
            .expect("real KG, comm and schedule registry");
        drop(entered);
        let expected = rt.block_on(seed(&registry));
        let fixture = Self {
            registry,
            rt,
            runtime,
            expected,
        };
        fixture.rt.block_on(fixture.validate_corpus());
        fixture
    }

    pub fn validate_page(&self, response: &Value, start: usize, count: usize) {
        assert_eq!(response["count"], count, "{response}");
        let events = response["events"].as_array().expect("agenda events");
        assert_eq!(events.len(), count);
        for (event, expected) in events.iter().zip(&self.expected[start..start + count]) {
            assert_eq!(event["full_id"], expected.id);
            assert_eq!(event["id"], &expected.id[..8]);
            assert_eq!(event["kind"], "scheduled_event");
            assert_eq!(event["namespace"], "local");
            assert_eq!(event["content"], expected.content);
            let properties = &event["properties"];
            assert_eq!(properties["trigger_at"], expected.at);
            assert_eq!(properties["status"], "pending");
            assert_eq!(properties["event_type"], "remind");
            assert_eq!(properties["created_by_actor"], ACTOR);
            for name in ["repeat", "payload", "fired_at", "cancelled_at"] {
                assert!(properties
                    .get(name)
                    .expect("stored nullable field")
                    .is_null());
            }
            for name in ["created_at", "updated_at"] {
                event[name]
                    .as_str()
                    .expect("timestamp string")
                    .parse::<DateTime<Utc>>()
                    .expect("stored RFC3339 timestamp");
            }
        }
        if count == 0 {
            assert!(response["next"].is_null());
        } else {
            let last = &self.expected[start + count - 1];
            assert_eq!(
                response["next"],
                json!({"after":last.at,"after_id":last.id})
            );
        }
    }

    async fn validate_corpus(&self) {
        assert_eq!(self.expected.len(), EVENTS);
        let mut params = json!({"limit":200});
        let mut seen = BTreeSet::new();
        for start in (0..=EVENTS).step_by(200) {
            let response = self
                .registry
                .dispatch("schedule.agenda", params.clone())
                .await
                .expect("full agenda census");
            let count = (EVENTS - start).min(200);
            self.validate_page(&response, start, count);
            for event in response["events"].as_array().expect("events") {
                assert!(seen.insert(event["full_id"].as_str().expect("full ID").to_string()));
            }
            if count != 0 {
                params["after"] = response["next"]["after"].clone();
                params["after_id"] = response["next"]["after_id"].clone();
            }
        }
        assert_eq!(seen.len(), EVENTS);
        assert_eq!(
            seen,
            self.expected.iter().map(|item| item.id.clone()).collect()
        );
    }

    pub fn close(self) {
        let Self {
            registry,
            rt,
            runtime,
            expected,
        } = self;
        rt.block_on(registry.shutdown_audit_batch())
            .expect("drain and join any dispatch audit work");
        assert!(!runtime.backend().pool().write_queue_active());
        drop(expected);
        drop(registry);
        drop(runtime);
        drop(rt);
    }
}

async fn seed(registry: &VerbRegistry) -> Vec<Expected> {
    let base = "2100-01-01T00:00:00Z"
        .parse::<DateTime<Utc>>()
        .expect("future UTC base");
    let mut expected = Vec::with_capacity(EVENTS);
    let mut unique = BTreeSet::new();
    for index in 0..EVENTS {
        let at = (base + Duration::hours(index as i64)).to_rfc3339_opts(SecondsFormat::Secs, true);
        let content = format!("Generated schedule benchmark event {index:04}");
        let response = registry
            .dispatch("schedule.remind", json!({"content":content,"at":at}))
            .await
            .expect("seed real pending reminder");
        let id = response["full_id"]
            .as_str()
            .expect("full reminder receipt ID")
            .to_string();
        assert_eq!(
            Uuid::parse_str(&id).expect("canonical UUID").to_string(),
            id
        );
        assert!(unique.insert(id.clone()));
        assert_eq!(response["status"], "pending");
        assert_eq!(response["event_type"], "remind");
        assert_eq!(response["trigger_at"], at);
        assert!(response["repeat"].is_null());
        expected.push(Expected { id, at, content });
    }
    assert_eq!(unique.len(), EVENTS);
    expected
}
