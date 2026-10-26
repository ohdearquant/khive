use super::super::{check, mask_secrets, scan};
use super::*;

#[test]
fn issue_6100_exact_path_and_sha_labels_are_admitted() {
    let hex = "0123456789abcdef".repeat(4);
    for label in ["sha", "sha256"] {
        let content =
            format!("docs/adr/ADR-115-secret-gate-content-manifest-exemption.md {label}={hex}");
        assert!(check(&content).is_ok(), "{content}: {:?}", scan(&content));
        assert_eq!(mask_secrets(&content), content);
    }
}

#[test]
fn issue_6100_existing_path_shapes_and_wrappers_are_admitted_on_either_side() {
    let hex = "0123456789abcdef".repeat(4);
    for path in [
        "/tmp/secret/guide.md",
        "docs/auth/guide.md",
        "./docs/api_key/guide.md",
        "../docs/access_key/guide.md",
        "~/docs/private_key/guide.md",
        "secret/file.md",
        "\"docs/secret/guide.md\"",
        "`docs/secret/guide.md`,",
        "([docs/secret/guide.md]),",
        "<docs/secret/guide.md>:10-12",
        "docs/secret:120",
        "docs/auth/guide.md:~120-140",
    ] {
        assert!(is_plausible_file_path(path), "{path}");
        for content in [
            format!("{path} sha={hex}"),
            format!("sha={hex} {path}"),
            format!("{path} {hex}"),
        ] {
            assert!(check(&content).is_ok(), "{content}: {:?}", scan(&content));
            assert_eq!(mask_secrets(&content), content);
        }
    }
    let content = format!("docs/secret/file.md docs/auth/file.md sha={hex}");
    assert!(check(&content).is_ok(), "{content}: {:?}", scan(&content));
    assert_eq!(mask_secrets(&content), content);
}

#[test]
fn issue_6100_clipped_windows_classify_the_complete_original_path() {
    let hex = "0123456789abcdef".repeat(4);
    let long_prefix = "section/".repeat(20);
    let long_suffix = "section/".repeat(20);
    for content in [
        format!("docs/{long_prefix}secret/guide.md sha={hex}"),
        format!("sha={hex} docs/secret/{long_suffix}guide.md"),
    ] {
        assert!(check(&content).is_ok(), "{content}: {:?}", scan(&content));
        assert_eq!(mask_secrets(&content), content);
    }
}

#[test]
fn issue_6100_standalone_and_compound_triggers_keep_their_detectors() {
    let hex = "0123456789abcdef".repeat(4);
    for (label, trigger) in [
        ("secret", "secret"),
        ("api_keyv2", "api_key"),
        ("access_keyv2", "access_key"),
        ("private_keyv2", "private_key"),
        ("token", "token"),
    ] {
        for content in [
            format!("docs/secret/guide.md {label}={hex}"),
            format!("the {label} is {hex}"),
        ] {
            let matched = scan(&content).expect("standalone credential label must refuse");
            assert_eq!(
                (matched.detector, matched.trigger),
                ("hex-credential-token", Some(trigger)),
                "{content}"
            );
            assert!(!mask_secrets(&content).contains(&hex), "{content}");
        }
    }
}

#[test]
fn issue_6100_credential_value_paths_retain_context_beyond_the_window() {
    let hex = "0123456789abcdef".repeat(4);
    let content = format!("secret{}docs/auth/file.md sha={hex}", " ".repeat(130));
    let context = EntropyScanContext::new(&content);
    let digest_offset = content.find("sha=").unwrap();
    assert!(digest_offset.saturating_sub(TRIGGER_WINDOW) > "secret".len());
    let before_digest = &content[digest_offset.saturating_sub(TRIGGER_WINDOW)..digest_offset];
    assert_eq!(
        find_surrounding_trigger(&content, &context, before_digest, false),
        Some("auth")
    );
    let matched = scan(&content).expect("credential-value path must retain attribution");
    assert_eq!(
        (matched.detector, matched.trigger),
        ("hex-credential-token", Some("auth"))
    );
    assert!(!mask_secrets(&content).contains(&hex));
}

