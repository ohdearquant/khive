use super::*;
use async_trait::async_trait;
use khive_runtime::pack::{AuditSuccessRowsMode, PackRuntime};
use khive_runtime::{
    classify_operation, EmbedderProvider, EngineConfig, Gate, GateDecision, GateError, GateRequest,
    OperationAccess, RuntimeError, RuntimeResult,
};
use khive_storage::{Event, EventFilter, PageRequest};
use khive_types::{EventKind, EventOutcome};
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::Notify;

const WATCHDOG: Duration = Duration::from_secs(15);

fn isolated() -> bool {
    let directory = tempfile::tempdir().unwrap();
    khive_storage::test_support::run_exact_test_in_child(
        "KHIVE_COMM_QUIET_CHILD",
        false,
        |command| {
            for (key, _) in std::env::vars_os() {
                if key.to_string_lossy().starts_with("KHIVE_") {
                    command.env_remove(key);
                }
            }
            command
                .current_dir(directory.path())
                .env("HOME", directory.path())
                .env("LATTICE_MODEL_CACHE", directory.path().join("models"))
                .env("KHIVE_BLOB_ROOT", directory.path().join("blobs"))
                .env("KHIVE_VOLUME_LOCK_DIR", directory.path().join("locks"));
        },
    )
}

fn run(future: impl std::future::Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            // No paused clock: background workers and this watchdog use real time.
            tokio::time::timeout(WATCHDOG, future)
                .await
                .expect("quiet fixture must settle within its wall-clock watchdog");
        });
}

fn runtime() -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap()
}

fn registry(runtime: &KhiveRuntime, namespace: &str, read_only: bool, audit: bool) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register_trusted(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register_trusted(CommPack::new(runtime.clone()));
    builder.with_default_namespace(namespace);
    builder.with_actor_id(Some("quiet-fixture".into()));
    if read_only {
        builder.with_gate(Arc::new(ReadOnly));
    }
    if audit {
        let token = runtime
            .authorize(Namespace::parse(namespace).unwrap())
            .unwrap();
        builder.with_event_store(runtime.events(&token).unwrap());
        builder.with_audit_success_rows_mode(AuditSuccessRowsMode::Reduced);
    }
    let registry = builder.build().unwrap();
    registry
        .apply_schema_plans_with_map(&HashMap::new(), runtime.backend())
        .unwrap();
    runtime.install_edge_rules(registry.all_edge_rules());
    registry
}

#[derive(Debug)]
struct ReadOnly;

impl Gate for ReadOnly {
    fn check(&self, request: &GateRequest) -> Result<GateDecision, GateError> {
        Ok(
            if classify_operation(&request.verb) == Some(OperationAccess::Read) {
                GateDecision::allow()
            } else {
                GateDecision::deny("fixture permits only reviewed reads")
            },
        )
    }
}

async fn deadline(registry: &VerbRegistry) -> Value {
    let health = registry.dispatch("comm.health", json!({})).await.unwrap();
    health["resource"]
        .get("quiet_until")
        .expect("quiet_until is always present")
        .clone()
}

