//! End-to-end integration tests for `khive-pack-session`.
//!
//! All tests use a file-backed runtime (TempDir + db_path: Some(path)).
//! In-memory runtimes are deliberately avoided: the ANN warm-path (ADR-079)
//! only writes v2 segments when the backend has a `data_dir`, so in-memory
//! runtimes silently skip persistence and can produce false negatives.

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

    let surface: Vec<&str> = registry.all_verbs().iter().map(|v| v.name).collect();
    for verb in [
        "session.store",
        "session.list",
        "session.resume",
        "session.export",
    ] {
        assert!(
            surface.contains(&verb),
            "{verb} must be on the agent-facing verb surface"
        );
        assert!(
            !registry.is_subhandler_verb(verb),
            "{verb} must not be a subhandler"
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
        .dispatch(
            "session.store",
            json!({
                "content": "hello session",
                "title": "My Session",
                "provider": "claude_code",
                "provider_session_id": "abc-123",
                "tags": ["a", "b"]
            }),
        )
        .await
        .expect("store ok");

    assert_eq!(result["ok"], true);
    let session = &result["session"];
    assert_eq!(session["kind"], "session");
    assert!(session["id"].as_str().is_some(), "id must be a string UUID");
    assert_eq!(session["content"], "hello session");
    assert_eq!(session["title"], "My Session");
    assert_eq!(session["provider"], "claude_code");
    assert_eq!(session["provider_session_id"], "abc-123");
    assert_eq!(session["tags"], json!(["a", "b"]));
    assert!(session["created_at"].as_str().is_some());
    assert!(session["updated_at"].as_str().is_some());
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
async fn store_missing_content_field_returns_error() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("store_missing_content.db"));
    let registry = build_registry(rt);

    let err = registry
        .dispatch("session.store", json!({"title": "no content field"}))
        .await;
    assert!(err.is_err(), "missing required content field must error");
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
async fn store_empty_optional_strings_and_tags_rejected() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("store_bad_optionals.db"));
    let registry = build_registry(rt);

    let title_err = registry
        .dispatch("session.store", json!({"content": "x", "title": "  "}))
        .await;
    assert!(title_err.is_err(), "blank title must be rejected");

    let provider_err = registry
        .dispatch("session.store", json!({"content": "x", "provider": ""}))
        .await;
    assert!(provider_err.is_err(), "empty provider must be rejected");

    let tags_err = registry
        .dispatch("session.store", json!({"content": "x", "tags": [""]}))
        .await;
    assert!(tags_err.is_err(), "empty tag entry must be rejected");
}

// ── session.list tests ────────────────────────────────────────────────────────

#[tokio::test]
async fn list_returns_all_sessions_newest_first_without_content() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("list.db"));
    let registry = build_registry(rt);

    store_session(&registry, "first").await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    store_session(&registry, "second").await;

    let result = registry
        .dispatch("session.list", json!({}))
        .await
        .expect("list ok");
    let sessions = result["sessions"].as_array().expect("sessions array");
    assert_eq!(sessions.len(), 2);
    assert_eq!(result["count"], 2);
    assert!(
        sessions[0]["created_at"].as_str().unwrap() >= sessions[1]["created_at"].as_str().unwrap(),
        "list must be newest-first"
    );
    assert!(
        sessions[0].get("content").is_none(),
        "summaries must not include content"
    );
}

#[tokio::test]
async fn list_filter_by_provider() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("list_provider.db"));
    let registry = build_registry(rt);

    registry
        .dispatch(
            "session.store",
            json!({"content": "alpha session", "provider": "codex"}),
        )
        .await
        .expect("store alpha");
    registry
        .dispatch(
            "session.store",
            json!({"content": "beta session", "provider": "claude_code"}),
        )
        .await
        .expect("store beta");

    let codex_only = registry
        .dispatch("session.list", json!({"provider": "codex"}))
        .await
        .expect("list codex");
    let arr = codex_only["sessions"].as_array().expect("array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["provider"], "codex");
}

