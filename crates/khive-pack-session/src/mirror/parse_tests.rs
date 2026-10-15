use super::*;

fn anthropic_api_key_fixture() -> String {
    format!("sk-ant-api03-{}AA", "A".repeat(93))
}

fn github_fine_grained_pat_fixture() -> String {
    format!("github_pat_{}", "A".repeat(82))
}

// Helper: build a minimal CC event JSON string.
fn make_line(uuid: &str, session_id: &str, type_: &str, extra: &str) -> String {
    format!(
        r#"{{"uuid":"{uuid}","sessionId":"{session_id}","type":"{type_}","timestamp":"2026-06-29T10:00:00Z"{extra}}}"#,
    )
}

#[test]
fn test_blank_line_returns_none() {
    assert!(parse_cc_line("").is_none());
    assert!(parse_cc_line("   ").is_none());
}

#[test]
fn test_no_uuid_returns_none() {
    // pr-link style: no uuid
    let line = r#"{"type":"pr-link","sessionId":"sess-1","url":"https://github.com/foo"}"#;
    assert!(parse_cc_line(line).is_none());
}

#[test]
fn test_no_session_id_returns_none() {
    let line = r#"{"uuid":"aaaa-bbbb","type":"user"}"#;
    assert!(parse_cc_line(line).is_none());
}

#[test]
fn test_user_text_line() {
    let line = make_line(
        "aaaa-bbbb",
        "sess-1111",
        "user",
        r#","message":{"role":"user","content":"Hello world"},"cwd":"/proj","gitBranch":"main","slug":"my-proj""#,
    );
    let ev = parse_cc_line(&line).expect("should parse");
    assert_eq!(ev.uuid, "aaaa-bbbb");
    assert_eq!(ev.session_id, "sess-1111");
    assert_eq!(ev.role.as_deref(), Some("user"));
    assert_eq!(ev.msg_type, "user");
    assert_eq!(ev.text.as_deref(), Some("Hello world"));
    assert_eq!(ev.cwd.as_deref(), Some("/proj"));
    assert_eq!(ev.git_branch.as_deref(), Some("main"));
    assert_eq!(ev.slug.as_deref(), Some("my-proj"));
    assert!(ev.created_at_micros > 0);
    assert!(!ev.is_sidechain);
}

#[test]
fn test_cc_line_masks_secret_bearing_slug() {
    let secret = format!("{}{}", "AKIA", "FAKEKEY1234567890");
    let line = make_line(
        "aaaa-bbbb",
        "sess-1111",
        "user",
        &format!(
            r#","message":{{"role":"user","content":"Hello world"}},"slug":"prod creds {secret}""#
        ),
    );
    let ev = parse_cc_line(&line).expect("should parse");
    let slug = ev.slug.as_deref().expect("slug");
    assert!(!slug.contains(&secret));
    assert!(slug.contains("***MASKED***"));
}

