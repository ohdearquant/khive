use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use khive_pack_kg::handlers::ValidatedSearchRequest;
use khive_pack_kg::KgPack;
use khive_runtime::pack::AuditSuccessRowsMode;
use khive_runtime::{
    BackendId, KhiveRuntime, Namespace, NamespaceToken, RuntimeConfig, RuntimeError,
    StorageBackend, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::event::IdempotentEventBatchResult;
use khive_storage::{
    BatchWriteSummary, EdgeRelation, Event, EventFilter, EventStore, Page, PageRequest,
    StorageError, StorageResult,
};
use serde_json::{json, Value};
use tokio::sync::watch;
use uuid::Uuid;

use crate::coordinator::tests::MockCoordinator;
use crate::coordinator::{CoordError, CoordLinkResult, CoordSearchResult, CoordinatorService};
use crate::server::KhiveMcpServer;
use crate::tools::request::RequestParams;

#[derive(Clone, Copy, Debug)]
enum Outcome {
    Answer,
    CancelThenAnswer,
    Timeout {
        cancel: bool,
        proof: bool,
        operation: &'static str,
    },
    CancelThenOtherError,
}

struct ProbeCoordinator {
    inner: Arc<MockCoordinator>,
    outcome: Outcome,
    cancellation: watch::Sender<bool>,
    calls: AtomicUsize,
}

impl ProbeCoordinator {
    fn finish(&self) -> Result<(), RuntimeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.outcome {
            Outcome::Answer => Ok(()),
            Outcome::CancelThenAnswer => {
                self.cancellation.send(true).expect("request receiver");
                Ok(())
            }
            Outcome::Timeout {
                cancel,
                proof,
                operation,
            } => {
                if cancel {
                    self.cancellation.send(true).expect("request receiver");
                }
                let error = RuntimeError::Storage(StorageError::Timeout {
                    operation: operation.into(),
                });
                Err(if proof {
                    error.before_requested_write()
                } else {
                    error
                })
            }
            Outcome::CancelThenOtherError => {
                self.cancellation.send(true).expect("request receiver");
                Err(RuntimeError::Internal("coordinator fixture error".into()))
            }
        }
    }
}

#[async_trait]
impl CoordinatorService for ProbeCoordinator {
    async fn locate(&self, id: Uuid) -> Option<BackendId> {
        self.inner.locate(id).await
    }

    fn record_created(&self, id: Uuid, backend_id: BackendId) {
        self.inner.record_created(id, backend_id);
    }

    fn primary_backend_id(&self) -> Option<BackendId> {
        self.inner.primary_backend_id()
    }

    #[allow(clippy::too_many_arguments)]
    async fn link(
        &self,
        namespace: &Namespace,
        source_id: Uuid,
        target_id: Uuid,
        relation: EdgeRelation,
        weight: f64,
        metadata: Option<Value>,
        resurrect: bool,
    ) -> Result<CoordLinkResult, CoordError> {
        self.inner
            .link(
                namespace, source_id, target_id, relation, weight, metadata, resurrect,
            )
            .await
    }

    async fn fan_out_search(
        &self,
        request: &ValidatedSearchRequest,
        namespace: &Namespace,
        extra_visible: &[Namespace],
    ) -> CoordSearchResult {
        self.inner
            .fan_out_search(request, namespace, extra_visible)
            .await
    }

    async fn fan_out_search_scoped(
        &self,
        request: &ValidatedSearchRequest,
        token: &NamespaceToken,
        args: &Value,
        extra_visible: &[Namespace],
    ) -> Result<CoordSearchResult, RuntimeError> {
        let result = self
            .inner
            .fan_out_search_scoped(request, token, args, extra_visible)
            .await?;
        self.finish()?;
        Ok(result)
    }

    async fn fan_out_note_search_scoped_with_count(
        &self,
        request: &ValidatedSearchRequest,
        token: &NamespaceToken,
        args: &Value,
        extra_visible: &[Namespace],
    ) -> Result<(CoordSearchResult, u64), RuntimeError> {
        let result = self
            .inner
            .fan_out_note_search_scoped_with_count(request, token, args, extra_visible)
            .await?;
        self.finish()?;
        Ok(result)
    }

    fn is_single_backend(&self) -> bool {
        false
    }
}

struct AuditSink {
    inner: Arc<dyn EventStore>,
    fail: bool,
    cancellation: watch::Sender<bool>,
    failures: AtomicUsize,
}

