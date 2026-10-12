use super::extract_codex_session_id;
use std::path::Path;

#[test]
fn real_codex_filename_yields_uuid() {
    let path = Path::new("rollout-2025-11-11T08-32-36-019a731e-4a58-71b1-a71f-a8d2f9782113.jsonl");
    assert_eq!(
        extract_codex_session_id(path).as_deref(),
        Some("019a731e-4a58-71b1-a71f-a8d2f9782113")
    );
}

#[test]
fn timestamp_only_stem_is_rejected() {
    // Regression: a stem with no UUID suffix has 4 hyphens
    // in its trailing segments and must NOT be accepted as a session id.
    let path = Path::new("rollout-2025-11-11T08-32-36.jsonl");
    assert_eq!(extract_codex_session_id(path), None);
}

#[test]
fn invalid_hex_suffix_is_rejected() {
    let path = Path::new("rollout-2025-11-11T08-32-36-zzzzzzzz-zzzz-zzzz-zzzz-zzzzzzzzzzzz.jsonl");
    assert_eq!(extract_codex_session_id(path), None);
}

#[test]
fn too_short_suffix_is_rejected() {
    let path = Path::new("rollout-2025-11-11T08-32-36-aaaa-bbbb-cccc-dddd.jsonl");
    assert_eq!(extract_codex_session_id(path), None);
}

#[test]
fn non_rollout_filename_is_rejected() {
    let path = Path::new("not-a-rollout-file.jsonl");
    assert_eq!(extract_codex_session_id(path), None);
}