#[test]
fn issue_6100_credential_token_labels_keep_remote_path_authority() {
    let hex = "0123456789abcdef".repeat(4);
    for label in [
        "refresh_token",
        "session_token",
        "access_token",
        "REFRESH_TOKEN",
    ] {
        let content = format!("{label}:{}docs/auth/file.md sha={hex}", " ".repeat(130));
        let context = EntropyScanContext::new(&content);
        let digest_offset = content.find("sha=").unwrap();
        let before_digest = &content[digest_offset - TRIGGER_WINDOW..digest_offset];
        assert_eq!(
            find_surrounding_trigger(&content, &context, before_digest, false),
            Some("auth"),
            "{label}"
        );
        let matched = scan(&content).expect("credential-token label governs the path");
        assert_eq!(
            (matched.detector, matched.trigger),
            ("hex-credential-token", Some("auth")),
            "{label}"
        );
        assert!(!mask_secrets(&content).contains(&hex));
    }
    for label in ["next_token", "visibility_token"] {
        let content = format!("{label}:{}docs/auth/file.md sha={hex}", " ".repeat(130));
        let context = EntropyScanContext::new(&content);
        let digest_offset = content.find("sha=").unwrap();
        let before_digest = &content[digest_offset - TRIGGER_WINDOW..digest_offset];
        assert_eq!(
            find_surrounding_trigger(&content, &context, before_digest, false),
            None,
            "{label}"
        );
        assert!(check(&content).is_ok(), "{content}: {:?}", scan(&content));
        assert_eq!(mask_secrets(&content), content);
    }
}

#[test]
fn issue_6100_reference_attribution_has_bounded_identifier_work() {
    use super::super::false_positive_filters::take_clause_identifier_work_bytes;
    let hex = "0123456789abcdef".repeat(4);
    for repetitions in [64, 128, 256] {
        for path in ["docs/auth/file.md ", "/./ ", "/-/ "] {
            let content = path.repeat(repetitions);
            take_clause_identifier_work_bytes();
            let context = EntropyScanContext::new(&content);
            let inspected = take_clause_identifier_work_bytes();
            let reference = path == "docs/auth/file.md ";
            assert!(context
                .path_references
                .iter()
                .all(|flag| *flag == reference));
            assert!(
                inspected <= content.len() * 16,
                "{repetitions} repeats of {path:?} inspected {inspected} bytes for {} source bytes",
                content.len()
            );

            // A later trigger-bearing reference may scan the identifier-free
            // prefix once; the prefix must not cost one full scan per token.
            let content = format!("{content}docs/secret/file.md sha={hex}");
            take_clause_identifier_work_bytes();
            let context = EntropyScanContext::new(&content);
            let inspected = take_clause_identifier_work_bytes();
            assert!(
                inspected <= content.len() * 16,
                "final reference after {repetitions} repeats of {path:?} inspected {inspected} bytes"
            );
            let digest_offset = content.find("sha=").unwrap();
            let before_digest =
                &content[digest_offset.saturating_sub(TRIGGER_WINDOW)..digest_offset];
            assert_eq!(
                find_surrounding_trigger(&content, &context, before_digest, false),
                None,
                "punctuation/reference prefix cannot supply a trigger: {path:?}"
            );
            assert!(check(&content).is_ok(), "{content}: {:?}", scan(&content));
            assert_eq!(mask_secrets(&content), content);
        }
    }
}

#[test]
fn issue_6100_path_attribution_preserves_clause_budget_exhaustion() {
    let hex = "0123456789abcdef".repeat(4);
    let content = format!(
        "{}regional deploy: docs/auth/file.md sha={hex}",
        "docs/note/file.md ".repeat(12)
    );
    let context = EntropyScanContext::new(&content);
    let digest_offset = content.find("sha=").unwrap();
    let before_digest = &content[digest_offset - TRIGGER_WINDOW..digest_offset];
    assert_eq!(
        find_surrounding_trigger(&content, &context, before_digest, false),
        Some("auth")
    );
    let matched = scan(&content).expect("exhausted assignment clause stays fail-closed");
    assert_eq!(
        (matched.detector, matched.trigger),
        ("hex-credential-token", Some("auth"))
    );
}

#[test]
fn issue_6100_credential_bearing_paths_keep_run_and_normalized_detection() {
    let first = "0123456789abcdef".repeat(2);
    let second = "fedcba9876543210".repeat(2);
    let opaque = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    for (left, right, detector) in [
        (
            format!("docs/secret/{first}"),
            format!("docs/auth/{second}"),
            "hex-credential-token",
        ),
        (
            format!("docs/secret/{}/{}", &first[..16], &first[16..]),
            format!("docs/auth/{}/{}", &second[..16], &second[16..]),
            "hex-credential-token",
        ),
        (
            format!("docs/secret/{opaque}"),
            format!("docs/auth/{opaque}"),
            "high-entropy-token",
        ),
    ] {
        // Three ordinary words break the backward clause walk, so another
        // guard cannot rescue an accidentally omitted credential path.
        let content = format!("{left} one two three {right}");
        let matched = scan(&content).expect("credential paths must still refuse");
        assert_eq!(
            (matched.detector, matched.trigger),
            (detector, Some("auth")),
            "{content}"
        );
        let masked = mask_secrets(&content);
        assert!(!masked.contains(&first), "{masked}");
        assert!(!masked.contains(&second), "{masked}");
        assert!(
            !masked.contains(&format!("{}/{}", &first[..16], &first[16..])),
            "{masked}"
        );
        assert!(
            !masked.contains(&format!("{}/{}", &second[..16], &second[16..])),
            "{masked}"
        );
        assert!(!masked.contains(opaque), "{masked}");
    }
}

