//! Initially red writer-drop measurement after the existing registry audit drain.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use khive_pack_kg::KgPack;
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

async fn join_writer(mut join: tokio::task::JoinHandle<()>) -> Value {
    match tokio::time::timeout(CLEANUP_LIMIT, &mut join).await {
        Ok(Ok(())) => json!({"joined": true}),
        Ok(Err(error)) => json!({"joined": false, "join_error": error.to_string()}),
        Err(_) => json!({"joined": false, "cleanup_timeout": true}),
    }
}

async fn reopen_first_write(root: &Path) -> Value {
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
    let join = pool.take_writer_task_join();
    drop(runtime);
    drop(pool);
    let cleanup = match join {
        Some(join) => join_writer(join).await,
        None => json!({"joined": false, "missing_writer_join": true}),
    };
    match result {
        Ok(id) => json!({
            "opened": true, "first_write_committed": true, "note_id": id,
            "single_writer_proven": false, "cleanup": cleanup,
            "pool_alive_after_cleanup": weak.upgrade().is_some(),
        }),
        Err(error) => json!({
            "opened": true, "first_write_committed": false, "error": error,
            "single_writer_proven": false, "cleanup": cleanup,
            "pool_alive_after_cleanup": weak.upgrade().is_some(),
        }),
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "baseline drop measurement; run explicitly before a close implementation, expected red"]
async fn runtime_drop_records_tasks_handles_and_first_reopened_write() {
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
    let writer_join = pool
        .take_writer_task_join()
        .expect("first writer join owner");
    let pool_weak = Arc::downgrade(&pool);

    let canary_path = root.join("census-canary");
    let canary = File::create(&canary_path).expect("live census canary");
    let before = support::open_store_handles(&root);
    eprintln!(
        "runtime_close_measurement {}",
        json!({"phase": "before_drop", "files": census_json(&before)})
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
    drop(registry);
    drop(runtime);
    drop(pool);

    // No await between releasing owners and this current-thread snapshot.
    let writer_finished_immediately = writer_join.is_finished();
    let pool_alive_immediately = pool_weak.upgrade().is_some();
    let immediate_files = support::open_store_handles(&root);
    let immediate = json!({
        "phase": "immediate_after_drop", "writer_finished": writer_finished_immediately,
        "pool_alive": pool_alive_immediately, "files": census_json(&immediate_files),
    });
    eprintln!("runtime_close_measurement {immediate}");

    // Keep the first join observation owned while testing the second runtime.
    let reopened = reopen_first_write(&root).await;
    let writer_finished_after_reopen = writer_join.is_finished();
    let cleanup = join_writer(writer_join).await;
    let post_join_files = support::open_store_handles(&root);
    let report = json!({
        "scope": "bare_single_backend_after_registry_audit_shutdown",
        "audit_pre_drained": true,
        "optional_host_loops_embeddings_events_split": false,
        "timeout_sink_may_be_process_global_under_store": true,
        "root": root, "immediate": immediate, "reopened": reopened,
        "first_writer_finished_after_reopen": writer_finished_after_reopen,
        "first_writer_cleanup": cleanup,
        "pool_alive_after_cleanup": pool_weak.upgrade().is_some(),
        "post_join_files": census_json(&post_join_files),
        "post_join_files_empty": post_join_files.as_ref().is_ok_and(Vec::is_empty),
    });
    eprintln!("runtime_close_measurement {report}");

    assert_eq!(report["first_writer_cleanup"]["joined"], true, "{report}");
    assert_eq!(
        report["reopened"]["first_write_committed"], true,
        "{report}"
    );
    assert_eq!(report["reopened"]["cleanup"]["joined"], true, "{report}");
    assert!(
        post_join_files.is_ok(),
        "post-join census incomplete: {report}"
    );
    assert!(
        writer_finished_immediately
            && !pool_alive_immediately
            && immediate_files.as_ref().is_ok_and(Vec::is_empty)
            && post_join_files.as_ref().is_ok_and(Vec::is_empty),
        "baseline drop did not meet immediate task/file release or settled file cleanup: {report}"
    );
}
