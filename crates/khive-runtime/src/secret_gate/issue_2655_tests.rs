use super::*;

fn opaque_fixture() -> String {
    [
        "ABCDEFGHIJKLMNOPQRSTUVWX",
        "abcdefghijklmnopqrstuvwxyz",
        "0123456789",
    ]
    .concat()
}

#[test]
fn issue_2655_compact_siblings_keep_all_entropy_shapes_member_local() {
    let hex = "0123456789abcdef".repeat(4);
    let opaque = opaque_fixture();
    let members = [
        format!("digest:{hex}"),
        "id:550e8400-e29b-41d4-a716-446655440000".to_owned(),
        format!("sum:sha256-{}=", "A".repeat(43)),
        format!("opaque:{opaque}"),
        format!("path:vault/{opaque}/rotate.md"),
        format!("digest:{}/{}", &hex[..16], &hex[16..32]),
    ];
    for separator in [',', ';', '&'] {
        for member in &members {
            for content in [
                format!("a_secret:x{separator}{member}"),
                format!("{member}{separator}a_secret:x"),
            ] {
                assert!(check(&content).is_ok(), "{content}: {:?}", scan(&content));
                assert_eq!(mask_secrets(&content), content);
            }
        }
    }
}

#[test]
fn issue_2655_quote_comma_is_an_inline_member_boundary() {
    let hex = "0123456789abcdef".repeat(4);
    for content in [
        format!(r#"{{"a_secret":"x","digest":"{hex}"}}"#),
        format!(r#"{{"digest":"{hex}","a_secret":"x"}}"#),
    ] {
        assert!(check(&content).is_ok(), "{content}: {:?}", scan(&content));
        assert_eq!(mask_secrets(&content), content);
    }
}

#[test]
fn issue_2655_external_window_keeps_short_spaced_refusals_and_long_controls() {
    let hex = "0123456789abcdef".repeat(4);
    for content in [
        format!("a_secret:x digest:{hex}"),
        format!("digest:{hex} a_secret:x"),
    ] {
        assert!(
            check(&content).is_err(),
            "short external context: {content}"
        );
    }
    let gap = " documentation".repeat(50);
    assert!(gap.len() > 485);
    for content in [
        format!("a_secret:x{gap} digest:{hex}"),
        format!("digest:{hex}{gap} a_secret:x"),
    ] {
        assert!(check(&content).is_ok(), "long external context: {content}");
        assert_eq!(mask_secrets(&content), content);
    }
}

#[test]
fn issue_2655_credential_assignments_and_nested_carriers_stay_refused() {
    let id = "550e8400-e29b-41d4-a716-446655440000";
    let hex = "0123456789abcdef".repeat(4);
    let hash = format!("sha256-{}=", "A".repeat(43));
    let opaque = opaque_fixture();
    for (content, value) in [
        (format!("api_key={id}"), id),
        (format!("secret={hex}"), hex.as_str()),
        (format!("api_key=label={id}"), id),
        (format!("secret=label={hash}"), hash.as_str()),
        (format!("secret:label={hash}"), hash.as_str()),
        (format!("payload=api_key={id}"), id),
        (format!("payload=auth={opaque}"), opaque.as_str()),
        (format!("secret=vault/{opaque}/rotate.md"), opaque.as_str()),
    ] {
        assert!(check(&content).is_err(), "{content}");
        assert!(!mask_secrets(&content).contains(value), "{content}");
    }
}

#[test]
fn issue_2655_later_assignment_does_not_label_an_earlier_value() {
    let id = "550e8400-e29b-41d4-a716-446655440000";
    let hex = "0123456789abcdef".repeat(4);
    let opaque = opaque_fixture();
    for value in [id, hex.as_str(), opaque.as_str()] {
        let content = format!("record={value}:a_secret=x");
        assert!(check(&content).is_ok(), "{content}: {:?}", scan(&content));
        assert_eq!(mask_secrets(&content), content);
    }
}

#[test]
fn issue_2655_refusal_reports_the_matched_members_trigger() {
    let id = "550e8400-e29b-41d4-a716-446655440000";
    let opaque = opaque_fixture();
    for (content, trigger) in [
        (format!("a_secret:x,api_key={id}"), "api_key"),
        (format!("api_key:x;auth={opaque}"), "auth"),
        (format!("secret:x&payload=auth={opaque}"), "auth"),
    ] {
        assert_eq!(
            scan(&content).and_then(|matched| matched.trigger),
            Some(trigger),
            "{content}"
        );
    }
}

#[test]
fn issue_2655_masking_continues_through_multiple_members() {
    let id = "550e8400-e29b-41d4-a716-446655440000";
    let hex = "0123456789abcdef".repeat(4);
    let benign = "fedcba9876543210".repeat(4);
    let opaque = opaque_fixture();
    for separator in [',', ';', '&'] {
        let content = format!(
            "api_key={id}{separator}digest:{benign}{separator}secret={hex}{separator}auth={opaque}"
        );
        ENTROPY_TOKENIZATION_COUNT.with(|count| count.set(0));
        let masked = mask_secrets(&content);
        assert_eq!(ENTROPY_TOKENIZATION_COUNT.with(|count| count.get()), 1);
        assert_eq!(masked.matches(REDACTION_MARKER).count(), 3, "{masked}");
        for value in [id, hex.as_str(), opaque.as_str()] {
            assert!(!masked.contains(value), "{masked}");
        }
        assert!(masked.contains(&format!("digest:{benign}")), "{masked}");
    }
}

#[test]
fn issue_2655_masking_continues_within_a_governed_member() {
    let first = "0123456789abcdef".repeat(2);
    let second = "fedcba9876543210".repeat(2);
    let content = format!("secret={first}/{second}");
    let masked = mask_secrets(&content);
    assert_eq!(masked.matches(REDACTION_MARKER).count(), 2, "{masked}");
    assert!(!masked.contains(&first));
    assert!(!masked.contains(&second));
}

#[test]
fn issue_2655_known_prefix_under_a_benign_member_stays_refused() {
    let fake = format!("ghp_{}", "A".repeat(36));
    let content = format!(r#"{{"a_secret":"x","digest":"{fake}"}}"#);
    assert_eq!(
        scan(&content).map(|matched| matched.detector),
        Some("github-token")
    );
    assert!(!mask_secrets(&content).contains(&fake));
}

#[test]
fn issue_2655_path_and_revision_guards_do_not_read_sibling_labels() {
    let revision = "0123456789abcdef0123456789abcdef01234567";
    for member in [
        format!("rev:{revision}"),
        format!("source=https://example.test/repo/commit/{revision}"),
        "path=docs/platform/credentials-and-authorization-architecture.md".to_owned(),
    ] {
        let content = format!("a_secret:x,{member}");
        assert!(check(&content).is_ok(), "{content}: {:?}", scan(&content));
        assert_eq!(mask_secrets(&content), content);
    }
    let credential = format!("secret=https://example.test/repo/commit/{revision}");
    assert!(check(&credential).is_err());
    assert!(!mask_secrets(&credential).contains(revision));
}

#[test]
fn issue_2655_inline_assignment_keeps_unicode_bridge_reconstruction() {
    // The original assignment token clears the existing 24-byte floor;
    // the two bare fragments remain below it and reconstruct to 32 hex.
    let first = "0123456789abcdef01";
    let second = "fedcba98765432";
    let content = format!("secret={first}\u{200b}{second}");
    assert!(check(&content).is_err());
    let masked = mask_secrets(&content);
    assert!(!masked.contains(first));
    assert!(!masked.contains(second));
}

#[test]
fn issue_2655_underscore_carriers_keep_nested_and_padded_forms() {
    let opaque = opaque_fixture();
    for prefix in ["session_secret_", "payload=session_secret_"] {
        for padding in ["", "=", "=="] {
            let content = format!("{prefix}{opaque}{padding}");
            assert!(check(&content).is_err(), "{content}");
            assert!(!mask_secrets(&content).contains(&opaque), "{content}");
        }
    }
}

#[test]
fn issue_2655_inline_bridge_uses_its_final_member() {
    let first = "0123456789abcdef01";
    let second = "fedcba98765432";
    let benign = "fedcba9876543210".repeat(4);
    for prefix in ["a:x".to_owned(), format!("digest:{benign}")] {
        let content = format!("{prefix},secret={first}\u{200b}{second}");
        assert!(check(&content).is_err(), "{content}");
        let masked = mask_secrets(&content);
        assert!(masked.starts_with(&prefix), "{masked}");
        assert!(!masked.contains(first), "{masked}");
        assert!(!masked.ends_with(second), "{masked}");
    }
}

#[test]
fn issue_2655_inline_bridge_stops_at_sibling_boundaries() {
    let first = "0123456789abcdef01";
    let second = "fedcba98765432";
    let benign = "fedcba9876543210".repeat(4);
    for content in [
        format!("secret=x,digest:{first}\u{200b}{second}"),
        format!("secret={first},digest:x\u{200b}{second}"),
        format!("secret={first},digest:{benign}\u{200b}{second}"),
        format!("{first}\u{200b}digest:x,secret={second}"),
    ] {
        assert!(check(&content).is_ok(), "{content}: {:?}", scan(&content));
        assert_eq!(mask_secrets(&content), content);
    }
}

#[test]
fn issue_2655_mask_budget_charges_compact_token_prefix_revisits() {
    let hex = "0123456789abcdef".repeat(2);
    let tail = format!(",secret={hex}").repeat(200);
    let content = format!("padding:{}{tail}", "a".repeat(900_000));
    let (spans, work) = collect_mask_spans(&content);
    assert!(
        work >= content.len() * 2,
        "each continuation revisits the full compact token"
    );
    assert!(work <= MAX_MASK_SCAN_WORK_BYTES);
    assert_eq!(spans.last().map(|span| span.1), Some(content.len()));
    let masked = mask_secrets(&content);
    assert!(!masked.contains(&hex));
    assert!(masked.matches(REDACTION_MARKER).count() < 200);
    assert!(masked.ends_with(REDACTION_MARKER));
}

#[test]
fn issue_2655_later_assignment_bridge_preserves_earlier_record_value() {
    let benign = "0123456789abcdef".repeat(4);
    let first = "a1b2c3d4e5f6071829";
    let second = "30415263748596";
    let prefix = format!("record={benign}:a_secret=");
    let content = format!("{prefix}{first}\u{200b}{second}");
    assert_eq!(
        scan(&content).and_then(|matched| matched.trigger),
        Some("secret")
    );
    let masked = mask_secrets(&content);
    assert!(masked.starts_with(&prefix), "{masked}");
    assert!(!masked.contains(first), "{masked}");
    assert!(!masked.contains(second), "{masked}");
    assert_eq!(masked.matches(REDACTION_MARKER).count(), 2, "{masked}");
}
