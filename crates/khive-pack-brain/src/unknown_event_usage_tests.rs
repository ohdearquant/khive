use std::ffi::OsString;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use khive_runtime::{
    KhiveRuntime, Namespace, NamespaceToken, PackRuntime, RuntimeConfig, RuntimeError,
    VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::usage::{self, UsageContext};
use khive_storage::{StorageCapability, StorageError, WriterTaskRequestState};
use khive_types::EventKind;
use rusqlite::hooks::{Action, AuthAction, AuthContext, Authorization, TransactionOperation};
use serde_json::{json, Value};

use crate::BrainPack;

struct WriteQueueEnv(Option<OsString>);

impl WriteQueueEnv {
    fn enable() -> Self {
        let previous = std::env::var_os("KHIVE_WRITE_QUEUE");
        std::env::set_var("KHIVE_WRITE_QUEUE", "1");
        Self(previous)
    }
}

impl Drop for WriteQueueEnv {
    fn drop(&mut self) {
        match &self.0 {
            Some(previous) => std::env::set_var("KHIVE_WRITE_QUEUE", previous),
            None => std::env::remove_var("KHIVE_WRITE_QUEUE"),
        }
    }
}

#[derive(Clone, Copy)]
enum Caller {
    DirectFeedback,
    UnattributedGate,
    SectionFeedback,
}

impl Caller {
    fn verb(self) -> &'static str {
        match self {
            Self::DirectFeedback | Self::UnattributedGate => "brain.feedback",
            Self::SectionFeedback => "brain.section_feedback",
        }
    }

    fn private_event_rows(self) -> usize {
        match self {
            Self::DirectFeedback | Self::SectionFeedback => 1,
            Self::UnattributedGate => 0,
        }
    }
}

#[derive(Default)]
struct TerminalFault {
    armed: AtomicBool,
    public_inserts: AtomicUsize,
    private_inserts: AtomicUsize,
    denied_commits: AtomicUsize,
    denied_rollbacks: AtomicUsize,
}

impl TerminalFault {
    fn assert_inserts(&self, caller: Caller) {
        assert_eq!(
            self.public_inserts.load(Ordering::SeqCst),
            1,
            "the real caller must execute its public events INSERT"
        );
        assert_eq!(
            self.private_inserts.load(Ordering::SeqCst),
            caller.private_event_rows(),
            "unattributed feedback must bypass the profile persistence transaction"
        );
    }

    fn arm(&self) {
        self.public_inserts.store(0, Ordering::SeqCst);
        self.private_inserts.store(0, Ordering::SeqCst);
        self.armed.store(true, Ordering::SeqCst);
    }
}

struct Fixture {
    runtime: KhiveRuntime,
    pack: BrainPack,
    registry: VerbRegistry,
    token: NamespaceToken,
    target: String,
    _directory: tempfile::TempDir,
    _queue: WriteQueueEnv,
}