#[tokio::test]
async fn list_filter_by_unknown_provider_returns_empty() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("list_unknown_provider.db"));
    let registry = build_registry(rt);

    registry
        .dispatch(
            "session.store",
            json!({"content": "alpha session", "provider": "codex"}),
        )
        .await
        .expect("store alpha");

    let result = registry
        .dispatch("session.list", json!({"provider": "nonexistent_provider"}))
        .await
        .expect("list with unknown provider filter must still return ok, not error");
    let arr = result["sessions"].as_array().expect("sessions array");
    assert_eq!(
        arr.len(),
        0,
        "unknown provider filter must yield no matches"
    );
    assert_eq!(result["count"], 0);
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
    assert_eq!(limited["sessions"].as_array().expect("array").len(), 3);
    assert_eq!(limited["limit"], 3);
}

#[tokio::test]
async fn list_rejects_out_of_range_limit() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("list_bad_limit.db"));
    let registry = build_registry(rt);

    let zero = registry.dispatch("session.list", json!({"limit": 0})).await;
    assert!(zero.is_err(), "limit=0 must be rejected");
    let too_big = registry
        .dispatch("session.list", json!({"limit": 201}))
        .await;
    assert!(too_big.is_err(), "limit=201 must be rejected");
}

#[tokio::test]
async fn list_offset_pagination() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("list_offset.db"));
    let registry = build_registry(rt);

    for i in 0..5 {
        store_session(&registry, &format!("session {i}")).await;
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    let all = registry
        .dispatch("session.list", json!({"limit": 5}))
        .await
        .expect("full list ok");
    let all_arr = all["sessions"].as_array().expect("all array");
    assert_eq!(all_arr.len(), 5, "expected 5 sessions");

    let paged = registry
        .dispatch("session.list", json!({"offset": 2, "limit": 2}))
        .await
        .expect("paged list ok");
    let paged_arr = paged["sessions"].as_array().expect("paged array");
    assert_eq!(paged_arr.len(), 2, "expected exactly 2 items");
    assert_eq!(paged_arr[0]["id"], all_arr[2]["id"]);
    assert_eq!(paged_arr[1]["id"], all_arr[3]["id"]);
}

// ── session.resume tests ───────────────────────────────────────────────────────

#[tokio::test]
async fn resume_by_full_uuid_returns_exact_content() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("resume.db"));
    let registry = build_registry(rt);

    let stored = store_session(&registry, "resumeable content").await;
    let id = stored["session"]["id"]
        .as_str()
        .expect("id present")
        .to_string();

    let resumed = registry
        .dispatch("session.resume", json!({"id": id}))
        .await
        .expect("resume ok");

    assert_eq!(resumed["ok"], true);
    assert_eq!(resumed["session"]["id"], id);
    assert_eq!(resumed["session"]["kind"], "session");
    assert_eq!(resumed["session"]["content"], "resumeable content");
}

#[tokio::test]
async fn resume_by_short_prefix_returns_same_note() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("resume_prefix.db"));
    let registry = build_registry(rt);

    let stored = store_session(&registry, "prefix content").await;
    let id = stored["session"]["id"]
        .as_str()
        .expect("id present")
        .to_string();
    let prefix = &id.replace('-', "")[..8];

    let resumed = registry
        .dispatch("session.resume", json!({"id": prefix}))
        .await
        .expect("resume by prefix ok");

    assert_eq!(resumed["session"]["id"], id);
    assert_eq!(resumed["session"]["content"], "prefix content");
}

#[tokio::test]
async fn resume_rejects_non_uuid_non_prefix() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("resume_bad_id.db"));
    let registry = build_registry(rt);

    let err = registry
        .dispatch("session.resume", json!({"id": "abc"}))
        .await;
    assert!(err.is_err(), "short non-hex id must be rejected");
}

#[tokio::test]
async fn resume_rejects_well_formed_but_nonexistent_uuid() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("resume_missing_uuid.db"));
    let registry = build_registry(rt);

    let err = registry
        .dispatch(
            "session.resume",
            json!({"id": "00000000-0000-0000-0000-000000000000"}),
        )
        .await;
    assert!(
        err.is_err(),
        "well-formed UUID with no matching record must error"
    );
}

