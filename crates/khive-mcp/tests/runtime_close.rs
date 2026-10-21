//! Explicit close acceptance for an evicted, quiescent single-backend host slot.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use khive_pack_kg::KgPack;
use khive_runtime::runtime::{
    RuntimeCloseError, RuntimeCloseHostTasks, RuntimeCloseOwner, RuntimeClosePhase,
    RuntimeCloseRootKind,
};
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use khive_storage::{types::PageRequest, EventFilter};
use khive_types::{EventKind, EventOutcome};
use serde_json::{json, Value};

#[path = "support/store_handle_census.rs"]
mod support;

const CLEANUP_LIMIT: Duration = Duration::from_secs(15);

fn config(root: &Path) -> RuntimeConfig {
    RuntimeConfig {
        db_path: Some(root.join("store.sqlite")),
        volume_lock_dir: Some(root.join("volume-locks")),
        default_namespace: Namespace::local(),
        visible_namespaces: Vec::new(),
        actor_id: Some("test:runtime-close".into()),
        brain_profile: None,
        events_split: None,
        mounts: Vec::new(),
        credentials: Vec::new(),
        packs: vec!["kg".into()],
        ..RuntimeConfig::no_embeddings()
    }
}

fn registry(runtime: &KhiveRuntime) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(runtime.config().actor_id.clone());
    builder.with_default_namespace("local");
    builder
        .with_runtime_event_store(runtime)
        .expect("real runtime audit store");
    builder.register(KgPack::new(runtime.clone()));
    let registry = builder.build().expect("real KG registry");
    registry
        .apply_schema_plans_with_map(&Default::default(), runtime.backend())
        .expect("KG schema plans");
    registry
}

fn census_json(result: &std::io::Result<Vec<std::path::PathBuf>>) -> Value {
    match result {
        Ok(paths) => json!({"complete": true, "open_paths": paths}),
        Err(error) => json!({"complete": false, "error": error.to_string()}),
    }
}

