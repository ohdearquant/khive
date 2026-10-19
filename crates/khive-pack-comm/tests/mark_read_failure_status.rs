use std::{ffi::OsString, sync::Arc, time::Duration};

use khive_pack_comm::CommPack;
use khive_runtime::{
    AllowAllGate, BackendId, KhiveRuntime, Namespace, RuntimeConfig, VerbRegistryBuilder,
};

fn assert_busy_mark_details(result: &serde_json::Value) {
    let details = &result["mark_error_details"];
    assert_eq!(details["code"], "sqlite_statement_failure");
    assert_eq!(details["stage"], "sqlite_statement_failure");
    assert_eq!(details["sqlite_write_stage"], "sqlite_statement_failure");
    assert_eq!(details["sqlite_primary_code"], 5);
    let extended = details["sqlite_extended_code"]
        .as_i64()
        .expect("native SQLite extended code");
    assert_eq!(extended & 0xff, 5, "SQLite busy primary-code family");
    assert_eq!(details["domain_disposition"], "not_committed");
}

#[tokio::test]
async fn lock_contention_reports_failed_mark_read_statuses() {
    let dir = tempfile::tempdir().expect("temporary database directory");
    let db_path = dir.path().join("mark-read-failure-status.db");

    let previous_write_queue: Option<OsString> = std::env::var_os("KHIVE_WRITE_QUEUE");
    std::env::set_var("KHIVE_WRITE_QUEUE", "0");
    let runtime = KhiveRuntime::new(RuntimeConfig {
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: Default::default(),
        wal_ceiling_env_raw: None,
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: khive_runtime::config::resolve_default_display_timezone(),
        events_split: None,
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
        exec: Default::default(),
        ..khive_runtime::RuntimeConfig::no_embeddings()
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
    let first_id = ids[0].clone();

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
    // Deliberately uncooperative external SQLite writer: this fixture must
    // observe native SQLITE_BUSY rather than application lease contention.
    let holder = rusqlite::Connection::open_with_flags(
        runtime.backend().pool().canonical_path().unwrap(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .expect("external lock-holder connection");
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
    assert!(single.get("content").is_none());
    assert!(single.get("subject").is_none());
    assert_busy_mark_details(&single);

    let bulk_read = registry
        .dispatch("comm.read", serde_json::json!({ "ids": ids.clone() }))
        .await
        .expect("best-effort bulk read returns degraded rows");
    assert_eq!(bulk_read["status"], "failed");
    assert_eq!(bulk_read["failed_count"], 2);
    assert!(bulk_read["results"]
        .as_array()
        .expect("bulk read result array")
        .iter()
        .all(|result| result.get("content").is_none() && result.get("subject").is_none()));
    for result in bulk_read["results"].as_array().expect("bulk read results") {
        assert_busy_mark_details(result);
    }

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
    for result in bulk["results"].as_array().expect("bulk mark results") {
        assert_busy_mark_details(result);
    }

    let still_unread = registry
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "unread", "limit": 2 }),
        )
        .await
        .expect("failed marks leave both messages unread");
    assert_eq!(
        still_unread["messages"]
            .as_array()
            .expect("unread messages")
            .len(),
        2
    );

    holder
        .execute_batch("ROLLBACK")
        .expect("release the SQLite writer lock");

    let retried = registry
        .dispatch("comm.read", serde_json::json!({ "id": first_id }))
        .await
        .expect("mark succeeds after lock release");
    assert_eq!(retried["status"], "success");
    assert_eq!(retried["read"], true);
    assert!(retried.get("mark_error").is_none());
    assert!(retried.get("mark_error_details").is_none());
}
