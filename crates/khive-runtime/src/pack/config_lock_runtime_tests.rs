//! Runtime-owned ConfigLocked acceptance through actual bound registry sinks.

use super::*;
use crate::audit_batch::{AuditBatchConfig, AuditBatchControl};
use crate::config::{BackendId, RuntimeConfig};
use crate::KhiveRuntime;
use std::time::Duration;

include!("config_lock_runtime_store.rs");

pub(super) struct Fixture {
    pub(super) runtime: KhiveRuntime,
    pub(super) registry: Arc<VerbRegistry>,
    pub(super) store: Arc<ControlledStore>,
    namespace: String,
    actor: String,
}

impl Fixture {
    pub(super) fn new() -> Self {
        Self::from_runtime(
            KhiveRuntime::memory().expect("runtime"),
            "local",
            "config-fixture",
        )
    }

    pub(super) fn from_runtime(runtime: KhiveRuntime, namespace: &str, actor: &str) -> Self {
        Self::configured(runtime, namespace, actor, audit_config(), |_| {})
    }

    pub(super) fn configured(
        runtime: KhiveRuntime,
        namespace: &str,
        actor: &str,
        config: AuditBatchConfig,
        customize: impl FnOnce(&mut VerbRegistryBuilder),
    ) -> Self {
        let captured = Arc::new(Mutex::new(None));
        let slot = captured.clone();
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder
            .with_default_namespace(namespace)
            .with_actor_id(Some(actor.into()));
        customize(&mut builder);
        builder
            .with_runtime_event_store(&runtime)
            .expect("runtime binding");
        builder.with_runtime_event_store_test_wrapper(move |raw| {
            let store = ControlledStore::new(raw);
            *slot.lock().unwrap() = Some(store.clone());
            store
        });
        builder.with_audit_batch_config(config);
        let registry = Arc::new(builder.build().expect("bound registry"));
        let store = captured
            .lock()
            .unwrap()
            .take()
            .expect("actual bound raw sink wrapped");
        assert!(registry.audit_batch_handle().is_some());
        Self {
            runtime,
            registry,
            store,
            namespace: namespace.into(),
            actor: actor.into(),
        }
    }

    pub(super) async fn dispatch(&self) {
        dispatch_ok(&self.registry, &self.namespace, &self.actor).await;
    }

    async fn settle(&self) {
        tokio::time::timeout(
            WATCHDOG,
            self.registry.audit_batch_handle().unwrap().quiesce(),
        )
        .await
        .expect("actual batch becomes idle")
        .expect("batch quiesce");
    }

    pub(super) async fn finish(&self) {
        self.settle().await;
        tokio::time::timeout(
            WATCHDOG,
            self.registry
                .audit_batch_handle()
                .unwrap()
                .close_and_wait_for_physical_completion(),
        )
        .await
        .expect("physical audit work finishes")
        .expect("physical audit drain");
    }

    pub(super) async fn rows(&self) -> Vec<Event> {
        self.settle().await;
        let filter = EventFilter {
            kinds: vec![EventKind::ConfigLocked],
            ..EventFilter::default()
        };
        let count = self
            .store
            .count_events(filter.clone())
            .await
            .expect("real ConfigLocked count");
        let page = self
            .store
            .query_events(
                filter,
                PageRequest {
                    limit: 100,
                    offset: 0,
                },
            )
            .await
            .expect("real stored ConfigLocked rows");
        assert_eq!(
            page.items.len() as u64,
            count,
            "oracle must inspect the complete namespace page"
        );
        page.items
    }

    pub(super) async fn assert_rows(&self, expected: &[(&str, &str)]) {
        let rows = self.rows().await;
        assert_config_rows(&rows, expected);
    }

