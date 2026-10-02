//! Real event-store append outcomes through per-operation request envelopes.
//! The timing fixture explicitly freezes the context inside the handler;
//! it does not exercise the generated enclosing audit append.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use khive_db::stores::event::SqlEventStore;
use khive_db::{StorageBackend, WriterTaskHandle};
use khive_mcp::server::KhiveMcpServer;
use khive_mcp::tools::request::RequestParams;
use khive_runtime::{NamespaceToken, PackRuntime, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use khive_storage::usage;
use khive_storage::{Event, EventStore, StorageError, WriterTaskRequestState};
use khive_types::{EventKind, HandlerDef, Pack, SubstrateKind, VerbCategory, Visibility};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
use serde_json::{json, Value};
use uuid::Uuid;

const PROBE: &str = "usage_append_probe";
const CONTROL: &str = "usage_append_control";

#[derive(Clone, Copy, Debug)]
enum AppendMethod {
    Single,
    Batch,
}

impl AppendMethod {
    fn rows(self) -> u64 {
        match self {
            Self::Single => 1,
            Self::Batch => 2,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Outcome {
    Success,
    RolledBack,
    Unknown,
    UnknownAfterFreeze,
}

#[derive(Debug, Default)]
struct Evidence {
    prime: Option<Uuid>,
    attempted: Vec<Uuid>,
    frozen_before_attempt: Option<Value>,
    observed_state: Option<WriterTaskRequestState>,
}

struct Lane {
    store: Arc<SqlEventStore>,
    writer: WriterTaskHandle,
    method: AppendMethod,
    outcome: Outcome,
    evidence: Mutex<Evidence>,
}

impl Lane {
    fn new(path: &std::path::Path, method: AppendMethod, outcome: Outcome) -> Self {
        let backend = StorageBackend::sqlite_for_test_with_journal_mode(
            path,
            true,
            Duration::from_millis(175),
        )
        .expect("private file-backed backend");
        backend.prepare_core_schema().expect("core schema");
        let writer = backend
            .pool()
            .writer_task_handle()
            .expect("writer task available")
            .expect("fixture explicitly enables the writer task");
        Self {
            store: Arc::new(SqlEventStore::new_scoped(backend.pool_arc(), true, "local")),
            writer,
            method,
            outcome,
            evidence: Mutex::new(Evidence::default()),
        }
    }

    async fn dispatch(&self) -> Result<Value, RuntimeError> {
        let prime = event();
        let prime_id = prime.id;
        self.store.append_event(prime).await?;
        self.evidence.lock().unwrap().prime = Some(prime_id);

        let context = usage::current().expect("actual request dispatch arms usage");
        assert_eq!(context.snapshot(), json!({"event_rows": 1}));
        if matches!(self.outcome, Outcome::UnknownAfterFreeze) {
            let frozen = context.freeze();
            assert_eq!(frozen, json!({"event_rows": 1}));
            self.evidence.lock().unwrap().frozen_before_attempt = Some(frozen);
        }

        if !matches!(self.outcome, Outcome::Success) {
            let deny_rollback =
                matches!(self.outcome, Outcome::Unknown | Outcome::UnknownAfterFreeze);
            self.writer
                .send_top_level(move |connection| {
                    connection
                        .authorizer(Some(move |context: AuthContext<'_>| {
                            match context.action {
                                // rusqlite 0.40 reports SQLite COMMIT as Unknown.
                                AuthAction::Transaction {
                                    operation: TransactionOperation::Unknown,
                                } => Authorization::Deny,
                                AuthAction::Transaction {
                                    operation: TransactionOperation::Rollback,
                                } if deny_rollback => Authorization::Deny,
                                _ => Authorization::Allow,
                            }
                        }))
                        .map_err(|error| StorageError::Pool {
                            operation: "usage_fixture_authorizer".into(),
                            message: error.to_string(),
                        })
                })
                .await?;
        }

        let events: Vec<Event> = (0..self.method.rows()).map(|_| event()).collect();
        self.evidence.lock().unwrap().attempted = events.iter().map(|event| event.id).collect();
        let result = match self.method {
            AppendMethod::Single => {
                self.store
                    .append_event(events.into_iter().next().expect("one event"))
                    .await
            }
            AppendMethod::Batch => self.store.append_events(events).await.map(|summary| {
                assert_eq!(summary.attempted, 2);
                assert_eq!(summary.affected, 2);
                assert_eq!(summary.failed, 0);
            }),
        };

        match result {
            Ok(()) => {
                assert!(matches!(self.outcome, Outcome::Success));
                Ok(json!({"id": prime_id}))
            }
            Err(error) => {
                let state = match (&self.outcome, &error) {
                    (
                        Outcome::RolledBack,
                        StorageError::WriterTaskRequestFailed {
                            request_state: WriterTaskRequestState::TransactionRolledBack,
                            ..
                        },
                    ) => WriterTaskRequestState::TransactionRolledBack,
                    (
                        Outcome::Unknown | Outcome::UnknownAfterFreeze,
                        StorageError::WriterTaskTerminated {
                            request_state: WriterTaskRequestState::SideEffectsUnknown,
                        },
                    ) => WriterTaskRequestState::SideEffectsUnknown,
                    other => panic!("unexpected real append outcome: {other:?}"),
                };
                self.evidence.lock().unwrap().observed_state = Some(state);
                if matches!(self.outcome, Outcome::RolledBack) {
                    self.writer
                        .send_top_level(|connection| {
                            connection
                                .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                                .map_err(|error| StorageError::Pool {
                                    operation: "usage_fixture_remove_authorizer".into(),
                                    message: error.to_string(),
                                })
                        })
                        .await?;
                }
                if matches!(self.outcome, Outcome::UnknownAfterFreeze) {
                    usage::count(usage::UsageUnit::DbRoundTrips, 1);
                    assert_eq!(context.snapshot()["db_round_trips"], 1);
                    assert_eq!(context.freeze(), json!({"event_rows": 1}));
                }
                Err(RuntimeError::Storage(error))
            }
        }
    }
}

fn event() -> Event {
    Event::new(
        "local",
        "usage_fixture_event",
        EventKind::Audit,
        SubstrateKind::Event,
        "fixture",
    )
}

struct AppendProbePack {
    probe: Arc<Lane>,
    control: Arc<Lane>,
}

impl Pack for AppendProbePack {
    const NAME: &'static str = "usage-append-fixture";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[
        HandlerDef {
            name: PROBE,
            description: "perform real event appends with a selected transaction outcome",
            visibility: Visibility::Verb,
            category: VerbCategory::Commissive,
            params: &[],
        },
        HandlerDef {
            name: CONTROL,
            description: "perform healthy real event appends on an independent writer",
            visibility: Visibility::Verb,
            category: VerbCategory::Commissive,
            params: &[],
        },
    ];
}

#[async_trait]
impl PackRuntime for AppendProbePack {
    fn name(&self) -> &str {
        <Self as Pack>::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <Self as Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <Self as Pack>::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        <Self as Pack>::HANDLERS
    }

    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        match verb {
            PROBE => self.probe.dispatch().await,
            CONTROL => self.control.dispatch().await,
            _ => panic!("unexpected fixture verb: {verb}"),
        }
    }
}

struct Fixture {
    server: KhiveMcpServer,
    probe: Arc<Lane>,
    _directory: tempfile::TempDir,
}

impl Fixture {
    fn new(method: AppendMethod, outcome: Outcome) -> Self {
        let directory = tempfile::tempdir().expect("private fixture directory");
        let probe = Arc::new(Lane::new(
            &directory.path().join("probe.db"),
            method,
            outcome,
        ));
        let control = Arc::new(Lane::new(
            &directory.path().join("control.db"),
            method,
            Outcome::Success,
        ));
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AppendProbePack {
            probe: Arc::clone(&probe),
            control,
        });
        let server = KhiveMcpServer::from_registry(builder.build().expect("fixture registry"));
        Self {
            server,
            probe,
            _directory: directory,
        }
    }

    async fn request(&self, ops: &str) -> Value {
        let response = tokio::time::timeout(
            Duration::from_secs(10),
            self.server.dispatch_request_local(RequestParams {
                ops: ops.into(),
                presentation: Some("verbose".into()),
                ..Default::default()
            }),
        )
        .await
        .expect("fixture dispatch must finish")
        .expect("valid actual request");
        let expected_state = match self.probe.outcome {
            Outcome::Success => None,
            Outcome::RolledBack => Some(WriterTaskRequestState::TransactionRolledBack),
            Outcome::Unknown | Outcome::UnknownAfterFreeze => {
                Some(WriterTaskRequestState::SideEffectsUnknown)
            }
        };
        assert_eq!(
            self.probe.evidence.lock().unwrap().observed_state,
            expected_state
        );
        serde_json::from_str(&response).expect("request JSON")
    }
}

