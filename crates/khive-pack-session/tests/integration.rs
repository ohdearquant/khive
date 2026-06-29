//! End-to-end integration tests for `khive-pack-session`.
//!
//! All tests use a file-backed runtime (TempDir + db_path: Some(path)).
//! In-memory runtimes are deliberately avoided: the ANN warm-path (ADR-079)
//! only writes v2 segments when the backend has a `data_dir`, so in-memory
//! runtimes silently skip persistence and can produce false negatives.
//!
//! FILE SIZE JUSTIFICATION: All tests share a single `file_rt` / `pack` helper
//! pair and a common `store_session` convenience. Splitting into multiple files
//! would either duplicate this fixture or require exposing it as a helper crate.

use std::sync::Arc;

use khive_pack_kg::KgPack;
use khive_pack_session::SessionPack;
use khive_runtime::{
    AllowAllGate, BackendId, KhiveRuntime, Namespace, RuntimeConfig, VerbRegistry,
    VerbRegistryBuilder,
};
use serde_json::{json, Value};
use tempfile::TempDir;

// ── helpers ───────────────────────────────────────────────────────────────────

fn file_rt(db_path: std::path::PathBuf) -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        db_path: Some(db_path),
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string(), "session".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
    })
    .expect("file-backed runtime")
}

fn build_registry(rt: KhiveRuntime) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(SessionPack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");
    rt.install_edge_rules(registry.all_edge_rules());
    registry
}

async fn store_session(registry: &VerbRegistry, content: &str) -> Value {
    registry
        .dispatch("session.store", json!({"content": content}))
        .await
        .expect("store ok")
}

// ── metadata tests ────────────────────────────────────────────────────────────

#[tokio::test]
async fn pack_metadata_matches_trait_consts() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("meta.db"));
    let registry = build_registry(rt);

    // The four session verbs are registered as internal subhandlers, not on
    // the agent-facing MCP surface, so they are absent from all_verbs() (which
    // is Visibility::Verb only) but report true from is_subhandler_verb and
    // remain dispatchable through the runtime registry directly.
    let surface: Vec<&str> = registry.all_verbs().iter().map(|v| v.name).collect();
    for verb in [
        "session.store",
        "session.list",
        "session.get",
        "session.export",
    ] {
        assert!(
            !surface.contains(&verb),
            "{verb} must NOT be on the agent-facing verb surface (subhandler)"
        );
        assert!(
            registry.is_subhandler_verb(verb),
            "{verb} must be registered as an internal subhandler"
        );
    }

    assert!(
        registry.all_note_kinds().contains(&"session"),
        "session not in note kinds"
    );
}

// ── session.store tests ───────────────────────────────────────────────────────

#[tokio::test]
async fn store_returns_session_envelope() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("store.db"));
    let registry = build_registry(rt);

    let result = registry
        .dispatch("session.store", json!({"content": "hello session"}))
        .await
        .expect("store ok");

    assert_eq!(result["kind"], "session");
    assert!(result["id"].as_str().is_some(), "id must be a string UUID");
    assert_eq!(result["content"], "hello session");
    assert!(
        result["created_at"].as_str().is_some(),
        "created_at present"
    );
}

#[tokio::test]
async fn store_with_agent_id_stored_in_properties() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("store_agent.db"));
    let registry = build_registry(rt);

    let result = registry
        .dispatch(
            "session.store",
            json!({"content": "agent session", "agent_id": "lambda:khive"}),
        )
        .await
        .expect("store ok");

    assert_eq!(result["agent_id"], "lambda:khive");
}

#[tokio::test]
async fn store_with_metadata_merged_into_properties() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("store_meta.db"));
    let registry = build_registry(rt);

    let result = registry
        .dispatch(
            "session.store",
            json!({
                "content": "metadata session",
                "metadata": {"source": "test", "version": 2}
            }),
        )
        .await
        .expect("store ok");

    assert_eq!(result["properties"]["source"], "test");
    assert_eq!(result["properties"]["version"], 2);
}

#[tokio::test]
async fn store_explicit_agent_id_wins_over_metadata() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("store_win.db"));
    let registry = build_registry(rt);

    let result = registry
        .dispatch(
            "session.store",
            json!({
                "content": "priority test",
                "agent_id": "explicit",
                "metadata": {"agent_id": "from_metadata"}
            }),
        )
        .await
        .expect("store ok");

    assert_eq!(result["agent_id"], "explicit", "explicit param must win");
}

#[tokio::test]
async fn store_empty_content_returns_error() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("store_empty.db"));
    let registry = build_registry(rt);

    let err = registry
        .dispatch("session.store", json!({"content": ""}))
        .await;
    assert!(err.is_err(), "empty content must return error");
}

