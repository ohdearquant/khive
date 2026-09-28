//! ADR-117a Amendment 1 acceptance tests for the session mirror's persisted identity.

use std::collections::HashMap;
use std::sync::Arc;

use khive_pack_kg::KgPack;
use khive_pack_session::mirror::ingest::{mirror_file, LineTailSource};
use khive_pack_session::SessionPack;
use khive_runtime::{
    AllowAllGate, BackendId, KhiveRuntime, Namespace, RuntimeConfig, RuntimeError, VerbRegistry,
    VerbRegistryBuilder,
};
use khive_storage::types::{SqlRow, SqlStatement, SqlValue};
use serde_json::json;
use tempfile::TempDir;

fn file_runtime(db_path: std::path::PathBuf) -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: khive_runtime::config::resolve_default_display_timezone(),
        events_split: None,
        db_path: Some(db_path),
        blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
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
        exec: Default::default(),
    })
    .expect("file-backed runtime")
}

fn registry(runtime: &KhiveRuntime) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(SessionPack::new(runtime.clone()));
    builder.build().expect("registry")
}

fn apply_pack_schema(runtime: &KhiveRuntime, registry: &VerbRegistry) {
    registry
        .apply_schema_plans_with_map(&HashMap::new(), runtime.backend())
        .expect("pack schema boot");
}

async fn rows(runtime: &KhiveRuntime, sql: &str) -> Vec<SqlRow> {
    let sql_access = runtime.sql();
    let mut reader = sql_access.reader().await.expect("SQL reader");
    reader
        .query_all(SqlStatement {
            sql: sql.to_string(),
            params: vec![],
            label: None,
        })
        .await
        .expect("query")
}

fn text<'a>(row: &'a SqlRow, column: &str) -> &'a str {
    match row.get(column) {
        Some(SqlValue::Text(value)) => value,
        value => panic!("expected text {column}, got {value:?}"),
    }
}

fn integer(row: &SqlRow, column: &str) -> i64 {
    match row.get(column) {
        Some(SqlValue::Integer(value)) => *value,
        value => panic!("expected integer {column}, got {value:?}"),
    }
}

#[tokio::test]
async fn equal_provider_and_event_ids_from_two_sources_both_survive_and_replay_is_idempotent() {
    let dir = TempDir::new().expect("tempdir");
    let runtime = file_runtime(dir.path().join("mirror.db"));
    let registry = registry(&runtime);
    apply_pack_schema(&runtime, &registry);

    let session_id = "shared-session";
    let event_id = "shared-session:0";
    let claude_path = dir.path().join("claude.jsonl");
    let codex_path = dir.path().join("codex.jsonl");
    let claude_line = json!({
        "uuid": event_id,
        "sessionId": session_id,
        "type": "user",
        "timestamp": "2026-09-23T10:00:00Z",
        "message": {"role": "user", "content": "claude collision marker"}
    });
    let codex_line = json!({
        "type": "response_item",
        "timestamp": "2026-09-23T10:00:01Z",
        "payload": {
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "codex collision marker"}]
        }
    });
    std::fs::write(&claude_path, format!("{claude_line}\n")).expect("Claude fixture");
    std::fs::write(&codex_path, format!("{codex_line}\n")).expect("Codex fixture");

    let claude = mirror_file(&runtime, &claude_path, 0, LineTailSource::ClaudeCode, None)
        .await
        .expect("Claude mirror");
    let codex = mirror_file(
        &runtime,
        &codex_path,
        0,
        LineTailSource::Codex,
        Some(session_id),
    )
    .await
    .expect("Codex mirror");
    assert_eq!(claude.inserted, 1);
    assert_eq!(
        codex.inserted, 1,
        "equal event IDs from two sources must both insert"
    );

    let sessions = rows(
        &runtime,
        "SELECT namespace, source, provider_session_id, message_count \
         FROM sessions WHERE provider_session_id='shared-session' ORDER BY source",
    )
    .await;
    assert_eq!(sessions.len(), 2, "source must be in the session key");
    assert_eq!(text(&sessions[0], "source"), "claude_code");
    assert_eq!(text(&sessions[1], "source"), "codex");
    for session in &sessions {
        assert_eq!(text(session, "namespace"), "local");
        assert_eq!(text(session, "provider_session_id"), session_id);
        assert_eq!(integer(session, "message_count"), 1);
    }

    let messages = rows(
        &runtime,
        "SELECT namespace, source, session_id, id, text, content_hash \
         FROM session_messages WHERE session_id='shared-session' ORDER BY source",
    )
    .await;
    assert_eq!(messages.len(), 2, "source must be in the event key");
    assert_eq!(text(&messages[0], "source"), "claude_code");
    assert_eq!(text(&messages[1], "source"), "codex");
    assert!(text(&messages[0], "text").contains("claude collision marker"));
    assert!(text(&messages[1], "text").contains("codex collision marker"));
    for message in &messages {
        assert_eq!(text(message, "namespace"), "local");
        assert_eq!(text(message, "session_id"), session_id);
        assert_eq!(text(message, "id"), event_id);
        assert!(!text(message, "content_hash").is_empty());
    }

    let replay_claude = mirror_file(&runtime, &claude_path, 0, LineTailSource::ClaudeCode, None)
        .await
        .expect("Claude replay");
    let replay_codex = mirror_file(
        &runtime,
        &codex_path,
        0,
        LineTailSource::Codex,
        Some(session_id),
    )
    .await
    .expect("Codex replay");
    assert_eq!(replay_claude.inserted, 0);
    assert_eq!(replay_codex.inserted, 0);
    assert_eq!(
        rows(
            &runtime,
            "SELECT id FROM session_messages WHERE session_id='shared-session'"
        )
        .await
        .len(),
        2
    );

    let changed_line = json!({
        "uuid": event_id,
        "sessionId": session_id,
        "type": "user",
        "timestamp": "2026-09-23T10:00:00Z",
        "message": {"role": "user", "content": "changed payload under same id"}
    });
    std::fs::write(&claude_path, format!("{changed_line}\n")).expect("changed replay fixture");
    let mismatch = mirror_file(&runtime, &claude_path, 0, LineTailSource::ClaudeCode, None)
        .await
        .expect("changed replay must not stall migration recovery");
    assert_eq!(mismatch.inserted, 0);
    assert_eq!(
        mismatch.replay_mismatches, 1,
        "changed replay must be reported"
    );
    let retained = rows(
        &runtime,
        "SELECT text FROM session_messages WHERE source='claude_code' AND id='shared-session:0'",
    )
    .await;
    assert_eq!(retained.len(), 1);
    assert!(text(&retained[0], "text").contains("claude collision marker"));
}