fn assert_unknown(entry: &Value) {
    assert_eq!(entry["ok"], false, "{entry}");
    assert_eq!(entry["error"]["code"], "writer_task_terminated", "{entry}");
    assert_eq!(
        entry["error"]["request_state"], "side_effects_unknown",
        "{entry}"
    );
    assert_eq!(entry["error"]["task_terminated"], true, "{entry}");
    assert_eq!(entry["error"]["retryable"], false, "{entry}");
    assert!(
        entry.get("usage").is_none(),
        "partial usage must be absent: {entry}"
    );
}

fn assert_success(entry: &Value, method: AppendMethod) {
    assert_eq!(entry["ok"], true, "{entry}");
    assert_eq!(
        entry["usage"],
        json!({"event_rows": 1 + method.rows()}),
        "{entry}"
    );
}

#[tokio::test]
async fn unknown_append_omits_partial_usage_in_single_request() {
    for method in [AppendMethod::Single, AppendMethod::Batch] {
        let fixture = Fixture::new(method, Outcome::Unknown);
        let body = fixture.request("usage_append_probe()").await;
        assert_unknown(&body["results"][0]);
        let prime = fixture
            .probe
            .evidence
            .lock()
            .unwrap()
            .prime
            .expect("prime committed");
        assert!(fixture
            .probe
            .store
            .get_event(prime)
            .await
            .unwrap()
            .is_some());
    }
}

