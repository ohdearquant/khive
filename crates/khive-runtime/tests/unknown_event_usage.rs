use std::ffi::OsString;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use khive_runtime::{KhiveRuntime, Namespace, NamespaceToken, RuntimeConfig};
use khive_storage::usage::{scope, UsageContext};
use khive_storage::{StorageCapability, StorageError, WriterTaskRequestState};
use rusqlite::hooks::{Action, AuthAction, Authorization, TransactionOperation};
use serde_json::json;
use uuid::Uuid;

struct QueueEnv(Option<OsString>);

impl QueueEnv {
    fn enable() -> Self {
        let previous = std::env::var_os("KHIVE_WRITE_QUEUE");
        std::env::set_var("KHIVE_WRITE_QUEUE", "1");
        Self(previous)
    }
}

impl Drop for QueueEnv {
    fn drop(&mut self) {
        match &self.0 {
            Some(previous) => std::env::set_var("KHIVE_WRITE_QUEUE", previous),
            None => std::env::remove_var("KHIVE_WRITE_QUEUE"),
        }
    }
}

#[derive(Default)]
struct Fault {
    armed: AtomicBool,
    inserted_events: AtomicU64,
    denied_commits: AtomicU64,
    denied_rollbacks: AtomicU64,
}

async fn install_fault(runtime: &KhiveRuntime) -> Arc<Fault> {
    let fault = Arc::new(Fault::default());
    let hooks = Arc::clone(&fault);
    let writer = runtime
        .backend()
        .pool()
        .writer_task_handle()
        .unwrap()
        .expect("the named production path must use the real queued writer");
    writer
        .send(move |connection| {
            let inserts = Arc::clone(&hooks);
            connection
                .update_hook(Some(move |action, _database: &str, table: &str, _rowid| {
                    if action == Action::SQLITE_INSERT && table == "events" {
                        inserts.inserted_events.fetch_add(1, Ordering::SeqCst);
                    }
                }))
                .map_err(|error| {
                    StorageError::driver(StorageCapability::Sql, "unknown_usage_update_hook", error)
                })?;
            connection
                .authorizer(Some(move |context: rusqlite::hooks::AuthContext<'_>| {
                    if !hooks.armed.load(Ordering::SeqCst) {
                        return Authorization::Allow;
                    }
                    match context.action {
                        // rusqlite represents COMMIT as Unknown, not a Commit variant.
                        AuthAction::Transaction {
                            operation: TransactionOperation::Unknown,
                        } => {
                            hooks.denied_commits.fetch_add(1, Ordering::SeqCst);
                            Authorization::Deny
                        }
                        AuthAction::Transaction {
                            operation: TransactionOperation::Rollback,
                        } => {
                            hooks.denied_rollbacks.fetch_add(1, Ordering::SeqCst);
                            Authorization::Deny
                        }
                        _ => Authorization::Allow,
                    }
                }))
                .map_err(|error| {
                    StorageError::driver(StorageCapability::Sql, "unknown_usage_authorizer", error)
                })?;
            Ok(())
        })
        .await
        .expect("unarmed hook installation must commit normally");
    fault
}

fn assert_unknown(fault: &Fault, context: &UsageContext, error: &khive_runtime::RuntimeError) {
    assert_eq!(
        fault.inserted_events.load(Ordering::SeqCst),
        1,
        "unknown failure must follow the named production event INSERT"
    );
    assert_eq!(fault.denied_commits.load(Ordering::SeqCst), 1);
    assert_eq!(fault.denied_rollbacks.load(Ordering::SeqCst), 1);
    assert_eq!(
        error.writer_task_failure_context().unwrap().request_state,
        WriterTaskRequestState::SideEffectsUnknown
    );
    assert_eq!(
        context.snapshot()["event_rows"],
        json!(1),
        "unknown failure must retain the already measured successful row in raw counters"
    );
    assert!(
        context.shipping_snapshot().is_none(),
        "the named caller must suppress previously frozen partial usage"
    );
}

fn runtime() -> (KhiveRuntime, tempfile::TempDir, NamespaceToken) {
    let directory = tempfile::tempdir().unwrap();
    let runtime = KhiveRuntime::new_for_test(RuntimeConfig {
        db_path: Some(directory.path().join("unknown-event-usage.db")),
        actor_id: Some("unknown-event-usage".into()),
        events_split: None,
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    (runtime, directory, token)
}

use khive_runtime::{ContentMergeStrategy, EntityDedupMergePolicy};

async fn seed_entity(runtime: &KhiveRuntime, token: &NamespaceToken, name: &str) -> Uuid {
    let entity = khive_storage::Entity::new("local", "concept", name);
    let id = entity.id;
    runtime
        .entities(token)
        .unwrap()
        .upsert_entity(entity)
        .await
        .unwrap();
    id
}

async fn seed_note(runtime: &KhiveRuntime, token: &NamespaceToken, content: &str) -> Uuid {
    let note = khive_storage::Note::new("local", "observation", content);
    let id = note.id;
    runtime
        .notes(token)
        .unwrap()
        .upsert_note(note)
        .await
        .unwrap();
    id
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn entity_merge_unknown_outcome_suppresses_frozen_usage_after_real_event_insert() {
    let _queue = QueueEnv::enable();
    let (runtime, _directory, token) = runtime();
    let into = seed_entity(&runtime, &token, "into").await;
    let first = seed_entity(&runtime, &token, "first").await;
    let second = seed_entity(&runtime, &token, "second").await;
    let context = UsageContext::new();
    scope(
        context.clone(),
        runtime.merge_entity(
            &token,
            into,
            first,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        ),
    )
    .await
    .unwrap();
    assert_eq!(context.shipping_snapshot().unwrap()["event_rows"], json!(1));
    context.freeze();
    let fault = install_fault(&runtime).await;
    fault.armed.store(true, Ordering::SeqCst);
    let error = scope(
        context.clone(),
        runtime.merge_entity(
            &token,
            into,
            second,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        ),
    )
    .await
    .unwrap_err();
    assert_unknown(&fault, &context, &error);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn note_merge_unknown_outcome_suppresses_frozen_usage_after_real_event_insert() {
    let _queue = QueueEnv::enable();
    let (runtime, _directory, token) = runtime();
    let into = seed_note(&runtime, &token, "into").await;
    let first = seed_note(&runtime, &token, "first").await;
    let second = seed_note(&runtime, &token, "second").await;
    let context = UsageContext::new();
    scope(
        context.clone(),
        runtime.merge_note(
            &token,
            into,
            first,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        ),
    )
    .await
    .unwrap();
    assert_eq!(context.shipping_snapshot().unwrap()["event_rows"], json!(1));
    context.freeze();
    let fault = install_fault(&runtime).await;
    fault.armed.store(true, Ordering::SeqCst);
    let error = scope(
        context.clone(),
        runtime.merge_note(
            &token,
            into,
            second,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        ),
    )
    .await
    .unwrap_err();
    assert_unknown(&fault, &context, &error);
}