#[test]
fn test_cc_line_masks_secret_bearing_cwd_and_git_branch() {
    let marker = "CwdCredMarkerXYZ789";
    let cwd = format!("/repo?token=postgres://svc:{marker}q@internal-host.example.test/db");
    let branch = format!("feature/postgres://svc:{marker}q@internal-host.example.test/db");
    let line = make_line(
        "aaaa-cwd1",
        "sess-1111",
        "user",
        &format!(
            r#","message":{{"role":"user","content":"Hello world"}},"cwd":"{cwd}","gitBranch":"{branch}""#
        ),
    );
    let ev = parse_cc_line(&line).expect("should parse");
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
fn test_assistant_with_text_and_tool_use_blocks() {
    let line = r#"{"uuid":"cccc-dddd","sessionId":"sess-1111","type":"assistant","timestamp":"2026-06-29T10:01:00Z","message":{"role":"assistant","content":[{"type":"text","text":"I'll run a search."},{"type":"tool_use","name":"bash","input":{"command":"ls"}}]}}"#
            .to_string();
    let ev = parse_cc_line(&line).expect("should parse");
    assert_eq!(ev.role.as_deref(), Some("assistant"));
    let text = ev.text.expect("text should be present");
    assert!(text.contains("I'll run a search."), "text: {text}");
    assert!(text.contains("[tool_use: bash]"), "text: {text}");
    assert!(text.contains("command"), "text: {text}");
}

#[test]
fn test_tool_use_input_credential_masked_before_truncation() {
    // The credential's terminating `@` lands past the 500-character
    // display cap applied to a tool_use input block. A masker that only
    // sees the already-truncated prefix can never recognize the span, so
    // the password marker survives untouched in the stored `text`.
    let marker = "ToolUseCredMarkerXYZ789";
    let padding = "z".repeat(600);
    let password = format!("{marker}{padding}");
    let url = format!("postgres://svc:{password}@internal-host.example.com/db");
    let input = serde_json::json!({ "url": url });
    let serialized_input = serde_json::to_string(&input).unwrap();
    let at_offset = serialized_input
        .find('@')
        .expect("fixture must contain '@'");
    assert!(at_offset > 500);

    let content = serde_json::json!([
        { "type": "tool_use", "name": "bash", "input": input }
    ]);
    let message = serde_json::json!({
        "uuid": "tool-use-cred",
        "sessionId": "sess-cred",
        "type": "assistant",
        "timestamp": "2026-06-29T10:01:00Z",
        "message": { "role": "assistant", "content": content }
    });
    let line = message.to_string();

    let ev = parse_cc_line(&line).expect("should parse");
    let text = ev.text.expect("text should be present");
    assert!(
        !text.contains(marker),
        "no fragment of the credential may survive masking: {text}"
    );
    assert!(
        text.contains("***MASKED***"),
        "MASKED marker must appear in text: {text}"
    );
}

#[test]
fn test_tool_result_content_credential_masked_before_truncation() {
    // Same shape as the tool_use case above, but for a tool_result
    // content string whose credential terminator crosses the 500-char cut.
    let marker = "ToolResultCredMarkerXYZ789";
    let padding = "z".repeat(600);
    let password = format!("{marker}{padding}");
    let url = format!("postgres://svc:{password}@internal-host.example.com/db");
    let at_offset = url.find('@').expect("fixture must contain '@'");
    assert!(at_offset > 500);

    let content = serde_json::json!([
        { "type": "tool_result", "content": url }
    ]);
    let message = serde_json::json!({
        "uuid": "tool-result-cred",
        "sessionId": "sess-cred",
        "type": "user",
        "timestamp": "2026-06-29T10:02:00Z",
        "message": { "role": "user", "content": content }
    });
    let line = message.to_string();

    let ev = parse_cc_line(&line).expect("should parse");
    let text = ev.text.expect("text should be present");
    assert!(
        !text.contains(marker),
        "no fragment of the credential may survive masking: {text}"
    );
    assert!(
        text.contains("***MASKED***"),
        "MASKED marker must appear in text: {text}"
    );
}

#[test]
fn test_tool_result_block() {
    let line = r#"{"uuid":"eeee-ffff","sessionId":"sess-1111","type":"user","timestamp":"2026-06-29T10:02:00Z","message":{"role":"user","content":[{"type":"tool_result","content":"file1.rs\nfile2.rs"}]}}"#
            .to_string();
    let ev = parse_cc_line(&line).expect("should parse");
    let text = ev.text.expect("text should be present");
    assert!(text.contains("[tool_result]"), "text: {text}");
    assert!(text.contains("file1.rs"), "text: {text}");
}

#[test]
fn test_attachment_line_no_message() {
    // uuid present, sessionId present, but no message -> role/text None
    let line = r#"{"uuid":"gggg-hhhh","sessionId":"sess-1111","type":"attachment","timestamp":"2026-06-29T10:02:00Z","filename":"file.txt"}"#
            .to_string();
    let ev = parse_cc_line(&line).expect("should parse");
    assert_eq!(ev.msg_type, "attachment");
    assert!(ev.role.is_none());
    assert!(ev.text.is_none());
}

#[test]
fn test_secret_masking_in_text_and_raw() {
    let secret = anthropic_api_key_fixture();
    let line = format!(
        r#"{{"uuid":"iiii-jjjj","sessionId":"sess-1111","type":"user","timestamp":"2026-06-29T10:03:00Z","message":{{"role":"user","content":"my key is {secret}"}}}}"#
    );
    let ev = parse_cc_line(&line).expect("should parse");

    let text = ev.text.expect("text should be present");
    assert!(
        !text.contains(secret.as_str()),
        "secret must not appear in text: {text}"
    );
    assert!(
        text.contains("***MASKED***"),
        "MASKED marker must appear in text: {text}"
    );

    assert!(
        !ev.raw.contains(secret.as_str()),
        "secret must not appear in raw: {}",
        ev.raw
    );
    assert!(
        ev.raw.contains("***MASKED***"),
        "MASKED marker must appear in raw: {}",
        ev.raw
    );
}

#[test]
fn test_github_pat_masked() {
    let secret = github_fine_grained_pat_fixture();
    let line = format!(
        r#"{{"uuid":"kkkk-llll","sessionId":"sess-2","type":"user","timestamp":"2026-06-29T10:04:00Z","message":{{"role":"user","content":"token={secret}"}}}}"#
    );
    let ev = parse_cc_line(&line).unwrap();
    assert!(!ev.raw.contains(secret.as_str()));
    assert!(ev.raw.contains("***MASKED***"));
}

#[test]
fn test_timestamp_to_micros() {
    let line = make_line(
        "ts-test",
        "sess-ts",
        "system",
        r#","timestamp":"2026-06-29T17:56:01.123Z""#,
    );
    let ev = parse_cc_line(&line).unwrap();
    // 2026-06-29T17:56:01.123Z in micros should be a large positive number
    assert!(ev.created_at_micros > 0, "created_at_micros should be > 0");
}

#[test]
fn test_sidechain_flag() {
    let line = make_line("side-uuid", "sess-side", "user", r#","isSidechain":true"#);
    let ev = parse_cc_line(&line).unwrap();
    assert!(ev.is_sidechain);
}

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

// ── parse_claude_ai_export: direct unit tests ───────────────────────────

#[test]
fn test_claude_ai_export_preserves_ids_order_roles_and_title() {
    let export = serde_json::json!([{
        "uuid": "claude-conv-1",
        "name": "Claude Web Conversation",
        "created_at": "2026-07-31T10:00:00Z",
        "current_leaf_message_uuid": "claude-msg-2",
        "chat_messages": [
            {
                "uuid": "claude-msg-2",
                "sender": "assistant",
                "index": 1,
                "parent_message_uuid": "claude-msg-1",
                "created_at": "2026-07-31T10:00:02Z",
                "content": [
                    {"type": "thinking", "thinking": "not display text"},
                    {"type": "text", "text": "Hello from Claude"}
                ]
            },
            {
                "uuid": "claude-msg-1",
                "sender": "human",
                "index": 0,
                "parent_message_uuid": "00000000-0000-4000-8000-000000000000",
                "created_at": "2026-07-31T10:00:01Z",
                "content": [{"type": "text", "text": "Hello"}]
            }
        ]
    }]);

    let events = parse_claude_ai_export(&export.to_string()).expect("valid export");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].uuid, "claude-msg-1");
    assert_eq!(events[0].session_id, "claude-conv-1");
    assert_eq!(events[0].role.as_deref(), Some("user"));
    assert_eq!(events[0].text.as_deref(), Some("Hello"));
    assert_eq!(events[0].parent_uuid, None);
    assert_eq!(events[0].slug.as_deref(), Some("Claude Web Conversation"));
    assert!(!events[0].is_sidechain);

    assert_eq!(events[1].uuid, "claude-msg-2");
    assert_eq!(events[1].role.as_deref(), Some("assistant"));
    assert_eq!(events[1].text.as_deref(), Some("Hello from Claude"));
    assert_eq!(events[1].parent_uuid.as_deref(), Some("claude-msg-1"));
    assert!(!events[1].is_sidechain);
    assert!(events[1].created_at_micros > events[0].created_at_micros);
}