#[tokio::test]
async fn store_unknown_field_returns_error() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("store_unknown.db"));
    let registry = build_registry(rt);

    let err = registry
        .dispatch(
            "session.store",
            json!({"content": "x", "unknown_field": true}),
        )
        .await;
    assert!(err.is_err(), "unknown field must be rejected");
}

#[tokio::test]
async fn store_metadata_non_object_returns_error() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("store_meta_bad.db"));
    let registry = build_registry(rt);

    let err = registry
        .dispatch(
            "session.store",
            json!({"content": "x", "metadata": "not-an-object"}),
        )
        .await;
    assert!(err.is_err(), "non-object metadata must be rejected");
}

// ── session.list tests ────────────────────────────────────────────────────────

#[tokio::test]
async fn list_returns_all_sessions_newest_first() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("list.db"));
    let registry = build_registry(rt);

    store_session(&registry, "first").await;
    // Small sleep to guarantee created_at ordering.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    store_session(&registry, "second").await;

    let all = registry
        .dispatch("session.list", json!({}))
        .await
        .expect("list ok");
    let arr = all.as_array().expect("list returns array");
    assert_eq!(arr.len(), 2);
    // Newest first: "second" must be at index 0.
    // The summary doesn't include content, but we can check created_at ordering.
    assert!(
        arr[0]["created_at"].as_str().unwrap() >= arr[1]["created_at"].as_str().unwrap(),
        "list must be newest-first"
    );
}

#[tokio::test]
async fn list_filter_by_agent_id() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("list_agent.db"));
    let registry = build_registry(rt);

    registry
        .dispatch(
            "session.store",
            json!({"content": "alpha session", "agent_id": "alpha"}),
        )
        .await
        .expect("store alpha");
    registry
        .dispatch(
            "session.store",
            json!({"content": "beta session", "agent_id": "beta"}),
        )
        .await
        .expect("store beta");

    let alpha_only = registry
        .dispatch("session.list", json!({"agent_id": "alpha"}))
        .await
        .expect("list alpha");
    let arr = alpha_only.as_array().expect("list returns array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["agent_id"], "alpha");
}

#[tokio::test]
async fn list_limit_respected() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("list_limit.db"));
    let registry = build_registry(rt);

    for i in 0..5 {
        store_session(&registry, &format!("session {i}")).await;
    }

    let limited = registry
        .dispatch("session.list", json!({"limit": 3}))
        .await
        .expect("list limited");
    assert_eq!(limited.as_array().expect("array").len(), 3);
}

#[tokio::test]
async fn list_since_filter() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("list_since.db"));
    let registry = build_registry(rt);

    store_session(&registry, "before").await;
    // Since filter: use a far-future timestamp that excludes the stored session.
    let future = "2099-01-01T00:00:00Z";
    let filtered = registry
        .dispatch("session.list", json!({"since": future}))
        .await
        .expect("list since ok");
    assert_eq!(
        filtered.as_array().expect("array").len(),
        0,
        "since=far-future must return empty"
    );

    // Use a past timestamp to confirm the session IS included.
    let past = "2020-01-01T00:00:00Z";
    let all = registry
        .dispatch("session.list", json!({"since": past}))
        .await
        .expect("list since past ok");
    assert_eq!(all.as_array().expect("array").len(), 1);
}

#[tokio::test]
async fn list_since_invalid_format_returns_error() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("list_since_bad.db"));
    let registry = build_registry(rt);

    let err = registry
        .dispatch("session.list", json!({"since": "not-a-date"}))
        .await;
    assert!(err.is_err(), "invalid since must return error");
}

// ── session.list offset pagination ───────────────────────────────────────────

#[tokio::test]
async fn list_offset_pagination() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("list_offset.db"));
    let registry = build_registry(rt);

    // Store 5 sessions with small delays to produce distinct created_at values.
    for i in 0..5 {
        store_session(&registry, &format!("session {i}")).await;
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    // Retrieve the full newest-first list.
    let all = registry
        .dispatch("session.list", json!({"limit": 5}))
        .await
        .expect("full list ok");
    let all_arr = all.as_array().expect("all array");
    assert_eq!(all_arr.len(), 5, "expected 5 sessions");

    // offset=2 limit=2 must return all[2] and all[3] in order.
    let paged = registry
        .dispatch("session.list", json!({"offset": 2, "limit": 2}))
        .await
        .expect("paged list ok");
    let paged_arr = paged.as_array().expect("paged array");
    assert_eq!(
        paged_arr.len(),
        2,
        "expected exactly 2 items with offset=2 limit=2"
    );
    assert_eq!(
        paged_arr[0]["id"], all_arr[2]["id"],
        "offset=2 first item must match all[2]"
    );
    assert_eq!(
        paged_arr[1]["id"], all_arr[3]["id"],
        "offset=2 second item must match all[3]"
    );
}

// ── session.get tests ──────────────────────────────────────────────────────

