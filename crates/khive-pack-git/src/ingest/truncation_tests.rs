use super::*;

#[test]
fn commit_embedding_cap_matches_embedding_service_limit() {
    assert_eq!(MAX_COMMIT_EMBED_BYTES, lattice_embed::MAX_TEXT_BYTES);
}

#[test]
fn under_cap_content_is_not_truncated() {
    let content = "a".repeat(MAX_COMMIT_EMBED_BYTES - 1);
    assert_eq!(truncated_embedding_head(&content), None);
}

#[test]
fn exactly_at_cap_content_is_not_truncated() {
    let content = "a".repeat(MAX_COMMIT_EMBED_BYTES);
    assert_eq!(truncated_embedding_head(&content), None);
}

#[test]
fn over_cap_content_is_truncated_to_exactly_the_cap() {
    let content = "a".repeat(MAX_COMMIT_EMBED_BYTES + 1);
    let head = truncated_embedding_head(&content).expect("over cap must truncate");
    assert_eq!(head.len(), MAX_COMMIT_EMBED_BYTES);
    assert!(content.starts_with(head));
}

/// Multibyte scalar straddling the byte cap must roll back to a char boundary.
#[test]
fn multibyte_scalar_straddling_cap_rolls_back_to_char_boundary() {
    // Fill up to one byte short of the cap with ASCII, then place a
    // 3-byte character exactly across the boundary.
    let mut content = "a".repeat(MAX_COMMIT_EMBED_BYTES - 1);
    content.push('€'); // 3 bytes: straddles byte 32_768..32_771
    content.push_str("tail-sentinel");

    let head = truncated_embedding_head(&content).expect("over cap must truncate");
    assert!(head.len() <= MAX_COMMIT_EMBED_BYTES);
    assert!(content.is_char_boundary(head.len()));
    assert!(std::str::from_utf8(head.as_bytes()).is_ok());
    assert!(content.starts_with(head));
    assert!(
        !head.contains("tail-sentinel"),
        "head must not include text past the cap"
    );
}
