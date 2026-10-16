//! Parsers for CLI transcripts and provider conversation exports.
//!
//! Every function here is deterministic and side-effect-free so the unit tests
//! can run without any runtime or DB setup.

use std::collections::{HashMap, HashSet};

use chrono::DateTime;
use khive_runtime::secret_gate;
use serde_json::{Map, Value};

fn mask_session_mirror(text: &str) -> std::borrow::Cow<'_, str> {
    secret_gate::mask_for_redaction_surface(secret_gate::RedactionSurface::SessionMirror, text)
}

/// Bound the masker's own input window (see `secret_gate::mask_bounded`)
/// before capping display text to `output_cap_chars`, rather than masking
/// the full, unbounded text: cost stays proportional to the shared window
/// regardless of input length, and a token straddling the window is dropped
/// whole rather than echoed unmasked.
fn mask_session_mirror_bounded(text: &str, output_cap_chars: usize) -> String {
    secret_gate::mask_bounded(
        secret_gate::RedactionSurface::SessionMirror,
        text,
        secret_gate::MASK_WINDOW_CHARS,
        output_cap_chars,
    )
    .text
}

/// A single parsed event, source-agnostic.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedEvent {
    /// Event UUID — the primary key for idempotency.
    ///
    /// For Claude Code events this is the top-level `uuid` field.
    /// For Codex events (which carry no per-message uuid) this is synthesised
    /// as `"{session_id}:{abs_byte_offset}"`.
    /// For ChatGPT export events this is the mapping node's `message.id`.
    /// For Claude.ai export events this is `chat_messages[].uuid`.
    pub uuid: String,
    /// Session UUID.
    pub session_id: String,
    /// Parent event UUID if present.
    pub parent_uuid: Option<String>,
    /// Whether this event is on a sidechain.
    pub is_sidechain: bool,
    /// Normalized provider role when present.
    pub role: Option<String>,
    /// Source event or content type.
    pub msg_type: String,
    /// Extracted display text, secrets masked; `None` for non-message events.
    pub text: Option<String>,
    /// Original line or serialized export message/node with secrets masked.
    pub raw: String,
    /// Source timestamp as microseconds since the Unix epoch; 0 if absent or unparseable.
    pub created_at_micros: i64,
    /// `cwd` if present, secrets masked.
    pub cwd: Option<String>,
    /// `gitBranch` (CC) or `payload.git.branch` (Codex) if present, secrets masked.
    pub git_branch: Option<String>,
    /// `slug` if present (CC: project slug; ChatGPT/Claude.ai export:
    /// conversation title; Codex files carry no slug concept).
    pub slug: Option<String>,
}

/// Session metadata parsed independently of retained message events.
///
/// Whole-file exports can contain valid conversations with no displayable
/// messages. Keeping this record separate from [`ParsedEvent`] lets the
/// ingester create the required `sessions` row without inventing a synthetic
/// `session_messages` row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ParsedSession {
    /// Provider conversation/session identifier.
    pub(crate) session_id: String,
    /// Source timestamp as microseconds since the Unix epoch; 0 if absent or unparseable.
    pub(crate) created_at_micros: i64,
    /// `cwd` if present.
    pub(crate) cwd: Option<String>,
    /// Source git branch if present.
    pub(crate) git_branch: Option<String>,
    /// Provider conversation title or project slug if present.
    pub(crate) slug: Option<String>,
}

/// Parsed whole-file export payload used by the ingest layer.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ParsedExport {
    pub(crate) sessions: Vec<ParsedSession>,
    pub(crate) events: Vec<ParsedEvent>,
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

    let cwd = map
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(|s| mask_session_mirror(s).into_owned());

    let git_branch = map
        .get("gitBranch")
        .and_then(|v| v.as_str())
        .map(|s| mask_session_mirror(s).into_owned());

    let slug = map
        .get("slug")
        .and_then(|v| v.as_str())
        .map(|s| mask_session_mirror(s).into_owned());

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
    let raw = mask_session_mirror(trimmed).into_owned();
    let text = text.map(|t| mask_session_mirror(&t).into_owned());

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
                .map(|s| mask_session_mirror(s).into_owned());

            let git_branch = payload
                .and_then(|p| p.get("git"))
                .and_then(|g| g.as_object())
                .and_then(|g| g.get("branch"))
                .and_then(|v| v.as_str())
                .map(|s| mask_session_mirror(s).into_owned());

            let uuid = format!("{session_id}:{abs_byte_offset}");
            let raw = mask_session_mirror(trimmed).into_owned();

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
            let text = text.map(|t| mask_session_mirror_bounded(&t, 500));

            let uuid = format!("{session_id}:{abs_byte_offset}");
            let raw = mask_session_mirror(trimmed).into_owned();

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

    // Both providers use the filename `conversations.json`. If operators point
    // both roots at the same directory, rejecting an unmistakable Claude.ai
    // file here prevents the ChatGPT ingester from consuming its cursor first.
    if conversations.iter().any(is_claude_ai_conversation) {
        return None;
    }

    let mut events = Vec::new();
    for conv in conversations {
        parse_conversation(conv, &mut events);
    }
    Some(events)
}

