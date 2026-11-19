use super::parse_touched_files;

#[test]
fn accepts_single_or_double_nul_commit_boundaries() {
    let sha_a = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let sha_b = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let sha_c = "cccccccccccccccccccccccccccccccccccccccc";
    let raw =
        format!("/\x1e{sha_a}\nfirst.rs\0second.rs\0/\x1e{sha_b}\nthird.rs\0\0/\x1e{sha_c}\0");

    let parsed = parse_touched_files(raw.as_bytes()).expect("well-formed stream");
    assert_eq!(
        parsed[sha_a],
        vec!["first.rs".to_string(), "second.rs".to_string()]
    );
    assert_eq!(parsed[sha_b], vec!["third.rs".to_string()]);
    assert!(parsed[sha_c].is_empty());
}

#[test]
fn preserves_delimiters_and_uses_lossy_utf8_path_normalization() {
    let sha = "dddddddddddddddddddddddddddddddddddddddd";
    let mut raw = format!("/\x1e{sha}\n").into_bytes();
    raw.extend_from_slice(b"src/caf\xc3\xa9\t\"quoted\"\\leaf\nline.rs\0bad-\xff.rs\0");

    let parsed = parse_touched_files(&raw).expect("well-formed stream");
    assert_eq!(
        parsed[sha],
        vec![
            "src/café\t\"quoted\"\\leaf\nline.rs".to_string(),
            "bad-�.rs".to_string(),
        ]
    );
}

#[test]
fn rejects_a_path_token_before_any_header() {
    // Silently dropping this token could store `[]` for a commit that
    // touched files; failing the phase preserves the retry contract.
    let sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let raw = format!("orphan.rs\0/\x1e{sha}\nfirst.rs\0");

    let err = parse_touched_files(raw.as_bytes()).expect_err("orphan path must fail");
    assert!(
        format!("{err}").contains("before any commit header"),
        "{err}"
    );
}

#[test]
fn masks_a_secret_shaped_orphan_token_in_the_error() {
    // The error snippet is a log path: it must carry the same masking as
    // stored changed_paths, never raw token bytes.
    let fake_token = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let raw = format!("src/{fake_token}.rs\0");

    let err = parse_touched_files(raw.as_bytes()).expect_err("orphan path must fail");
    let text = format!("{err}");
    assert!(!text.contains(fake_token), "{text}");
    assert!(text.contains("MASKED"), "{text}");
}

#[test]
fn masks_a_secret_shaped_malformed_header_in_the_error() {
    let fake_token = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let raw = format!("/\x1e{fake_token}\0");

    let err = parse_touched_files(raw.as_bytes()).expect_err("bad header must fail");
    let text = format!("{err}");
    assert!(!text.contains(fake_token), "{text}");
    assert!(text.contains("MASKED"), "{text}");
}

#[test]
fn masks_an_orphan_secret_before_truncating_the_error() {
    let fake_token = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    // The credential crosses the old byte-80 cut, so masking only the
    // prefix would leave the raw secret in the error path.
    let token = format!("{} {fake_token}", "x".repeat(59));
    assert!(token.len() > 80);
    let raw = format!("{token}\0");

    let err = parse_touched_files(raw.as_bytes()).expect_err("orphan path must fail");
    let text = format!("{err}");
    assert!(!text.contains(fake_token), "{text}");
    assert!(text.contains("MASKED"), "{text}");
}

#[test]
fn rejects_a_malformed_header_sha() {
    let raw = "/\x1enot-hex-at-all\nfirst.rs\0second.rs\0";

    let err = parse_touched_files(raw.as_bytes()).expect_err("bad header must fail");
    assert!(
        format!("{err}").contains("malformed commit header"),
        "{err}"
    );
}

#[test]
fn rejects_a_header_remainder_without_the_newline_separator() {
    // A header remainder that does not start with exactly one newline is
    // a shape git never emits; silently discarding it could lose the
    // first path or misread it as a leading-newline path.
    let sha = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let raw = format!("/\x1e{sha}XXfirst.rs\0");

    let err = parse_touched_files(raw.as_bytes()).expect_err("bad separator must fail");
    assert!(format!("{err}").contains("first-path separator"), "{err}");
}
