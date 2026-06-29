//! Pure CC JSONL line parser — no I/O, heavily unit-tested.
//!
//! Every function here is deterministic and side-effect-free so the unit tests
//! can run without any runtime or DB setup.

use std::sync::LazyLock;

use chrono::DateTime;
use regex::Regex;
use serde_json::Value;

/// A single parsed event from a CC session JSONL file.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedEvent {
    /// CC event UUID (the primary key for idempotency).
    pub uuid: String,
    /// CC session UUID (`sessionId` field).
    pub session_id: String,
    /// Parent event UUID if present.
    pub parent_uuid: Option<String>,
    /// Whether this event is on a sidechain.
    pub is_sidechain: bool,
    /// `message.role` when present.
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
    /// `gitBranch` if present.
    pub git_branch: Option<String>,
    /// `slug` if present.
    pub slug: Option<String>,
}

/// Compiled secret-masking pattern (built once, reused for every line).
///
/// Order matters: longer/more-specific prefixes first to avoid partial matches.
///
/// Note: regular string literal (not raw) so that `\<newline><whitespace>` is a
/// Rust string-continuation (stripping the newline and leading whitespace).
/// A raw `r"..."` string would make those backslashes literal regex characters.
static SECRET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        "sk-ant-[A-Za-z0-9_-]+\
         |sk-[A-Za-z0-9]{20,}\
         |github_pat_[A-Za-z0-9_]+\
         |ghp_[A-Za-z0-9]+\
         |gho_[A-Za-z0-9]+\
         |AKIA[0-9A-Z]{16}\
         |xox[baprs]-[A-Za-z0-9-]+\
         |AIza[0-9A-Za-z_-]{35}",
    )
    .expect("secret regex is valid")
});

/// Replace all recognized secret token shapes with `***MASKED***`.
fn mask_secrets(s: &str) -> String {
    SECRET_RE.replace_all(s, "***MASKED***").into_owned()
}

/// Parse one CC JSONL line.
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

    // Apply masking to raw line and to the extracted text.
    let raw = mask_secrets(trimmed);
    let text = text.map(|t| mask_secrets(&t));

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

/// Extract a display-friendly text string from a message `content` value.
///
/// Handles both the string form and the structured-block array form.
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

/// Extract a display string from a single content block.
fn extract_block(block: &Value) -> Option<String> {
    let map = block.as_object()?;
    match map.get("type")?.as_str()? {
        "text" => map.get("text").and_then(|v| v.as_str()).map(str::to_string),
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
}
