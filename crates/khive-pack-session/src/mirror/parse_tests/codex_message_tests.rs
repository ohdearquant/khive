// ── parse_codex_line tests ─────────────────────────────────────────────────

const CDX_SID: &str = "cdx-session-0001-0001-0001-000000000001";

/// Build a Codex user message line using the real `input_text` block shape.
fn codex_user_msg(text: &str) -> String {
    format!(
        r#"{{"type":"response_item","timestamp":"2026-06-30T09:00:00Z","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"{text}"}}]}}}}"#
    )
}

/// Build a Codex assistant message line using the real `output_text` block shape.
fn codex_asst_msg(text: &str) -> String {
    format!(
        r#"{{"type":"response_item","timestamp":"2026-06-30T09:00:00Z","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"{text}"}}]}}}}"#
    )
}

/// Build a Codex session_meta line.
fn codex_meta(cwd: &str, branch: &str) -> String {
    format!(
        r#"{{"type":"session_meta","timestamp":"2026-06-30T09:00:00Z","payload":{{"id":"{CDX_SID}","cwd":"{cwd}","git":{{"branch":"{branch}","commit_hash":"abc123","repository_url":"https://github.com/example/repo"}}}}}}"#
    )
}

/// Build a Codex response_item with a tool_use block (no text block).
fn codex_tool_use_msg() -> String {
    r#"{"type":"response_item","timestamp":"2026-06-30T09:01:00Z","payload":{"type":"message","role":"assistant","content":[{"type":"tool_use","name":"bash","input":{"command":"cargo test"}}]}}"#.to_string()
}

/// Build a Codex event_msg line (duplicate; must be skipped).
fn codex_event_msg() -> String {
    r#"{"type":"event_msg","timestamp":"2026-06-30T09:00:00Z","payload":{"type":"user_message","content":"duplicate"}}"#.to_string()
}

#[test]
fn test_codex_blank_returns_none() {
    assert!(parse_codex_line("", CDX_SID, 0).is_none());
    assert!(parse_codex_line("   ", CDX_SID, 0).is_none());
}

#[test]
fn test_codex_event_msg_skipped() {
    let line = codex_event_msg();
    assert!(
        parse_codex_line(&line, CDX_SID, 42).is_none(),
        "event_msg must be skipped"
    );
}

#[test]
fn test_codex_unknown_type_skipped() {
    let line = r#"{"type":"some_other_event","timestamp":"2026-06-30T09:00:00Z","payload":{}}"#;
    assert!(parse_codex_line(line, CDX_SID, 0).is_none());
}

#[test]
fn test_codex_session_meta_produces_event() {
    let line = codex_meta("/workspace/proj", "main");
    let ev = parse_codex_line(&line, CDX_SID, 0).expect("session_meta should parse");
    assert_eq!(ev.session_id, CDX_SID);
    assert_eq!(ev.msg_type, "session_meta");
    assert_eq!(ev.cwd.as_deref(), Some("/workspace/proj"));
    assert_eq!(ev.git_branch.as_deref(), Some("main"));
    assert!(ev.role.is_none());
    assert!(ev.text.is_none());
    // Synthetic uuid: "{session_id}:{offset}".
    assert_eq!(ev.uuid, format!("{CDX_SID}:0"));
    assert!(ev.created_at_micros > 0);
}

#[test]
fn test_codex_session_meta_masks_secret_bearing_cwd_and_branch() {
    let marker = "CodexCwdCredMarkerXYZ789";
    let cwd = format!("/repo?token=postgres://svc:{marker}q@internal-host.example.test/db");
    let branch = format!("feature/postgres://svc:{marker}q@internal-host.example.test/db");
    let line = codex_meta(&cwd, &branch);
    let ev = parse_codex_line(&line, CDX_SID, 0).expect("session_meta should parse");
    let stored_cwd = ev.cwd.as_deref().expect("cwd");
    let stored_branch = ev.git_branch.as_deref().expect("git_branch");
    assert!(!stored_cwd.contains(marker), "cwd: {stored_cwd}");
    assert!(stored_cwd.contains("***MASKED***"), "cwd: {stored_cwd}");
    assert!(
        !stored_branch.contains(marker),
        "git_branch: {stored_branch}"
    );
    assert!(
        stored_branch.contains("***MASKED***"),
        "git_branch: {stored_branch}"
    );
}

