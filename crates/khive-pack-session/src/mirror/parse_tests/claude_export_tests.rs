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
