use super::*;

#[test]
fn many_control_bytes_retain_only_the_first_hit() {
    let raw = "\u{0}".repeat(4096);
    let normalized = normalize_quoted_string(&raw);
    let hit = normalized
        .first_control_byte
        .expect("first control byte must be recorded");
    assert_eq!(
        hit.raw_pos, 0,
        "must record the earliest hit, not a later one"
    );
    assert_eq!(hit.byte, 0);
}

#[test]
fn control_free_string_stays_borrowed_after_single_scan() {
    let raw = r#""quoted key""#;
    let normalized = normalize_quoted_string(raw);
    assert!(matches!(normalized.text, Cow::Borrowed(_)));
    assert!(normalized.first_control_byte.is_none());
}