#[test]
fn test_claude_ai_export_preserves_branches_and_marks_inactive_path() {
    let export = serde_json::json!([{
        "uuid": "claude-conv-branch",
        "name": "Branching Claude Conversation",
        "current_leaf_message_uuid": "claude-main",
        "chat_messages": [
            {
                "uuid": "claude-user",
                "sender": "human",
                "index": 0,
                "parent_message_uuid": null,
                "content": [{"type": "text", "text": "Question"}]
            },
            {
                "uuid": "claude-main",
                "sender": "assistant",
                "index": 1,
                "parent_message_uuid": "claude-user",
                "content": [{"type": "text", "text": "Current answer"}]
            },
            {
                "uuid": "claude-alt",
                "sender": "assistant",
                "index": 2,
                "parent_message_uuid": "claude-user",
                "content": [{"type": "text", "text": "Alternate answer"}]
            }
        ]
    }]);

    let events = parse_claude_ai_export(&export.to_string()).expect("valid branch export");
    assert_eq!(events.len(), 3, "alternate branches must be retained");
    assert!(!events[0].is_sidechain);
    assert!(!events[1].is_sidechain);
    assert!(events[2].is_sidechain);
    assert_eq!(events[2].uuid, "claude-alt");
    assert_eq!(events[2].parent_uuid.as_deref(), Some("claude-user"));
}

