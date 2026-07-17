//! JSONL line parsers for Claude Code and Codex CLI session transcripts.
//!
//! Every function here is deterministic and side-effect-free so the unit tests
//! can run without any runtime or DB setup.

use std::collections::HashSet;

use chrono::DateTime;
use khive_runtime::secret_gate;
use serde_json::{Map, Value};

/// A single parsed event, source-agnostic.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedEvent {
    /// Event UUID — the primary key for idempotency.
    ///
    /// For Claude Code events this is the top-level `uuid` field.
    /// For Codex events (which carry no per-message uuid) this is synthesised
    /// as `"{session_id}:{abs_byte_offset}"`.
    /// For ChatGPT export events this is the mapping node's `message.id`.
    pub uuid: String,
    /// Session UUID.
    pub session_id: String,
    /// Parent event UUID if present.
    pub parent_uuid: Option<String>,
    /// Whether this event is on a sidechain.
    pub is_sidechain: bool,
    /// `message.role` (CC) or `payload.role` (Codex) when present.
    pub role: Option<String>,
    /// Top-level `type` field.
    pub msg_type: String,
    /// Extracted display text, secrets masked; `None` for non-message events.
    pub text: Option<String>,
    /// Full original line with secrets masked.
    pub raw: String,
    /// `timestamp` as microseconds since the Unix epoch; 0 if absent or unparseable.
    pub created_at_micros: i64,
    /// `cwd` if present.
    pub cwd: Option<String>,
    /// `gitBranch` (CC) or `payload.git.branch` (Codex) if present.
    pub git_branch: Option<String>,
    /// `slug` if present (CC: project slug; ChatGPT export: conversation
    /// title; Codex files carry no slug concept).
    pub slug: Option<String>,
}

/// Parse one Claude Code JSONL line.
///
/// Returns `None` for:
/// - blank or whitespace-only lines
/// - lines that are not valid JSON objects
/// - lines that lack a top-level `uuid` field
/// - lines that lack a top-level `sessionId` field
///
/// The returned `raw` and `text` fields have secrets masked.
pub fn parse_cc_line(line: &str) -> Option<ParsedEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    let obj: Value = serde_json::from_str(trimmed).ok()?;
    let map = obj.as_object()?;

    // Both uuid and sessionId are required for idempotency and routing.
    let uuid = map.get("uuid")?.as_str()?.to_string();
    if uuid.is_empty() {
        return None;
    }
    let session_id = map.get("sessionId")?.as_str()?.to_string();
    if session_id.is_empty() {
        return None;
    }

    let parent_uuid = map
        .get("parentUuid")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let is_sidechain = map
        .get("isSidechain")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let msg_type = map
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let cwd = map.get("cwd").and_then(|v| v.as_str()).map(str::to_string);

    let git_branch = map
        .get("gitBranch")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let slug = map.get("slug").and_then(|v| v.as_str()).map(str::to_string);

    let created_at_micros = map
        .get("timestamp")
        .and_then(|v| v.as_str())
        .and_then(|ts| DateTime::parse_from_rfc3339(ts).ok())
        .map(|dt| dt.timestamp_micros())
        .unwrap_or(0);

    // Extract role and text from message when present.
    let (role, text) = match map.get("message").and_then(|m| m.as_object()) {
        None => (None, None),
        Some(msg) => {
            let role = msg.get("role").and_then(|v| v.as_str()).map(str::to_string);
            let text = extract_text(msg.get("content"));
            (role, text)
        }
    };

    // Apply masking to the raw line and the extracted text, reusing the
    // canonical write-time secret detector (khive-runtime) — never a second,
    // weaker masker.
    let raw = secret_gate::mask_secrets(trimmed).into_owned();
    let text = text.map(|t| secret_gate::mask_secrets(&t).into_owned());

    Some(ParsedEvent {
        uuid,
        session_id,
        parent_uuid,
        is_sidechain,
        role,
        msg_type,
        text,
        raw,
        created_at_micros,
        cwd,
        git_branch,
        slug,
    })
}