    pub(super) async fn assert_refused_once_then_recovered(&self, key: &'static str, denied: bool) {
        self.settle().await;
        let submitted = self.store.submitted(key);
        assert_eq!(
            submitted.len(),
            1,
            "one logical ledger submission reached the actual sink"
        );
        let attempts = self.store.attempts(key);
        assert_eq!(
            attempts.len(),
            1,
            "one physical attempt with retries disabled"
        );
        assert_eq!(attempts[0].id, submitted[0].id);
        assert!(
            self.store.refused.lock().unwrap().contains(&attempts[0].id),
            "the controlled actual sink refused this attempted row"
        );
        assert!(
            self.rows().await.is_empty(),
            "refused row was not persisted"
        );
        self.store.recover();
        self.runtime
            .record_config_locked(key, || panic!("a taken name must remain seen"));
        let result = self.registry.dispatch("list", Value::Null).await;
        if denied {
            assert!(matches!(result, Err(RuntimeError::PermissionDenied { .. })));
        } else {
            assert_eq!(
                result.expect("healthy dispatch after recovery"),
                serde_json::json!({"pack":"alpha","verb":"list"})
            );
        }
        self.finish().await;
        assert!(
            self.rows().await.is_empty(),
            "recovery does not re-enqueue a refused ledger entry"
        );
        assert_eq!(
            self.store.submitted(key).len(),
            1,
            "no second logical submission"
        );
        assert_eq!(
            self.store.attempts(key).len(),
            1,
            "no post-recovery physical retry"
        );
    }
}

pub(super) fn audit_config() -> AuditBatchConfig {
    AuditBatchConfig {
        max_commit_attempts: std::num::NonZeroU8::new(1).unwrap(),
        admission_deadline: WATCHDOG,
        resolution_deadline: WATCHDOG * 2,
        ..AuditBatchConfig::default()
    }
}

async fn dispatch_ok(registry: &VerbRegistry, namespace: &str, actor: &str) {
    let token = NamespaceToken::mint_authorized(
        khive_types::Namespace::parse(namespace).expect("fixture namespace"),
        ActorRef::new("agent", actor),
    );
    let value = registry
        .dispatch_with_identity(
            "list",
            serde_json::json!({"namespace": namespace}),
            Some(RequestIdentity::from_token(&token)),
        )
        .await
        .expect("real AlphaPack dispatch");
    assert_eq!(value, serde_json::json!({"pack":"alpha","verb":"list"}));
}

fn assert_config_rows(rows: &[Event], expected: &[(&str, &str)]) {
    assert_eq!(rows.len(), expected.len(), "exact ConfigLocked cardinality");
    let actual: BTreeMap<_, _> = rows
        .iter()
        .map(|event| {
            assert_eq!(event.verb, "config.lock");
            assert_eq!(
                event.payload.as_object().unwrap().len(),
                2,
                "payload stays exactly key/value"
            );
            (
                event.payload["key"].as_str().unwrap(),
                event.payload["value"].as_str().unwrap(),
            )
        })
        .collect();
    let expected: BTreeMap<_, _> = expected.iter().copied().collect();
    assert_eq!(actual, expected);
    assert_eq!(
        rows.iter()
            .map(|event| event.id)
            .collect::<BTreeSet<_>>()
            .len(),
        rows.len()
    );
}

fn prepared_backend() -> Arc<khive_db::StorageBackend> {
    let backend = khive_db::StorageBackend::memory().expect("shared memory backend");
    backend.prepare_core_schema().expect("core schema");
    Arc::new(backend)
}

fn runtime_on(backend: Arc<khive_db::StorageBackend>, id: &str) -> KhiveRuntime {
    let mut config = RuntimeConfig::no_embeddings();
    config.backend_id = BackendId::parse(id).expect("backend id");
    KhiveRuntime::from_backend(backend, config)
}

include!("config_lock_runtime_ownership_tests.rs");
include!("config_lock_runtime_destination_tests.rs");
include!("config_lock_runtime_model_tests.rs");
include!("config_lock_runtime_race_tests.rs");