#[tokio::test]
async fn unknown_append_omits_usage_without_poisoning_parallel_sibling() {
    for method in [AppendMethod::Single, AppendMethod::Batch] {
        let fixture = Fixture::new(method, Outcome::Unknown);
        let body = fixture
            .request("[usage_append_probe(), usage_append_control()]")
            .await;
        assert_unknown(&body["results"][0]);
        assert_success(&body["results"][1], method);
        assert_eq!(body["summary"]["succeeded"], 1, "{body}");
        assert_eq!(body["summary"]["failed"], 1, "{body}");
    }
}

#[tokio::test]
async fn unknown_append_omits_usage_in_chain_after_measured_success() {
    for method in [AppendMethod::Single, AppendMethod::Batch] {
        let fixture = Fixture::new(method, Outcome::Unknown);
        let body = fixture
            .request("usage_append_control() | usage_append_probe()")
            .await;
        assert_success(&body["results"][0], method);
        assert_unknown(&body["results"][1]);
    }
}

#[tokio::test]
async fn unknown_append_after_explicit_freeze_omits_frozen_partial_usage() {
    for method in [AppendMethod::Single, AppendMethod::Batch] {
        let fixture = Fixture::new(method, Outcome::UnknownAfterFreeze);
        let body = fixture.request("usage_append_probe()").await;
        assert_unknown(&body["results"][0]);
        assert_eq!(
            fixture.probe.evidence.lock().unwrap().frozen_before_attempt,
            Some(json!({"event_rows": 1}))
        );
    }
}

#[tokio::test]
async fn clean_successful_appends_retain_measured_usage() {
    for method in [AppendMethod::Single, AppendMethod::Batch] {
        let fixture = Fixture::new(method, Outcome::Success);
        let body = fixture.request("usage_append_probe()").await;
        assert_success(&body["results"][0], method);
        let attempted = fixture.probe.evidence.lock().unwrap().attempted.clone();
        for id in attempted {
            assert!(fixture.probe.store.get_event(id).await.unwrap().is_some());
        }
    }
}

#[tokio::test]
async fn clean_rolled_back_appends_retain_measured_partial_usage() {
    for method in [AppendMethod::Single, AppendMethod::Batch] {
        let fixture = Fixture::new(method, Outcome::RolledBack);
        let body = fixture.request("usage_append_probe()").await;
        let entry = &body["results"][0];
        assert_eq!(entry["ok"], false, "{entry}");
        assert_eq!(
            entry["error"]["code"], "writer_task_request_failed",
            "{entry}"
        );
        assert_eq!(
            entry["error"]["request_state"], "transaction_rolled_back",
            "{entry}"
        );
        assert_eq!(entry["error"]["task_terminated"], false, "{entry}");
        assert_eq!(entry["usage"], json!({"event_rows": 1}), "{entry}");
        let (prime, attempted) = {
            let evidence = fixture.probe.evidence.lock().unwrap();
            (
                evidence.prime.expect("prime committed"),
                evidence.attempted.clone(),
            )
        };
        assert!(fixture
            .probe
            .store
            .get_event(prime)
            .await
            .unwrap()
            .is_some());
        for id in attempted {
            assert!(fixture.probe.store.get_event(id).await.unwrap().is_none());
        }
    }
}