#[tokio::test]
async fn resume_rejects_hex_prefix_matching_nothing() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("resume_bad_prefix.db"));
    let registry = build_registry(rt);

    let err = registry
        .dispatch("session.resume", json!({"id": "deadbeef"}))
        .await;
    assert!(err.is_err(), "8+ hex prefix matching no records must error");
}

#[tokio::test]
async fn resume_rejects_non_session_note() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("resume_wrong_kind.db"));
    let registry = build_registry(rt);

    let observation = registry
        .dispatch(
            "create",
            json!({"kind": "observation", "content": "not a session", "name": "obs"}),
        )
        .await
        .expect("create observation ok");
    let observation_id = observation["id"]
        .as_str()
        .expect("observation id")
        .to_string();

    let err = registry
        .dispatch("session.resume", json!({"id": observation_id}))
        .await;
    assert!(err.is_err(), "non-session note must be rejected");
}

// ── session.export tests ──────────────────────────────────────────────────────

#[tokio::test]
async fn export_default_json() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("export_json.db"));
    let registry = build_registry(rt);

    let stored = store_session(&registry, "exportable content").await;
    let id = stored["session"]["id"]
        .as_str()
        .expect("id present")
        .to_string();

    let exported = registry
        .dispatch("session.export", json!({"id": id}))
        .await
        .expect("export ok");

    assert_eq!(exported["ok"], true);
    assert_eq!(exported["format"], "json");
    assert_eq!(exported["session"]["content"], "exportable content");
}

#[tokio::test]
async fn export_markdown_contains_metadata_and_content() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("export_md.db"));
    let registry = build_registry(rt);

    let stored = registry
        .dispatch(
            "session.store",
            json!({"content": "md body", "title": "MD Title", "provider": "codex"}),
        )
        .await
        .expect("store ok");
    let id = stored["session"]["id"]
        .as_str()
        .expect("id present")
        .to_string();

    let exported = registry
        .dispatch("session.export", json!({"id": id, "format": "markdown"}))
        .await
        .expect("export markdown ok");

    assert_eq!(exported["ok"], true);
    assert_eq!(exported["format"], "markdown");
    let content = exported["content"].as_str().expect("content string");
    assert!(content.starts_with("# MD Title"));
    assert!(content.contains("- provider: codex"));
    assert!(content.contains("## Content"));
    assert!(content.contains("md body"));
}

#[tokio::test]
async fn export_rejects_invalid_format() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("export_bad_format.db"));
    let registry = build_registry(rt);

    let stored = store_session(&registry, "content").await;
    let id = stored["session"]["id"]
        .as_str()
        .expect("id present")
        .to_string();

    let err = registry
        .dispatch("session.export", json!({"id": id, "format": "text"}))
        .await;
    assert!(err.is_err(), "format=text must be rejected");
}

#[tokio::test]
async fn export_rejects_missing_id() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("export_missing_id.db"));
    let registry = build_registry(rt);

    let err = registry
        .dispatch("session.export", json!({"format": "json"}))
        .await;
    assert!(err.is_err(), "missing required id field must error");
}

// ── round-trip regression ────────────────────────────────────────────────────

#[tokio::test]
async fn round_trip_store_resume_list_export() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("round_trip.db"));
    let registry = build_registry(rt);

    let stored = registry
        .dispatch(
            "session.store",
            json!({
                "content": "round trip content",
                "provider": "codex",
                "provider_session_id": "codex-session-1"
            }),
        )
        .await
        .expect("store ok");
    let id = stored["session"]["id"]
        .as_str()
        .expect("id present")
        .to_string();

    let resumed = registry
        .dispatch("session.resume", json!({"id": id}))
        .await
        .expect("resume ok");
    assert_eq!(resumed["session"]["content"], "round trip content");

    let listed = registry
        .dispatch("session.list", json!({}))
        .await
        .expect("list ok");
    let ids: Vec<&str> = listed["sessions"]
        .as_array()
        .expect("sessions array")
        .iter()
        .map(|s| s["id"].as_str().expect("id"))
        .collect();
    assert!(ids.contains(&id.as_str()));

    let exported = registry
        .dispatch("session.export", json!({"id": id}))
        .await
        .expect("export ok");
    assert!(
        exported["session"].is_object(),
        "export json must parse as an object"
    );
}