#[async_trait]
impl EventStore for AuditSink {
    async fn append_event(&self, event: Event) -> StorageResult<()> {
        self.inner.append_event(event).await
    }
    async fn append_events(&self, events: Vec<Event>) -> StorageResult<BatchWriteSummary> {
        self.inner.append_events(events).await
    }
    async fn get_event(&self, id: Uuid) -> StorageResult<Option<Event>> {
        self.inner.get_event(id).await
    }
    async fn query_events(
        &self,
        filter: EventFilter,
        page: PageRequest,
    ) -> StorageResult<Page<Event>> {
        self.inner.query_events(filter, page).await
    }
    async fn count_events(&self, filter: EventFilter) -> StorageResult<u64> {
        self.inner.count_events(filter).await
    }
    fn preflight_event(&self, event: &Event) -> StorageResult<()> {
        self.inner.preflight_event(event)
    }
    fn supports_idempotent_audit_batch(&self) -> bool {
        true
    }
    async fn append_events_idempotent(
        &self,
        events: Vec<Event>,
    ) -> StorageResult<IdempotentEventBatchResult> {
        if self.fail {
            self.failures.fetch_add(1, Ordering::SeqCst);
            self.cancellation
                .send(true)
                .expect("request receiver during audit");
            return Err(StorageError::Internal(
                "coordinated search audit fixture failure".into(),
            ));
        }
        self.inner.append_events_idempotent(events).await
    }
}

struct Fixture {
    server: KhiveMcpServer,
    registry: VerbRegistry,
    probe: Arc<ProbeCoordinator>,
    sink: Arc<AuditSink>,
    receiver: watch::Receiver<bool>,
}

fn fixture(outcome: Outcome, fail_audit: bool) -> Fixture {
    let backend = Arc::new(StorageBackend::memory().expect("memory backend"));
    backend.prepare_core_schema().expect("core schema");
    let runtime = KhiveRuntime::from_backend(
        backend,
        RuntimeConfig {
            db_path: None,
            events_split: None,
            actor_id: Some("test:coordinated-search-stop".into()),
            packs: vec!["kg".into()],
            ..RuntimeConfig::no_embeddings()
        },
    );
    let (cancellation, receiver) = watch::channel(false);
    let token = runtime.authorize(Namespace::local()).expect("local token");
    let sink = Arc::new(AuditSink {
        inner: runtime.events(&token).expect("audit store"),
        fail: fail_audit,
        cancellation: cancellation.clone(),
        failures: AtomicUsize::new(0),
    });
    let mut builder = VerbRegistryBuilder::new();
    builder.with_gate(runtime.config().gate.clone());
    builder.with_default_namespace("local");
    builder.with_actor_id(runtime.config().actor_id.clone());
    builder.with_audit_success_rows_mode(AuditSuccessRowsMode::All);
    builder.with_event_store(sink.clone());
    builder.register_trusted(KgPack::new(runtime.clone()));
    let registry = builder.build().expect("KG registry");
    registry
        .apply_schema_plans_with_map(&HashMap::new(), runtime.backend())
        .expect("KG schema");
    runtime.install_edge_rules(registry.all_edge_rules());
    let probe = Arc::new(ProbeCoordinator {
        inner: MockCoordinator::multi_backend(),
        outcome,
        cancellation,
        calls: AtomicUsize::new(0),
    });
    let server = KhiveMcpServer::from_registry(registry.clone()).with_coordinator(probe.clone());
    Fixture {
        server,
        registry,
        probe,
        sink,
        receiver,
    }
}

