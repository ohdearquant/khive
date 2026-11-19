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
