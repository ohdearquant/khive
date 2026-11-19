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