#[test]
fn quiet_dispatch_reports_shared_state_and_preserves_it_on_refusal() {
    if isolated() {
        return;
    }
    run(async {
        let runtime = runtime();
        let registry = registry(&runtime, "local", false, false);
        let other_runtime = self::runtime();
        let other = self::registry(&other_runtime, "another-namespace", false, false);
        let denied = self::registry(&runtime, "local", true, false);
        let catalog = registry
            .dispatch("verbs", json!({"pack": "comm"}))
            .await
            .unwrap();
        assert_eq!(catalog["total"], 16);
        let quiet = catalog["verbs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|verb| verb["verb"] == "comm.quiet")
            .unwrap();
        assert_eq!(quiet["category"], "Directive");
        assert_eq!(quiet["signature"], "comm.quiet(ttl_secs)");
        assert!(deadline(&registry).await.is_null());

        let accepted = registry
            .dispatch("comm.quiet", json!({"ttl_secs": 300}))
            .await
            .unwrap();
        let expected = accepted["quiet_until"].clone();
        assert!(chrono::DateTime::parse_from_rfc3339(expected.as_str().unwrap()).is_ok());
        assert_eq!(deadline(&registry).await, expected);
        assert_eq!(deadline(&other).await, expected);

        for params in [
            json!({}),
            json!({"ttl_secs": null}),
            json!({"ttl_secs": "1"}),
            json!({"ttl_secs": true}),
            json!({"ttl_secs": -1}),
            json!({"ttl_secs": 0}),
            json!({"ttl_secs": 301}),
            json!({"ttl_secs": 1.5}),
            json!({"ttl_secs": u64::MAX}),
            json!({"ttl_secs": 1e100}),
            json!({"ttl_secs": 300, "extra": true}),
            json!([1]),
            json!(null),
        ] {
            let error = registry
                .dispatch("comm.quiet", params.clone())
                .await
                .unwrap_err();
            assert!(
                matches!(error, RuntimeError::InvalidInput(_)),
                "{params}: {error:?}"
            );
            assert_eq!(
                deadline(&registry).await,
                expected,
                "refusal changed quiet: {params}"
            );
        }
        let error = denied
            .dispatch("comm.quiet", json!({"ttl_secs": 300}))
            .await
            .unwrap_err();
        assert!(matches!(error, RuntimeError::PermissionDenied { .. }));
        assert_eq!(deadline(&denied).await, expected);

        // Canonical integer parameters normalize whole-valued JSON numbers first.
        let shorter = registry
            .dispatch("comm.quiet", json!({"ttl_secs": 1.0}))
            .await
            .unwrap();
        assert_eq!(shorter["quiet_until"], expected);
    });
}

#[test]
fn quiet_health_becomes_null_after_monotonic_expiry() {
    if isolated() {
        return;
    }
    run(async {
        let runtime = runtime();
        let registry = registry(&runtime, "local", false, false);
        let requested = Instant::now();
        registry
            .dispatch("comm.quiet", json!({"ttl_secs": 1}))
            .await
            .unwrap();
        while !deadline(&registry).await.is_null() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(requested.elapsed() >= Duration::from_secs(1));
    });
}

async fn events(runtime: &KhiveRuntime, verb: &str) -> Vec<Event> {
    let token = runtime.authorize(Namespace::local()).unwrap();
    runtime
        .events(&token)
        .unwrap()
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 100,
                offset: 0,
            },
        )
        .await
        .unwrap()
        .items
        .into_iter()
        .filter(|event| event.verb == verb)
        .collect()
}

#[test]
fn quiet_success_audit_is_retained_in_reduced_mode() {
    if isolated() {
        return;
    }
    run(async {
        let runtime = runtime();
        let registry = registry(&runtime, "local", false, true);
        registry
            .dispatch("comm.quiet", json!({"ttl_secs": 1}))
            .await
            .unwrap();
        let rows = events(&runtime, "comm.quiet").await;
        assert_eq!(
            rows.len(),
            1,
            "ordinary success audit must not be omitted or sampled"
        );
        assert_eq!(rows[0].kind, EventKind::Audit);
        assert_eq!(rows[0].outcome, EventOutcome::Success);
        assert!(
            rows[0].payload.get("retention").is_none(),
            "quiet audit must not be expendable"
        );
    });
}

#[derive(Default)]
struct Probe {
    entries: Mutex<Vec<Instant>>,
    entered: Notify,
    release: Notify,
}

struct Provider(Arc<Probe>);
struct Service(Arc<Probe>);

#[async_trait]
impl EmbedderProvider for Provider {
    fn name(&self) -> &str {
        "quiet-warm-fixture"
    }
    fn dimensions(&self) -> usize {
        2
    }
    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(Arc::new(Service(self.0.clone())))
    }
}

#[async_trait]
impl EmbeddingService for Service {
    async fn embed(
        &self,
        texts: &[String],
        _: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        assert_eq!(texts, &["khive warmup"]);
        self.0.entries.lock().unwrap().push(Instant::now());
        self.0.entered.notify_one();
        self.0.release.notified().await;
        Ok(texts.iter().map(|_| vec![1.0, 0.0]).collect())
    }
    fn supports_model(&self, _: EmbeddingModel) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "quiet-warm-service"
    }
}

fn warm_runtime() -> (KhiveRuntime, Arc<Probe>) {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        engines: Some(vec![EngineConfig {
            name: "quiet-warm-fixture".into(),
            weight: 1.0,
            dims: Some(2),
        }]),
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let probe = Arc::new(Probe::default());
    runtime
        .try_register_embedder(Provider(probe.clone()))
        .unwrap();
    runtime.finalize_embedding_engines().unwrap();
    (runtime, probe)
}