#[tokio::test]
async fn equal_ids_in_another_namespace_do_not_collide_with_local_rows() {
    let dir = TempDir::new().expect("tempdir");
    let runtime = file_runtime(dir.path().join("namespaces.db"));
    let registry = registry(&runtime);
    apply_pack_schema(&runtime, &registry);

    let sql_access = runtime.sql();
    let mut writer = sql_access.writer().await.expect("SQL writer");
    writer
        .execute_script(
            "INSERT INTO sessions \
             (id, provider_session_id, source, message_count, first_seen_at, last_seen_at, namespace) \
             VALUES \
             ('same-session', 'same-session', 'claude_code', 1, 1, 1, 'local'), \
             ('same-session', 'same-session', 'claude_code', 1, 1, 1, 'team:b'); \
             INSERT INTO session_messages \
             (id, session_id, source, seq, msg_type, text, raw, created_at, namespace, content_hash) \
             VALUES \
             ('same-event', 'same-session', 'claude_code', 0, 'user', 'local marker', '{}', 1, 'local', 'same-hash'), \
             ('same-event', 'same-session', 'claude_code', 0, 'user', 'tenant marker', '{}', 1, 'team:b', 'same-hash');"
                .to_string(),
        )
        .await
        .expect("equal IDs in two namespaces");
    drop(writer);

    let sessions = rows(
        &runtime,
        "SELECT namespace, source, provider_session_id FROM sessions ORDER BY namespace",
    )
    .await;
    let messages = rows(
        &runtime,
        "SELECT namespace, source, session_id, id FROM session_messages ORDER BY namespace",
    )
    .await;
    assert_eq!(sessions.len(), 2, "namespace must be in the session key");
    assert_eq!(messages.len(), 2, "namespace must be in the event key");
    for (session, message) in sessions.iter().zip(&messages) {
        assert_eq!(text(session, "namespace"), text(message, "namespace"));
        assert_eq!(text(session, "source"), "claude_code");
        assert_eq!(text(message, "source"), "claude_code");
        assert_eq!(text(session, "provider_session_id"), "same-session");
        assert_eq!(text(message, "session_id"), "same-session");
        assert_eq!(text(message, "id"), "same-event");
    }
}