#[tokio::test]
async fn resume_returns_full_record() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("resume.db"));
    let registry = build_registry(rt);

    let stored = store_session(&registry, "resumeable content").await;
    let id = stored["id"].as_str().expect("id present").to_string();

    let resumed = registry
        .dispatch("session.get", json!({"id": id}))
        .await
        .expect("resume ok");

    assert_eq!(resumed["id"], id);
    assert_eq!(resumed["kind"], "session");
    assert_eq!(resumed["content"], "resumeable content");
    assert!(resumed["created_at"].as_str().is_some());
}

#[tokio::test]
async fn resume_not_found_returns_error() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("resume_404.db"));
    let registry = build_registry(rt);

    let err = registry
        .dispatch(
            "session.get",
            json!({"id": "00000000-0000-0000-0000-000000000001"}),
        )
        .await;
    assert!(err.is_err(), "missing session must return NotFound error");
}

#[tokio::test]
async fn resume_invalid_uuid_returns_error() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("resume_bad_id.db"));
    let registry = build_registry(rt);

    let err = registry
        .dispatch("session.get", json!({"id": "not-a-uuid"}))
        .await;
    assert!(err.is_err(), "invalid UUID must return error");
}

// ── soft-delete regression: resume ───────────────────────────────────────────

#[tokio::test]
async fn resume_soft_deleted_returns_error() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("resume_soft_del.db"));
    let registry = build_registry(rt);

    let stored = store_session(&registry, "soft-delete me").await;
    let id = stored["id"].as_str().expect("id present").to_string();

    // Soft-delete the session note via the KG delete verb.
    registry
        .dispatch("delete", json!({"id": id, "kind": "session"}))
        .await
        .expect("soft-delete ok");

    let err = registry.dispatch("session.get", json!({"id": id})).await;
    assert!(
        err.is_err(),
        "resume of a soft-deleted session must return an error"
    );
}

// ── soft-delete regression: export ───────────────────────────────────────────

#[tokio::test]
async fn export_soft_deleted_returns_error() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("export_soft_del.db"));
    let registry = build_registry(rt);

    let stored = store_session(&registry, "export-delete me").await;
    let id = stored["id"].as_str().expect("id present").to_string();

    // Soft-delete the session note via the KG delete verb.
    registry
        .dispatch("delete", json!({"id": id, "kind": "session"}))
        .await
        .expect("soft-delete ok");

    let err = registry.dispatch("session.export", json!({"id": id})).await;
    assert!(
        err.is_err(),
        "export of a soft-deleted session must return an error"
    );
}

// ── session.export tests ──────────────────────────────────────────────────────

#[tokio::test]
async fn export_json_format_returns_full_envelope() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("export_json.db"));
    let registry = build_registry(rt);

    let stored = store_session(&registry, "exportable content").await;
    let id = stored["id"].as_str().expect("id present").to_string();

    let exported = registry
        .dispatch("session.export", json!({"id": id}))
        .await
        .expect("export json ok");

    assert_eq!(exported["id"], id);
    assert_eq!(exported["kind"], "session");
    assert_eq!(exported["content"], "exportable content");
}

#[tokio::test]
async fn export_text_format_returns_content_string() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("export_text.db"));
    let registry = build_registry(rt);

    let stored = store_session(&registry, "text-only content").await;
    let id = stored["id"].as_str().expect("id present").to_string();

    let exported = registry
        .dispatch("session.export", json!({"id": id, "format": "text"}))
        .await
        .expect("export text ok");

    assert_eq!(
        exported.as_str().expect("text format returns string"),
        "text-only content"
    );
}

#[tokio::test]
async fn export_explicit_json_format() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("export_json_explicit.db"));
    let registry = build_registry(rt);

    let stored = store_session(&registry, "json explicit").await;
    let id = stored["id"].as_str().expect("id present").to_string();

    let exported = registry
        .dispatch("session.export", json!({"id": id, "format": "json"}))
        .await
        .expect("export json explicit ok");

    assert!(exported.is_object(), "json format must return object");
    assert_eq!(exported["content"], "json explicit");
}

#[tokio::test]
async fn export_invalid_format_returns_error() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("export_bad_fmt.db"));
    let registry = build_registry(rt);

    let stored = store_session(&registry, "x").await;
    let id = stored["id"].as_str().expect("id present").to_string();

    let err = registry
        .dispatch("session.export", json!({"id": id, "format": "yaml"}))
        .await;
    assert!(err.is_err(), "invalid format must return error");
}

#[tokio::test]
async fn export_not_found_returns_error() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("export_404.db"));
    let registry = build_registry(rt);

    let err = registry
        .dispatch(
            "session.export",
            json!({"id": "00000000-0000-0000-0000-000000000002"}),
        )
        .await;
    assert!(err.is_err(), "missing session must return error");
}