#[test]
fn test_claude_ai_export_keeps_blocks_and_distinct_top_level_answer() {
    let export = serde_json::json!([{
        "uuid": "claude-conv-agent",
        "chat_messages": [
            {
                "uuid": "claude-agent-turn",
                "sender": "assistant",
                "text": "Final answer",
                "content": [
                    {"type": "tool_use", "name": "bash_tool", "input": {"command": "pwd"}},
                    {"type": "tool_result", "content": [{"type": "text", "text": "/tmp"}]}
                ]
            },
            {
                "uuid": "claude-deduplicated-text",
                "sender": "assistant",
                "text": "Already present",
                "content": [{"type": "text", "text": "Already present"}]
            }
        ]
    }]);

    let events = parse_claude_ai_export(&export.to_string()).expect("valid agent export");
    assert_eq!(events.len(), 2);
    let agent_text = events[0].text.as_deref().expect("agent turn text");
    assert!(agent_text.contains("[tool_use: bash_tool]"));
    assert!(agent_text.contains("[tool_result]"));
    assert!(agent_text.ends_with("Final answer"));
    assert_eq!(events[1].text.as_deref(), Some("Already present"));
}

#[test]
fn test_claude_ai_export_accepts_legacy_role_and_voice_note_text() {
    let export = serde_json::json!([{
        "uuid": "claude-conv-voice",
        "chat_messages": [{
            "uuid": "claude-voice-turn",
            "role": "human",
            "content": [{"type": "voice_note", "title": "Memo", "text": "Spoken prompt"}]
        }]
    }]);

    let events = parse_claude_ai_export(&export.to_string()).expect("valid legacy export");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].role.as_deref(), Some("user"));
    assert_eq!(events[0].text.as_deref(), Some("Spoken prompt"));
}

#[test]
fn test_claude_ai_export_legacy_flat_text_is_masked() {
    let secret = format!("{}{}", "AKIA", "FAKEKEY1234567890");
    let export = serde_json::json!([{
        "uuid": "claude-conv-flat",
        "summary": "Legacy export",
        "chat_messages": [
            {
                "uuid": "claude-flat-user",
                "sender": "human",
                "text": format!("key={secret}"),
                "created_at": "2026-07-31T10:00:00Z"
            },
            {
                "uuid": "claude-flat-assistant",
                "sender": "assistant",
                "text": "Legacy assistant text",
                "content": [{"type": "thinking", "thinking": "not visible"}],
                "created_at": "2026-07-31T10:00:01Z"
            }
        ]
    }]);

    let events = parse_claude_ai_export(&export.to_string()).expect("legacy export");
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|event| !event.is_sidechain));
    assert_eq!(events[0].slug.as_deref(), Some("Legacy export"));
    assert!(!events[0].text.as_deref().unwrap().contains(&secret));
    assert!(events[0].text.as_deref().unwrap().contains("***MASKED***"));
    assert!(!events[0].raw.contains(&secret));
    assert_eq!(events[1].text.as_deref(), Some("Legacy assistant text"));
}

#[test]
fn test_claude_ai_export_masks_secret_bearing_title_and_summary() {
    let secret = format!("{}{}", "AKIA", "FAKEKEY1234567890");
    let export = serde_json::json!([
        {
            "uuid": "claude-conv-secret-name",
            "name": format!("prod creds {secret}"),
            "chat_messages": [{
                "uuid": "claude-secret-name-user",
                "sender": "human",
                "text": "hello",
                "created_at": "2026-07-31T10:00:00Z"
            }]
        },
        {
            "uuid": "claude-conv-secret-summary",
            "summary": format!("key={secret}"),
            "chat_messages": []
        }
    ]);

    let parsed =
        parse_claude_ai_export_with_sessions(&export.to_string()).expect("secret-title export");
    assert_eq!(parsed.sessions.len(), 2);
    for session in &parsed.sessions {
        let slug = session.slug.as_deref().expect("session slug");
        assert!(!slug.contains(&secret));
        assert!(slug.contains("***MASKED***"));
    }
    assert_eq!(parsed.events.len(), 1);
    let event_slug = parsed.events[0].slug.as_deref().expect("event slug");
    assert!(!event_slug.contains(&secret));
    assert!(event_slug.contains("***MASKED***"));
}