impl Fixture {
    async fn new() -> Self {
        let queue = WriteQueueEnv::enable();
        let directory = tempfile::tempdir().expect("private fixture directory");
        let runtime = KhiveRuntime::new_for_test(RuntimeConfig {
            db_path: Some(directory.path().join("brain-unknown-append.db")),
            actor_id: Some("lambda:unknown-brain-event-usage".into()),
            brain_profile: None,
            events_split: None,
            packs: vec!["kg".into(), "brain".into()],
            embedding_model: None,
            additional_embedding_models: vec![],
            ..RuntimeConfig::no_embeddings()
        })
        .expect("private file-backed runtime without embeddings");
        let pool = runtime.backend().pool_arc();
        assert_eq!(pool.config().write_queue_enabled, Some(true));
        assert!(pool.writer_task_handle().unwrap().is_some());
        assert!(runtime.config().events_split.is_none());
        let token = runtime
            .authorize(Namespace::local())
            .expect("fixture actor");
        let target = runtime
            .create_entity_with_embedding_report(
                &token,
                "concept",
                None,
                "unknown append feedback target",
                None,
                None,
                vec![],
            )
            .await
            .expect("real feedback target")
            .0
            .id
            .to_string();
        let registry = VerbRegistryBuilder::new().build().expect("empty registry");
        let pack = BrainPack::new(runtime.clone());
        pack.dispatch(
            "brain.profile",
            json!({"profile_id":"balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .expect("bootstrap default profile before fault installation");
        Self {
            runtime,
            pack,
            registry,
            token,
            target,
            _directory: directory,
            _queue: queue,
        }
    }

    async fn install_fault(&self) -> Arc<TerminalFault> {
        let fault = Arc::new(TerminalFault::default());
        let authorize_fault = Arc::clone(&fault);
        let insert_fault = Arc::clone(&fault);
        self.runtime
            .backend()
            .pool_arc()
            .writer_task_handle()
            .unwrap()
            .expect("same main-store writer used by atomic_unit")
            .send(move |conn| {
                conn.authorizer(Some(move |context: AuthContext<'_>| {
                    if !authorize_fault.armed.load(Ordering::SeqCst) {
                        return Authorization::Allow;
                    }
                    match context.action {
                        // rusqlite maps SQLite's COMMIT spelling to Unknown.
                        AuthAction::Transaction {
                            operation: TransactionOperation::Unknown,
                        } => {
                            authorize_fault
                                .denied_commits
                                .fetch_add(1, Ordering::SeqCst);
                            Authorization::Deny
                        }
                        AuthAction::Transaction {
                            operation: TransactionOperation::Rollback,
                        } => {
                            authorize_fault
                                .denied_rollbacks
                                .fetch_add(1, Ordering::SeqCst);
                            Authorization::Deny
                        }
                        _ => Authorization::Allow,
                    }
                }))
                .map_err(|error| {
                    StorageError::driver(StorageCapability::Sql, "test.install_authorizer", error)
                })?;
                conn.update_hook(Some(move |action, database: &str, table: &str, _rowid| {
                    if action == Action::SQLITE_INSERT && database == "main" {
                        match table {
                            "events" => {
                                insert_fault.public_inserts.fetch_add(1, Ordering::SeqCst);
                            }
                            "brain_event_log" => {
                                insert_fault.private_inserts.fetch_add(1, Ordering::SeqCst);
                            }
                            _ => {}
                        }
                    }
                }))
                .map_err(|error| {
                    StorageError::driver(StorageCapability::Sql, "test.install_update_hook", error)
                })
            })
            .await
            .expect("unarmed installation transaction must commit");
        fault
    }

    async fn call(&self, caller: Caller) -> Result<Value, RuntimeError> {
        match caller {
            Caller::DirectFeedback => {
                self.pack
                    .dispatch(
                        "brain.feedback",
                        json!({
                            "target_id":self.target,
                            "signal":"explicit_positive",
                            "served_by_profile_id":"balanced-recall-v1",
                        }),
                        &self.registry,
                        &self.token,
                    )
                    .await
            }
            Caller::UnattributedGate => {
                self.pack
                    .dispatch(
                        "brain.feedback",
                        json!({
                            "target_id":self.target,
                            "signal":"implicit_positive",
                            "serve_attribution":"unattributed",
                        }),
                        &self.registry,
                        &self.token,
                    )
                    .await
            }
            Caller::SectionFeedback => {
                self.pack
                    .apply_profile_section_feedback(
                        &self.token,
                        "balanced-recall-v1",
                        json!({"overview":"useful"}),
                        Some("atom:outside-the-kg".into()),
                    )
                    .await
            }
        }
    }

    async fn assert_baseline_event(&self, caller: Caller, result: &Value) {
        let event_id = result["event_id"].as_str().expect("committed event UUID");
        let event = self
            .runtime
            .events(&self.token)
            .unwrap()
            .get_event(event_id.parse().unwrap())
            .await
            .unwrap()
            .expect("event persisted in the main store");
        assert_eq!(event.verb, caller.verb());
        assert_eq!(event.kind, EventKind::FeedbackExplicit);
        match caller {
            Caller::DirectFeedback => {
                assert_eq!(event.payload["signal"], "explicit_positive");
                assert_eq!(event.payload["served_by_profile_id"], "balanced-recall-v1");
                assert!(event.payload.get("gate").is_none());
            }
            Caller::UnattributedGate => {
                assert_eq!(result["serve_attribution"], "unattributed");
                assert!(result["served_by_profile_id"].is_null());
                assert_eq!(event.payload["profile_resolution"], "serve_unattributed");
                assert_eq!(event.payload["gate"]["forced_zero_weight"], true);
                assert_eq!(event.payload["gate"]["effective_weight"], 0.0);
                assert!(event.payload["served_by_profile_id"].is_null());
            }
            Caller::SectionFeedback => {
                assert!(event.target_id.is_none());
                assert_eq!(
                    event.payload["section_signals"],
                    json!({"overview":"useful"})
                );
                assert_eq!(event.payload["target_attribution"], "atom:outside-the-kg");
            }
        }
    }
}

async fn assert_unknown_append(caller: Caller) {
    let fixture = Fixture::new().await;
    let fault = fixture.install_fault().await;
    let usage = UsageContext::new();
    let state_before = serde_json::to_value(fixture.pack.snapshot()).unwrap();
    let result = usage::scope(usage.clone(), fixture.call(caller))
        .await
        .expect("unarmed caller must commit its positive baseline");
    assert_eq!(result["emitted"], true);
    fault.assert_inserts(caller);
    fixture.assert_baseline_event(caller, &result).await;
    if matches!(caller, Caller::UnattributedGate) {
        assert_eq!(
            serde_json::to_value(fixture.pack.snapshot()).unwrap(),
            state_before
        );
    }
    assert_eq!(usage.snapshot()["event_rows"], json!(1u64));
    let frozen = usage.freeze();
    assert_eq!(usage.shipping_snapshot(), Some(frozen.clone()));

    fault.arm();
    let error = usage::scope(usage.clone(), fixture.call(caller))
        .await
        .expect_err("COMMIT and ROLLBACK denial must make append outcome unknown");
    fault.assert_inserts(caller);
    assert!(fault.denied_commits.load(Ordering::SeqCst) > 0);
    assert!(fault.denied_rollbacks.load(Ordering::SeqCst) > 0);
    let context = error
        .writer_task_failure_context()
        .expect("real append must return typed writer-task failure provenance");
    assert_eq!(
        context.request_state,
        WriterTaskRequestState::SideEffectsUnknown
    );
    assert_eq!(usage.shipping_snapshot(), None);
    assert_eq!(usage.snapshot()["event_rows"], json!(1u64));
    assert_eq!(usage.frozen_or_snapshot(), frozen);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn brain_feedback_direct_unknown_append_suppresses_frozen_shipping() {
    assert_unknown_append(Caller::DirectFeedback).await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn brain_feedback_unattributed_gate_unknown_append_suppresses_frozen_shipping() {
    assert_unknown_append(Caller::UnattributedGate).await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn section_feedback_unknown_append_suppresses_frozen_shipping() {
    assert_unknown_append(Caller::SectionFeedback).await;
}