struct CloseOnDrop(khive_runtime::resource_admission::BackgroundDomainControl);
impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        let _ = self.0.close_with_policy();
    }
}

async fn poll_pending<F: std::future::Future>(mut future: std::pin::Pin<&mut F>) {
    std::future::poll_fn(|cx| {
        assert!(
            future.as_mut().poll(cx).is_pending(),
            "warm retains its inline completion barrier"
        );
        std::task::Poll::Ready(())
    })
    .await;
}

#[test]
fn quiet_defers_real_kg_warm_until_expiry() {
    if isolated() {
        return;
    }
    run(async {
        let (runtime, probe) = warm_runtime();
        let _close = CloseOnDrop(runtime.background_domain().control());
        let registry = registry(&runtime, "local", false, false);
        let pack = khive_pack_kg::KgPack::new(runtime.clone());
        let requested = Instant::now();
        registry
            .dispatch("comm.quiet", json!({"ttl_secs": 1}))
            .await
            .unwrap();
        let earliest_entry = requested + Duration::from_secs(1);
        let mut warm = Box::pin(pack.warm());
        poll_pending(warm.as_mut()).await;
        tokio::select! {
            _ = warm.as_mut() => panic!("warm returned before its real provider completed"),
            _ = probe.entered.notified() => {}
        }
        let entries = probe.entries.lock().unwrap().clone();
        assert_eq!(entries.len(), 1);
        assert!(
            entries[0] >= earliest_entry,
            "provider entered before quiet expiry"
        );
        poll_pending(warm.as_mut()).await;
        let started = events(&runtime, "kg.embedder_warm").await;
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].kind, EventKind::PhaseStarted);
        probe.release.notify_one();
        warm.await;
        let phases = events(&runtime, "kg.embedder_warm").await;
        assert_eq!(phases.len(), 2);
        assert_eq!(
            phases
                .iter()
                .filter(|event| event.kind == EventKind::PhaseStarted)
                .count(),
            1
        );
        assert_eq!(
            phases
                .iter()
                .filter(|event| event.kind == EventKind::PhaseCompleted)
                .count(),
            1
        );
        assert!(phases
            .iter()
            .all(|event| event.payload["work_class"] == "warm"
                && event.payload["phase"] == "kg_embedder_warm"));
        let report = runtime
            .background_domain()
            .control()
            .shutdown(Instant::now() + Duration::from_secs(5))
            .await
            .unwrap();
        assert!(report.workers_exited && report.coordinator_joined);
        assert_eq!(report.outstanding_jobs, 0);
    });
}

#[test]
fn quiet_waiting_kg_warm_preserves_foreground_and_domain_shutdown() {
    if isolated() {
        return;
    }
    run(async {
        let (runtime, probe) = warm_runtime();
        let control = runtime.background_domain().control();
        let _close = CloseOnDrop(control.clone());
        let registry = registry(&runtime, "local", false, false);
        registry
            .dispatch("comm.quiet", json!({"ttl_secs": 30}))
            .await
            .unwrap();
        let pack = khive_pack_kg::KgPack::new(runtime.clone());
        let mut warm = Box::pin(pack.warm());
        poll_pending(warm.as_mut()).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            assert!(!deadline(&registry).await.is_null());
            let identity = registry.dispatch("whoami", json!({})).await.unwrap();
            assert_eq!(identity["actor_id"], "quiet-fixture");
            assert_eq!(identity["namespace"], "local");
        })
        .await
        .expect("foreground reads must complete during the thirty-second quiet interval");
        assert!(events(&runtime, "kg.embedder_warm").await.is_empty());
        let shutdown = control.shutdown(Instant::now() + Duration::from_secs(5));
        let ((), report) = tokio::join!(warm, shutdown);
        let report = report.unwrap();
        assert!(report.workers_exited && report.coordinator_joined);
        assert_eq!(report.outstanding_jobs, 0);
        assert!(probe.entries.lock().unwrap().is_empty());
        assert!(events(&runtime, "kg.embedder_warm").await.is_empty());
        assert!(
            !deadline(&registry).await.is_null(),
            "shutdown must finish before the quiet deadline"
        );
    });
}