/// Parse one Codex CLI JSONL line.
///
/// `session_id` must be derived from the filename before calling this function
/// (e.g. from `rollout-<timestamp>-<uuid>.jsonl`).  `abs_byte_offset` is the
/// file byte offset of the **start** of this line; it is embedded in the
/// synthesised event UUID so that `INSERT OR IGNORE` on `session_messages.id`
/// is idempotent across re-tails of an append-only file.
///
/// Returns `None` for:
/// - blank or whitespace-only lines
/// - lines that are not valid JSON objects
/// - lines whose top-level `type` is `"event_msg"` — these are duplicate
///   event-stream representations of messages and must not be double-stored
/// - any other line type that carries no useful message content
///
/// The returned `raw` and `text` fields have secrets masked.
pub fn parse_codex_line(line: &str, session_id: &str, abs_byte_offset: u64) -> Option<ParsedEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    let obj: Value = serde_json::from_str(trimmed).ok()?;
    let map = obj.as_object()?;

    let line_type = map.get("type")?.as_str()?;

    // event_msg lines are duplicate event-stream representations — skip them.
    if line_type == "event_msg" {
        return None;
    }

    let created_at_micros = map
        .get("timestamp")
        .and_then(|v| v.as_str())
        .and_then(|ts| DateTime::parse_from_rfc3339(ts).ok())
        .map(|dt| dt.timestamp_micros())
        .unwrap_or(0);

    match line_type {
        "session_meta" => {
            // session_meta carries cwd and git metadata; the session UUID in
            // payload.id should match the filename-derived session_id, but we
            // do NOT use it as the event id — use the synthesised offset key so
            // the message row is unique and idempotent.
            let payload = map.get("payload").and_then(|v| v.as_object());

            let cwd = payload
                .and_then(|p| p.get("cwd"))
                .and_then(|v| v.as_str())
                .map(str::to_string);

            let git_branch = payload
                .and_then(|p| p.get("git"))
                .and_then(|g| g.as_object())
                .and_then(|g| g.get("branch"))
                .and_then(|v| v.as_str())
                .map(str::to_string);

            let uuid = format!("{session_id}:{abs_byte_offset}");
            let raw = secret_gate::mask_secrets(trimmed).into_owned();

            Some(ParsedEvent {
                uuid,
                session_id: session_id.to_string(),
                parent_uuid: None,
                is_sidechain: false,
                role: None,
                msg_type: "session_meta".to_string(),
                text: None,
                raw,
                created_at_micros,
                cwd,
                git_branch,
                slug: None,
            })
        }
        "response_item" => {
            let payload = map.get("payload").and_then(|v| v.as_object())?;

            // Only ingest message items; skip tool_call, completion, etc.
            if payload.get("type").and_then(|v| v.as_str()) != Some("message") {
                return None;
            }

            let role = payload
                .get("role")
                .and_then(|v| v.as_str())
                .map(str::to_string);

            let text = extract_text(payload.get("content"));
            let text = text.map(|t| {
                let masked = secret_gate::mask_secrets(&t).into_owned();
                truncate(&masked, 500)
            });

            let uuid = format!("{session_id}:{abs_byte_offset}");
            let raw = secret_gate::mask_secrets(trimmed).into_owned();

            Some(ParsedEvent {
                uuid,
                session_id: session_id.to_string(),
                parent_uuid: None,
                is_sidechain: false,
                role,
                msg_type: "response_item".to_string(),
                text,
                raw,
                created_at_micros,
                cwd: None,
                git_branch: None,
                slug: None,
            })
        }
        // Unknown line types are silently skipped.
        _ => None,
    }
}

/// Parse a ChatGPT data-export `conversations.json` file: parses the whole
/// file at once (unlike the line-at-a-time `parse_cc_line`/`parse_codex_line`)
/// and returns every message-bearing event across every conversation, in
/// deterministic DFS preorder per conversation.
///
/// Returns `None` when `content` is not valid JSON or the top level is not a
/// JSON array — the caller treats that as a per-file error so the mirror
/// cursor does not advance. A malformed *conversation* inside an otherwise-
/// valid array is skipped individually, not the whole file. The returned
/// `raw` and `text` fields have secrets masked, exactly like
/// `parse_cc_line`/`parse_codex_line`. See
/// `crates/khive-pack-session/docs/api/mirror-parse.md` for the DFS/sidechain
/// algorithm detail.
pub fn parse_chatgpt_export(content: &str) -> Option<Vec<ParsedEvent>> {
    let value: Value = serde_json::from_str(content).ok()?;
    let conversations = value.as_array()?;

    let mut events = Vec::new();
    for conv in conversations {
        parse_conversation(conv, &mut events);
    }
    Some(events)
}

/// Extract a display-friendly text string from a message `content` value
/// (string form or structured-block array form).
fn extract_text(content: Option<&Value>) -> Option<String> {
    match content? {
        Value::String(s) => Some(s.clone()),
        Value::Array(blocks) => {
            let parts: Vec<String> = blocks.iter().filter_map(extract_block).collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n"))
            }
        }
        _ => None,
    }
}