/// Parse a claude.ai data-export `conversations.json` file.
///
/// Claude.ai exports are a top-level JSON array whose conversation objects
/// carry `uuid`, `name`, and `chat_messages`. Message UUIDs and conversation
/// UUIDs are preserved as the idempotency and provider-session anchors. When
/// `current_leaf_message_uuid` is present, messages outside its parent chain
/// are retained with `is_sidechain = true`; older flat exports without that
/// field keep every message on the main path.
///
/// Returns `None` when `content` is not valid JSON or the top level is not an
/// array. Malformed conversation objects inside a valid array are skipped.
pub fn parse_claude_ai_export(content: &str) -> Option<Vec<ParsedEvent>> {
    parse_claude_ai_export_with_sessions(content).map(|parsed| parsed.events)
}

/// Parse a claude.ai export while retaining conversation metadata even when a
/// conversation has no displayable messages.
pub(crate) fn parse_claude_ai_export_with_sessions(content: &str) -> Option<ParsedExport> {
    let value: Value = serde_json::from_str(content).ok()?;
    let conversations = value.as_array()?;

    // Symmetric source-shape check for accidentally overlapping export roots.
    // Empty and wholly malformed arrays remain valid so per-conversation
    // isolation keeps its existing semantics.
    if conversations.iter().any(is_chatgpt_conversation) {
        return None;
    }

    let mut sessions = Vec::new();
    let mut events = Vec::new();
    for conversation in conversations {
        if let Some(session) = parse_claude_ai_conversation(conversation, &mut events) {
            sessions.push(session);
        }
    }
    Some(ParsedExport { sessions, events })
}

fn is_chatgpt_conversation(conversation: &Value) -> bool {
    conversation.as_object().is_some_and(|conversation| {
        conversation
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty())
            && conversation.get("mapping").is_some_and(Value::is_object)
    })
}

fn is_claude_ai_conversation(conversation: &Value) -> bool {
    conversation.as_object().is_some_and(|conversation| {
        conversation
            .get("uuid")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty())
            && conversation
                .get("chat_messages")
                .is_some_and(Value::is_array)
    })
}

fn parse_claude_ai_conversation(
    conversation: &Value,
    out: &mut Vec<ParsedEvent>,
) -> Option<ParsedSession> {
    let conversation = conversation.as_object()?;
    let session_id = conversation
        .get("uuid")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())?;
    let messages = conversation
        .get("chat_messages")
        .and_then(Value::as_array)?;

    let slug = conversation
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .or_else(|| {
            conversation
                .get("summary")
                .and_then(Value::as_str)
                .filter(|summary| !summary.is_empty())
        })
        .map(|title| mask_session_mirror(title).into_owned());
    let conversation_created_at_micros = conversation
        .get("created_at")
        .and_then(parse_rfc3339_micros)
        .or_else(|| {
            conversation
                .get("updated_at")
                .and_then(parse_rfc3339_micros)
        })
        .unwrap_or(0);

    let mut parent_by_id: HashMap<String, Option<String>> = HashMap::new();
    for message in messages {
        let Some(message) = message.as_object() else {
            continue;
        };
        let Some(uuid) = message
            .get("uuid")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        parent_by_id
            .entry(uuid.to_string())
            .or_insert_with(|| claude_ai_parent_uuid(message));
    }

    let current_path = claude_ai_current_path(conversation, &parent_by_id);
    let mut order: Vec<usize> = (0..messages.len()).collect();
    order.sort_by_key(|&position| {
        let source_index = messages[position]
            .as_object()
            .and_then(|message| message.get("index"))
            .and_then(Value::as_i64)
            .unwrap_or(position as i64);
        (source_index, position)
    });

    let mut emitted = HashSet::new();
    let start = out.len();
    for position in order {
        let Some(message) = messages[position].as_object() else {
            continue;
        };
        let Some(uuid) = message
            .get("uuid")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        if !emitted.insert(uuid.to_string()) {
            continue;
        }
        if let Some(event) = build_claude_ai_event(
            message,
            session_id,
            slug.as_deref(),
            conversation_created_at_micros,
            &current_path,
        ) {
            out.push(event);
        }
    }

    let stored_ids: HashSet<String> = out[start..]
        .iter()
        .map(|event| event.uuid.clone())
        .collect();
    for event in &mut out[start..] {
        if event
            .parent_uuid
            .as_ref()
            .is_some_and(|parent| !stored_ids.contains(parent))
        {
            event.parent_uuid = None;
        }
    }

    Some(ParsedSession {
        session_id: session_id.to_string(),
        created_at_micros: conversation_created_at_micros,
        cwd: None,
        git_branch: None,
        slug,
    })
}

