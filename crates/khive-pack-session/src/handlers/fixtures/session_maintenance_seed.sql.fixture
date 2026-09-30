INSERT INTO sessions (id, provider_session_id, source, first_seen_at, last_seen_at, namespace)
VALUES ('fixture-session', 'fixture-session', 'codex', 1, 1, 'local');
INSERT INTO session_messages
    (id, session_id, seq, msg_type, text, raw, created_at, namespace, source, content_hash)
VALUES
    ('fixture-message', 'fixture-session', 1, 'message', 'hello', '{}', 1, 'local', 'codex', 'fixture-hash');
INSERT INTO session_mirror_cursor (file_path, session_id, byte_offset, updated_at)
VALUES ('fixture.jsonl', 'fixture-session', 1, 1);