#[test]
fn test_claude_ai_export_rejects_bad_top_level_and_skips_bad_conversations() {
    assert!(parse_claude_ai_export("not json").is_none());
    assert!(parse_claude_ai_export(r#"{"uuid":"not-an-array"}"#).is_none());

    let export = serde_json::json!([
        {"uuid": "missing-messages"},
        {
            "uuid": "claude-valid",
            "chat_messages": [{
                "uuid": "claude-valid-message",
                "sender": "human",
                "text": "kept"
            }]
        }
    ]);
    let events = parse_claude_ai_export(&export.to_string()).expect("valid array");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].session_id, "claude-valid");
}

#[test]
fn test_provider_export_parsers_reject_the_other_source_shape() {
    let claude_ai = serde_json::json!([{
        "uuid": "claude-conversation",
        "chat_messages": []
    }]);
    assert!(parse_chatgpt_export(&claude_ai.to_string()).is_none());

    let chatgpt = serde_json::json!([{
        "id": "chatgpt-conversation",
        "mapping": {}
    }]);
    assert!(parse_claude_ai_export(&chatgpt.to_string()).is_none());

    let mixed = serde_json::json!([
        {"uuid": "claude-conversation", "chat_messages": []},
        {"id": "chatgpt-conversation", "mapping": {}}
    ]);
    assert!(parse_chatgpt_export(&mixed.to_string()).is_none());
    assert!(parse_claude_ai_export(&mixed.to_string()).is_none());

    assert_eq!(parse_chatgpt_export("[]"), Some(Vec::new()));
    assert_eq!(parse_claude_ai_export("[]"), Some(Vec::new()));
}

// ── parse_chatgpt_export: direct unit tests ─────────────────────────────
//
// Unlike the end-to-end mirror_chatgpt_export_file tests in ingest.rs
// (which go through file I/O + SQL), these exercise the pure JSON-tree
// parsing seam directly with no runtime or DB setup.

#[test]
fn test_chatgpt_export_minimal_valid_export() {
    let export = serde_json::json!([{
        "id": "conv-min",
        "title": "Minimal Export",
        "current_node": "msg-assistant",
        "create_time": 1_700_000_000.0,
        "mapping": {
            "root": {
                "id": "root",
                "message": null,
                "parent": null,
                "children": ["msg-user"]
            },
            "msg-user": {
                "id": "msg-user",
                "parent": "root",
                "children": ["msg-assistant"],
                "message": {
                    "id": "msg-user",
                    "author": {"role": "user"},
                    "content": {"content_type": "text", "parts": ["Hello"]}
                }
            },
            "msg-assistant": {
                "id": "msg-assistant",
                "parent": "msg-user",
                "children": [],
                "message": {
                    "id": "msg-assistant",
                    "author": {"role": "assistant"},
                    "content": {"content_type": "text", "parts": ["Hi there"]}
                }
            }
        }
    }]);
    let content = serde_json::to_string(&export).unwrap();

    let events = parse_chatgpt_export(&content).expect("valid export must parse");
    assert_eq!(events.len(), 2, "root has no message, 2 nodes carry one");

    assert_eq!(events[0].uuid, "msg-user");
    assert_eq!(events[0].session_id, "conv-min");
    assert_eq!(events[0].role.as_deref(), Some("user"));
    assert_eq!(events[0].text.as_deref(), Some("Hello"));
    assert_eq!(events[0].parent_uuid, None, "root carries no message");
    assert!(!events[0].is_sidechain);
    assert_eq!(events[0].slug.as_deref(), Some("Minimal Export"));

    assert_eq!(events[1].uuid, "msg-assistant");
    assert_eq!(events[1].role.as_deref(), Some("assistant"));
    assert_eq!(events[1].text.as_deref(), Some("Hi there"));
    assert_eq!(events[1].parent_uuid.as_deref(), Some("msg-user"));
    assert!(!events[1].is_sidechain);
}