fn claude_ai_current_path(
    conversation: &Map<String, Value>,
    parent_by_id: &HashMap<String, Option<String>>,
) -> HashSet<String> {
    let Some(current_leaf) = conversation
        .get("current_leaf_message_uuid")
        .and_then(Value::as_str)
        .filter(|id| parent_by_id.contains_key(*id))
    else {
        return parent_by_id.keys().cloned().collect();
    };

    let mut path = HashSet::new();
    let mut cursor = Some(current_leaf.to_string());
    while let Some(message_id) = cursor {
        if !path.insert(message_id.clone()) {
            break;
        }
        cursor = parent_by_id.get(&message_id).cloned().flatten();
    }
    path
}

fn build_claude_ai_event(
    message: &Map<String, Value>,
    session_id: &str,
    slug: Option<&str>,
    conversation_created_at_micros: i64,
    current_path: &HashSet<String>,
) -> Option<ParsedEvent> {
    let uuid = message
        .get("uuid")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())?
        .to_string();

    let role = message
        .get("sender")
        .or_else(|| message.get("role"))
        .and_then(Value::as_str)
        .filter(|sender| !sender.is_empty())
        .map(|sender| match sender {
            "human" | "user" => "user".to_string(),
            other => other.to_string(),
        });
    let text = extract_claude_ai_text(message)?;
    if text.trim().is_empty() {
        return None;
    }

    let created_at_micros = message
        .get("created_at")
        .and_then(parse_rfc3339_micros)
        .or_else(|| message.get("updated_at").and_then(parse_rfc3339_micros))
        .unwrap_or(conversation_created_at_micros);
    let parent_uuid = claude_ai_parent_uuid(message);
    let raw_json = serde_json::to_string(message).unwrap_or_default();

    Some(ParsedEvent {
        uuid: uuid.clone(),
        session_id: session_id.to_string(),
        parent_uuid,
        is_sidechain: !current_path.contains(&uuid),
        role,
        msg_type: "message".to_string(),
        text: Some(mask_session_mirror(&text).into_owned()),
        raw: mask_session_mirror(&raw_json).into_owned(),
        created_at_micros,
        cwd: None,
        git_branch: None,
        slug: slug.map(str::to_string),
    })
}

fn claude_ai_parent_uuid(message: &Map<String, Value>) -> Option<String> {
    message
        .get("parent_message_uuid")
        .and_then(Value::as_str)
        .filter(|parent| !parent.is_empty() && *parent != "00000000-0000-4000-8000-000000000000")
        .map(str::to_string)
}

fn extract_claude_ai_text(message: &Map<String, Value>) -> Option<String> {
    let top_level_text = message
        .get("text")
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty());
    let mut parts = Vec::new();

    match message.get("content") {
        Some(Value::Array(blocks)) => {
            for part in blocks
                .iter()
                .filter_map(extract_block)
                .filter(|part| !part.trim().is_empty())
            {
                parts.push(part);
            }
        }
        Some(Value::String(text)) if !text.trim().is_empty() => parts.push(text.clone()),
        Some(_) | None => {}
    }

    if let Some(text) = top_level_text {
        if !parts.iter().any(|part| part == text) {
            parts.push(text.to_string());
        }
    }

    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn parse_rfc3339_micros(value: &Value) -> Option<i64> {
    value
        .as_str()
        .and_then(|timestamp| DateTime::parse_from_rfc3339(timestamp).ok())
        .map(|timestamp| timestamp.timestamp_micros())
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
/// `"input_text"` / `"output_text"` / `"voice_note"` / `"tool_use"` /
/// `"tool_result"`). See
/// `crates/khive-pack-session/docs/api/mirror-parse.md#extract_text--extract_block-claude-code-codex-and-claudeai-blocks`.
fn extract_block(block: &Value) -> Option<String> {
    let map = block.as_object()?;
    match map.get("type")?.as_str()? {
        // Claude Code text block and Codex user/assistant text blocks all carry
        // their display text in a "text" field — same extraction logic.
        "text" | "input_text" | "output_text" | "voice_note" => {
            map.get("text").and_then(|v| v.as_str()).map(str::to_string)
        }
        "tool_use" => {
            let name = map
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let input = map.get("input").cloned().unwrap_or(Value::Null);
            let input_json = serde_json::to_string(&input).unwrap_or_default();
            let input_str = mask_session_mirror_bounded(&input_json, 500);
            Some(format!("[tool_use: {name}] {input_str}"))
        }
        "tool_result" => {
            let content_val = map.get("content").cloned().unwrap_or(Value::Null);
            let content_str = match &content_val {
                Value::String(s) => s.clone(),
                other => serde_json::to_string(other).unwrap_or_default(),
            };
            Some(format!(
                "[tool_result] {}",
                mask_session_mirror_bounded(&content_str, 500)
            ))
        }
        _ => None,
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
        .map(|title| mask_session_mirror(title).into_owned());

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
    let raw = mask_session_mirror(&raw_json).into_owned();
    let text = mask_session_mirror(&text).into_owned();

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
        return extract_chatgpt_parts(content);
    }

    if let Some(text) = content.get("text").and_then(|v| v.as_str()) {
        return Some(text.to_string());
    }

    extract_chatgpt_parts(content)
}

fn extract_chatgpt_parts(content: &Map<String, Value>) -> Option<String> {
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
#[path = "parse_tests.rs"]
mod tests;