#[test]
fn issue_6100_reconstructed_path_bridges_retain_neighbor_attribution() {
    let first = "0123456789abcdef";
    let second = "fedcba9876543210";
    let hex = "0123456789abcdef".repeat(4);
    let content = format!("docs/secret/{first} {second} sha={hex}");
    let context = EntropyScanContext::new(&content);
    let before_digest = &content[..content.find("sha=").unwrap()];
    assert_eq!(
        find_surrounding_trigger(&content, &context, before_digest, false),
        Some("secret")
    );
    let matched = scan(&content).expect("reconstructed path keeps existing context");
    assert_eq!(
        (matched.detector, matched.trigger),
        ("hex-credential-token", Some("secret"))
    );
}

#[test]
fn issue_6100_previous_line_fallback_distinguishes_paths_from_labels() {
    let revision = "0123456789abcdef0123456789abcdef01234567";
    let reference = format!("docs/secret/file.md:\n{revision}");
    assert!(check(&reference).is_ok(), "{:?}", scan(&reference));
    assert_eq!(mask_secrets(&reference), reference);
    for content in [
        format!("docs/secret:\n{revision}"),
        format!("secret:\n{revision}"),
    ] {
        let matched = scan(&content).expect("previous-line assignment keeps its trigger");
        assert_eq!(
            (matched.detector, matched.trigger),
            ("hex-credential-token", Some("secret")),
            "{content}"
        );
    }
}

#[test]
fn issue_6100_path_clause_view_preserves_source_boundaries_and_byte_offsets() {
    let content = "([../docs/._secret/file.md]), docs/auth/file.md.\u{200b}next";
    let tokens = tokenize_entropy_tokens(content);
    let mut attribution = content.to_owned();
    for (offset, raw) in tokens.iter().copied().take(2) {
        assert!(is_plausible_file_path(raw), "{raw}");
        let placeholder = path_clause_placeholder(raw);
        assert_eq!(placeholder.len(), raw.len());
        attribution.replace_range(offset..offset + raw.len(), &placeholder);
    }
    assert_eq!(attribution.len(), content.len());
    for (offset, _) in content.char_indices() {
        assert_eq!(
            before_first_sentence_boundary(&content[offset..]).len(),
            before_first_sentence_boundary(&attribution[offset..]).len(),
            "boundary after byte {offset}"
        );
    }
    let original_tail = after_last_sentence_boundary(content);
    let attribution_tail = after_last_sentence_boundary(&attribution);
    assert_eq!(
        content.len() - original_tail.len(),
        attribution.len() - attribution_tail.len()
    );
    assert_eq!(original_tail, attribution_tail);
}

#[test]
fn issue_6100_same_token_assignments_and_path_values_still_refuse() {
    let hex = "0123456789abcdef".repeat(4);
    for content in [
        format!("secret=docs/auth/{hex}"),
        format!("docs/secret=sha={hex}"),
        format!("docs/secret: sha={hex}"),
        format!("secret = docs/auth/file.md sha={hex}"),
    ] {
        let matched = scan(&content).expect("credential syntax must refuse");
        assert_eq!(matched.detector, "hex-credential-token", "{content}");
        assert_eq!(matched.trigger, Some("secret"), "{content}");
        assert!(!mask_secrets(&content).contains(&hex), "{content}");
    }
}

#[test]
fn issue_6100_windows_and_url_spellings_keep_the_existing_path_grammar() {
    let hex = "0123456789abcdef".repeat(4);
    for path in [
        r"C:\docs\secret\file.md",
        "C:/docs/secret/file.md",
        "https://example.test/secret/file.md",
    ] {
        assert!(!is_plausible_file_path(path), "{path}");
        let content = format!("{path} sha={hex}");
        let matched = scan(&content).expect("unrecognized path spelling retains context");
        assert_eq!(
            (matched.detector, matched.trigger),
            ("hex-credential-token", Some("secret")),
            "{content}"
        );
    }
}

