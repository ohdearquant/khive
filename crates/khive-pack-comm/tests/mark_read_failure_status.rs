use std::{ffi::OsString, sync::Arc, time::Duration};

use khive_pack_comm::CommPack;
use khive_runtime::{
    AllowAllGate, BackendId, KhiveRuntime, Namespace, RuntimeConfig, VerbRegistryBuilder,
};

#[tokio::test]
async fn lock_contention_reports_failed_mark_read_statuses() {
    let dir = tempfile::tempdir().expect("temporary database directory");
    let db_path = dir.path().join("mark-read-failure-status.db");

    let previous_write_queue: Option<OsString> = std::env::var_os("KHIVE_WRITE_QUEUE");
    std::env::set_var("KHIVE_WRITE_QUEUE", "0");
    let runtime = KhiveRuntime::new(RuntimeConfig {
        git_write: Default::default(),
        display_timezone: khive_runtime::config::resolve_default_display_timezone(),
        db_path: Some(db_path),
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
    });
    match previous_write_queue {
        Some(value) => std::env::set_var("KHIVE_WRITE_QUEUE", value),
        None => std::env::remove_var("KHIVE_WRITE_QUEUE"),
    }
    let runtime = runtime.expect("file-backed runtime");

    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(CommPack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");

    for content in ["first unread message", "second unread message"] {
        registry
            .dispatch(
                "comm.send",
                serde_json::json!({ "to": "local", "content": content }),
            )
            .await
            .expect("self-send seeds one inbound message");
    }
    let inbox = registry
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "unread", "limit": 2 }),
        )
        .await
        .expect("inbox lists both unread messages");
    let ids: Vec<String> = inbox["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .map(|message| {
            message["full_id"]
                .as_str()
                .expect("full message id")
                .to_string()
        })
        .collect();
    assert_eq!(ids.len(), 2);

    {
        let writer = runtime
            .backend()
            .pool()
            .try_writer()
            .expect("pooled writer");
        writer
            .conn()
            .busy_timeout(Duration::ZERO)
            .expect("zero busy timeout");
    }
    let holder = runtime
        .backend()
        .pool()
        .open_standalone_writer()
        .expect("lock-holder connection");
    holder
        .execute_batch("BEGIN IMMEDIATE")
        .expect("lock holder synchronously owns the SQLite writer lock");

    let single = registry
        .dispatch("comm.read", serde_json::json!({ "id": ids[0] }))
        .await
        .expect("best-effort read returns its degraded result");
    assert_eq!(single["status"], "failed");
    assert_eq!(single["read"], false);
    assert!(single["mark_error"]
        .as_str()
        .is_some_and(|error| error.contains("locked")));

    let bulk = registry
        .dispatch("comm.mark_read", serde_json::json!({ "ids": ids }))
        .await
        .expect("best-effort bulk mark returns its degraded results");
    assert_eq!(bulk["status"], "failed");
    assert_eq!(bulk["marked_count"], 0);
    assert_eq!(bulk["failed_count"], 2);
    assert!(bulk["results"]
        .as_array()
        .expect("bulk result array")
        .iter()
        .all(|result| result["status"] == "failed" && result["read"] == false));

    holder
        .execute_batch("ROLLBACK")
        .expect("release the SQLite writer lock");
}