#[test]
fn test_codex_user_message_input_text_block() {
    // Regression: real Codex user messages use `input_text` blocks.
    let line = codex_user_msg("Hello Codex");
    let ev = parse_codex_line(&line, CDX_SID, 128).expect("user message should parse");
    assert_eq!(ev.session_id, CDX_SID);
    assert_eq!(ev.msg_type, "response_item");
    assert_eq!(ev.role.as_deref(), Some("user"));
    // text must NOT be None — this was the NULL bug.
    let text = ev.text.expect("text must be non-NULL for input_text block");
    assert_eq!(text, "Hello Codex");
    assert_eq!(ev.uuid, format!("{CDX_SID}:128"));
}

#[test]
fn test_codex_assistant_message_output_text_block() {
    // Regression: real Codex assistant messages use `output_text` blocks.
    let line = codex_asst_msg("Hello from assistant");
    let ev = parse_codex_line(&line, CDX_SID, 256).expect("assistant message should parse");
    assert_eq!(ev.role.as_deref(), Some("assistant"));
    // text must NOT be None.
    let text = ev
        .text
        .expect("text must be non-NULL for output_text block");
    assert_eq!(text, "Hello from assistant");
    assert_eq!(ev.uuid, format!("{CDX_SID}:256"));
}

#[test]
fn test_codex_tool_use_block_extracted() {
    let line = codex_tool_use_msg();
    let ev = parse_codex_line(&line, CDX_SID, 512).expect("tool_use message should parse");
    assert_eq!(ev.role.as_deref(), Some("assistant"));
    let text = ev.text.expect("text must be present");
    assert!(text.contains("[tool_use: bash]"), "text: {text}");
    assert!(text.contains("cargo test"), "text: {text}");
}

#[test]
fn test_codex_text_truncated_at_500_chars() {
    // input_text block with 600-char body — must be truncated.
    let long_text = "x".repeat(600);
    let line = codex_user_msg(&long_text);
    let ev = parse_codex_line(&line, CDX_SID, 0).expect("should parse");
    let text = ev.text.expect("text must be present");
    // char count must be ≤ 501 (500 + the '…' ellipsis char).
    assert!(
        text.chars().count() <= 501,
        "text must be truncated: len={}",
        text.chars().count()
    );
    assert!(text.ends_with('…'), "truncated text must end with ellipsis");
}

#[test]
fn test_codex_secret_masked_in_text_and_raw() {
    // input_text block carrying a secret — masking must apply to both text and raw.
    let secret = anthropic_api_key_fixture();
    let line = codex_user_msg(&secret);
    let ev = parse_codex_line(&line, CDX_SID, 0).expect("should parse");
    let text = ev.text.expect("text present");
    assert!(
        !text.contains(secret.as_str()),
        "secret must not appear in text"
    );
    assert!(
        text.contains("***MASKED***"),
        "MASKED marker must appear in text"
    );
    assert!(
        !ev.raw.contains(secret.as_str()),
        "secret must not appear in raw"
    );
    assert!(
        ev.raw.contains("***MASKED***"),
        "MASKED marker must appear in raw"
    );
}

#[test]
fn test_codex_synthetic_uuid_stable_across_calls() {
    // The same line at the same offset must produce the same uuid regardless
    // of how many times it is called (deterministic, no random component).
    let line = codex_user_msg("consistency");
    let ev1 = parse_codex_line(&line, CDX_SID, 999).unwrap();
    let ev2 = parse_codex_line(&line, CDX_SID, 999).unwrap();
    assert_eq!(ev1.uuid, ev2.uuid);
    assert_eq!(ev1.uuid, format!("{CDX_SID}:999"));
}

#[test]
fn test_codex_response_item_non_message_payload_skipped() {
    // A response_item where payload.type != "message" must be skipped.
    let line = r#"{"type":"response_item","timestamp":"2026-06-30T09:00:00Z","payload":{"type":"tool_call","name":"some_tool"}}"#;
    assert!(parse_codex_line(line, CDX_SID, 0).is_none());
}

#[test]
fn test_cc_text_block_still_works() {
    // Regression guard: adding input_text/output_text must not break the
    // existing CC "text" block handling.
    let line = r#"{"uuid":"cc-t1","sessionId":"cc-sess","type":"assistant","timestamp":"2026-06-30T09:00:00Z","message":{"role":"assistant","content":[{"type":"text","text":"CC still works"}]}}"#;
    let ev = parse_cc_line(line).expect("CC text block must parse");
    assert_eq!(ev.text.as_deref(), Some("CC still works"));
}