#[tokio::test]
async fn legacy_migration_backfills_parent_source_and_keeps_orphans_as_unknown() {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("legacy.db");
    let runtime = file_runtime(db_path.clone());
    let migration = khive_db::migrations::MIGRATIONS
        .iter()
        .find(|step| step.name == "session_source_scoped_identity")
        .expect("session identity migration registered");
    let replay_path = dir.path().join("suppressed-claude.jsonl");
    let suppressed_line = json!({
        "uuid": "event-codex",
        "sessionId": "legacy-codex",
        "type": "user",
        "message": {"role": "user", "content": "recovered collision marker"}
    });
    let replay_bytes = format!("{suppressed_line}\n");
    std::fs::write(&replay_path, &replay_bytes).expect("suppressed source fixture");

    let sql_access = runtime.sql();
    let mut writer = sql_access.writer().await.expect("SQL writer");
    writer
        .execute_script(
            "DROP TABLE session_messages_fts; \
             DROP TABLE session_messages; \
             DROP TABLE sessions; \
             CREATE TABLE sessions ( \
                id TEXT PRIMARY KEY, provider_session_id TEXT NOT NULL, \
                source TEXT NOT NULL DEFAULT 'claude_code', cwd TEXT, git_branch TEXT, slug TEXT, \
                message_count INTEGER NOT NULL DEFAULT 0, first_seen_at INTEGER NOT NULL, \
                last_seen_at INTEGER NOT NULL, namespace TEXT); \
             CREATE TABLE session_messages ( \
                id TEXT PRIMARY KEY, session_id TEXT NOT NULL, seq INTEGER NOT NULL, \
                parent_uuid TEXT, is_sidechain INTEGER NOT NULL DEFAULT 0, role TEXT, \
                msg_type TEXT NOT NULL, text TEXT, raw TEXT NOT NULL, created_at INTEGER NOT NULL, \
                namespace TEXT); \
             CREATE TABLE session_mirror_cursor ( \
                file_path TEXT PRIMARY KEY, session_id TEXT, \
                byte_offset INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL); \
             INSERT INTO sessions \
                (id, provider_session_id, source, message_count, first_seen_at, last_seen_at, namespace) \
             VALUES \
                ('legacy-codex', 'legacy-codex', 'codex', 1, 1, 1, NULL), \
                ('legacy-chatgpt', 'legacy-chatgpt', 'chatgpt_export', 1, 2, 2, 'team:b'); \
             INSERT INTO session_messages \
                (id, session_id, seq, msg_type, text, raw, created_at, namespace) \
             VALUES \
                ('event-codex', 'legacy-codex', 0, 'response_item', 'codex marker', '{}', 1, NULL), \
                ('event-chatgpt', 'legacy-chatgpt', 0, 'message', 'chatgpt marker', '{}', 2, 'team:b'), \
                ('event-orphan', 'missing-parent', 0, 'user', 'orphan marker', '{}', 3, NULL);"
                .to_string(),
        )
        .await
        .expect("legacy mirror fixtures");
    writer
        .execute(SqlStatement {
            sql:
                "INSERT INTO session_mirror_cursor(file_path, session_id, byte_offset, updated_at) \
                  VALUES(?1, ?2, ?3, 1)"
                    .to_string(),
            params: vec![
                SqlValue::Text(replay_path.to_string_lossy().into_owned()),
                SqlValue::Text("legacy-codex".to_string()),
                SqlValue::Integer(replay_bytes.len() as i64),
            ],
            label: None,
        })
        .await
        .expect("old cursor past suppressed event");
    writer
        .execute(SqlStatement {
            sql: "DELETE FROM _schema_migrations WHERE version >= ?1".to_string(),
            params: vec![SqlValue::Integer(i64::from(migration.version))],
            label: None,
        })
        .await
        .expect("restore pre-migration ledger");
    drop(writer);
    let writer_task_join = runtime.backend().pool().take_writer_task_join();
    drop(sql_access);
    drop(runtime);
    if let Some(writer_task_join) = writer_task_join {
        writer_task_join
            .await
            .expect("fixture writer task shutdown");
    }

    let runtime = file_runtime(db_path);
    let registry = registry(&runtime);
    apply_pack_schema(&runtime, &registry);
    apply_pack_schema(&runtime, &registry);

    let sessions = rows(
        &runtime,
        "SELECT namespace, source, provider_session_id FROM sessions ORDER BY provider_session_id",
    )
    .await;
    assert_eq!(sessions.len(), 2);
    assert_eq!(text(&sessions[0], "provider_session_id"), "legacy-chatgpt");
    assert_eq!(text(&sessions[0], "namespace"), "team:b");
    assert_eq!(text(&sessions[0], "source"), "chatgpt_export");
    assert_eq!(text(&sessions[1], "provider_session_id"), "legacy-codex");
    assert_eq!(text(&sessions[1], "namespace"), "local");
    assert_eq!(text(&sessions[1], "source"), "codex");

    let messages = rows(
        &runtime,
        "SELECT id, namespace, source, session_id FROM session_messages ORDER BY id",
    )
    .await;
    assert_eq!(messages.len(), 3, "migration must retain every message");
    assert_eq!(text(&messages[0], "id"), "event-chatgpt");
    assert_eq!(text(&messages[0], "namespace"), "team:b");
    assert_eq!(text(&messages[0], "source"), "chatgpt_export");
    assert_eq!(text(&messages[1], "id"), "event-codex");
    assert_eq!(text(&messages[1], "namespace"), "local");
    assert_eq!(text(&messages[1], "source"), "codex");
    assert_eq!(text(&messages[2], "id"), "event-orphan");
    assert_eq!(text(&messages[2], "namespace"), "local");
    assert_eq!(
        text(&messages[2], "source"),
        "unknown",
        "migration orphan must keep explicit unknown source"
    );
    assert_eq!(text(&messages[2], "session_id"), "missing-parent");

    let indexed = rows(
        &runtime,
        "SELECT m.id FROM session_messages_fts \
         JOIN session_messages m ON m.mirror_rowid=session_messages_fts.rowid \
         WHERE session_messages_fts MATCH 'codex' AND m.namespace='local' AND m.source='codex'",
    )
    .await;
    assert_eq!(
        indexed.len(),
        1,
        "migration must rebuild FTS for retained rows"
    );
    let default_orphans = rows(
        &runtime,
        "SELECT m.id FROM session_messages_fts \
         JOIN session_messages m ON m.mirror_rowid=session_messages_fts.rowid \
         WHERE session_messages_fts MATCH 'orphan' AND m.namespace='local' AND m.source<>'unknown'",
    )
    .await;
    assert!(
        default_orphans.is_empty(),
        "default search excludes orphan source"
    );
    let explicit_orphans = rows(
        &runtime,
        "SELECT m.id FROM session_messages_fts \
         JOIN session_messages m ON m.mirror_rowid=session_messages_fts.rowid \
         WHERE session_messages_fts MATCH 'orphan' AND m.namespace='local' AND m.source='unknown'",
    )
    .await;
    assert_eq!(
        explicit_orphans.len(),
        1,
        "explicit unknown finds retained orphan"
    );
    assert_eq!(text(&explicit_orphans[0], "id"), "event-orphan");

    let audit = rows(
        &runtime,
        "SELECT session_rows, message_rows, orphan_rows, cursors_reset \
         FROM session_mirror_migration_audit WHERE name='adr117a_source_scoped_identity'",
    )
    .await;
    assert_eq!(audit.len(), 1, "migration must leave one durable report");
    assert_eq!(integer(&audit[0], "session_rows"), 2);
    assert_eq!(integer(&audit[0], "message_rows"), 3);
    assert_eq!(integer(&audit[0], "orphan_rows"), 1);
    assert_eq!(integer(&audit[0], "cursors_reset"), 1);

    let cursors = rows(
        &runtime,
        "SELECT byte_offset FROM session_mirror_cursor WHERE session_id='legacy-codex'",
    )
    .await;
    assert_eq!(cursors.len(), 1);
    assert_eq!(integer(&cursors[0], "byte_offset"), 0);

    let replay = mirror_file(&runtime, &replay_path, 0, LineTailSource::ClaudeCode, None)
        .await
        .expect("replay previously suppressed source");
    assert_eq!(replay.inserted, 1);
    assert_eq!(
        rows(
            &runtime,
            "SELECT source FROM sessions WHERE provider_session_id='legacy-codex' ORDER BY source"
        )
        .await
        .len(),
        2
    );
    assert_eq!(
        rows(
            &runtime,
            "SELECT source FROM session_messages WHERE id='event-codex' ORDER BY source"
        )
        .await
        .len(),
        2
    );
}

#[tokio::test]
async fn public_search_refuses_until_deletion_and_continuity_are_ready() {
    let dir = TempDir::new().expect("tempdir");
    let runtime = file_runtime(dir.path().join("search-gate.db"));
    let registry = registry(&runtime);
    apply_pack_schema(&runtime, &registry);

    let error = registry
        .dispatch("session.search", json!({"query": "any"}))
        .await
        .expect_err("transcript deletion and continuity support are required before public search");
    assert!(
        matches!(error, RuntimeError::Unconfigured(ref reason)
            if reason.contains("transcript deletion") && reason.contains("resume/export continuity")),
        "expected explicit dependency gate, got {error:?}"
    );
}