/// Extract a display string from a single content block (`"text"` /
/// `"input_text"` / `"output_text"` / `"tool_use"` / `"tool_result"`). See
/// `crates/khive-pack-session/docs/api/mirror-parse.md#extract_text--extract_block-claude-code--codex-block-extraction`.
fn extract_block(block: &Value) -> Option<String> {
    let map = block.as_object()?;
    match map.get("type")?.as_str()? {
        // Claude Code text block and Codex user/assistant text blocks all carry
        // their display text in a "text" field — same extraction logic.
        "text" | "input_text" | "output_text" => {
            map.get("text").and_then(|v| v.as_str()).map(str::to_string)
        }
        "tool_use" => {
            let name = map
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let input = map.get("input").cloned().unwrap_or(Value::Null);
            let input_str = truncate(&serde_json::to_string(&input).unwrap_or_default(), 500);
            Some(format!("[tool_use: {name}] {input_str}"))
        }
        "tool_result" => {
            let content_val = map.get("content").cloned().unwrap_or(Value::Null);
            let content_str = match &content_val {
                Value::String(s) => s.clone(),
                other => serde_json::to_string(other).unwrap_or_default(),
            };
            Some(format!("[tool_result] {}", truncate(&content_str, 500)))
        }
        _ => None,
    }
}

/// Truncate a string to at most `max_chars` characters, appending `…` if truncated.
fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars).collect();
        out.push('…');
        out
    }
}

/// Context threaded through one conversation's DFS walk. See
/// `crates/khive-pack-session/docs/api/mirror-parse.md#convcontext`.
struct ConvContext<'a> {
    mapping: &'a Map<String, Value>,
    current_path: &'a HashSet<String>,
    session_id: &'a str,
    conv_created_at_micros: i64,
    slug: Option<&'a str>,
}

