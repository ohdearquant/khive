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

include!("parse_tests/claude_code_message_tests.rs");

include!("parse_tests/codex_message_tests.rs");

include!("parse_tests/claude_export_tests.rs");

include!("parse_tests/chatgpt_export_tests.rs");