impl Fixture {
    async fn entry(&self, kind: &str) -> Value {
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            khive_storage::scope_request_read_cancellation(
                self.receiver.clone(),
                self.server.dispatch_request_local(RequestParams {
                    ops:
                        json!([{"tool":"search","args":{"kind":kind,"query":"fixture","limit":2}}])
                            .to_string(),
                    presentation: Some("verbose".into()),
                    format: Some("json".into()),
                    ..Default::default()
                }),
            ),
        )
        .await
        .expect("request settles")
        .expect("request envelope");
        assert_eq!(
            self.probe.calls.load(Ordering::SeqCst),
            1,
            "actual coordinator was entered"
        );
        let envelope: Value = serde_json::from_str(&response).expect("JSON envelope");
        assert_eq!(envelope["results"].as_array().expect("results").len(), 1);
        envelope["results"][0].clone()
    }

    async fn close(self) {
        let outcome =
            tokio::time::timeout(Duration::from_secs(5), self.registry.shutdown_audit_batch())
                .await
                .expect("audit batch settles");
        if !self.sink.fail {
            outcome.expect("audit drain");
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn coordinated_request_stop_is_not_committed_and_retryable() {
    for kind in ["entity", "note"] {
        let control = fixture(Outcome::Answer, false);
        let entry = control.entry(kind).await;
        assert_eq!(entry["ok"], true, "{entry}");
        assert_eq!(
            entry["result"].as_array().expect("nonempty control").len(),
            1
        );
        control.close().await;

        for fail_audit in [false, true] {
            let cancelled = fixture(Outcome::CancelThenAnswer, fail_audit);
            let entry = cancelled.entry(kind).await;
            assert_eq!(entry["ok"], false, "{entry}");
            assert!(
                entry.get("result").is_none(),
                "no cancelled result page: {entry}"
            );
            assert_eq!(entry["domain_disposition"], "not_committed");
            assert_eq!(entry["error"]["domain_disposition"], "not_committed");
            assert_eq!(entry["error"]["kind"], "runtime_error");
            assert_eq!(entry["error"]["retryable"], true);
            assert_eq!(
                entry["error"]["message"],
                RuntimeError::Storage(StorageError::Timeout {
                    operation: "search".into(),
                })
                .to_string()
            );
            if fail_audit {
                assert!(
                    cancelled.sink.failures.load(Ordering::SeqCst) > 0,
                    "the original refusal survives an actual audit failure"
                );
            }
            cancelled.close().await;
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ordinary_coordinator_failures_do_not_acquire_retryability() {
    for outcome in [
        Outcome::Timeout {
            cancel: false,
            proof: false,
            operation: "search",
        },
        Outcome::Timeout {
            cancel: true,
            proof: false,
            operation: "search",
        },
        Outcome::Timeout {
            cancel: false,
            proof: true,
            operation: "search",
        },
        Outcome::Timeout {
            cancel: true,
            proof: true,
            operation: "other_read",
        },
        Outcome::CancelThenOtherError,
    ] {
        for kind in ["entity", "note"] {
            let f = fixture(outcome, false);
            let entry = f.entry(kind).await;
            assert_eq!(entry["ok"], false, "{outcome:?}: {entry}");
            let disposition = if matches!(outcome, Outcome::Timeout { proof: true, .. }) {
                "not_committed"
            } else {
                "unknown"
            };
            assert_eq!(entry["domain_disposition"], disposition, "{outcome:?}");
            assert_eq!(
                entry["error"]["domain_disposition"], disposition,
                "{outcome:?}"
            );
            assert_eq!(entry["error"]["kind"], "runtime_error");
            let expected_message = match outcome {
                Outcome::Timeout { operation, .. } => {
                    RuntimeError::Storage(StorageError::Timeout {
                        operation: operation.into(),
                    })
                    .to_string()
                }
                Outcome::CancelThenOtherError => {
                    RuntimeError::Internal("coordinator fixture error".into()).to_string()
                }
                _ => unreachable!("only refusal controls"),
            };
            assert_eq!(entry["error"]["message"], expected_message);
            assert!(
                entry["error"].get("retryable").is_none(),
                "{outcome:?}: {entry}"
            );
            f.close().await;
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn audit_failure_after_search_success_keeps_committed_disposition() {
    for kind in ["entity", "note"] {
        let f = fixture(Outcome::Answer, true);
        let entry = f.entry(kind).await;
        assert!(
            f.sink.failures.load(Ordering::SeqCst) > 0,
            "actual audit write failed"
        );
        assert!(*f.receiver.borrow(), "cancellation arrived during audit");
        assert_eq!(entry["ok"], false, "{entry}");
        assert_eq!(entry["domain_disposition"], "committed");
        assert_eq!(entry["error"]["domain_disposition"], "committed");
        assert_eq!(entry["error"]["kind"], "obligation");
        assert_eq!(
            entry["error"]["domain_result"]
                .as_array()
                .expect("canonical result retained")
                .len(),
            1
        );
        assert_ne!(entry["error"]["retryable"], true);
        f.close().await;
    }
}