async fn reopen_first_write(root: &Path, previous_writer_finished: bool) -> Value {
    let runtime = match KhiveRuntime::new(config(root)) {
        Ok(runtime) => runtime,
        Err(error) => return json!({"opened": false, "error": error.to_string()}),
    };
    let operation = async {
        let token = runtime
            .authorize(Namespace::local())
            .map_err(|error| error.to_string())?;
        let note = runtime
            .create_note(
                &token,
                "observation",
                None,
                "The reopened runtime commits its first ordinary note write.",
                None,
                None,
                Vec::new(),
            )
            .await
            .map_err(|error| error.to_string())?;
        let persisted = runtime
            .notes(&token)
            .map_err(|error| error.to_string())?
            .get_note(note.id)
            .await
            .map_err(|error| error.to_string())?;
        if persisted.as_ref().map(|stored| stored.content.as_str()) != Some(note.content.as_str()) {
            return Err("first reopened write was not readable as committed".to_string());
        }
        Ok::<_, String>(note.id.to_string())
    };
    // This bounds the async wait, not synchronous native I/O or settlement.
    // Keep the runtime so a timeout still reaches the actual writer join below.
    let result = match tokio::time::timeout(CLEANUP_LIMIT, operation).await {
        Ok(result) => result,
        Err(_) => Err("reopened write/readback timed out; settlement is not inferred".to_string()),
    };
    let pool = runtime.backend().pool_arc();
    let weak = Arc::downgrade(&pool);
    let registry = registry(&runtime);
    let mut owner =
        RuntimeCloseOwner::new(registry, vec![runtime], RuntimeCloseHostTasks::default());
    drop(pool);
    let cleanup_result = tokio::time::timeout(CLEANUP_LIMIT, owner.close()).await;
    let cleanup = json!({"joined": matches!(cleanup_result, Ok(Ok(()))) && owner.writer_tasks_finished(), "result": format!("{cleanup_result:?}")});
    match result {
        Ok(id) => json!({
            "opened": true, "first_write_committed": true, "note_id": id,
            "single_writer_proven": previous_writer_finished, "cleanup": cleanup,
            "pool_alive_after_cleanup": weak.upgrade().is_some(),
        }),
        Err(error) => json!({
            "opened": true, "first_write_committed": false, "error": error,
            "single_writer_proven": previous_writer_finished, "cleanup": cleanup,
            "pool_alive_after_cleanup": weak.upgrade().is_some(),
        }),
    }
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(config_ledger)]
async fn runtime_close_records_tasks_handles_and_first_reopened_write() {
    let directory = tempfile::tempdir().expect("private store root");
    let root = directory
        .path()
        .canonicalize()
        .expect("canonical store root");
    let locks = root.join("volume-locks");
    std::fs::create_dir(&locks).expect("configured volume-lock directory must exist");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&locks, std::fs::Permissions::from_mode(0o700))
            .expect("private volume-lock directory");
    }
    let runtime = KhiveRuntime::new(config(&root)).expect("bare file-backed runtime");
    assert!(runtime.registered_embedding_model_names().is_empty());
    assert!(runtime.config().events_split.is_none());
    assert!(runtime.config().mounts.is_empty());
    let pool = runtime.backend().pool_arc();
    assert!(
        pool.write_queue_active(),
        "measurement requires the real queued writer; set KHIVE_WRITE_QUEUE=1"
    );
    let registry = registry(&runtime);
    assert!(registry.audit_batch_handle().is_some());
    let entity = registry
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "Runtime close entity", "skip_dedup_check": true}),
        )
        .await
        .expect("create real entity");
    assert!(entity["id"].as_str().is_some());
    let note = registry
        .dispatch(
            "create",
            json!({"kind": "observation", "content": "Runtime close note without embeddings."}),
        )
        .await
        .expect("create real note");
    assert!(note["id"].as_str().is_some());
    registry
        .dispatch("stats", json!({}))
        .await
        .expect("real audited dispatch");
    tokio::time::timeout(CLEANUP_LIMIT, registry.shutdown_audit_batch())
        .await
        .expect("audit drain must settle before measurement")
        .expect("accepted audit rows committed");
    let audits = registry
        .event_store()
        .expect("real audit sink")
        .query_events(
            EventFilter {
                kinds: vec![EventKind::Audit],
                verbs: vec!["stats".into()],
                outcome: Some(EventOutcome::Success),
                ..Default::default()
            },
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .expect("read committed audit")
        .items;
    assert_eq!(audits.len(), 1, "stats must have its real persisted audit");
    assert!(pool.writer_task_join_was_stored());
    let pool_weak = Arc::downgrade(&pool);

    let canary_path = root.join("census-canary");
    let canary = File::create(&canary_path).expect("live census canary");
    let before = support::open_store_handles(&root);
    eprintln!(
        "runtime_close_measurement {}",
        json!({"phase": "before_close", "files": census_json(&before)})
    );
    let paths = before.expect("supported, complete process-file census");
    assert!(
        paths.contains(&canary_path),
        "census missed live self canary: {paths:?}"
    );
    assert!(
        paths.contains(&root.join("store.sqlite")),
        "census missed open database: {paths:?}"
    );
    drop(canary);
    // Ensure the actual sink opened before the no-sink-close control. A file
    // merely existing on disk would not establish a live descriptor.
    tokio::time::timeout(CLEANUP_LIMIT, async {
        loop {
            let paths = support::open_store_handles(&root).expect("complete sink census");
            if paths.iter().any(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("writer_timeouts."))
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("real sink must open before close");
    let counter_clone = runtime.clone();
    let mut owner = RuntimeCloseOwner::new(
        registry,
        vec![runtime, counter_clone],
        RuntimeCloseHostTasks::default(),
    );
    drop(pool);
    tokio::time::timeout(CLEANUP_LIMIT, owner.close())
        .await
        .expect("bounded close")
        .expect("physical close");
    assert!(owner.is_closed());
    let writer_finished = owner.writer_tasks_finished();
    let after_files = support::open_store_handles(&root);
    let closed = json!({
        "phase": "after_close", "writer_finished": writer_finished,
        "pool_alive": pool_weak.upgrade().is_some(), "files": census_json(&after_files),
    });
    eprintln!("runtime_close_measurement {closed}");
    assert!(
        writer_finished,
        "writer still running after close: {closed}"
    );
    assert!(
        pool_weak.upgrade().is_none(),
        "pool survived close: {closed}"
    );
    assert!(
        after_files.as_ref().is_ok_and(Vec::is_empty),
        "close left store handles open: {closed}"
    );
    owner.close().await.expect("idempotent close");

    let reopened = reopen_first_write(&root, writer_finished).await;
    let final_files = support::open_store_handles(&root);
    let report = json!({
        "scope": "bare_single_backend_explicit_close",
        "audit_pre_drained": true, "skipped_owner_configurations_claimed": false,
        "root": root, "closed": closed, "reopened": reopened,
        "post_reopen_files": census_json(&final_files),
    });
    eprintln!("runtime_close_measurement {report}");
    assert_eq!(
        report["reopened"]["first_write_committed"], true,
        "{report}"
    );
    assert_eq!(report["reopened"]["single_writer_proven"], true, "{report}");
    assert_eq!(report["reopened"]["cleanup"]["joined"], true, "{report}");
    assert!(final_files.as_ref().is_ok_and(Vec::is_empty), "{report}");
}

fn private_root() -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("private root");
    let locks = directory.path().join("volume-locks");
    std::fs::create_dir(&locks).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&locks, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    directory
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(config_ledger)]
async fn external_runtime_and_capability_refuse_until_released() {
    let root = private_root();
    let runtime = KhiveRuntime::new(config(root.path())).unwrap();
    let registry = registry(&runtime);
    let forgotten = runtime.clone();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let capability = runtime.notes(&token).unwrap();
    let mut owner =
        RuntimeCloseOwner::new(registry, vec![runtime], RuntimeCloseHostTasks::default());
    assert!(matches!(
        owner.close().await,
        Err(RuntimeCloseError::Incomplete { .. })
    ));
    assert!(!owner.is_closed());
    drop(forgotten);
    assert!(matches!(
        owner.close().await,
        Err(RuntimeCloseError::Incomplete { .. })
    ));
    drop(capability);
    tokio::time::timeout(CLEANUP_LIMIT, owner.close())
        .await
        .unwrap()
        .unwrap();
    owner.close().await.unwrap();
    assert!(owner.writer_tasks_finished());
    assert!(support::open_store_handles(root.path()).unwrap().is_empty());
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(config_ledger)]
async fn cancelled_close_resumes_and_waits_for_accepted_native_write() {
    let root = private_root();
    let runtime = KhiveRuntime::new(config(root.path())).unwrap();
    let registry = registry(&runtime);
    // Finish audit before deliberately holding the writer's native closure.
    registry.shutdown_audit_batch().await.unwrap();
    let pool = runtime.backend().pool_arc();
    let writer = pool.writer_task_handle().unwrap().expect("queued writer");
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let request = tokio::spawn(async move {
        writer.send(move |connection| {
            let _ = entered_tx.send(());
            // Bounded native wait still releases on unwinding a failed test.
            release_rx.recv_timeout(CLEANUP_LIMIT).expect("release native closure");
            connection.execute_batch("CREATE TABLE close_settlement (value INTEGER); INSERT INTO close_settlement VALUES (1)")
                .map_err(|error| khive_storage::StorageError::Internal(error.to_string()))?;
            Ok(())
        }).await
    });
    tokio::time::timeout(CLEANUP_LIMIT, entered_rx)
        .await
        .unwrap()
        .unwrap();
    // Cancel only the caller. The admitted native write must remain live.
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    let weak = Arc::downgrade(&pool);
    drop(pool);
    let mut owner =
        RuntimeCloseOwner::new(registry, vec![runtime], RuntimeCloseHostTasks::default());
    for _ in 0..2 {
        assert!(
            tokio::time::timeout(Duration::from_millis(20), owner.close())
                .await
                .is_err(),
            "close returned before the held native writer settled"
        );
        assert!(!owner.writer_tasks_finished());
        assert!(!owner.is_closed());
    }
    assert!(weak.upgrade().is_none());
    assert!(!support::open_store_handles(root.path()).unwrap().is_empty());
    release_tx.send(()).unwrap();
    tokio::time::timeout(CLEANUP_LIMIT, owner.close())
        .await
        .unwrap()
        .unwrap();
    assert!(owner.writer_tasks_finished());
    assert!(support::open_store_handles(root.path()).unwrap().is_empty());
    // Settlement includes the write, not merely a detached/aborted parent.
    let connection = rusqlite::Connection::open(root.path().join("store.sqlite")).unwrap();
    let value: i64 = connection
        .query_row("SELECT value FROM close_settlement", [], |row| row.get(0))
        .unwrap();
    assert_eq!(value, 1);
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(config_ledger)]
async fn a_join_taken_elsewhere_is_never_reported_as_completed() {
    let root = private_root();
    let runtime = KhiveRuntime::new(config(root.path())).unwrap();
    let registry = registry(&runtime);
    let pool = runtime.backend().pool_arc();
    let sink = pool.timeout_sink_close_handle();
    let join = pool.take_writer_task_join().expect("writer join");
    drop(pool);
    let mut owner =
        RuntimeCloseOwner::new(registry, vec![runtime], RuntimeCloseHostTasks::default());
    assert_eq!(
        owner.close().await,
        Err(RuntimeCloseError::WriterJoinUnavailable)
    );
    tokio::time::timeout(CLEANUP_LIMIT, join)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        owner.close().await,
        Err(RuntimeCloseError::WriterJoinUnavailable)
    );
    assert!(!owner.is_closed());
    if let Some(sink) = sink {
        sink.close().await.unwrap();
    }
    assert!(support::open_store_handles(root.path()).unwrap().is_empty());
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(config_ledger)]
async fn reversible_refusal_preserves_real_dispatch_and_readback() {
    let root = private_root();
    let runtime = KhiveRuntime::new(config(root.path())).unwrap();
    let registry = registry(&runtime);
    let mut owner = RuntimeCloseOwner::new(
        registry,
        vec![runtime],
        RuntimeCloseHostTasks {
            mcp_session_sweep: true,
            daemon_checkpoint: false,
        },
    );
    assert_eq!(
        owner.close().await,
        Err(RuntimeCloseError::Refused("MCP session sweep"))
    );
    assert_eq!(owner.phase(), RuntimeClosePhase::Ready);
    let registry = owner.open_registry().expect("registry remains usable");
    let created = registry
        .dispatch(
            "create",
            json!({"kind":"observation","content":"still usable after refusal"}),
        )
        .await
        .unwrap();
    let found = registry
        .dispatch("get", json!({"id": created["id"]}))
        .await
        .unwrap();
    assert!(
        found.to_string().contains("still usable after refusal"),
        "{found}"
    );
    registry.shutdown_audit_batch().await.unwrap();
    // The conservative host declaration cannot be cleared on a closing owner.
    // This refusal fixture cleans up its resources via the still-owned runtimes.
    let runtime = owner.open_runtimes().unwrap()[0].clone();
    let pool = runtime.backend().pool_arc();
    let sink = pool.timeout_sink_close_handle();
    let join = pool.take_writer_task_join();
    drop(owner);
    drop(runtime);
    drop(pool);
    if let Some(join) = join {
        tokio::time::timeout(CLEANUP_LIMIT, join)
            .await
            .unwrap()
            .unwrap();
    }
    if let Some(sink) = sink {
        sink.close().await.unwrap();
    }
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(config_ledger)]
async fn incomplete_names_retained_pool_and_resumes_before_reopen() {
    let root = private_root();
    let runtime = KhiveRuntime::new(config(root.path())).unwrap();
    let registry = registry(&runtime);
    let retained_pool = runtime.backend().pool_arc();
    let mut owner =
        RuntimeCloseOwner::new(registry, vec![runtime], RuntimeCloseHostTasks::default());
    let Err(RuntimeCloseError::Incomplete { roots }) = owner.close().await else {
        panic!("retained pool must prevent completion")
    };
    assert_eq!(
        roots,
        vec![khive_runtime::runtime::RuntimeCloseRoot {
            kind: RuntimeCloseRootKind::Pool,
            strong_count: 1
        }]
    );
    assert_eq!(owner.phase(), RuntimeClosePhase::Closing);
    assert!(owner.open_registry().is_none());
    assert!(owner.open_runtimes().is_none());
    drop(retained_pool);
    tokio::time::timeout(CLEANUP_LIMIT, owner.close())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(owner.phase(), RuntimeClosePhase::Closed);
    assert!(support::open_store_handles(root.path()).unwrap().is_empty());
    let reopened = reopen_first_write(root.path(), owner.writer_tasks_finished()).await;
    assert_eq!(reopened["first_write_committed"], true, "{reopened}");
    assert_eq!(reopened["single_writer_proven"], true, "{reopened}");
}