#[test]
fn test_chatgpt_export_masks_secret_bearing_title() {
    let secret = format!("{}{}", "AKIA", "FAKEKEY1234567890");
    let export = serde_json::json!([{
        "id": "conv-secret-title",
        "title": format!("prod creds {secret}"),
        "current_node": "msg-user",
        "create_time": 1_700_000_000.0,
        "mapping": {
            "root": {
                "id": "root",
                "message": null,
                "parent": null,
                "children": ["msg-user"]
            },
            "msg-user": {
                "id": "msg-user",
                "parent": "root",
                "children": [],
                "message": {
                    "id": "msg-user",
                    "author": {"role": "user"},
                    "content": {"content_type": "text", "parts": ["Hello"]}
                }
            }
        }
    }]);
    let content = serde_json::to_string(&export).unwrap();

    let events = parse_chatgpt_export(&content).expect("valid export must parse");
    assert_eq!(events.len(), 1);
    let slug = events[0].slug.as_deref().expect("event slug");
    assert!(!slug.contains(&secret));
    assert!(slug.contains("***MASKED***"));
}

#[test]
fn test_chatgpt_export_multi_branch_tree_dfs_order_and_sidechain() {
    // root -> user -> {main (current path), alt (regenerated/abandoned)}
    let export = serde_json::json!([{
        "id": "conv-branch",
        "title": "Branching Conversation",
        "current_node": "msg-main",
        "mapping": {
            "root": {
                "id": "root",
                "message": null,
                "parent": null,
                "children": ["msg-user"]
            },
            "msg-user": {
                "id": "msg-user",
                "parent": "root",
                "children": ["msg-main", "msg-alt"],
                "message": {
                    "id": "msg-user",
                    "author": {"role": "user"},
                    "content": {"content_type": "text", "parts": ["Question"]}
                }
            },
            "msg-main": {
                "id": "msg-main",
                "parent": "msg-user",
                "children": [],
                "message": {
                    "id": "msg-main",
                    "author": {"role": "assistant"},
                    "content": {"content_type": "text", "parts": ["Main answer"]}
                }
            },
            "msg-alt": {
                "id": "msg-alt",
                "parent": "msg-user",
                "children": [],
                "message": {
                    "id": "msg-alt",
                    "author": {"role": "assistant"},
                    "content": {"content_type": "text", "parts": ["Alternate answer"]}
                }
            }
        }
    }]);
    let content = serde_json::to_string(&export).unwrap();

    let events = parse_chatgpt_export(&content).expect("branch export must parse");
    assert_eq!(events.len(), 3);

    // DFS preorder must follow the mapping's `children` array order, not
    // JSON object key order: user, then main, then alt.
    let uuids: Vec<&str> = events.iter().map(|e| e.uuid.as_str()).collect();
    assert_eq!(uuids, vec!["msg-user", "msg-main", "msg-alt"]);

    let by_uuid = |id: &str| events.iter().find(|e| e.uuid == id).unwrap();
    assert!(!by_uuid("msg-user").is_sidechain);
    assert!(
        !by_uuid("msg-main").is_sidechain,
        "current_node's own path must not be flagged"
    );
    assert!(
        by_uuid("msg-alt").is_sidechain,
        "branch off current_node path must be flagged sidechain"
    );
    assert_eq!(by_uuid("msg-alt").text.as_deref(), Some("Alternate answer"));
}

#[test]
fn test_chatgpt_export_malformed_inputs() {
    // Top level not valid JSON at all -> None (caller must not advance cursor).
    assert!(parse_chatgpt_export("not json").is_none());

    // Valid JSON but not an array -> None.
    assert!(parse_chatgpt_export(r#"{"oops": "not an array"}"#).is_none());

    // Valid array, but individual malformed conversations (missing
    // mapping, missing id, non-object entry) must be skipped
    // individually rather than sinking the whole file.
    let export = serde_json::json!([
        {"id": "no-mapping"},
        {"mapping": {}},
        "not-an-object",
        {
            "id": "conv-good",
            "current_node": "msg-good",
            "mapping": {
                "root": {"id": "root", "message": null, "parent": null, "children": ["msg-good"]},
                "msg-good": {
                    "id": "msg-good",
                    "parent": "root",
                    "children": [],
                    "message": {
                        "id": "msg-good",
                        "author": {"role": "user"},
                        "content": {"content_type": "text", "parts": ["Still works"]}
                    }
                }
            }
        }
    ]);
    let content = serde_json::to_string(&export).unwrap();

    let events = parse_chatgpt_export(&content)
        .expect("array with some malformed conversations still parses");
    assert_eq!(
        events.len(),
        1,
        "malformed conversations skipped, valid one still yields its event"
    );
    assert_eq!(events[0].uuid, "msg-good");
    assert_eq!(events[0].session_id, "conv-good");
}