#[test]
fn issue_6100_json_scalar_attribution_is_unchanged() {
    let hex = "0123456789abcdef".repeat(4);
    let sibling = format!(r#"{{"path":"docs/secret/file.md","digest":"{hex}"}}"#);
    assert!(check(&sibling).is_ok(), "{:?}", scan(&sibling));
    assert_eq!(mask_secrets(&sibling), sibling);
    for content in [
        format!(r#"{{"message":"docs/secret/file.md sha={hex}"}}"#),
        format!(r#"{{"secret":{{"digest":"{hex}"}}}}"#),
    ] {
        let matched = scan(&content).expect("same-scalar or owning-key label still refuses");
        assert_eq!(
            (matched.detector, matched.trigger),
            ("hex-credential-token", Some("secret")),
            "{content}"
        );
        assert!(!mask_secrets(&content).contains(&hex), "{content}");
    }
}

#[test]
fn issue_6100_disjoint_context_preserves_priority_without_joining_labels() {
    assert_eq!(
        find_trigger_in_contexts(&["auth ", " key"], false),
        Some("key")
    );
    assert_eq!(
        find_trigger_in_contexts(&["myprivate_keyv2=", " token "], false),
        Some("token")
    );
    assert_eq!(find_trigger_in_contexts(&["api_", "keyv2"], false), None);
    assert_eq!(
        find_trigger_in_contexts(&["association_key:", " docs"], true),
        None
    );
}

#[test]
fn issue_6100_wrapped_and_padded_sha_values_do_not_widen_digest_grammar() {
    let hex = "0123456789abcdef".repeat(4);
    for value in [
        format!("sha={hex}"),
        format!("sha256=({hex})="),
        format!("sha256=(({hex}))=="),
    ] {
        let content = format!("secret docs/auth/file.md {value}");
        let matched = scan(&content).expect("standalone trigger retains exact digest grammar");
        assert_eq!(
            (matched.detector, matched.trigger),
            ("hex-credential-token", Some("secret")),
            "{content}"
        );
        assert!(!mask_secrets(&content).contains(&hex), "{content}");
    }
}

#[test]
fn issue_6100_later_and_overflowing_bridge_spans_keep_path_authority() {
    let h32 = "0123456789abcdef".repeat(2);
    let h40 = format!("{h32}01234567");
    let h128 = "0123456789abcdef".repeat(8);
    let first = "abcdef0123456789";
    let second = "fedcba9876543210";
    let h24 = format!("{first}abcdef01");
    for content in [
        format!("{h32} docs/secret/{first} {second}"),
        format!("{h32} \"docs/secret/{first}\" {second}"),
        format!("{h40} /{h24}/secret"),
        format!("{h32} /abcdef01/secret"),
        format!("{h128} docs/secret/{first} {second}"),
        format!("{h128} {first} /{second}/secret"),
        format!("{first} {second}/secret {h32}"),
    ] {
        let tokens = tokenize_entropy_tokens(&content);
        let path_index = tokens
            .iter()
            .position(|&(_, raw)| is_plausible_file_path(raw))
            .expect("fixture contains one path");
        assert!(
            !is_path_reference_context(&content, &content, &tokens, path_index),
            "path contributes to a credential-length bridge: {content}"
        );
        let context = EntropyScanContext::new(&content);
        let (first_offset, first_raw) = tokens[0];
        let after_first = &content[first_offset + first_raw.len()..];
        assert_eq!(
            find_surrounding_trigger(&content, &context, after_first, false),
            Some("secret"),
            "{content}"
        );
        let matched = scan(&content).expect("path bridge keeps its neighboring refusal");
        assert_eq!(
            (matched.detector, matched.trigger),
            ("hex-credential-token", Some("secret")),
            "{content}"
        );
    }
}

#[test]
fn issue_6100_complete_neighbor_digests_alone_do_not_govern_paths() {
    let h32 = "0123456789abcdef".repeat(2);
    let h40 = format!("{h32}01234567");
    for content in [
        format!("{h32} docs/secret/file.md"),
        format!("docs/secret/file.md {h32}"),
        format!("\"docs/secret/file.md\" {h32}"),
        format!("{h32} docs/secret/file.md {h40}"),
    ] {
        let tokens = tokenize_entropy_tokens(&content);
        let path_index = tokens
            .iter()
            .position(|&(_, raw)| is_plausible_file_path(raw))
            .expect("fixture contains one ordinary path");
        assert!(
            is_path_reference_context(&content, &content, &tokens, path_index),
            "only complete independent neighboring digests: {content}"
        );
        assert!(check(&content).is_ok(), "{content}: {:?}", scan(&content));
        assert_eq!(mask_secrets(&content), content);
    }
}

#[test]
fn issue_6100_attribution_subranges_do_not_widen_legacy_normalization() {
    let leading = "a".repeat(24);
    let later = "b".repeat(32);
    let token = format!("{leading}/{later}");
    assert_eq!(normalized_hex_credential_span(&token), None);
    assert!(has_normalized_hex_credential_span_overlapping(
        &token,
        leading.len() + 1,
        token.len(),
    ));
    let h40 = "a".repeat(40);
    let h24 = "b".repeat(24);
    let token = format!("{h40}/{h24}");
    assert_eq!(normalized_hex_credential_span(&token), Some(h40.as_str()));
    assert!(has_normalized_hex_credential_span_overlapping(
        &token,
        h40.len() + 1,
        token.len(),
    ));
}