/// Parse one ChatGPT export conversation object (skips the whole
/// conversation on a missing/empty `id` or missing `mapping`), appending its
/// message-bearing nodes to `out` in deterministic DFS preorder. See
/// `crates/khive-pack-session/docs/api/mirror-parse.md#parse_conversation`.
fn parse_conversation(conv: &Value, out: &mut Vec<ParsedEvent>) {
    let Some(conv_obj) = conv.as_object() else {
        return;
    };
    let Some(session_id) = conv_obj
        .get("id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    let Some(mapping) = conv_obj.get("mapping").and_then(|v| v.as_object()) else {
        return;
    };

    let slug = conv_obj
        .get("title")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let conv_created_at_micros = conv_obj
        .get("create_time")
        .and_then(|v| v.as_f64())
        .map(|secs| (secs * 1_000_000.0) as i64)
        .unwrap_or(0);

    // current-path set: walk current_node -> parent -> ... -> root; off-path
    // nodes are flagged is_sidechain (see docs guide).
    let mut current_path: HashSet<String> = HashSet::new();
    if let Some(current_node) = conv_obj.get("current_node").and_then(|v| v.as_str()) {
        let mut cursor = Some(current_node.to_string());
        while let Some(node_id) = cursor {
            if !current_path.insert(node_id.clone()) {
                break; // cycle guard against malformed mapping data
            }
            cursor = mapping
                .get(&node_id)
                .and_then(|n| n.as_object())
                .and_then(|n| n.get("parent"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
        }
    }

    let root_id = mapping.iter().find_map(|(id, node)| {
        let node_obj = node.as_object()?;
        let parent_is_null = node_obj.get("parent").map(|v| v.is_null()).unwrap_or(true);
        parent_is_null.then(|| id.clone())
    });
    let Some(root_id) = root_id else {
        return;
    };

    let ctx = ConvContext {
        mapping,
        current_path: &current_path,
        session_id,
        conv_created_at_micros,
        slug: slug.as_deref(),
    };

    // Deterministic DFS preorder, explicit stack (not recursion — see docs guide).
    let mut stack: Vec<String> = vec![root_id];
    let mut visited: HashSet<String> = HashSet::new();

    while let Some(node_id) = stack.pop() {
        if !visited.insert(node_id.clone()) {
            continue; // cycle guard
        }
        let Some(node) = mapping.get(&node_id).and_then(|n| n.as_object()) else {
            continue;
        };

        if let Some(message) = node.get("message").filter(|m| !m.is_null()) {
            if let Some(message_obj) = message.as_object() {
                if let Some(ev) = build_chatgpt_event(&node_id, node, message_obj, &ctx) {
                    out.push(ev);
                }
            }
        }

        if let Some(children) = node.get("children").and_then(|c| c.as_array()) {
            // Push in reverse so the first child in the array is popped (and
            // thus visited) first — preorder must follow children order.
            for child in children.iter().rev() {
                if let Some(child_id) = child.as_str() {
                    stack.push(child_id.to_string());
                }
            }
        }
    }
}

/// Build a `ParsedEvent` for a single message-bearing mapping node; `None`
/// on a missing `id` or empty/whitespace-only extracted text (ChatGPT
/// scaffolding nodes). See
/// `crates/khive-pack-session/docs/api/mirror-parse.md#build_chatgpt_event`.
fn build_chatgpt_event(
    node_id: &str,
    node: &Map<String, Value>,
    message: &Map<String, Value>,
    ctx: &ConvContext,
) -> Option<ParsedEvent> {
    let uuid = message
        .get("id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?
        .to_string();

    let role = message
        .get("author")
        .and_then(|a| a.as_object())
        .and_then(|a| a.get("role"))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let content = message.get("content").and_then(|c| c.as_object());
    let content_type = content
        .and_then(|c| c.get("content_type"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let text = extract_chatgpt_text(&content_type, content)?;
    if text.trim().is_empty() {
        return None;
    }

    let created_at_micros = message
        .get("create_time")
        .and_then(|v| v.as_f64())
        .map(|secs| (secs * 1_000_000.0) as i64)
        .unwrap_or(ctx.conv_created_at_micros);

    // parent_uuid is Some only when the parent node itself carries a message
    // (provenance linkage — see docs guide).
    let parent_uuid = node
        .get("parent")
        .and_then(|v| v.as_str())
        .filter(|pid| {
            ctx.mapping
                .get(*pid)
                .and_then(|p| p.as_object())
                .and_then(|p| p.get("message"))
                .map(|m| !m.is_null())
                .unwrap_or(false)
        })
        .map(str::to_string);

    let is_sidechain = !ctx.current_path.contains(node_id);

    let raw_json = serde_json::to_string(node).unwrap_or_default();
    let raw = secret_gate::mask_secrets(&raw_json).into_owned();
    let text = secret_gate::mask_secrets(&text).into_owned();

    Some(ParsedEvent {
        uuid,
        session_id: ctx.session_id.to_string(),
        parent_uuid,
        is_sidechain,
        role,
        msg_type: content_type,
        text: Some(text),
        raw,
        created_at_micros,
        cwd: None,
        git_branch: None,
        slug: ctx.slug.map(str::to_string),
    })
}

/// Extract display text from a ChatGPT message `content` object per its
/// `content_type`. See
/// `crates/khive-pack-session/docs/api/mirror-parse.md#extract_chatgpt_text`.
fn extract_chatgpt_text(
    content_type: &str,
    content: Option<&Map<String, Value>>,
) -> Option<String> {
    let content = content?;

    if content_type == "text" {
        let parts = content.get("parts")?.as_array()?;
        let joined: Vec<String> = parts
            .iter()
            .filter_map(|p| p.as_str().map(str::to_string))
            .collect();
        return if joined.is_empty() {
            None
        } else {
            Some(joined.join("\n"))
        };
    }

    if let Some(text) = content.get("text").and_then(|v| v.as_str()) {
        return Some(text.to_string());
    }

    let parts = content.get("parts")?.as_array()?;
    let joined: Vec<String> = parts
        .iter()
        .filter_map(|p| p.as_str().map(str::to_string))
        .collect();
    if joined.is_empty() {
        None
    } else {
        Some(joined.join("\n"))
    }
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
        let secret = "sk-ant-api03-AAABBBCCCDDDEEEFFFGGG-XXXXX";
        let line = format!(
            r#"{{"uuid":"iiii-jjjj","sessionId":"sess-1111","type":"user","timestamp":"2026-06-29T10:03:00Z","message":{{"role":"user","content":"my key is {secret}"}}}}"#
        );
        let ev = parse_cc_line(&line).expect("should parse");

        let text = ev.text.expect("text should be present");
        assert!(
            !text.contains(secret),
            "secret must not appear in text: {text}"
        );
        assert!(
            text.contains("***MASKED***"),
            "MASKED marker must appear in text: {text}"
        );

        assert!(
            !ev.raw.contains(secret),
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
        let secret = "github_pat_ABCDE12345fghij67890KLMNO";
        let line = format!(
            r#"{{"uuid":"kkkk-llll","sessionId":"sess-2","type":"user","timestamp":"2026-06-29T10:04:00Z","message":{{"role":"user","content":"token={secret}"}}}}"#
        );
        let ev = parse_cc_line(&line).unwrap();
        assert!(!ev.raw.contains(secret));
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
        let secret = "sk-ant-api03-AAABBBCCCDDDEEEFFFGGG-XXXXX";
        let line = codex_user_msg(secret);
        let ev = parse_codex_line(&line, CDX_SID, 0).expect("should parse");
        let text = ev.text.expect("text present");
        assert!(!text.contains(secret), "secret must not appear in text");
        assert!(
            text.contains("***MASKED***"),
            "MASKED marker must appear in text"
        );
        assert!(!ev.raw.contains(secret), "secret must not appear in raw");
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
}
