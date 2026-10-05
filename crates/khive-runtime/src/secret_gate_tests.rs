use super::*;

fn github_fine_grained_pat_fixture() -> String {
    format!("github_pat_{}", "A".repeat(82))
}

fn openai_project_key_fixture() -> String {
    format!("sk-proj-{}", "A".repeat(80))
}

fn anthropic_api_key_fixture() -> String {
    format!("sk-ant-api03-{}AA", "A".repeat(93))
}

#[test]
fn blocks_aws_akia() {
    // FAKE key: prefix is real shape, 16-char suffix invented.
    let fake = "AKIAFAKEKEY1234567890";
    assert!(scan(fake).is_some(), "AKIA must be caught");
    let m = scan(fake).unwrap();
    assert_eq!(m.detector, "aws-access-key-id");
    // Masked excerpt must not echo the full key.
    assert!(
        !m.masked.contains("FAKEKEY1234567890"),
        "must not echo the secret: {}",
        m.masked
    );
}

#[test]
fn blocks_aws_asia() {
    let fake = "ASIAFAKEKEY00000000000";
    let m = scan(fake);
    assert!(m.is_some(), "ASIA must be caught");
    assert_eq!(m.unwrap().detector, "aws-access-key-id");
}

#[test]
fn blocks_github_ghp() {
    // 36 chars total to pass min_len.
    let fake = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    assert!(scan(fake).is_some(), "ghp_ must be caught");
}

#[test]
fn blocks_github_gho() {
    let fake = "gho_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
    assert!(scan(fake).is_some(), "gho_ must be caught");
}

#[test]
fn blocks_github_pat() {
    let fake = github_fine_grained_pat_fixture();
    assert!(scan(&fake).is_some(), "github_pat_ must be caught");
}

#[test]
fn blocks_openai_sk() {
    let fake = "sk-aaaaaabbbbbbccccccddddddeeeeeeffffgg";
    assert!(scan(fake).is_some(), "sk- must be caught");
}

#[test]
fn blocks_anthropic_sk_ant() {
    let fake = anthropic_api_key_fixture();
    assert!(scan(&fake).is_some(), "sk-ant- must be caught");
    assert_eq!(scan(&fake).unwrap().detector, "anthropic-api-key");
}

#[test]
fn vendor_prefix_minimums_allow_short_documentation_fragments() {
    let fragments = [
        "github_pat_[A-Za-z0-9_]{82}".to_owned(),
        "sk-proj-[A-Za-z0-9_-]{80,}".to_owned(),
        "sk-ant-api03-[A-Za-z0-9_-]{93}AA".to_owned(),
        "github_pat_FAKE_CANARY".to_owned(),
        "sk-ant-api03-FAKE_CANARY".to_owned(),
        format!("github_pat_{}", "A".repeat(81)),
        format!("sk-proj-{}", "A".repeat(79)),
        format!("sk-ant-api03-{}AA", "A".repeat(92)),
    ];

    for fragment in fragments {
        assert!(
            check(&fragment).is_ok(),
            "short documentation fragment must pass: {fragment}, got {:?}",
            scan(&fragment)
        );
    }
}

#[test]
fn vendor_prefix_minimums_keep_plausible_keys_blocked() {
    let github = github_fine_grained_pat_fixture();
    let openai = openai_project_key_fixture();
    let anthropic = anthropic_api_key_fixture();

    for (candidate, detector) in [
        (github.as_str(), "github-token"),
        (openai.as_str(), "openai-api-key"),
        (anthropic.as_str(), "anthropic-api-key"),
    ] {
        let matched = scan(candidate).expect("plausible-length vendor key must remain blocked");
        assert_eq!(matched.detector, detector);
    }
}

#[test]
fn short_specialized_sk_prefix_does_not_hide_later_generic_key() {
    let short_vendor_fragment = format!("sk-proj-{}", "A".repeat(79));
    let generic_key = format!("sk-{}", "A1".repeat(20));
    let content = format!("{short_vendor_fragment} {generic_key}");

    let (matched, detector) =
        scan_match(&content).expect("later generic sk key must remain detectable");
    assert_eq!(matched, generic_key);
    assert_eq!(detector, "openai-api-key");
}

#[test]
fn glued_short_specialized_prefix_does_not_hide_later_generic_key() {
    let generic_key = format!("sk-{}A", "A1".repeat(21));
    let content = format!("sk-proj-X,{generic_key}");

    assert!(
        check(&content).is_err(),
        "a generic key after a rejected vendor prefix must remain blocked"
    );
    let masked = mask_secrets(&content).into_owned();
    assert!(
        !masked.contains(&generic_key),
        "the later generic key must not survive masking: {masked}"
    );
    assert!(
        masked.contains(REDACTION_MARKER),
        "the later generic key must be replaced: {masked}"
    );
}

#[test]
fn blocks_stripe_live() {
    let fake = "sk_live_FAKESTRIPE0000000000000"; // gitleaks:allow
    assert!(scan(fake).is_some(), "sk_live_ must be caught");
    assert_eq!(scan(fake).unwrap().detector, "stripe-secret-key");
}

#[test]
fn blocks_stripe_restricted() {
    let fake = "rk_live_FAKESTRIPE0000000000000"; // gitleaks:allow
    assert!(scan(fake).is_some(), "rk_live_ must be caught");
    assert_eq!(scan(fake).unwrap().detector, "stripe-restricted-key");
}

#[test]
fn blocks_fly_flyv1() {
    let fake = "FlyV1 FAKEFLYTOKEN000000000000000000";
    assert!(scan(fake).is_some(), "FlyV1 must be caught");
    assert_eq!(scan(fake).unwrap().detector, "fly-token");
}

#[test]
fn short_flyv1_marker_does_not_hide_later_token() {
    let content = "FlyV1 ab FlyV1 FAKEFLYTOKEN000000000000000000";
    let (matched, detector) = scan_match(content).expect("later FlyV1 token must be detected");
    assert_eq!(detector, "fly-token");
    assert_eq!(matched, "FlyV1 FAKEFLYTOKEN000000000000000000");
    assert_eq!(
        matched.as_ptr() as usize - content.as_ptr() as usize,
        content.rfind("FlyV1 ").unwrap()
    );
    assert_eq!(scan(content).unwrap().detector, "fly-token");
}

#[test]
fn non_boundary_flyv1_marker_does_not_hide_later_token() {
    for content in [
        "xFlyV1 abc FlyV1 FAKEFLYTOKEN000000000000000000",
        "xFlyV1 FAKE FlyV1 FAKEFLYTOKEN000000000000000000",
    ] {
        let (matched, detector) = scan_match(content).expect("later FlyV1 token must be detected");
        assert_eq!(detector, "fly-token");
        assert_eq!(matched, "FlyV1 FAKEFLYTOKEN000000000000000000");
        assert_eq!(
            matched.as_ptr() as usize - content.as_ptr() as usize,
            content.rfind("FlyV1 ").unwrap()
        );
        assert_eq!(scan(content).unwrap().detector, "fly-token");
    }
}

#[test]
fn blocks_fly_fm2() {
    let fake = "fm2_FAKEFLYTOKEN00000000000000000";
    assert!(scan(fake).is_some(), "fm2_ must be caught");
    assert_eq!(scan(fake).unwrap().detector, "fly-token");
}

#[test]
fn blocks_vercel_token() {
    let fake = "vercel_FAKETOKEN00000000000000000";
    assert!(scan(fake).is_some(), "vercel_ must be caught");
    assert_eq!(scan(fake).unwrap().detector, "vercel-token");
}

#[test]
fn prefix_detectors_allow_lowercase_source_filenames() {
    for &(_, needle, min_len) in PREFIX_DETECTORS {
        let padding_len = min_len.saturating_sub(needle.len() + "provider_.py".len());
        let filename_body = format!("provider_{}.py", "a".repeat(padding_len));
        let candidate = format!("{needle}{filename_body}");
        assert!(
            candidate.len() >= min_len,
            "negative control must exercise {needle}'s length tier"
        );
        assert!(
            find_prefix_token(&candidate, needle, min_len).is_none(),
            "lowercase source filename must not match prefix {needle}: {candidate}"
        );
        assert!(
            check(&candidate).is_ok(),
            "lowercase source filename must pass the canonical scanner: {candidate}, got {:?}",
            scan(&candidate)
        );
    }
}

#[test]
fn prefix_detectors_keep_value_shaped_payloads_with_source_suffixes() {
    for &(_, needle, min_len) in PREFIX_DETECTORS {
        let required_payload_len = min_len.saturating_sub(needle.len()).max(2);
        let value_payload = "A1".repeat(required_payload_len.div_ceil(2));
        let candidate = format!("{needle}{value_payload}.py");
        assert!(
            find_prefix_token(&candidate, needle, min_len).is_some(),
            "uppercase/digit value evidence must preserve prefix {needle}: {candidate}"
        );
    }
}

#[test]
fn prefix_detectors_keep_lowercase_single_run_payloads_with_source_suffixes() {
    for &(_, needle, min_len) in PREFIX_DETECTORS {
        let required_payload_len = min_len.saturating_sub(needle.len()).max(24);
        let value_payload = "a".repeat(required_payload_len);
        let candidate = format!("{needle}{value_payload}.py");
        assert!(
            find_prefix_token(&candidate, needle, min_len).is_some(),
            "an extension alone must not exempt prefix {needle}: {candidate}"
        );
    }
}

#[test]
fn allows_vercel_prefixed_source_filename_on_every_scanner_surface() {
    let content = "review `vercel_deployment_monitoring_adapter.py` before release";

    assert!(check(content).is_ok(), "write gate must allow filename");
    assert!(scan(content).is_none(), "scanner must not report filename");
    assert_eq!(
        mask_secrets(content).as_ref(),
        content,
        "masking surface must preserve the filename byte-for-byte"
    );
}

#[test]
fn admits_source_citation_line_reference_and_still_blocks_random_payload() {
    // Arm 1 — admitted: a file-and-line citation, both the single-line and
    // range forms, across two different source extensions, including a
    // path-bearing stem (`/` is in the allowed stem punctuation set).
    let admitted = [
        "vercel_deployment_monitor.py:412",
        "vercel_deployment_monitor.py:412-418",
        "vercel_src/runtime/checkpoint_loader.rs:97-103",
    ];
    for content in admitted {
        assert!(
            find_prefix_token(content, "vercel_", 20).is_none(),
            "file-and-line citation must not be flagged as a credential: {content}"
        );
        assert!(
            check(content).is_ok(),
            "file-and-line citation must pass the write gate: {content}, got {:?}",
            scan(content)
        );
    }

    // Arm 2 — must-FAIL control, in the SAME test as arm 1: a provider
    // prefix followed by an ordinary random-looking value is still a
    // credential. This is the arm a loose "trailing digits anywhere"
    // suffix match would silently stop catching.
    let random_payload = "vercel_aB3xQ9mK7pL2wZ8nR4tY6uV1"; // gitleaks:allow
    assert_eq!(random_payload.len() - "vercel_".len(), 24);
    assert!(
        find_prefix_token(random_payload, "vercel_", 20).is_some(),
        "random payload after the provider prefix must still be flagged: {random_payload}"
    );
    assert_eq!(
        scan(random_payload).map(|matched| matched.detector),
        Some("vercel-token"),
        "random-looking payload after the provider prefix must still be scanned as a credential"
    );
}

#[test]
fn source_citation_line_reference_boundary_arms_still_refused() {
    let needle = "vercel_";
    let cases = [
        "vercel_deployment_monitor.py:4a2", // non-digit byte in the run
        "vercel_deployment_monitor.py:12-", // empty second digit run
        "vercel_deployment_monitor.py:12-34-56", // more than one '-'
        // Refused by the SEPARATOR predicate, not the digit one: this stem
        // carries no `_`, `-`, `/` or `.`. A real shape, but it cannot
        // isolate the all-lowercase rule.
        "vercel_deployment2monitor.py:412",
        // Digit inside the stem WITH a separator present, so the
        // all-lowercase predicate is the only thing refusing it. This is
        // the isolating arm for that predicate: loosen it to tolerate
        // digits and this case alone is admitted.
        "vercel_deployment2_monitor.py:412",
    ];
    for token in cases {
        assert!(
            !is_filename_shaped_prefix_match(token, needle),
            "must still be refused: {token}"
        );
    }

    // A bare trailing colon is NOT a boundary case of the line-reference
    // grammar; it is sentence punctuation, and the generic trim has always
    // removed it before this function looks at anything. It was admitted
    // before line references were understood here and must stay admitted:
    // this change widens what the carve-out accepts and narrows nothing.
    // Without this arm, a later reading of the boundary list above would
    // conclude the colon belongs in the grammar and quietly turn a prose
    // citation into a refusal.
    assert!(
        is_filename_shaped_prefix_match("vercel_deployment_monitor.py:", needle),
        "a trailing colon is prose punctuation and was always trimmed"
    );
}

#[test]
fn blocks_slack_xoxb() {
    let fake = "xoxb-FAKE-SLACKTOKEN-000000000000000000000000";
    assert!(scan(fake).is_some(), "xoxb- must be caught");
    assert_eq!(scan(fake).unwrap().detector, "slack-token");
}

#[test]
fn blocks_pem_private_key() {
    // Split the header so the literal detector-trigger string is not present
    // verbatim in source — pre-commit's detect-private-key hook would fire.
    // The gate detects it at runtime because scan() sees the assembled string.
    let header = ["-----BEGIN RSA", " PRIVATE KEY-----"].concat(); // gitleaks:allow
    let fake = format!("{}\nMIIEo\u{2026}\n-----END RSA PRIVATE KEY-----", header);
    assert!(scan(&fake).is_some(), "PEM private key must be caught");
    assert_eq!(scan(&fake).unwrap().detector, "pem-private-key");
}

#[test]
fn blocks_pem_ec_private_key() {
    let header = ["-----BEGIN EC", " PRIVATE KEY-----"].concat(); // gitleaks:allow
    let fake = format!("{}\nMHQCAQEE\u{2026}\n-----END EC PRIVATE KEY-----", header);
    assert!(scan(&fake).is_some(), "EC PEM must be caught");
}

#[test]
fn pem_header_alone_is_a_mention_not_a_key() {
    // A documentation page names the header label with no END marker and
    // no base64 under it: the format is mentioned, no key is present.
    let header = ["-----BEGIN RSA", " PRIVATE KEY-----"].concat(); // gitleaks:allow
    let doc = format!(
        "A PEM private key file starts with the line `{}` and the key\n\
             material follows it on wrapped lines. Keep such files out of chat.\n",
        header
    );
    assert!(
        scan(&doc).is_none(),
        "a header with no body must not be reported as a key"
    );
    // Positive control on the same predicate: the same header with a
    // matching END marker is a block, however short its body.
    let block = format!("{}\nMIIEo\u{2026}\n-----END RSA PRIVATE KEY-----", header);
    assert_eq!(scan(&block).unwrap().detector, "pem-private-key");
}

#[test]
fn pem_many_headers_without_bodies_are_accepted() {
    // Eight header mentions across a page (the second reported shape),
    // none followed by an END marker or a base64 line.
    let header = ["-----BEGIN EC", " PRIVATE KEY-----"].concat(); // gitleaks:allow
    let mut doc = String::new();
    for kind in [
        "RSA",
        "EC",
        "DSA",
        "OPENSSH",
        "ENCRYPTED",
        "",
        "PGP",
        "X25519",
    ] {
        doc.push_str(&format!(
            "Use `-----BEGIN {} PRIVATE KEY-----` for this key type.\n",
            kind
        ));
    }
    assert!(doc.matches("-----BEGIN").count() == 8);
    assert!(scan(&doc).is_none(), "{:?}", scan(&doc));
    // Same page with one real block appended is refused, and the
    // candidate is that block, not the page from the first mention down.
    let block = format!("{header}\nMHQCAQEE\u{2026}\n-----END EC PRIVATE KEY-----\n");
    let with_block = format!("{doc}{block}");
    let m = scan(&with_block).unwrap();
    assert_eq!(m.detector, "pem-private-key");
    assert!(
        m.masked
            .ends_with(&format!("...{}chars", block.chars().count())),
        "masked: {}",
        m.masked
    );
}

#[test]
fn pem_header_with_base64_body_and_no_end_marker_is_refused() {
    // Key material pasted without its END line is still a key.
    let header = ["-----BEGIN RSA", " PRIVATE KEY-----"].concat(); // gitleaks:allow
    let body_line = "MIIEowIBAAKCAQEA0Z3VS5JJcds3xfn/ygWyF8PbnGYPYFqHlZ4kUmUqQ7fEd7Uw";
    let trailing =
        "and then a long paragraph of ordinary prose that is not part of the key block at all";
    let text = format!("{header}\n{body_line}\n{body_line}\n\n{trailing}\n");
    let m = scan(&text).expect("a header with a base64 body is a key");
    assert_eq!(m.detector, "pem-private-key");
    // The candidate is bounded to the header plus its base64 lines.
    let block_len = header.chars().count() + 1 + (body_line.len() + 1) * 2;
    let reported_len: usize = m
        .masked
        .trim_end_matches("chars")
        .rsplit("...")
        .next()
        .and_then(|s| s.parse().ok())
        .expect("masked preview ends in the candidate length");
    assert_eq!(reported_len, block_len, "masked: {}", m.masked);
    // A short base64 tail alone (below key-block width) is not a body.
    let short = format!("{header}\nMIIEowIBAAKCAQEA\n{trailing}\n");
    assert!(scan(&short).is_none(), "{:?}", scan(&short));
    // After a full-width line, a short final line is the end of the block
    // and stays inside the candidate rather than surviving past it.
    let tail = "MIIEowIBAAKCAQEA";
    let with_tail = format!("{header}\n{body_line}\n{tail}\n{trailing}\n");
    let m = scan(&with_tail).expect("a full line plus a short tail is a key");
    let tail_len = header.chars().count() + 1 + body_line.len() + 1 + tail.len() + 1;
    assert!(
        m.masked.ends_with(&format!("...{tail_len}chars")),
        "masked: {}",
        m.masked
    );
    let masked = mask_secrets(&with_tail);
    assert!(!masked.contains(tail), "tail survived masking: {masked}");
}

#[test]
fn pem_block_inside_serialized_json_is_still_caught_and_a_mention_is_not() {
    // A note's properties or a stream payload reach the gate as compact
    // JSON, where every newline is the two-character escape. The same
    // rules apply on that form.
    let header = ["-----BEGIN RSA", " PRIVATE KEY-----"].concat(); // gitleaks:allow
    let body_line = "MIIEowIBAAKCAQEA0Z3VS5JJcds3xfn/ygWyF8PbnGYPYFqHlZ4kUmUqQ7fEd7Uw";
    let block = serde_json::json!({"payload": format!("{header}\n{body_line}\n{body_line}")});
    let serialized = serde_json::to_string(&block).unwrap();
    assert!(
        serialized.contains("\\n"),
        "fixture must carry escaped newlines"
    );
    let m = scan(&serialized).expect("a key block in serialized JSON is a key");
    assert_eq!(m.detector, "pem-private-key");
    // Bounded to the block: the closing quote and brace are not part of it.
    let block_len = header.chars().count() + 2 + (body_line.len() + 2) + body_line.len();
    assert!(
        m.masked.ends_with(&format!("...{block_len}chars")),
        "masked: {}",
        m.masked
    );
    let with_end = serde_json::json!({
        "payload": format!("{header}\nMIIEo\u{2026}\n-----END RSA PRIVATE KEY-----\ntrailing")
    });
    let serialized = serde_json::to_string(&with_end).unwrap();
    assert_eq!(scan(&serialized).unwrap().detector, "pem-private-key");

    let mention = serde_json::json!({
        "doc": format!("A key file starts with `{header}` and continues on wrapped lines.")
    });
    let serialized = serde_json::to_string(&mention).unwrap();
    assert!(scan(&serialized).is_none(), "{:?}", scan(&serialized));
}

#[test]
fn pem_block_after_an_unrelated_begin_marker_is_still_caught() {
    // A certificate header earlier in the text must not hide the key
    // block behind it, and the candidate starts at the key header.
    let header = ["-----BEGIN RSA", " PRIVATE KEY-----"].concat(); // gitleaks:allow
    let text = format!(
        "-----BEGIN CERTIFICATE-----\nMIIB\u{2026}\n-----END CERTIFICATE-----\n{}\nMIIEo\u{2026}\n-----END RSA PRIVATE KEY-----\n",
        header
    );
    let m = scan(&text).expect("the key block must be caught");
    assert_eq!(m.detector, "pem-private-key");
    assert!(
        !m.masked.starts_with("-----BEGIN C"),
        "candidate must start at the key header: {}",
        m.masked
    );
}

#[test]
fn blocks_age_secret_key() {
    // AGE-SECRET-KEY- followed by 59 base32 chars (Bech32m body).
    let fake = "AGE-SECRET-KEY-1QQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQ";
    assert!(scan(fake).is_some(), "AGE-SECRET-KEY- must be caught");
    assert_eq!(scan(fake).unwrap().detector, "age-secret-key");
}

#[test]
fn blocks_jwt_triple() {
    // Synthetic JWT structure: header.payload.signature (no real key).
    let fake =
        "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.FAKE_SIG_XXXXXXXXXXXX"; // gitleaks:allow
    assert!(scan(fake).is_some(), "JWT triple must be caught");
    assert_eq!(scan(fake).unwrap().detector, "jwt");
}

#[test]
fn blocks_url_userinfo() {
    let fake = "postgresql://dbuser:S3cr3tP4ss@db.example.com:5432/mydb";
    assert!(scan(fake).is_some(), "URL userinfo must be caught");
    assert_eq!(scan(fake).unwrap().detector, "url-userinfo");
}

#[test]
fn url_userinfo_placeholder_refusal_explains_effective_remedies() {
    for content in [
        "postgresql://dbuser:<PASSWORD>@db.example.com:5432/mydb",
        "postgresql://<USER>:<PASSWORD>@db.example.com:5432/mydb",
        "redis://:<PASSWORD>@cache.example.com:6379",
    ] {
        let error = check(content).expect_err("URL credential placeholders still match");
        let RuntimeError::SecretDetected(matched) = &error else {
            panic!("expected a secret refusal, got {error}");
        };
        assert_eq!(matched.detector, "url-userinfo");
        let rendered = error.to_string();
        for guidance in [
            "Placeholders in URL credential positions still match",
            "Replace the whole URL",
            "environment-variable name or config key",
            "remove the entire user/password segment",
        ] {
            assert!(
                rendered.contains(guidance),
                "missing {guidance:?}: {rendered}"
            );
        }
        assert!(!rendered.contains(content), "refusal must not echo the URL");
    }
}

#[test]
fn url_userinfo_guidance_remedies_are_accepted() {
    for content in [
        "Use the DATABASE_URL environment variable.",
        "Use the database.connection_url config key.",
        "postgresql://db.example.com:5432/mydb",
        "redis://cache.example.com:6379",
    ] {
        assert!(
            check(content).is_ok(),
            "URL reference or URL without credentials must be accepted: {content:?}"
        );
    }
}

#[test]
fn blocks_and_masks_url_userinfo_with_short_passwords() {
    for password in ["a", "ab", "abc"] {
        let content =
            format!("gate backend probe failed: postgres://svc:{password}@internal-host refused");

        let detected = scan(&content).expect("short URL password must be detected");
        assert_eq!(detected.detector, "url-userinfo");
        assert!(
            check(&content).is_err(),
            "write gate must block {content:?}"
        );
        assert_eq!(
            bounded_masked_log_text(&content),
            "gate backend probe failed: ***MASKED*** refused",
            "log boundary must mask {content:?}"
        );
    }
}

#[test]
fn keeps_url_without_userinfo() {
    let content = "gate backend probe failed: postgres://host:8080/path refused";

    assert!(check(content).is_ok(), "host:port is not URL userinfo");
    assert_eq!(bounded_masked_log_text(content), content);
}

#[test]
fn blocks_and_masks_empty_username_url_passwords() {
    // Standard empty-user connection strings: the password is the
    // credential whether or not a username precedes the colon.
    for password in ["a", "ab", "%40", "密码"] {
        let content =
            format!("gate backend probe failed: redis://:{password}@internal-host refused");

        let detected = scan(&content).expect("empty-username URL password must be detected");
        assert_eq!(detected.detector, "url-userinfo");
        assert!(
            check(&content).is_err(),
            "write gate must block {content:?}"
        );
        assert_eq!(
            bounded_masked_log_text(&content),
            "gate backend probe failed: ***MASKED*** refused",
            "log boundary must mask {content:?}"
        );
    }
}

#[test]
fn keeps_colon_at_pairs_outside_the_authority() {
    // An `@` past the authority boundary is path/query/fragment text,
    // not userinfo: `host/a:x@next` must not read as user `host/a` with
    // password `x`.
    for content in [
        "see https://host/a:x@next for details",
        "see https://host?time=12:30@zone for details",
        "see https://host#frag:1@anchor for details",
    ] {
        assert!(
            check(content).is_ok(),
            "path/query/fragment `:`+`@` text is not userinfo: {content:?}"
        );
        assert_eq!(bounded_masked_log_text(content), content);
    }
}

#[test]
fn blocks_high_entropy_near_bearer_word() {
    // 32 random-looking base64 chars adjacent to the word "bearer".
    let fake = "Bearer token: Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvM"; // gitleaks:allow
    assert!(
        scan(fake).is_some(),
        "high-entropy value near 'bearer' must be caught"
    );
    assert_eq!(scan(fake).unwrap().detector, "high-entropy-token");
}

#[test]
fn blocks_high_entropy_near_secret_word() {
    let fake = "secret=Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvM"; // gitleaks:allow
    assert!(
        scan(fake).is_some(),
        "high-entropy value near 'secret' must be caught"
    );
}

#[test]
fn error_message_masks_secret() {
    let fake = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let m = scan(fake).unwrap();
    // Masked form: first 6 chars + "...N chars".
    // Must NOT contain the full suffix.
    let masked = &m.masked;
    assert!(
        !masked.contains("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
        "mask must not echo the full secret value; got: {masked}"
    );
    // Must start with "ghp_AA" (first 6 chars of the token).
    assert!(
        masked.starts_with("ghp_AA"),
        "mask must show first 6 chars; got: {masked}"
    );
}

// ── False-positive suite ─────────────────────────────────────────────────

#[test]
fn allows_sha256_hex() {
    // 64-char lowercase hex — typical sha256 digest.
    let sha = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    assert!(
        scan(sha).is_none(),
        "sha256 hex must pass (allowlisted); fired: {:?}",
        scan(sha)
    );
}

#[test]
fn allows_uuid() {
    let uuid = "550e8400-e29b-41d4-a716-446655440000";
    assert!(
        scan(uuid).is_none(),
        "UUID must pass; fired: {:?}",
        scan(uuid)
    );
}

#[test]
fn allows_git_sha() {
    // 40-char lowercase git SHA.
    let sha = "d362950a3c9b1a4cb47d97f1623e38f1a1e6bcdf";
    assert!(
        scan(sha).is_none(),
        "git SHA must pass; fired: {:?}",
        scan(sha)
    );
}

#[test]
fn allows_normal_prose() {
    let prose =
        "The FlashAttention paper introduces IO-aware tiling for transformer self-attention.";
    assert!(scan(prose).is_none(), "normal prose must pass");
}

#[test]
fn allows_code_snippet() {
    let code = r#"fn create_entity(name: &str, kind: &str) -> RuntimeResult<Entity> {
    self.validate_entity_kind(kind)?;
    Ok(Entity::new("local", kind, name))
}"#;
    assert!(
        scan(code).is_none(),
        "code snippet must pass; fired: {:?}",
        scan(code)
    );
}

#[test]
fn allows_long_url_without_credentials() {
    let url = "https://docs.example.com/api/v2/entities?kind=concept&limit=100";
    assert!(scan(url).is_none(), "URL without userinfo must pass");
}

#[test]
fn allows_base64_image_stub() {
    // Realistic short base64 data URI stub — no trigger words, below threshold length.
    let b64 =
        "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVQI12NgAAIABQ";
    assert!(
        scan(b64).is_none(),
        "base64 image stub without trigger word must pass; fired: {:?}",
        scan(b64)
    );
}

#[test]
fn allows_long_plain_url() {
    let url = "https://api.github.com/repos/ohdearquant/khive/pulls/76/comments?per_page=100";
    assert!(
        scan(url).is_none(),
        "plain URL must pass; fired: {:?}",
        scan(url)
    );
}

#[test]
fn allows_manifest_content_hash() {
    // A string like what appears in Cargo.lock or npm lockfiles.
    let line = "checksum = \"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\"";
    assert!(
        scan(line).is_none(),
        "manifest content hash line must pass; fired: {:?}",
        scan(line)
    );
}

#[test]
fn masked_excerpt_format() {
    let fake = "AKIAFAKEKEY1234567890";
    let m = scan(fake).unwrap();
    // Format: first6...Nchars
    assert!(m.masked.contains("..."), "masked must contain '...'");
    assert!(m.masked.ends_with("chars"), "masked must end with 'chars'");
}

// ── Gate function ────────────────────────────────────────────────────────

#[test]
fn check_returns_ok_for_safe_content() {
    assert!(check("A normal memory note about LoRA.").is_ok());
}

#[test]
fn check_returns_err_for_secret() {
    let fake = "AKIAFAKEKEY1234567890";
    let result = check(fake);
    assert!(result.is_err(), "check must fail for AKIA key");
    let err = result.unwrap_err();
    assert!(
        matches!(err, RuntimeError::SecretDetected(_)),
        "error variant must be SecretDetected"
    );
}

// ── Entropy helpers ──────────────────────────────────────────────────────

#[test]
fn entropy_of_uniform_string_is_zero() {
    let s = "aaaaaaaaaaaaaaaa";
    assert!(shannon_entropy(s.as_bytes()) < 0.01);
}

#[test]
fn entropy_of_random_bytes_is_high() {
    // A truly random-looking string should exceed 4.5 bits/char.
    let s = b"X9kZ2vQpLrT8nJwYuAeHfBsDcGiONvM1"; // 32 mixed base64 chars
    assert!(shannon_entropy(s) > 4.5, "entropy={}", shannon_entropy(s));
}

#[test]
fn cjk_prose_near_trigger_is_not_flagged() {
    // Regression: a multibyte CJK run (~19 chars = 57 bytes) clears the
    // byte-length floor, and `shannon_entropy` over UTF-8 bytes reads it as
    // high-entropy — so a Chinese title near the `auth` trigger word used to
    // false-positive as `high-entropy-token`.  Non-ASCII tokens are now
    // skipped by the entropy heuristic: real base64/hex credentials are
    // ASCII, so this cannot hide a secret.
    let content = "更新 auth 配置数据库连接管理系统核心模块设计文档";
    assert!(
        check(content).is_ok(),
        "CJK prose near a trigger word must not be flagged as a secret"
    );
}

#[test]
fn ascii_secret_near_trigger_still_flagged() {
    // The non-ASCII skip must NOT weaken detection of genuine ASCII
    // high-entropy credentials near a trigger word.
    let content = "api_key X9kZ2vQpLrT8nJwYuAeHfBsDcGiONvM1";
    assert!(
        check(content).is_err(),
        "ASCII high-entropy token near a trigger word must still be blocked"
    );
}

#[test]
fn ascii_secret_in_cjk_context_does_not_panic_and_is_flagged() {
    // The ±120-byte trigger window around an ASCII token can land in the
    // middle of a multibyte CJK character when the token is embedded in
    // non-Latin prose.  Slicing on a non-char-boundary would panic — the
    // window bounds are snapped via `floor_char_boundary`.  Detection of
    // the genuine ASCII secret must still fire.
    let cjk = "数据库连接管理系统核心模块设计文档".repeat(6); // 17 chars × 6 = 306 bytes
                                                              // The leading single-byte `x` breaks 3-byte CJK alignment so the window
                                                              // start (token_offset - 120) lands mid-character without the snap.
    let content = format!("{cjk}x api_key X9kZ2vQpLrT8nJwYuAeHfBsDcGiONvM1 {cjk}");
    assert!(
        check(&content).is_err(),
        "ASCII secret in CJK context must still be blocked (and must not panic)"
    );
}

#[test]
fn ascii_secret_glued_to_cjk_is_still_flagged() {
    // Regression: a prefixless high-entropy credential glued (no ASCII
    // whitespace) to CJK text, CJK brackets/quotes, a fullwidth space, or a
    // fullwidth colon used to slip through, because the whole whitespace token
    // contained a non-ASCII byte and was skipped wholesale.  Non-ASCII is now
    // a token delimiter, so the ASCII credential run is isolated and
    // entropy-checked while the surrounding ±120-byte window still sees the
    // trigger word.
    let secret = "X9kZ2vQpLrT8nJwYuAeHfBsDcGiONvM1"; // gitleaks:allow
    let cases = [
        format!("api_key {secret}数据"),     // CJK suffix glued to the token
        format!("api_key 「{secret}」"),     // CJK brackets wrap the token
        format!("api_key　{secret}"),        // U+3000 ideographic space separator
        format!("api_key：{secret}"),        // U+FF1A fullwidth colon separator
        format!("数据{secret}更新 api_key"), // CJK-glued prefix, trigger after
    ];
    for content in &cases {
        assert!(
            check(content).is_err(),
            "ASCII secret glued to CJK must be blocked: {content:?}"
        );
    }
}

#[test]
fn high_entropy_ascii_run_without_trigger_is_not_flagged() {
    // The non-ASCII-as-delimiter change must not weaken the trigger-context
    // discipline: a high-entropy ASCII run isolated from CJK prose but NOT
    // near a credential trigger word is still allowed (only the tokenizer
    // changed, not the `near_trigger` gate).
    let secret = "X9kZ2vQpLrT8nJwYuAeHfBsDcGiONvM1"; // gitleaks:allow
    let content = format!("数据库连接{secret}核心模块设计文档");
    assert!(
        check(&content).is_ok(),
        "high-entropy ASCII run with no trigger word must not be flagged"
    );
}

#[test]
fn known_prefix_secret_glued_after_cjk_is_still_flagged() {
    // A Layer-1 known-prefix secret glued directly after
    // CJK prose (no ASCII whitespace) was missed, because the prefix boundary
    // check used `is_alphanumeric` — which Rust counts true for CJK — so the
    // preceding ideograph was not treated as a delimiter.  These credentials
    // must be caught with no nearby ASCII trigger word, on the left side too.
    let cases = [
        "数据AKIAIOSFODNN7EXAMPLE".to_owned(), // gitleaks:allow
        format!("令牌{}", github_fine_grained_pat_fixture()),
        format!("密钥{}", anthropic_api_key_fixture()),
        "配置FlyV1 fm2_AAAABBBBCCCCDDDD".to_owned(), // gitleaks:allow
    ];
    for content in &cases {
        assert!(
            check(content).is_err(),
            "known-prefix secret glued after CJK must be blocked: {content:?}"
        );
    }
}

#[test]
fn url_userinfo_after_cjk_does_not_panic_and_is_flagged() {
    // A credential URL glued after CJK prose panicked,
    // because scheme_start was (separator byte index + 1) — one byte into a
    // multibyte CJK separator — and the slice fell on a non-char boundary.
    // The public check() API must return a controlled error, never panic.
    let cases = [
        "数据postgresql://dbuser:S3cr3tP4ss@db.example.com/db", // gitleaks:allow
        "配置mysql://root:hunter2pw@10.0.0.1:3306/app",         // gitleaks:allow
        "连接redis://svc:V3ryS3cretPw@cache.internal:6379",     // gitleaks:allow
    ];
    for content in cases {
        assert!(
            check(content).is_err(),
            "credential URL after CJK must be blocked, not panic: {content:?}"
        );
    }
}

#[test]
fn non_ascii_glued_token_trigger_is_still_flagged() {
    // `token=`/`token:`/standalone `token` glued directly
    // after non-ASCII prose was missed because has_standalone_token /
    // has_token_assignment used is_alphanumeric for the word boundary — CJK,
    // accented letters, and fullwidth digits all count as alphanumeric in
    // Rust, so the preceding char was not seen as a boundary and the `token`
    // trigger was suppressed, leaving the high-entropy value unflagged.
    let opaque = "Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvMabcdef"; // gitleaks:allow
    let blocked = [
        format!("数据token={opaque}"),    // CJK + assignment form, ASCII '='
        format!("配置token: {opaque}"),   // CJK + assignment form, ASCII ':'
        format!("密钥token {opaque}"),    // CJK + standalone-word form
        format!("résumétoken: {opaque}"), // accented letter before `token`
        format!("１token: {opaque}"),     // fullwidth digit before `token`
    ];
    for content in &blocked {
        assert!(
            check(content).is_err(),
            "non-ASCII-glued token trigger must flag the value: {content:?}"
        );
    }
    // Compound identifiers stay excluded — the `_` boundary rule is unchanged
    // and an ASCII letter before `token` is still a continuation, so these
    // (including the pure-ASCII `servicetoken:`) must still pass.
    let allowed = [
        format!("数据next_token: {opaque}"),
        format!("数据token_count: {opaque}"),
        format!("servicetoken: {opaque}"),
    ];
    for content in &allowed {
        assert!(
            check(content).is_ok(),
            "compound token identifier must not be flagged: {content:?}"
        );
    }
}

#[test]
fn allowlist_passes_sha256() {
    // A plain sha256 hex digest passes via `is_pure_hex` (not `is_allowlisted`
    // because hex is now context-dependent; this tests the primitive directly).
    let sha = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    assert!(is_pure_hex(sha));
}

#[test]
fn allowlist_passes_uuid_canonical() {
    assert!(is_uuid_canonical("550e8400-e29b-41d4-a716-446655440000"));
}

#[test]
fn allowlist_does_not_pass_mixed_token() {
    // A token that starts with letters but mixes in non-hex chars.
    assert!(!is_pure_hex("sk-aaaaaabbbbbbccccccddddddeeeeeeffffgg"));
}

// ── Structured-field gate helpers ────────────────────────────────────────

#[test]
fn check_json_blocks_secret_in_object_value() {
    let props = serde_json::json!({ "api_key": "AKIAFAKEKEY1234567890" });
    assert!(
        check_json(&props).is_err(),
        "secret in properties object value must be blocked"
    );
}

#[test]
fn check_json_blocks_secret_in_nested_object() {
    let props = serde_json::json!({
        "credentials": { "token": openai_project_key_fixture() }
    });
    assert!(
        check_json(&props).is_err(),
        "secret in nested properties object must be blocked"
    );
}

#[test]
fn check_json_blocks_secret_in_array() {
    let props = serde_json::json!(["normal", "AKIAFAKEKEY1234567890"]);
    assert!(
        check_json(&props).is_err(),
        "secret in JSON array must be blocked"
    );
}

// ── Reserved secret-gate property key (ADR-115 Amendment 1) ─────────────

#[test]
fn reject_reserved_key_passes_absent_properties() {
    assert!(reject_reserved_secret_gate_property(None).is_ok());
}

#[test]
fn reject_reserved_key_passes_unrelated_properties() {
    let props = serde_json::json!({"name": "value", "tags": ["a", "b"]});
    assert!(reject_reserved_secret_gate_property(Some(&props)).is_ok());
}

#[test]
fn reject_reserved_key_passes_non_object_properties() {
    // Non-object properties cannot name a top-level key at all.
    let props = serde_json::json!("just a string");
    assert!(reject_reserved_secret_gate_property(Some(&props)).is_ok());
    let arr = serde_json::json!(["a", "b"]);
    assert!(reject_reserved_secret_gate_property(Some(&arr)).is_ok());
}

#[test]
fn reject_reserved_key_blocks_top_level_key_creation() {
    let props = serde_json::json!({"khive:secret_gate": "exempted:content-sha256-manifest-v1"});
    let err = reject_reserved_secret_gate_property(Some(&props)).unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidInput(ref msg) if msg.contains("khive:secret_gate") && msg.contains("runtime-owned")),
        "unexpected error: {err:?}"
    );
}

#[test]
fn reject_reserved_web_receipt_provenance_in_generic_properties() {
    for value in [serde_json::json!("v1"), serde_json::Value::Null] {
        let props = serde_json::json!({"khive:web_receipt": value});
        let error = reject_reserved_secret_gate_property(Some(&props)).unwrap_err();
        assert!(
            matches!(error, RuntimeError::InvalidInput(message) if message.contains("khive:web_receipt"))
        );
    }
}

#[test]
fn reject_reserved_key_blocks_regardless_of_value_shape() {
    // Presence alone is rejected — arbitrary value, null (explicit removal
    // shape), and a value that happens to match the real stamp format are
    // all rejected identically; a caller can never legitimately write this
    // key by any value.
    for value in [
        serde_json::json!(null),
        serde_json::json!(42),
        serde_json::json!({"nested": "object"}),
        serde_json::json!("exempted:content-sha256-manifest-v1"),
    ] {
        let props = serde_json::json!({"khive:secret_gate": value});
        assert!(
            reject_reserved_secret_gate_property(Some(&props)).is_err(),
            "must reject value shape: {props:?}"
        );
    }
}

#[test]
fn reject_reserved_key_allows_nested_non_top_level_occurrence() {
    // The same spelling nested inside a value is ordinary content, not a
    // posture mutation — only the exact top-level key is reserved.
    let props = serde_json::json!({"notes": {"khive:secret_gate": "not-a-stamp"}});
    assert!(reject_reserved_secret_gate_property(Some(&props)).is_ok());
}

#[test]
fn reject_reserved_key_blocks_alongside_other_legitimate_keys() {
    let props = serde_json::json!({
        "name": "value",
        "khive:secret_gate": "exempted:content-sha256-manifest-v1",
    });
    assert!(reject_reserved_secret_gate_property(Some(&props)).is_err());
}

#[test]
fn check_json_passes_safe_properties() {
    let props = serde_json::json!({
        "domain": "attention",
        "status": "researched",
        "year": 2024
    });
    assert!(
        check_json(&props).is_ok(),
        "normal properties must pass; fired: {:?}",
        check_json(&props).err()
    );
}

#[test]
fn check_tags_blocks_credential_tag() {
    let tags = vec![
        "type:concept".to_string(),
        "AKIAFAKEKEY1234567890".to_string(),
    ];
    assert!(
        check_tags(&tags).is_err(),
        "credential-shaped tag must be blocked"
    );
}

#[test]
fn check_tags_passes_normal_tags() {
    let tags = vec!["type:concept".to_string(), "domain:attention".to_string()];
    assert!(
        check_tags(&tags).is_ok(),
        "normal tags must pass; fired: {:?}",
        check_tags(&tags).err()
    );
}

// ── False-positive: sk-learn and scikit-learn slugs ──────────────────────

#[test]
fn allows_sk_learn_prose() {
    // scikit-learn slug used as an entity name or knowledge atom.
    let texts = &[
        "sk-learn is a Python machine learning library",
        "sk-learn-compatible transformer pipeline reference",
        "sk-learn scikit-learn estimator interface",
    ];
    for t in texts {
        assert!(
            scan(t).is_none(),
            "sk-learn prose must pass; fired: {:?} on {:?}",
            scan(t),
            t
        );
    }
}

#[test]
fn blocks_openai_sk_proj_not_confused_with_sk_learn() {
    // Real OpenAI key shape must still be caught.
    let fake = openai_project_key_fixture();
    assert!(
        scan(&fake).is_some(),
        "sk-proj- key must still be caught after sk-learn exemption"
    );
}

// ── False-positive: SRI / tokenizer hash metadata ────────────────────────

#[test]
fn blocks_sri_hash_near_key_word_accepted_fp() {
    // SRI hash as used in HTML integrity attributes (sha384, base64-encoded),
    // placed directly beside the trigger word "key". The content-hash
    // allowlist is a prose-context exemption, not unconditional: near a
    // credential trigger, a sha-prefixed hash falls through to the explicit
    // near-trigger content-hash detector like any other high-entropy
    // candidate. This is an accepted false positive on a real but rare
    // shape (an integrity hash literally next to the word "key").
    let line =
        "integrity key: sha384-oqVuAfXRKap7fdgcCY5uykM6+R9GqQ8K/uxy9rx7HNQlGYl1kPzQho1wx4JwY8wC";
    assert!(
        scan(line).is_some(),
        "SRI hash near trigger word 'key' must now be blocked (accepted FP); passed unexpectedly"
    );
}

#[test]
fn allows_base64_tokenizer_hash_metadata() {
    // Tokenizer metadata containing a base64 hash near technical keywords.
    let line = "tokenizer_vocab_hash: Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvM"; // gitleaks:allow
    assert!(
        scan(line).is_none(),
        "tokenizer hash metadata must pass; fired: {:?}",
        scan(line)
    );
}

#[test]
fn allows_npm_lockfile_integrity() {
    // npm lockfile integrity line with sha512 base64url hash (86 base64 chars + ==).
    // sha512 digest = 64 bytes → base64 = 88 chars (86 unpadded + ==).
    let body_86 =
        "Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvM1234567890abcdefghijklmnopqrstuvwxABCDEFGHIJKLMNOPQRST";
    assert_eq!(body_86.len(), 86, "test body must be exactly 86 chars");
    let line = format!(
        "resolved: https://registry.npmjs.org/foo/-/foo-1.0.0.tgz\nintegrity: sha512-{body_86}=="
    );
    assert!(
        scan(&line).is_none(),
        "npm lockfile integrity must pass; fired: {:?}",
        scan(&line)
    );
}

// ── False-positive: tokenizer vs token trigger word ─────────────────────

#[test]
fn allows_tokenizer_vocab_hash_no_block() {
    // `tokenizer_vocab_hash` contains the substring "token" but NOT as a
    // standalone word (followed by 'i' which is alphanumeric), so the
    // standalone-token boundary check must not fire here.
    let line = "tokenizer_vocab_hash = Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvM"; // gitleaks:allow
    assert!(
        scan(line).is_none(),
        "tokenizer_vocab_hash must pass; 'token' is only standalone-word matched; fired: {:?}",
        scan(line)
    );
}

// ── True-positives: bare base64 at sha-lengths near trigger words ────────

#[test]
fn blocks_bare_base64url_43chars_near_key() {
    // A 43-char base64url token (= sha256 body length) near the word "key".
    // Without a sha<N>- prefix this MUST be caught, not allowlisted.
    let token_43 = "wJalrXUtnFEMI-K7MDENGbPxRfiCYEXAMPLEKEYX123"; // gitleaks:allow
    assert_eq!(token_43.len(), 43, "test token must be exactly 43 chars");
    let line = format!("api key {token_43}");
    assert!(
        scan(&line).is_some(),
        "43-char base64url token near 'key' must be caught (no sha-prefix = not a hash); fired: {:?}",
        scan(&line)
    );
}

#[test]
fn blocks_bare_base64url_64chars_near_secret() {
    // A 64-char base64url token (= sha384 body length) near "secret".
    // Must be caught without sha<N>- prefix.
    let token_64 = "wJalrXUtnFEMI-K7MDENGbPxRfiCYEXAMPLEKEYX123wJalrXUtnFEMI-K7MDENa"; // gitleaks:allow
    assert_eq!(token_64.len(), 64, "test token must be exactly 64 chars");
    let line = format!("secret: {token_64}");
    assert!(
        scan(&line).is_some(),
        "64-char base64url token near 'secret' must be caught; got: {:?}",
        scan(&line)
    );
}

#[test]
fn blocks_bare_base64url_86chars_near_auth() {
    // An 86-char base64url token (= sha512 body length) near "auth".
    // Must be caught without sha<N>- prefix.
    let token_86 =
        "wJalrXUtnFEMI-K7MDENGbPxRfiCYEXAMPLEKEYX123wJalrXUtnFEMI-K7MDENwJalrXUtnFEMI-K7MDENabc"; // gitleaks:allow
    assert_eq!(token_86.len(), 86, "test token must be exactly 86 chars");
    let line = format!("auth header {token_86}");
    assert!(
        scan(&line).is_some(),
        "86-char base64url token near 'auth' must be caught; got: {:?}",
        scan(&line)
    );
}

// ── True-positives: standalone `token` trigger ───────────────────────────

#[test]
fn blocks_service_token_opaque_value() {
    // "service token <opaque-high-entropy>" — `token` as a standalone word
    // with a high-entropy value must be caught.
    let opaque = "Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvMabcdef"; // gitleaks:allow
    assert!(
        opaque.len() >= 24,
        "opaque must be long enough for entropy check"
    );
    let line = format!("service token {opaque}");
    assert!(
        scan(&line).is_some(),
        "service token <opaque> must be caught by standalone 'token' check; got: {:?}",
        scan(&line)
    );
}

#[test]
fn blocks_token_equals_credential() {
    // `token=<high-entropy>` (assignment form) must be caught via has_token_assignment.
    let opaque = "Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvMabcdef"; // gitleaks:allow
    let line = format!("token={opaque}");
    assert!(
        scan(&line).is_some(),
        "token=<value> must be caught via token= trigger; got: {:?}",
        scan(&line)
    );
}

#[test]
fn blocks_token_colon_credential() {
    // `token: <high-entropy>` (key-value form) must be caught via has_token_assignment.
    let opaque = "Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvMabcdef"; // gitleaks:allow
    let line = format!("token: {opaque}");
    assert!(
        scan(&line).is_some(),
        "token: <value> must be caught via token: trigger; got: {:?}",
        scan(&line)
    );
}

#[test]
fn allows_next_token_technical_context() {
    // `next_token` is a technical term; the high-entropy value here has low
    // entropy anyway, so it must pass.
    let line = "next_token: cursor-page-2-abcdef12345678";
    assert!(
        scan(line).is_none(),
        "next_token technical context must not be blocked; fired: {:?}",
        scan(line)
    );
}

// ── Boundary-aware token= / token: (compound identifiers must pass) ─────

#[test]
fn allows_next_token_high_entropy_cursor() {
    // `next_token:` with a realistic high-entropy pagination cursor must NOT be
    // blocked.  `next_token` has `_token` suffix — not a standalone assignment form.
    let cursor = "Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvMabcdef"; // gitleaks:allow
    let line = format!("next_token: {cursor}");
    assert!(
        scan(&line).is_none(),
        "next_token with high-entropy cursor must pass (compound identifier); fired: {:?}",
        scan(&line)
    );
}

#[test]
fn allows_token_count_high_entropy() {
    // `token_count:` with a high-entropy value must NOT be blocked.
    // `token_count` has `token_` prefix — the word boundary after `token` is `_`,
    // which is excluded by has_token_assignment.
    let opaque = "Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvMabcdef"; // gitleaks:allow
    let line = format!("token_count: {opaque}");
    assert!(
        scan(&line).is_none(),
        "token_count with high-entropy value must pass; fired: {:?}",
        scan(&line)
    );
}

// ── Hex allowlist is not applied when trigger context is present ────────
// Pure hex tops out at log2(16) = 4.0 bits/char, below ENTROPY_THRESHOLD (4.5), so
// the entropy heuristic alone never flags it. The hex allowlist must only apply
// when NOT near a trigger; the tests below guard that ordering.

#[test]
fn hex_near_key_blocked_in_credential_context() {
    // A pure-hex 32-char token near "api key" is a credential-shaped hex
    // token in trigger context.  Entropy alone cannot flag it (hex max =
    // 4.0 < 4.5 threshold), but the explicit hex-credential-token path
    // must catch it.
    let hex32 = "4f9c2e8a1d3b5c7e9f0a2b4d6e8c0a2b";
    assert_eq!(hex32.len(), 32);
    let line = format!("api key {hex32}");
    assert!(
        scan(&line).is_some(),
        "32-char pure hex near 'api key' must be blocked; got None"
    );
}

#[test]
fn issue_2056_hex_bridge_masks_the_matched_candidate() {
    let hex32 = "0af7651916cd43dd8448eb211c80319c";
    let content = format!("api key values verbatim:\n  {hex32}");

    let (candidate, detector) = scan_match(&content).expect("credential must have a span");
    assert_eq!(detector, "hex-credential-token");
    assert_eq!(candidate, hex32);
    assert!(candidate.bytes().all(|byte| byte.is_ascii_hexdigit()));

    let matched = scan(&content).expect("credential-labeled hex must be blocked");
    assert_eq!(matched.detector, "hex-credential-token");
    assert_eq!(matched.masked, "0af765...32chars");
}

#[test]
fn issue_2056_detector_name_does_not_trigger_across_sentence_boundary() {
    let content = "The hex-credential-token bucket held 18. Six are two values verbatim:\n\
                       0af7651916cd43dd8448eb211c80319c -- spec example identifier";

    assert!(
        check(content).is_ok(),
        "a detector name in an earlier sentence must not trigger: {:?}",
        scan(content)
    );
}

#[test]
fn issue_2076_allows_repository_revision_links_near_prose_triggers() {
    let revision = "d362950a3c9b1a4cb47d97f1623e38f1a1e6bcdf";
    let contents = [
        format!(
            "key-scoped source citation: \
                 https://github.com/acme/widgets/blob/{revision}/src/runtime/mod.rs"
        ),
        format!("auth-bound rendering keeps <a href=\"{revision}\">the commit link</a> intact"),
    ];

    for content in contents {
        assert!(
            check(&content).is_ok(),
            "repository revision reference must pass: {content:?}, got {:?}",
            scan(&content)
        );
    }
}

#[test]
fn issue_2076_repository_revision_links_do_not_hide_labeled_secrets() {
    let revision = "d362950a3c9b1a4cb47d97f1623e38f1a1e6bcdf";
    let contents = [
        format!("api key: https://github.com/acme/widgets/blob/{revision}/src/runtime/mod.rs"),
        format!("secret key: href=\"{revision}\""),
    ];

    for content in contents {
        assert!(
            check(&content).is_err(),
            "a direct credential label must still block: {content:?}"
        );
    }
}

#[test]
fn issue_2076_guidance_names_a_boundary_that_changes_the_predicate() {
    let guidance = block_guidance("high-entropy-token");
    assert!(guidance.contains("sentence") || guidance.contains("paragraph"));
    assert!(
        !guidance.contains("own line"),
        "a single newline remains inside the trigger window"
    );
}

#[test]
fn issue_1988_allows_dense_latex_fragments_near_math_vocabulary() {
    let latex = r"\operatorname{Spec}_{H_0^1(\Omega)}(K_N(\tau_{\omega}))^{1/2}";
    assert!(latex.len() >= MIN_ENTROPY_LEN);
    assert!(shannon_entropy(latex.as_bytes()) >= ENTROPY_THRESHOLD);
    let content = format!("the attention key estimate uses {latex}");

    assert!(
        check(&content).is_ok(),
        "LaTeX structure must not be mistaken for a credential: {:?}",
        scan(&content)
    );
}

#[test]
fn issue_1988_latex_exemption_does_not_hide_credential_runs() {
    let credential = "a3f5c2e9d1b8047e63a1f4c2d5b6e8f1a9c3d2e4"; // gitleaks:allow
    let content = format!(r"api key: \texttt{{{credential}}}");

    assert!(
        check(&content).is_err(),
        "credential-shaped runs inside LaTeX must remain blocked"
    );
}

#[test]
fn issue_1988_published_secret_vectors_remain_fail_closed() {
    // RFC 8032 Ed25519ph example secret. Published status cannot be
    // inferred from shape, so exact vectors remain blocked unless a
    // caller uses the separately audited exemption contract.
    let published = "833fe62409237b9d62ec77587520911e9a759cec1d19755b7da901b96dca3d42";
    let content = format!("RFC 8032 secret key test vector: {published}");

    assert!(check(&content).is_err());
}

#[test]
fn hex_credential_lengths_blocked_near_trigger() {
    // Verify all four credential-shaped lengths are caught near a trigger.
    let hex40 = "a3f5c2e9d1b8047e63a1f4c2d5b6e8f1a9c3d2e4";
    let hex64 = "1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2b";
    let hex128 = format!("{hex64}{hex64}");
    assert_eq!(hex40.len(), 40);
    assert_eq!(hex64.len(), 64);
    assert_eq!(hex128.len(), 128);

    for (label, hex) in &[
        ("hex40", hex40),
        ("hex64", hex64),
        ("hex128", hex128.as_str()),
    ] {
        let line = format!("secret key: {hex}");
        assert!(
            scan(&line).is_some(),
            "{label} near 'secret key' must be blocked; got None"
        );
    }
}

#[test]
fn hex_blocked_when_trigger_and_hash_word_coexist() {
    // Credential trigger dominates: adding "hash" or "sha" to the window does
    // not rescue a pure-hex token when a credential trigger is also present.
    // An attacker controlling the prose could otherwise bypass the gate with
    // one extra word, so the hash-word exception must NOT apply in trigger context.
    let hex32 = "4f9c2e8a1d3b5c7e9f0a2b4d6e8c0a2b";
    let key_hash_line = format!("api key hash {hex32}");
    let secret_sha_line = format!("secret sha {hex32}");
    assert!(
        scan(&key_hash_line).is_some(),
        "'api key hash <hex32>' must be blocked; got None"
    );
    assert!(
        scan(&secret_sha_line).is_some(),
        "'secret sha <hex32>' must be blocked; got None"
    );
}

#[test]
fn hex_near_sha_context_word_allowed() {
    // A 40-char hex with "sha" or "commit" in the window — but no credential
    // trigger — must be allowed (git SHA or content hash in normal prose).
    let hex40 = "da39a3ee5e6b4b0d3255bfef95601890afd80709";
    let sha_line = format!("sha1: {hex40}");
    let commit_line = format!("commit sha {hex40}");
    assert!(
        scan(&sha_line).is_none(),
        "hex40 near 'sha1' context must be allowed; fired: {:?}",
        scan(&sha_line)
    );
    assert!(
        scan(&commit_line).is_none(),
        "hex40 near 'commit sha' context must be allowed; fired: {:?}",
        scan(&commit_line)
    );
}

const GIT_LENGTH_FIXTURE: &str = "da39a3ee5e6b4b0d3255bfef95601890afd80709";

#[test]
fn bare_forty_hex_line_three_allows_trigger_on_line_one_within_window() {
    let content = format!("auth changes\nready\n{GIT_LENGTH_FIXTURE}");
    assert!(content.len() < TRIGGER_WINDOW);
    assert!(check(&content).is_ok());
    assert_eq!(mask_secrets(&content), content);
}

#[test]
fn bare_forty_hex_line_three_allows_trigger_beyond_window() {
    let content = format!("auth changes\n{}\n{GIT_LENGTH_FIXTURE}", "-".repeat(150));
    assert!(check(&content).is_ok());
}

#[test]
fn forty_hex_allows_direct_sha_or_commit_marker_in_prose() {
    for marker in ["sha:", "commit"] {
        let content = format!("auth changes\nready\n{marker} {GIT_LENGTH_FIXTURE}");
        assert!(check(&content).is_ok(), "{marker}");
    }
}

#[test]
fn forty_hex_refuses_same_line_token_assignment() {
    let content = format!("token: {GIT_LENGTH_FIXTURE}");
    let matched = scan(&content).expect("explicit credential assignment");
    assert_eq!(matched.detector, "hex-credential-token");
    assert_eq!(matched.trigger, Some("token"));
    assert!(check(&content).is_err());
    assert!(!mask_secrets(&content).contains(GIT_LENGTH_FIXTURE));
}

#[test]
fn forty_hex_refuses_previous_label_line_ending_colon_or_equals() {
    for newline in ["\n", "\r\n", "\r"] {
        for delimiter in [":", "="] {
            let content = format!("token{delimiter}{newline}  `{GIT_LENGTH_FIXTURE}`");
            let matched = scan(&content).expect("previous line labels the value");
            assert_eq!(matched.detector, "hex-credential-token");
            assert_eq!(matched.trigger, Some("token"));
            assert!(!mask_secrets(&content).contains(GIT_LENGTH_FIXTURE));
        }
    }
}

#[test]
fn forty_hex_context_stays_on_line_except_immediate_assignment_label() {
    for content in [
        format!("token\n{GIT_LENGTH_FIXTURE}"),
        format!("token:\n\n{GIT_LENGTH_FIXTURE}"),
        format!("{GIT_LENGTH_FIXTURE}\nauth changes"),
        format!("token_count:\n{GIT_LENGTH_FIXTURE}"),
        format!("authorized:\n{GIT_LENGTH_FIXTURE}"),
        format!("{}\n{GIT_LENGTH_FIXTURE}\n密钥", "文".repeat(80)),
    ] {
        assert!(check(&content).is_ok(), "{content}");
    }
    for content in [
        format!("{GIT_LENGTH_FIXTURE} auth"),
        format!("api_keyv2 =\n{GIT_LENGTH_FIXTURE}"),
        format!("secret for deploy: \n{GIT_LENGTH_FIXTURE}"),
    ] {
        assert!(check(&content).is_err(), "{content}");
    }
}

#[test]
fn other_hex_lengths_still_refuse_cross_line_trigger_context() {
    for length in [32, 64, 128] {
        let value = "a".repeat(length);
        let content = format!("auth changes\nready\n{value}");
        let matched = scan(&content).expect("unchanged cross-line context");
        assert_eq!(matched.detector, "hex-credential-token");
        assert_eq!(matched.trigger, Some("auth"));
    }
}

#[test]
fn prefixed_hex_of_forty_bytes_keeps_cross_line_trigger_context() {
    for prefix in ["0x", "0X"] {
        let value = format!("{prefix}{}", "a".repeat(38));
        let content = format!("auth changes\nready\n{value}");
        assert!(check(&content).is_err());
        assert!(!mask_secrets(&content).contains(&value));
    }
}

#[test]
fn forty_hex_beside_bridgeable_prose_keeps_conservative_trigger_context() {
    let content = format!("auth changes\ncompleted\n{GIT_LENGTH_FIXTURE}");
    assert!(check(&content).is_err());
    assert!(!mask_secrets(&content).contains(GIT_LENGTH_FIXTURE));
}

#[test]
fn forty_hex_bridge_fragments_keep_cross_line_detection_and_full_masking() {
    for lengths in [(40, 24), (24, 40), (40, 40)] {
        let first = "a".repeat(lengths.0);
        let second = "b".repeat(lengths.1);
        let content = format!("auth changes\nready\n{first}\u{200B}{second}");
        let matched = scan(&content).expect("fragment keeps original trigger context");
        assert_eq!(matched.detector, "hex-credential-token");
        assert_eq!(matched.trigger, Some("auth"));
        let masked = mask_secrets(&content);
        assert!(!masked.contains(&first), "first fragment remains");
        assert!(!masked.contains(&second), "second fragment remains");
        assert_eq!(
            masked,
            "auth changes\nready\n***MASKED***\u{200B}***MASKED***"
        );
    }
}

#[test]
fn refusal_names_rule_and_canonical_trigger_without_candidate_text() {
    for (label, trigger) in [
        ("token", "token"),
        ("AUTH", "auth"),
        ("api_keyv2", "api_key"),
    ] {
        let content = format!("{label}: {GIT_LENGTH_FIXTURE}");
        let error = check(&content).unwrap_err().to_string();
        assert!(error.contains("hex-credential-token"));
        assert!(error.contains(&format!("near '{trigger}'")), "{error}");
        assert!(!error.contains(&GIT_LENGTH_FIXTURE[..6]));
    }
    let opaque = "Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvMabcdef"; // gitleaks:allow
    let error = check(&format!("secret: {opaque}")).unwrap_err().to_string();
    assert!(error.contains("high-entropy-token near 'secret'"));
    assert!(!error.contains(&opaque[..6]));
    let provider = "AKIAFAKEKEY1234567890";
    let matched = scan(provider).unwrap();
    assert_eq!(matched.trigger, None);
    assert!(!matched.to_string().contains(&provider[..6]));
}

#[test]
fn allows_git_revision_reference_near_ordinary_key_and_token_prose() {
    let revision = "d362950a3c9b1a4cb47d97f1623e38f1a1e6bcdf";
    let contents = [
        format!("the primary key behavior is pinned to commit {revision}"),
        format!("revision: {revision} emits one extra token"),
        format!("primary key behavior at revision:{revision};"),
        format!("configuration key\n\nrev {revision}."),
        format!("one extra token was introduced by sha: {revision}"),
    ];
    for content in &contents {
        assert!(
            check(content).is_ok(),
            "git revision reference in technical prose must pass: {content:?}, got {:?}",
            scan(content)
        );
    }
}

#[test]
fn git_revision_reference_does_not_exempt_credential_assignments() {
    let revision = "d362950a3c9b1a4cb47d97f1623e38f1a1e6bcdf";
    for content in [
        format!("api_key={revision}"),
        format!("secret key: {revision}"),
        format!("token={revision}"),
        format!("api key hash {revision}"),
    ] {
        assert!(
            check(&content).is_err(),
            "credential assignment must remain blocked: {content:?}"
        );
    }
}

#[test]
fn blocks_forty_hex_after_credential_phrase_with_vcs_marker() {
    let revision = "d362950a3c9b1a4cb47d97f1623e38f1a1e6bcdf";
    for content in [
        format!("api key value is commit {revision}"),
        format!("api key value is revision {revision}"),
        format!("the secret is rev {revision}"),
        format!("token value sha {revision}"),
    ] {
        assert_eq!(
            scan(&content).map(|matched| matched.detector),
            Some("hex-credential-token"),
            "a credential phrase must not be hidden by connector words before \
                 a VCS marker: {content:?}"
        );
    }
}

#[test]
fn blocks_unicode_punctuation_between_vcs_marker_and_forty_hex() {
    let revision = "d362950a3c9b1a4cb47d97f1623e38f1a1e6bcdf";
    let content = format!("api_key value commit\u{200B}{revision}");
    assert!(
        check(&content).is_err(),
        "a zero-width space between marker and value must not rescue a \
             labeled credential: {content:?}, got {:?}",
        scan(&content)
    );
}

#[test]
fn blocks_slash_bearing_base64_credential_in_value_syntax() {
    // 40-char standard-base64-alphabet value whose `/` splits it into two
    // runs (19 and 20 bytes) each below MIN_ENTROPY_LEN — per-run checks
    // never see it, so the block must come from refusing the path
    // exemption for a credential-value clause and applying whole-token
    // entropy.
    let content = "api key value is Xk9mZ2vQpLrT8nJwYuA/HfBsDcGiONvMabcdefgh";
    assert_eq!(
        scan(content).map(|matched| matched.detector),
        Some("high-entropy-token"),
        "slash-bearing base64 credential in value syntax must be blocked"
    );
}

#[test]
fn blocks_angle_bracket_line_range_base64_credential_in_value_syntax() {
    let content = "api key value is <Xk9mZ2vQpLrT8nJwYuA/HfBsDcGiONvMabcdefgh>:~97-103";
    assert_eq!(
        scan(content).map(|matched| matched.detector),
        Some("high-entropy-token"),
        "angle-bracket/line-range dressing must not exempt a credential in \
             value syntax"
    );
}

#[test]
fn blocks_split_hex_credential_with_marker_adjacent_fragment() {
    // A 64-hex credential split 40+24 by a zero-width space, with the
    // first fragment hiding behind a VCS marker. The marker-adjacent
    // fragment is exempt as its own anchor, but the second fragment
    // anchors its own reconstruction chain, walks back across the gap,
    // and accumulates 40+24=64 — the symmetric-anchor property the
    // vcs-exempt bridge skip relies on.
    let content = concat!(
        "api token context: commit ",
        "d362950a3c9b1a4cb47d97f1623e38f1a1e6bcdf",
        "\u{200B}",
        "0123456789abcdef01234567"
    );
    assert_eq!(
        scan(content).map(|matched| matched.detector),
        Some("hex-credential-token"),
        "split credential with a marker-adjacent fragment must be blocked"
    );
}

#[test]
fn blocks_forty_hex_behind_qualified_label_with_delimiter() {
    // A label with qualifier words the connector set cannot enumerate
    // ("for deploy") followed by a value delimiter is assignment syntax:
    // once the walk crosses the `:`/`=`, every label-side identifier is
    // stepped over until the trigger word.
    let revision = "d362950a3c9b1a4cb47d97f1623e38f1a1e6bcdf";
    for content in [
        format!("api key for deploy: commit {revision}"),
        format!("prod api key for deploy = commit {revision}"),
    ] {
        assert_eq!(
            scan(&content).map(|matched| matched.detector),
            Some("hex-credential-token"),
            "a qualified label before a value delimiter must not be \
                 hidden from the exemption guard: {content:?}"
        );
    }
}

#[test]
fn blocks_slash_base64_behind_qualified_label_with_delimiter() {
    let content = "api key for deploy: Xk9mZ2vQpLrT8nJwYuA/HfBsDcGiONvMabcdefgh";
    assert_eq!(
        scan(content).map(|matched| matched.detector),
        Some("high-entropy-token"),
        "a qualified label before a value delimiter must refuse the path \
             exemption for a slash-bearing base64 credential"
    );
}

#[test]
fn blocks_forty_hex_behind_versioned_label() {
    // `v1.2` splits into version fragments under identifier extraction;
    // the intra-token dot must not read as a sentence boundary and the
    // fragments must be stepped over like connector words.
    let revision = "d362950a3c9b1a4cb47d97f1623e38f1a1e6bcdf";
    let content = format!("api key v1.2 value is commit {revision}");
    assert_eq!(
        scan(&content).map(|matched| matched.detector),
        Some("hex-credential-token"),
        "a versioned credential label must stay reachable through its \
             version fragments: {content:?}"
    );
}

#[test]
fn blocks_labeled_inline_marker_split_credential() {
    // The r2 medium probes: `marker:value` inline forms carrying a split
    // credential, with a credential label ahead of a value delimiter.
    // The clause guard disables the VCS exemption, and whole-token
    // normalized-hex accumulation fires on the fused token.
    for content in [
        concat!(
            "api token context: rev:",
            "d362950a3c9b1a4cb47d97f1623e38f1a1e6bcdf",
            "\u{200B}",
            "0123456789abcdef01234567"
        )
        .to_string(),
        concat!(
            "api token context: ",
            "0123456789abcdef01234567",
            "\u{200B}",
            "rev:d362950a3c9b1a4cb47d97f1623e38f1a1e6bcdf"
        )
        .to_string(),
    ] {
        assert!(
            check(&content).is_err(),
            "a labeled inline-marker split credential must be blocked: \
                 {content:?}, got {:?}",
            scan(&content)
        );
    }
}

#[test]
fn blocks_forty_hex_behind_multiword_label_and_marker_delimiter() {
    // Coverage: qualifier nouns beyond the two-word case are
    // reachable once prepositions/possessives read as glue, and a
    // delimiter attached to the VCS marker itself ("deploy sha: <hex>")
    // is assignment syntax like any other delimiter.
    let revision = "d362950a3c9b1a4cb47d97f1623e38f1a1e6bcdf";
    for content in [
        format!("api key for deploy sha: {revision}"),
        format!("prod api key for deploy sha: {revision}"),
        format!("api key for production deploy: commit {revision}"),
    ] {
        assert_eq!(
            scan(&content).map(|matched| matched.detector),
            Some("hex-credential-token"),
            "a natural multiword credential label must not be hidden by \
                 glue words or a marker-attached delimiter: {content:?}"
        );
    }
}

#[test]
fn blocks_slash_base64_behind_possessive_qualified_label() {
    let content = "api key for our production deploy: Xk9mZ2vQpLrT8nJwYuA/HfBsDcGiONvMabcdefgh";
    assert_eq!(
        scan(content).map(|matched| matched.detector),
        Some("high-entropy-token"),
        "possessive-qualified credential label must refuse the path \
             exemption"
    );
}

#[test]
fn blocks_forty_hex_behind_participial_adjective_qualifier() {
    // Coverage: a past-participle word in ADJECTIVE position
    // (followed by a content noun: "shared deploy", "encrypted backup")
    // is a label qualifier, not verb-phrase prose, and must not end the
    // walk.
    let revision = "d362950a3c9b1a4cb47d97f1623e38f1a1e6bcdf";
    let content = format!("api key for shared deploy: commit {revision}");
    assert_eq!(
        scan(&content).map(|matched| matched.detector),
        Some("hex-credential-token"),
        "a participial-adjective label qualifier must stay walkable: \
             {content:?}"
    );
}

#[test]
fn blocks_slash_base64_behind_participial_adjective_qualifier() {
    let content = "api key for encrypted backup: Xk9mZ2vQpLrT8nJwYuA/HfBsDcGiONvMabcdefgh";
    assert_eq!(
        scan(content).map(|matched| matched.detector),
        Some("high-entropy-token"),
        "a participial-adjective label qualifier must refuse the path \
             exemption"
    );
}

#[test]
fn blocks_forty_hex_behind_chained_qualifier_label() {
    // Coverage: a chain of qualifiers between the value and
    // the trigger ("shared encrypted deploy") must not exhaust the walk
    // before the label head is reached. Any per-clause content-word cap
    // re-admits this bypass one qualifier past the cap.
    let revision = "d362950a3c9b1a4cb47d97f1623e38f1a1e6bcdf";
    let content = format!("api key for shared encrypted deploy: commit {revision}");
    assert_eq!(
        scan(&content).map(|matched| matched.detector),
        Some("hex-credential-token"),
        "a chained-qualifier credential label must stay walkable: \
             {content:?}"
    );
}

#[test]
fn blocks_slash_base64_behind_chained_qualifier_label() {
    let content = "api key for shared encrypted deploy: Xk9mZ2vQpLrT8nJwYuA/HfBsDcGiONvMabcdefgh";
    assert_eq!(
        scan(content).map(|matched| matched.detector),
        Some("high-entropy-token"),
        "a chained-qualifier credential label must refuse the path \
             exemption"
    );
}

#[test]
fn blocks_over_limit_qualified_label_fails_closed() {
    // Coverage: labels whose clause exhausts the walk budget
    // (a marker, an extra qualifier, or an interleaved glue word pushes
    // the trigger past the limit). Exhaustion after a value delimiter
    // fails closed — clause length must not launder a labeled credential
    // into the exemptions.
    let revision = "d362950a3c9b1a4cb47d97f1623e38f1a1e6bcdf";
    let opaque = "Xk9mZ2vQpLrT8nJwYuA/HfBsDcGiONvMabcdefgh";
    for content in [
        format!("api key for the new shared encrypted staging deploy: commit {revision}"),
        format!("api key for the new shared encrypted regional staging deploy: {opaque}"),
        format!("api key for the new shared and encrypted staging deploy: {opaque}"),
        format!("api key for the new shared encrypted regional staging deploy: commit {revision}"),
        format!("api key for the new shared and encrypted staging deploy: commit {revision}"),
        format!("api key for the new shared encrypted staging deploy: {opaque}"),
    ] {
        assert!(
            check(&content).is_err(),
            "an over-limit qualified credential label must fail closed: \
                 {content:?}, got {:?}",
            scan(&content)
        );
    }
}

#[test]
fn accepted_false_positive_topical_trigger_before_delimited_path() {
    // The clause walk treats ANY reachable pre-delimiter trigger as a
    // credential label — it has no grammar to tell a label head ("api
    // key ...") from a topical object ("testing auth against parser").
    // Distinguishing them would reopen the labeled-value bypasses, so
    // this ordinary prose shape blocks. Accepted false positive,
    // conservative direction; documented in docs/api/secret_gate.md.
    let content = "results from testing auth against parser: \
             internal/workspaces/20260701/cloud-rebuild/R1-repo-audit.md";
    assert!(
        check(content).is_err(),
        "accepted-FP contract changed: topical trigger before a \
             delimited path no longer blocks — update the docs if deliberate"
    );
}

#[test]
fn blocks_participle_before_trigger_word() {
    // Ordering contract: a past-participle word BEFORE the trigger never
    // matters — the walk reaches the trigger first. Pinned so the
    // verb-position rule cannot regress into shielding these labels.
    let revision = "d362950a3c9b1a4cb47d97f1623e38f1a1e6bcdf";
    let opaque = "Xk9mZ2vQpLrT8nJwYuA/HfBsDcGiONvMabcdefgh";
    for content in [
        format!("shared key: {revision}"),
        format!("generated api key: {revision}"),
        format!("encrypted token = {opaque}"),
    ] {
        assert!(
            check(&content).is_err(),
            "a participle before the trigger word must not shield the \
                 label: {content:?}, got {:?}",
            scan(&content)
        );
    }
}

#[test]
fn accepted_false_positive_docs_path_behind_attributive_trigger_and_delimiter() {
    // "auth setup: <path>" carries a trigger word in clause range ahead
    // of a value delimiter; the walk cannot distinguish an attributive
    // trigger ("auth setup") from a label head ("api key ...") without
    // reopening the labeled-value bypasses, so this ordinary prose shape
    // blocks. Accepted false positive — conservative direction under the
    // threat model; documented in docs/api/secret_gate.md.
    let content = "see the docs for auth setup: \
             internal/workspaces/20260701/cloud-rebuild/R1-repo-audit.md";
    assert!(
        check(content).is_err(),
        "accepted-FP contract changed: attributive trigger before a \
             delimited path no longer blocks — update the docs if deliberate"
    );
}

#[test]
fn allows_verb_phrase_prose_with_delimiter_before_trigger_word() {
    // Past-participle content words are verb-phrase evidence: the clause
    // narrates an action on the value instead of labeling it. These are
    // the false-positive shapes the exemptions exist for, and they must
    // survive the marker-attached-delimiter and glue-word widenings.
    let revision = "d362950a3c9b1a4cb47d97f1623e38f1a1e6bcdf";
    for content in [
        format!("one extra token was introduced by sha: {revision}"),
        format!("the api key was rotated. deploy notes reference commit {revision}"),
        format!("api key updated: commit {revision}"),
    ] {
        assert!(
            check(&content).is_ok(),
            "verb-phrase prose must keep the VCS exemption: {content:?}, \
                 got {:?}",
            scan(&content)
        );
    }
}

#[test]
fn allows_unlabeled_unknown_connector_without_delimiter() {
    // VCS coordinates retain the strict no-delimiter tier: an identifier
    // outside the connector set ends the walk. The bounded bridge used by
    // file paths must not re-block this ordinary revision prose.
    let revision = "d362950a3c9b1a4cb47d97f1623e38f1a1e6bcdf";
    for content in [
        format!("the key changes are in commit {revision}"),
        format!("deployed at commit {revision}"),
    ] {
        assert!(
            check(&content).is_ok(),
            "prose without a value delimiter must keep the VCS exemption: \
                 {content:?}, got {:?}",
            scan(&content)
        );
    }
}

#[test]
fn blocks_direct_no_delimiter_labels_for_both_file_path_value_families() {
    let values = [
        "Xk9mZ2vQpLrT8nJwYuA/HfBsDcGiONvMabcdefgh",
        "internal/workspaces/20260701/cloud-rebuild/R1-repo-audit.md",
    ];

    for value in values {
        for label in ["api key found", "auth scanner found", "secret note"] {
            let content = format!("{label} {value}");
            assert!(
                check(&content).is_err(),
                "a direct no-delimiter label must refuse the file-path exemption: \
                     {content:?}, got {:?}",
                scan(&content)
            );
        }
    }
}

#[test]
fn blocks_direct_gerund_and_see_labels_for_both_file_path_value_families() {
    let values = [
        "Xk9mZ2vQpLrT8nJwYuA/HfBsDcGiONvMabcdefgh",
        "internal/workspaces/20260701/cloud-rebuild/R1-repo-audit.md",
    ];

    for value in values {
        for label in [
            "api key handling",
            "auth scanner handling",
            "api key see",
            "key: see",
        ] {
            let content = format!("{label} {value}");
            assert!(
                check(&content).is_err(),
                "narrative-looking adjacency must not shield a direct credential label: \
                     {content:?}, got {:?}",
                scan(&content)
            );
        }
    }
}

#[test]
fn blocks_direct_participle_labels_for_both_file_path_value_families() {
    let values = [
        "Xk9mZ2vQpLrT8nJwYuA/HfBsDcGiONvMabcdefgh",
        "internal/workspaces/20260701/cloud-rebuild/R1-repo-audit.md",
    ];

    for value in values {
        for label in ["api key leaked", "auth scanner leaked"] {
            let content = format!("{label} {value}");
            assert!(
                check(&content).is_err(),
                "step-zero participle adjacency must not shield a direct credential label: \
                     {content:?}, got {:?}",
                scan(&content)
            );
        }
    }
}

#[test]
fn allows_no_delimiter_narrative_file_path_prose() {
    // The value-side `file`/`this` identifiers provide real walk context
    // before `flagged`; the step-zero direct-label tightening must not
    // remove this required narrative exemption.
    let content = "the auth scanner flagged this file \
            internal/workspaces/20260701/cloud-rebuild/R1-repo-audit.md";

    assert!(
        check(content).is_ok(),
        "a regular participle in narrative position must preserve the path: got {:?}",
        scan(content)
    );
}

#[test]
fn suffix_word_hundred_is_not_a_narrative_participle() {
    assert!(is_clause_narrative_participle("flagged"));
    assert!(is_clause_narrative_participle("INTRODUCED"));
    assert!(!is_clause_narrative_participle("found"));
    assert!(!is_clause_narrative_participle("hundred"));
    assert!(is_clause_narrative_gerund("HANDLING"));
    assert!(!is_clause_narrative_gerund("found"));

    for value in [
        "Xk9mZ2vQpLrT8nJwYuA/HfBsDcGiONvMabcdefgh",
        "internal/workspaces/20260701/cloud-rebuild/R1-repo-audit.md",
    ] {
        let content = format!("api key hundred: {value}");
        assert!(
            check(&content).is_err(),
            "a lexical -ed suffix must not shield a credential label: \
                 {content:?}, got {:?}",
            scan(&content)
        );
    }
}

#[test]
fn allows_vcs_reference_with_prose_boundary_before_trigger_word() {
    // A sentence or paragraph boundary between a trigger word and the
    // marker/value clause means the trigger is prose context, not this
    // value's label — the clause walk must stop at the boundary.
    let revision = "d362950a3c9b1a4cb47d97f1623e38f1a1e6bcdf";
    for content in [
        format!("configuration key\n\nrev {revision}."),
        format!("rotate the api key. the fix is commit {revision}"),
    ] {
        assert!(
            check(&content).is_ok(),
            "boundary-separated trigger prose must not block a VCS \
                 reference: {content:?}, got {:?}",
            scan(&content)
        );
    }
}

#[test]
fn blocks_sha_revision_marker_immediately_labeled_as_api_key() {
    let revision = "d362950a3c9b1a4cb47d97f1623e38f1a1e6bcdf";
    let content = format!("api_key sha:{revision}");

    assert!(
        check(&content).is_err(),
        "credential-labeled SHA value must remain blocked: {content:?}"
    );
}

#[test]
fn blocks_rev_revision_marker_immediately_labeled_as_token() {
    let revision = "d362950a3c9b1a4cb47d97f1623e38f1a1e6bcdf";
    let content = format!("token rev:{revision}");

    assert!(
        check(&content).is_err(),
        "credential-labeled revision value must remain blocked: {content:?}"
    );
}

#[test]
fn hex64_near_hash_context_allowed() {
    // A 64-char hex near "sha256" or "hash" — with no credential trigger —
    // must be allowed (content digest in normal prose).
    let hex64 = "1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2b";
    let sha_line = format!("sha256: {hex64}");
    let hash_line = format!("hash value {hex64}");
    assert!(
        scan(&sha_line).is_none(),
        "hex64 near 'sha256' must be allowed; fired: {:?}",
        scan(&sha_line)
    );
    assert!(
        scan(&hash_line).is_none(),
        "hex64 near 'hash' must be allowed; fired: {:?}",
        scan(&hash_line)
    );
}

#[test]
fn blocks_high_entropy_hex_like_token_near_key() {
    // A token whose character set exceeds pure hex (contains mixed-case, digits,
    // and non-hex chars) that ALSO passes `is_pure_hex = false` AND has high
    // entropy AND appears near "key" MUST be caught.  This is the realistic
    // real-world case: hex-looking API tokens often mix case and non-hex chars.
    // Example: a 32-char mixed-charset token near "api key".
    let mixed = "Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvM"; // gitleaks:allow — not pure hex
    assert!(!is_pure_hex(mixed), "test token must not be pure hex");
    let line = format!("api key {mixed}");
    assert!(
        scan(&line).is_some(),
        "mixed-charset high-entropy token near 'api key' must be caught; got: {:?}",
        scan(&line)
    );
}

#[test]
fn allows_hex40_without_trigger() {
    // 40-char hex string in a neutral context (no trigger word) must still pass —
    // it's likely a git commit SHA or content hash.
    let hex40 = "da39a3ee5e6b4b0d3255bfef95601890afd80709";
    let line = format!("commit: {hex40}");
    assert!(
        scan(&line).is_none(),
        "40-char hex without trigger word must pass; fired: {:?}",
        scan(&line)
    );
}

// ── check_json scans object keys ─────────────────────────────────────────

#[test]
fn check_json_blocks_secret_in_object_key() {
    // A credential used as a JSON object key (not a value) must be caught.
    let props = serde_json::json!({ "ghp_FakeGitHubToken0000000000000000000": "redacted" }); // gitleaks:allow
    assert!(
        check_json(&props).is_err(),
        "credential as JSON object key must be blocked"
    );
}

#[test]
fn check_json_blocks_nested_secret_key() {
    // Nested credential key must be caught.
    let props = serde_json::json!({
        "metadata": {
            "AKIAFAKEKEY000000000": "value" // gitleaks:allow
        }
    });
    assert!(
        check_json(&props).is_err(),
        "nested credential as JSON object key must be blocked"
    );
}

// ── PEM masking format ───────────────────────────────────────────────────

#[test]
fn pem_masked_excerpt_reflects_block_length_not_rest_of_string() {
    let header = ["-----BEGIN RSA", " PRIVATE KEY-----"].concat(); // gitleaks:allow
    let fake = format!(
        "{}\nMIIEo\u{2026}\n-----END RSA PRIVATE KEY-----\nsome trailing text that is very long",
        header
    );
    let m = scan(&fake).unwrap();
    assert_eq!(m.detector, "pem-private-key");
    // The masked length should reflect only the key block, not the whole string.
    // "some trailing text that is very long" is ~37 chars; total string is much longer.
    // The block ends after "-----END RSA PRIVATE KEY-----\n".
    // We just verify it is shorter than the full string length.
    let full_len = fake.chars().count();
    let reported_len: usize = m
        .masked
        .trim_end_matches("chars")
        .rsplit("...")
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(full_len + 1);
    assert!(
        reported_len < full_len,
        "masked length ({reported_len}) should be less than full string length ({full_len})"
    );
}

// ── UTF-8 char-boundary reproduction tests ───────────────────────────────
//
// These tests verify that no code path in secret_gate panics when multibyte
// UTF-8 characters (emoji, CJK, accented Latin) appear at positions where
// byte-level slicing could land mid-codepoint.  Each test targets a specific
// code path.  A panic means the bug is live; a pass means the path is safe.

/// `build_match` masked preview: if the detected candidate starts with
/// multibyte chars the "first 6 chars" preview must not slice on a byte
/// boundary that falls mid-codepoint.  build_match already uses
/// `chars().take(6)`, but we exercise it with emoji-prefixed candidates.
#[test]
fn utf8_build_match_preview_multibyte_prefix_no_panic() {
    // "🔑" = 4 bytes; repeat 3 times = 12 bytes for only 3 chars.
    // A ghp_-prefixed token with an emoji: let's construct a scenario where
    // a known-prefix secret is immediately adjacent to multibyte content so
    // that build_match receives a slice starting at a multibyte char.
    // PEM block with multibyte chars in the body exercises build_match on a
    // candidate that may contain non-ASCII.
    let header = ["-----BEGIN RSA", " PRIVATE KEY-----"].concat(); // gitleaks:allow
    let fake = format!("{}\n🔑密钥\n-----END RSA PRIVATE KEY-----", header);
    // Must not panic; mask must not echo full body.
    let m = scan(&fake);
    assert!(m.is_some(), "PEM with emoji body must still be caught");
    let m = m.unwrap();
    assert!(
        !m.masked.contains("🔑密钥"),
        "mask must not echo the emoji body"
    );
}

/// `extract_token` called with a string starting with multibyte chars:
/// the FlyV1 handler calls `extract_token(&text[payload_start..])` where
/// `payload_start` is just past "FlyV1 " (ASCII).  If the payload is ASCII
/// this is trivially safe, but we verify it cannot panic when the rest of
/// the text after the payload contains multibyte chars.
#[test]
fn utf8_extract_token_multibyte_suffix_no_panic() {
    // "FlyV1 ABCDEFGHIJ密钥" — the payload is "ABCDEFGHIJ密钥"; extract_token
    // must stop at the ideographic chars (which are NOT ASCII whitespace) and
    // return the whole glued run without panicking.
    let text = "FlyV1 ABCDEFGHIJ密钥";
    // scan() must not panic.
    let _ = scan(text);
}

/// `find_prefix_token` with multibyte chars immediately before and after
/// the known prefix: checks text[..abs] boundary slices and
/// extract_token(&text[abs..]) do not panic.
#[test]
fn utf8_prefix_detector_multibyte_adjacent_no_panic() {
    // 🔑 (4 bytes) immediately before AKIA: boundary at abs = 4, which is a
    // valid char boundary (end of the emoji).  extract_token sees ASCII from abs.
    let text = "🔑AKIAFAKEKEY00000000000000";
    let _ = scan(text); // must not panic

    // é (U+00E9 = 2 bytes) immediately before ghp_:
    let text2 = "éghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let _ = scan(text2); // must not panic

    // Emoji immediately after the token — extract_token ends at the emoji
    // (non-whitespace, but non-ASCII acts as delimiter in entropy heuristic).
    // For prefix tokens extract_token stops at ASCII whitespace only, so the
    // emoji would be included in the token length measurement.
    let text3 = "AKIAFAKEKEY00000000000000🔑";
    let _ = scan(text3); // must not panic
}

/// `find_jwt` with multibyte chars as "whitespace" adjacent to a JWT-like
/// candidate: `i = end + 1` could skip into a multibyte char if `end`
/// pointed at a non-ASCII byte.  The position() search only looks for ASCII
/// whitespace bytes, so a multibyte space (U+3000) is NOT found — `end`
/// equals bytes.len() and `i = bytes.len() + 1` exits the loop.  Still
/// verify no panic on CJK-surrounded JWT-like content.
#[test]
fn utf8_jwt_multibyte_adjacent_no_panic() {
    // A (fake) JWT-like triple surrounded by CJK text.
    let jwt =
        "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.FAKE_SIG_XXXXXXXXXXXX"; // gitleaks:allow
    let text = format!("数据{jwt}密钥");
    let _ = scan(&text); // must not panic

    // JWT followed by ideographic space (U+3000 = 3 bytes 0xE3 0x80 0x80) —
    // not matched by the ASCII-whitespace position() search.
    let text2 = format!("{jwt}\u{3000}morecontent");
    let _ = scan(&text2); // must not panic

    // JWT followed by emoji
    let text3 = format!("{jwt}🔑");
    let _ = scan(&text3); // must not panic
}

/// `find_url_userinfo` with multibyte chars between "://" and "@":
/// `at_pos` from `rest.find('@')` and `colon` from `userinfo.find(':')` are
/// ASCII markers (char boundaries), but `scheme_start` calculation uses
/// char_indices().rev() which must handle multibyte chars in the scheme
/// prefix correctly.
#[test]
fn utf8_url_userinfo_multibyte_scheme_no_panic() {
    // CJK glued to a credential URL — the scheme_start walker must not place
    // the start inside a multibyte codepoint.
    let cases = [
        "🔑postgresql://dbuser:S3cr3tP4ss@db.example.com/db", // gitleaks:allow
        "密钥mysql://root:hunter2pw@10.0.0.1:3306/app",       // gitleaks:allow
        "éredis://svc:V3ryS3cretPw@cache.internal:6379",      // gitleaks:allow
    ];
    for text in &cases {
        // Must not panic and must detect the credential.
        let result = scan(text);
        assert!(
            result.is_some(),
            "URL credential after multibyte must be caught: {text:?}"
        );
    }
}

/// `check_entropy_heuristic` window slicing with multibyte content at the
/// ±TRIGGER_WINDOW boundary: `floor_char_boundary` must prevent slicing
/// on a non-char boundary.
#[test]
fn utf8_entropy_window_multibyte_boundary_no_panic() {
    // Construct content where the TRIGGER_WINDOW (120 bytes) boundary falls
    // inside a 3-byte CJK character.  Repeat "数" (U+6570 = 3 bytes) to fill
    // exactly 119 bytes, then add an ASCII trigger word + high-entropy token.
    // Window start: token_offset - 120 = lands inside one of the CJK chars.
    let cjk_fill = "数".repeat(39); // 39 × 3 = 117 bytes
    assert_eq!(cjk_fill.len(), 117);
    // Pad with 2 more ASCII chars ("xy") so that the 120-byte window lands at
    // byte 119 which is the second byte of the 40th "数" — mid-multibyte.
    let secret = "Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvM1"; // gitleaks:allow
    let content = format!("{cjk_fill}xy key {secret}");
    let _ = scan(&content); // must not panic

    // Also test the right edge: token ends at byte offset, window_end =
    // token_offset + raw_token.len() + 120 may land mid-multibyte.
    let content2 = format!("key {secret}{cjk_fill}xy");
    let _ = scan(&content2); // must not panic
}

/// `check()` top-level fuzz: a large batch of inputs with multibyte
/// characters at various offsets to catch any remaining panic sites.
/// All results must be either Ok or Err (not a panic).
#[test]
fn utf8_no_panic_property_test() {
    let multibyte_items = [
        "🔑",       // 4-byte emoji
        "密",       // 3-byte CJK
        "é",        // 2-byte accented Latin
        "\u{3000}", // 3-byte ideographic space
        "🇺🇸",       // 8-byte emoji flag (two surrogate-like scalars)
    ];
    let secrets = [
        "AKIAFAKEKEY00000000000000".to_owned(),
        "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_owned(),
        anthropic_api_key_fixture(),
        "Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvM1".to_owned(),
        "FlyV1 fm2_AAAABBBBCCCCDDDDEEEEFFFF".to_owned(),
    ];
    for mb in &multibyte_items {
        for secret in &secrets {
            for sep in &["", " ", "\n"] {
                // multibyte before secret
                let s = format!("{mb}{sep}{secret}");
                let _ = check(&s);
                // multibyte after secret
                let s = format!("{secret}{sep}{mb}");
                let _ = check(&s);
                // multibyte both sides
                let s = format!("{mb}{sep}{secret}{sep}{mb}");
                let _ = check(&s);
                // repeated multibyte filling TRIGGER_WINDOW boundary
                let fill = mb.repeat(50);
                let s = format!("{fill} api_key {secret} {fill}");
                let _ = check(&s);
            }
        }
    }
}

// ── mask_secrets: in-place redaction reusing the canonical detector ───────

#[test]
fn bounded_masked_log_text_masks_connection_string_credentials() {
    let raw = "gate backend probe failed: postgres://svc:not-a-real-secret@internal-host refused"; // gitleaks:allow
    let rendered = bounded_masked_log_text(raw);
    assert!(
        !rendered.contains("not-a-real-secret"),
        "credential must be masked: {rendered:?}"
    );
    assert!(
        rendered.contains("***MASKED***"),
        "masked marker must record that detail was redacted: {rendered:?}"
    );
    assert!(
        rendered.contains("gate backend probe failed"),
        "non-secret diagnostic prose must survive: {rendered:?}"
    );
}

#[test]
fn bounded_masked_log_text_bounds_pathological_input() {
    let raw = "x".repeat(MAX_LOG_TEXT_OUTPUT_CHARS + 500);
    let rendered = bounded_masked_log_text(&raw);
    assert!(
        rendered.chars().count() <= MAX_LOG_TEXT_OUTPUT_CHARS + 1,
        "output must be bounded: {} chars",
        rendered.chars().count()
    );
    assert!(
        rendered.ends_with('…'),
        "truncated output must declare its own truncation"
    );

    let short = "gate policy file unreadable";
    assert_eq!(
        bounded_masked_log_text(short),
        short,
        "short clean text passes through unchanged"
    );
}

#[test]
fn bounded_masked_log_text_masks_low_entropy_password_past_old_truncation_bound() {
    // Regression: masking used to run AFTER truncating the raw input to a
    // few KB, so a connection string whose password ran past that bound
    // lost its terminating `@` before the url-userinfo detector (a
    // shape match, not an entropy one) ever saw it — a low-entropy
    // password can't trip the entropy heuristic either, so it leaked
    // verbatim into the log. Masking now runs on the full input first,
    // so a password far longer than the old 4096-char bound is still
    // caught.
    let long_low_entropy_password = "a".repeat(5_000);
    let raw = format!(
        "gate backend probe failed: postgres://svc:{long_low_entropy_password}@internal-host refused"
    );
    let rendered = bounded_masked_log_text(&raw);
    assert!(
        !rendered.contains(&"a".repeat(50)),
        "long low-entropy password must not survive in the log: {rendered:?}"
    );
    assert!(
        rendered.contains("***MASKED***"),
        "masked marker must record that the credential was redacted: {rendered:?}"
    );
}

/// Regression for the crossing-boundary leak: a password longer than
/// [`MAX_LOG_TEXT_MASK_INPUT_CHARS`] never gets a chance to show its
/// terminating `@` to `find_url_userinfo` inside the truncated scan
/// input, so the shape detector alone can never catch it — and it's
/// deliberately low-entropy so the entropy heuristic can't catch it
/// either. Only `redact_crossing_boundary_url_userinfo` can close this.
#[test]
fn bounded_masked_log_text_redacts_password_crossing_mask_input_cap() {
    let huge_low_entropy_password = "a".repeat(MAX_LOG_TEXT_MASK_INPUT_CHARS + 1000);
    let raw =
        format!("gate backend probe failed: postgres://svc:{huge_low_entropy_password}@internal-host refused");
    let rendered = bounded_masked_log_text(&raw);
    assert!(
        !rendered.contains(&"a".repeat(50)),
        "no password fragment may survive when the password crosses the mask-input cap: {rendered:?}"
    );
    assert!(
        rendered.contains("***MASKED***"),
        "mask marker must record that the credential was redacted: {rendered:?}"
    );
    assert!(
        rendered.ends_with('…'),
        "truncated record must declare its own incompleteness: {rendered:?}"
    );
}

/// Regression: the crossing fallback follows the same empty-username
/// rule as the canonical detector — `redis://:<password>` with the
/// terminating `@` beyond the mask-input cap must still be redacted.
#[test]
fn bounded_masked_log_text_redacts_crossing_empty_username_password() {
    let huge_low_entropy_password = "b".repeat(MAX_LOG_TEXT_MASK_INPUT_CHARS + 1000);
    let raw = format!(
        "gate backend probe failed: redis://:{huge_low_entropy_password}@internal-host refused"
    );
    let rendered = bounded_masked_log_text(&raw);
    assert!(
        !rendered.contains(&"b".repeat(50)),
        "no empty-username password fragment may survive the cap crossing: {rendered:?}"
    );
    assert!(
        rendered.contains("***MASKED***"),
        "mask marker must record that the credential was redacted: {rendered:?}"
    );
}

/// Regression: the crossing fallback shares the canonical detector's
/// authority boundary — a colon after `/`, `?`, or `#` is path/query
/// text, so a capped input whose only colon-at pair sits in the path
/// must NOT be masked even when the `@` lies beyond the cap.
#[test]
fn bounded_masked_log_text_keeps_path_colon_text_crossing_the_cap() {
    let filler = "z".repeat(MAX_LOG_TEXT_MASK_INPUT_CHARS + 1000);
    let raw = format!("see https://host/a:x{filler}@next for details");
    let rendered = bounded_masked_log_text(&raw);
    assert!(
        !rendered.contains("***MASKED***"),
        "path-colon text must not read as a crossing credential: {rendered:?}"
    );
    assert!(
        rendered.starts_with("see https://host/a:x"),
        "the non-credential prefix must survive verbatim: {rendered:?}"
    );
}

/// Regression: a crossing password that itself contains `://` must not
/// let the fallback anchor at that nested delimiter. Anchoring at the
/// last `://` would redact only from the nested span onward, leaving
/// the real `user:<password prefix>` before it in the emitted log. The
/// fallback must anchor at the earliest unterminated credential
/// opening, so zero password characters survive.
#[test]
fn bounded_masked_log_text_redacts_crossing_password_containing_nested_scheme() {
    let mut password = "a".repeat(1000);
    password.push_str("://h:");
    password.push_str(&"b".repeat(MAX_LOG_TEXT_MASK_INPUT_CHARS + 1000));
    let raw = format!("gate backend probe failed: postgres://svc:{password}@internal-host refused");
    let rendered = bounded_masked_log_text(&raw);
    assert!(
        !rendered.contains(&"a".repeat(50)),
        "password prefix before the nested delimiter must not survive: {rendered:?}"
    );
    assert!(
        !rendered.contains(&"b".repeat(50)),
        "password tail after the nested delimiter must not survive: {rendered:?}"
    );
    assert!(
        rendered.contains("***MASKED***"),
        "mask marker must record that the credential was redacted: {rendered:?}"
    );
}

/// Regression: a complete URL earlier in the text must not stop the
/// fallback from catching a later credential run that crosses the cap.
/// The earlier URL's span terminates inside the text (whitespace after
/// it), so it is skipped; the later unterminated `user:<password-run>`
/// is the one redacted.
#[test]
fn bounded_masked_log_text_redacts_crossing_credential_after_complete_url() {
    let huge_low_entropy_password = "c".repeat(MAX_LOG_TEXT_MASK_INPUT_CHARS + 1000);
    let raw = format!(
        "probe of https://ok-host/health failed; retry hit postgres://svc:{huge_low_entropy_password}@internal-host refused"
    );
    let rendered = bounded_masked_log_text(&raw);
    assert!(
        rendered.contains("ok-host"),
        "the earlier complete URL must survive untouched: {rendered:?}"
    );
    assert!(
        !rendered.contains(&"c".repeat(50)),
        "no password fragment may survive: {rendered:?}"
    );
    assert!(
        rendered.contains("***MASKED***"),
        "mask marker must record that the credential was redacted: {rendered:?}"
    );
}

/// Regression: the crossing-boundary fallback must not fire when the
/// password's terminating `@` sits safely inside the (untruncated)
/// bounded input — the existing `find_url_userinfo` arm alone must
/// still catch and mask it, with exactly one mask marker.
#[test]
fn bounded_masked_log_text_masks_password_just_under_cap_with_terminating_at() {
    let password_under_cap = "a".repeat(MAX_LOG_TEXT_MASK_INPUT_CHARS - 1000);
    let raw = format!(
        "gate backend probe failed: postgres://svc:{password_under_cap}@internal-host refused"
    );
    assert!(
        raw.chars().count() < MAX_LOG_TEXT_MASK_INPUT_CHARS,
        "test precondition: whole input must stay under the mask-input cap so no truncation occurs"
    );
    let rendered = bounded_masked_log_text(&raw);
    assert!(
        !rendered.contains(&"a".repeat(50)),
        "terminated credential must still be masked normally: {rendered:?}"
    );
    assert_eq!(
        rendered.matches("***MASKED***").count(),
        1,
        "exactly one mask marker — the crossing fallback must not fire early: {rendered:?}"
    );
}

/// Regression: giant truncated text with no `://user:` shape at all must
/// pass through unmodified by the crossing-boundary fallback — it must
/// not manufacture a false redaction out of ordinary non-credential
/// prose that happens to be long enough to hit the mask-input cap.
#[test]
fn bounded_masked_log_text_giant_non_credential_text_unaffected_by_crossing_fallback() {
    let raw = "x".repeat(MAX_LOG_TEXT_MASK_INPUT_CHARS + 1000);
    let rendered = bounded_masked_log_text(&raw);
    assert!(
        !rendered.contains("***MASKED***"),
        "non-credential text must never be redacted: {rendered:?}"
    );
    assert!(
        rendered.ends_with('…'),
        "truncated record must still declare its own incompleteness: {rendered:?}"
    );
    assert!(
        rendered.starts_with("xxxx"),
        "non-credential content must survive verbatim up to the output bound: {rendered:?}"
    );
}

/// Regression: an ordinary short connection string (well under both
/// caps) must keep being masked exactly as before — the crossing
/// fallback is gated on truncation and must never touch this path.
#[test]
fn bounded_masked_log_text_masks_ordinary_short_connection_string() {
    let raw = "gate backend probe failed: postgres://svc:hunter2pw@internal-host refused"; // gitleaks:allow
    let rendered = bounded_masked_log_text(raw);
    assert!(
        !rendered.contains("hunter2pw"),
        "credential must be masked: {rendered:?}"
    );
    assert_eq!(
        rendered.matches("***MASKED***").count(),
        1,
        "exactly one mask marker for the one credential: {rendered:?}"
    );
    assert!(
        !rendered.ends_with('…'),
        "short untruncated text must not declare truncation: {rendered:?}"
    );
}

#[test]
fn bounded_masked_log_text_neutralizes_ascii_control_chars() {
    let raw = "line one\r\ninjected: \u{1b}[31mFAKE ALERT\u{1b}[0m line two";
    let rendered = bounded_masked_log_text(raw);
    assert!(
        !rendered.contains('\r') && !rendered.contains('\n'),
        "CR/LF must be neutralized so a single log line cannot be split/forged: {rendered:?}"
    );
    assert!(
        !rendered.contains('\u{1b}'),
        "ESC control character must be neutralized: {rendered:?}"
    );
    assert!(
        rendered.contains("line one") && rendered.contains("line two"),
        "surrounding prose must survive neutralization: {rendered:?}"
    );
}

#[test]
fn bounded_masked_log_text_neutralizes_each_unsafe_unicode_category() {
    let cases = [
        ("Cc", '\u{1b}', "\\u{001b}"),
        ("Cf", '\u{202e}', "\\u{202e}"),
        ("Zl", '\u{2028}', "\\u{2028}"),
        ("Zp", '\u{2029}', "\\u{2029}"),
    ];

    for (category, unsafe_char, escaped) in cases {
        let raw = format!("before{unsafe_char}after");
        assert_eq!(
            bounded_masked_log_text(&raw),
            format!("before{escaped}after"),
            "Unicode category {category} must be neutralized"
        );
    }
}

#[test]
fn bounded_masked_log_text_keeps_space_separators_tabs_accented_and_cjk_text() {
    let raw = "ordinary whitespace\tcafé résumé 日本語のテキスト\u{3000}数据库连接管理";
    assert_eq!(
        bounded_masked_log_text(raw),
        raw,
        "ordinary whitespace, Zs separators, accented text, and CJK prose must pass through unmodified"
    );
}

#[test]
fn mask_secrets_borrows_clean_text() {
    let clean = "The FlashAttention paper introduces IO-aware tiling.";
    let masked = mask_secrets(clean);
    assert!(
        matches!(masked, std::borrow::Cow::Borrowed(_)),
        "clean text must not allocate"
    );
    assert_eq!(masked, clean);
}

#[test]
fn named_redaction_surfaces_are_permanently_mask_only() {
    let contracts = [
        (RedactionSurface::GitIngest, Some(GIT_INGEST_STORED_TARGET)),
        (
            RedactionSurface::SessionMirror,
            Some(SESSION_MIRROR_STORED_TARGET),
        ),
        (RedactionSurface::McpDiagnostic, None),
        (RedactionSurface::GateProbe, None),
    ];

    for (surface, expected_target) in contracts {
        let contract = redaction_surface_contract(surface);
        assert_eq!(contract.mode, RedactionSurfaceMode::PermanentMaskOnly);
        assert_eq!(contract.final_stored_target, expected_target);
        assert_eq!(contract.stamp_property, None);
        assert_eq!(contract.atomic_success_event, None);
    }
}

#[test]
fn named_redaction_surfaces_mask_without_exemption_admission() {
    let content = format!("credential: {}", openai_project_key_fixture());

    for surface in [
        RedactionSurface::GitIngest,
        RedactionSurface::SessionMirror,
        RedactionSurface::McpDiagnostic,
        RedactionSurface::GateProbe,
    ] {
        let masked = mask_for_redaction_surface(surface, &content);
        assert!(masked.contains(REDACTION_MARKER));
        assert!(check(masked.as_ref()).is_ok());
    }
}

#[test]
fn mask_bounded_matches_unbounded_masking_when_input_fits_the_window() {
    let content = format!("credential: {}", openai_project_key_fixture());
    let expected = mask_for_redaction_surface(RedactionSurface::McpDiagnostic, &content);
    let result = mask_bounded(RedactionSurface::McpDiagnostic, &content, 4_096, 1_024);
    assert_eq!(result.text, expected.as_ref());
    assert!(!result.truncated);
    assert!(result.redacted);
}

#[test]
fn mask_bounded_masks_a_credential_whose_terminator_sits_inside_the_window() {
    // The window is far larger than the message, and the credential's
    // terminating `@` sits well inside it — the ordinary case where the
    // masker sees the whole token and can recognize its shape.
    let password = format!("PlainPassMarker{}", "q".repeat(24));
    let url = format!("postgres://svc:{password}@internal-host.example.test/db");
    let message = format!("backend probe failed: {url}");
    assert!(message.chars().count() < 200, "fixture must fit the window");

    let result = mask_bounded(RedactionSurface::McpDiagnostic, &message, 200, 100);
    assert!(
        !result.text.contains("PlainPassMarker"),
        "credential must be masked: {}",
        result.text
    );
    assert!(result.text.contains("***MASKED***"));
    assert!(result.redacted);
}

#[test]
fn mask_bounded_drops_a_token_straddling_the_window_boundary_without_partial_leak() {
    // Constructed so the credential token starts before the window ends
    // but its terminating `@` lands well past it — a masker restricted
    // to the window can never observe that `@`, so a truncate-then-mask
    // policy (the pre-fix behavior at this call site) would recognize no
    // span and let the visible prefix, including the marker below,
    // survive untouched in the output. Bounding the input before
    // masking must not reopen that hole: since the token has no internal
    // whitespace, the fix drops it whole rather than emitting any
    // fragment of it.
    let window_chars = 200;
    let marker = "StraddleMarkerXYZ789";
    let padding = "q".repeat(window_chars + 100);
    let password = format!("{marker}{padding}");
    let url = format!("postgres://svc:{password}@internal-host.example.test/db");
    let message = format!("benign leading prose here {url}");

    let at_offset = message.find('@').expect("fixture must contain '@'");
    assert!(
        at_offset > window_chars,
        "credential must straddle the window"
    );

    let result = mask_bounded(RedactionSurface::McpDiagnostic, &message, window_chars, 100);
    assert!(
        !result.text.contains(marker),
        "no fragment of the credential may survive: {}",
        result.text
    );
    assert!(
        !result.text.contains("postgres://"),
        "the straddling token must be dropped whole, not partially echoed: {}",
        result.text
    );
    assert!(result.truncated);
}

#[test]
fn mask_bounded_replaces_a_single_oversized_token_with_the_truncation_marker_alone() {
    // One token (no whitespace anywhere) longer than the window: there is
    // no earlier whitespace to fall back to, so nothing from the window
    // can be shown safely.
    let window_chars = 50;
    let text = "q".repeat(window_chars * 4);

    let result = mask_bounded(RedactionSurface::McpDiagnostic, &text, window_chars, 20);
    assert_eq!(result.text, "…");
    assert!(result.truncated);
    assert!(result.redacted);
}

#[test]
fn mask_bounded_bounds_output_to_the_window_regardless_of_input_size() {
    // Several megabytes of benign, whitespace-separated text. A
    // truncate-then-mask or mask-then-truncate policy alike would still
    // need to allocate and scan the full input before this function ever
    // gets to cap the *output*; the point of `mask_bounded` is that the
    // *masker* itself only ever sees `window_chars`. That is asserted
    // here as a length invariant (never by wall-clock): the returned
    // text can never exceed the window, no matter how large the input.
    let window_chars = 4_096;
    let huge = "benign word ".repeat(500_000);
    assert!(huge.len() > 5_000_000, "fixture must be several megabytes");

    let result = mask_bounded(RedactionSurface::McpDiagnostic, &huge, window_chars, 500);
    assert!(result.truncated);
    assert!(result.text.chars().count() <= 501);
}

/// A 40-hex-char credential (the SHA-1/git-SHA-doubled shape) built at
/// test time by cycling a short literal, never committed whole.
fn forty_char_hex_fixture() -> String {
    "a1b2c3d4e5f6".chars().cycle().take(40).collect()
}

#[test]
fn mask_bounded_masks_a_bridged_credential_whose_fragments_straddle_the_window_boundary() {
    // A 40-char hex credential split into two whitespace-separated
    // fragments, each individually too short (20 chars) to be
    // recognized as hex-credential-shaped or high-entropy on its own —
    // recoverable only by bridging the two fragments together. Expected
    // arm: before the fix, the window cut lands inside the second
    // fragment, the existing partial-token drop removes only that
    // fragment's remnant, and the first fragment (alone, unrecognizable)
    // survives visible in the bounded output.
    let hex = forty_char_hex_fixture();
    let (frag1, frag2) = hex.split_at(20);
    let message = format!("credential: {frag1} {frag2}");

    // Control: the unbounded masker sees both fragments and masks the
    // reconstructed credential.
    let full = mask_for_redaction_surface(RedactionSurface::McpDiagnostic, &message);
    assert!(
        !full.contains(frag1) && !full.contains(frag2),
        "control: unbounded masking must reconstruct and mask the split credential: {full}"
    );

    // Land the window a few characters into frag2, so the existing
    // partial-token drop removes only frag2's remnant.
    let window_chars = "credential: ".len() + frag1.len() + 1 + 5;
    assert!(
        window_chars < message.len(),
        "fixture must exceed the window"
    );

    let result = mask_bounded(
        RedactionSurface::McpDiagnostic,
        &message,
        window_chars,
        window_chars,
    );
    assert!(result.truncated);
    assert!(
        !result.text.contains(frag1),
        "no fragment of a bridged credential may survive a window cut mid-chain: {:?}",
        result.text
    );
    assert!(result.redacted);
}

#[test]
fn mask_bounded_masks_a_bridged_credential_entirely_inside_the_window() {
    // Control: both fragments AND the rest of the message fit inside the
    // window — ordinary in-window bridge reconstruction, unaffected by
    // the boundary-drop logic.
    let hex = forty_char_hex_fixture();
    let (frag1, frag2) = hex.split_at(20);
    let message = format!("credential: {frag1} {frag2} trailing prose after the secret");
    let window_chars = message.chars().count() + 10;

    let result = mask_bounded(
        RedactionSurface::McpDiagnostic,
        &message,
        window_chars,
        window_chars,
    );
    assert!(!result.truncated);
    assert!(!result.text.contains(frag1));
    assert!(!result.text.contains(frag2));
}

#[test]
fn mask_bounded_never_reveals_a_bridged_credential_entirely_outside_the_window() {
    // Control: the window cut lands well before the credential even
    // starts, so neither fragment is ever read into the window.
    let hex = forty_char_hex_fixture();
    let (frag1, frag2) = hex.split_at(20);
    let prefix = "benign leading prose that pads well past the cut point here ";
    let message = format!("{prefix}credential: {frag1} {frag2}");
    let window_chars = prefix.chars().count() - 10;

    let result = mask_bounded(
        RedactionSurface::McpDiagnostic,
        &message,
        window_chars,
        window_chars,
    );
    assert!(!result.text.contains(frag1));
    assert!(!result.text.contains(frag2));
}

#[test]
fn mask_bounded_masks_a_bridged_credential_whose_trigger_follows_the_window() {
    // Three-way split (14/13/13 chars) of a 40-char hex credential, each
    // fragment individually too short to be recognized alone. The only
    // trigger word sits AFTER the last fragment ("... is the api key
    // for ..."), never inside the truncated window. Expected arm
    // (pre-fix): `trailing_bridge_fragment_cut` gated its backward walk
    // on a trigger word in the window, and the window here carries no
    // trigger at all — the walk never ran, so frag1 and frag2 (both
    // read whole into the window) survived the partial-token drop and
    // leaked.
    let hex = forty_char_hex_fixture();
    let (frag1, rest) = hex.split_at(14);
    let (frag2, frag3) = rest.split_at(13);
    let message = format!("values: {frag1} {frag2} {frag3} is the api key for the service");

    // Control: the unbounded masker sees the trigger after the
    // fragments and still reconstructs and masks the whole credential.
    let full = mask_for_redaction_surface(RedactionSurface::McpDiagnostic, &message);
    assert!(
        !full.contains(frag1) && !full.contains(frag2) && !full.contains(frag3),
        "control: unbounded masking must reconstruct and mask the split \
             credential even though its trigger word comes after the \
             fragments: {full}"
    );

    // Land the window a few characters into frag3, so the existing
    // partial-token drop removes only frag3's remnant and leaves frag1
    // and frag2 whole in the window; the trigger word stays entirely
    // outside the window.
    let window_chars = "values: ".len() + frag1.len() + 1 + frag2.len() + 1 + 5;
    assert!(
        window_chars < message.len(),
        "fixture must exceed the window"
    );
    assert!(
        find_trigger(&message[..window_chars], false).is_none(),
        "fixture must carry no trigger word inside the window"
    );

    let result = mask_bounded(
        RedactionSurface::McpDiagnostic,
        &message,
        window_chars,
        window_chars,
    );
    assert!(result.truncated);
    assert!(
        !result.text.contains(frag1) && !result.text.contains(frag2),
        "no fragment of a bridged credential may survive a window cut \
             mid-chain, even when the credential's only trigger word lies \
             past the window boundary: {:?}",
        result.text
    );
}

#[test]
fn mask_bounded_keeps_a_non_fragment_tail_of_an_untriggered_truncated_window() {
    // No trigger word anywhere in the message, and the tokens at the
    // tail of the truncated window are ordinary short words (each under
    // `MIN_BRIDGE_FRAGMENT_LEN`) rather than fragment-shaped. The
    // unconditional backward walk must still leave them alone: nothing
    // at the tail looks like a bridged credential fragment.
    let prefix = "benign status update about the lazy owls and cats over ";
    let message = format!("{prefix}here while more prose keeps going past the window");
    assert!(
        find_trigger(&message, false).is_none(),
        "fixture must carry no trigger word"
    );
    let window_chars = prefix.chars().count() + 2;

    let result = mask_bounded(
        RedactionSurface::McpDiagnostic,
        &message,
        window_chars,
        window_chars,
    );
    assert!(result.truncated);
    assert_eq!(result.text, format!("{prefix}{TRUNCATION_MARKER}"));
}

#[test]
fn mask_bounded_drops_a_fragment_shaped_tail_of_an_untriggered_truncated_window() {
    // No trigger word anywhere in the message. A single fragment-shaped
    // identifier (alphanumeric, >= MIN_BRIDGE_FRAGMENT_LEN) sits whole
    // in the window, followed by a short word that gets cut mid-token by
    // the boundary. Documents the trade the unconditional walk makes:
    // the walk cannot tell this lone identifier apart from a genuine
    // bridged fragment, so it drops it too even though nothing is
    // actually chained to it and no trigger word is anywhere nearby.
    let prefix = "an ordinary status line about the current build before ";
    let identifier = "deadbeefcafefeed01234567";
    let message = format!("{prefix}{identifier} zzzzzzzzzz");
    assert!(
        find_trigger(&message, false).is_none(),
        "fixture must carry no trigger word"
    );
    assert!(identifier.len() >= MIN_BRIDGE_FRAGMENT_LEN);

    let window_chars = prefix.chars().count() + identifier.chars().count() + 1 + 3;
    assert!(
        window_chars < message.len(),
        "fixture must exceed the window"
    );

    let result = mask_bounded(
        RedactionSurface::McpDiagnostic,
        &message,
        window_chars,
        window_chars,
    );
    assert!(result.truncated);
    assert!(
        !result.text.contains(identifier),
        "a lone fragment-shaped tail token is dropped even without a \
             chained neighbor or a trigger word: {:?}",
        result.text
    );
}

#[test]
fn mask_bounded_keeps_the_whole_token_prefix_of_an_untriggered_sentence_cut_mid_word() {
    // No trigger word anywhere in the message: the boundary-drop must
    // never fire, so the existing partial-token-drop behavior is
    // unchanged and nothing extra is dropped.
    let prefix = "the quick brown fox jumps over the lazy ";
    let message = format!("{prefix}dogs while writing documentation");
    assert!(
        find_trigger(&message, false).is_none(),
        "fixture must carry no trigger word"
    );
    let window_chars = prefix.chars().count() + 2;

    let result = mask_bounded(
        RedactionSurface::McpDiagnostic,
        &message,
        window_chars,
        window_chars,
    );
    assert!(result.truncated);
    assert_eq!(result.text, format!("{prefix}{TRUNCATION_MARKER}"));
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "the input window must stay at least as large as the output cap")]
fn mask_bounded_debug_asserts_when_window_is_smaller_than_output_cap() {
    let _ = mask_bounded(
        RedactionSurface::McpDiagnostic,
        "some diagnostic text",
        10,
        50,
    );
}

#[test]
#[cfg(not(debug_assertions))]
fn mask_bounded_clamps_output_cap_to_window_chars_outside_debug_assertions() {
    // Inverted pair (window_chars < output_cap_chars): outside debug
    // assertions this must not panic, and the returned text must
    // respect window_chars as the effective cap, never the larger
    // output_cap_chars a misconfigured call site passed in. Uses a
    // degenerate `scheme://user:pass@host` short enough (9 chars) that
    // its `***MASKED***` replacement (12 chars) GROWS past window_chars
    // — the one case where an uncapped `output_cap_chars` would let the
    // returned text exceed the window it was supposed to be bounded by.
    let window_chars = 9;
    let output_cap_chars = 500;
    let text = "a://b:c@d"; // scheme=a, user=b, pass=c, host=d — 9 chars, fits the window whole
    assert_eq!(
        text.len(),
        window_chars,
        "fixture must exactly fill the window"
    );

    let result = mask_bounded(
        RedactionSurface::McpDiagnostic,
        text,
        window_chars,
        output_cap_chars,
    );
    assert!(
        result.text.chars().count() <= window_chars + 1,
        "output must never exceed the window (plus one truncation-marker char) \
             even when output_cap_chars is misconfigured larger than window_chars: {:?}",
        result.text
    );
}

#[test]
fn mask_secrets_redacts_shapes_the_old_mirror_regex_missed() {
    // These are exactly the detectors the session mirror's previous local
    // regex did NOT cover, which is why it now shares this masker.
    let cases = [
        format!("key: {}", openai_project_key_fixture()),
        "cred ASIAFAKEKEY00000000000".to_owned(), // gitleaks:allow
        "stripe sk_live_FAKESTRIPE0000000000000".to_owned(), // gitleaks:allow
        "db postgresql://dbuser:S3cr3tP4ss@db.example.com/db".to_owned(), // gitleaks:allow
    ];
    for c in &cases {
        let masked = mask_secrets(c);
        assert!(
            masked.contains(REDACTION_MARKER),
            "must redact: {c:?} -> {masked:?}"
        );
    }
}

#[test]
fn mask_secrets_redacts_every_span_and_keeps_prose() {
    let line = format!(
        "first {} then ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA end",
        anthropic_api_key_fixture()
    );
    let masked = mask_secrets(&line);
    assert!(
        !masked.contains("sk-ant-api03") && !masked.contains("ghp_AAAA"),
        "no secret may survive: {masked}"
    );
    assert_eq!(
        masked.matches(REDACTION_MARKER).count(),
        2,
        "both secrets must be redacted: {masked}"
    );
    assert!(masked.starts_with("first "), "prose preserved: {masked}");
    assert!(masked.ends_with(" end"), "prose preserved: {masked}");
}

#[test]
fn mask_secrets_public_api_redacts_unscanned_dense_tail() {
    let token = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let segment = format!("{token} keep ");
    // Keep this fixture independent of implementation-only constants: the
    // pre-bound implementation must compile with the test present.
    let line = segment.repeat(512);
    assert!(
        line.len() >= 20_000,
        "fixture must exceed the cumulative scan-work threshold"
    );

    let masked = mask_secrets(&line);
    assert!(
        !masked.contains(token),
        "no credential may survive fail-closed tail redaction"
    );
    assert!(
        masked.matches("***MASKED***").count() < line.matches(token).count(),
        "the public masker must redact the unscanned tail wholesale"
    );
    assert!(
        masked.ends_with("***MASKED***"),
        "the fail-closed tail redaction must reach the end of the public result"
    );
}

// White-box complement only: the public test above is the independent
// guard for the fail-closed work bound.
#[test]
fn mask_secrets_tokenizes_concentrated_tail_once() {
    let token = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let prefix = "clean ".repeat(1_000_000 / "clean ".len());
    let tail: String = (0..200).map(|_| format!("{token} ")).collect();
    let line = format!("{prefix}{tail}");
    assert_eq!(line.matches(token).count(), 200);

    ENTROPY_TOKENIZATION_COUNT.with(|count| count.set(0));
    let masked = mask_secrets(&line);
    let tokenization_count = ENTROPY_TOKENIZATION_COUNT.with(|count| count.get());

    assert!(
        !masked.contains(token),
        "the public masker must redact every concentrated-tail credential"
    );
    assert_eq!(
        tokenization_count, 1,
        "the full input token vector must be built once, not once per tail credential"
    );
}

#[test]
fn mask_secrets_output_passes_check() {
    // The masked output must itself be clean — no credential left for the
    // write-time gate to catch.
    let line = "token=ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA and AKIAFAKEKEY1234567890";
    let masked = mask_secrets(line).into_owned();
    assert!(
        check(&masked).is_ok(),
        "masked output must pass the gate: {masked}"
    );
}

#[test]
fn mask_secrets_redacts_entropy_secret_left_of_known_secret() {
    // Cross-layer leftmost regression: a Layer-2 entropy secret sits to the
    // LEFT of a Layer-1 known-prefix secret. A scan that short-circuits on
    // the first known match (or returns first-by-detector-priority) would
    // redact `ghp_…` and copy the entropy token before it verbatim — leaking
    // it. `scan_match` must fold both layers through leftmost selection.
    let line = "secret=Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvM and ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"; // gitleaks:allow
    let masked = mask_secrets(line).into_owned();
    assert!(
        !masked.contains("Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvM") && !masked.contains("ghp_AAAA"),
        "neither the entropy secret nor the known secret may survive: {masked}"
    );
    assert_eq!(
        masked.matches(REDACTION_MARKER).count(),
        2,
        "both secrets must be redacted exactly once: {masked}"
    );
    assert!(
        check(&masked).is_ok(),
        "masked output must pass the gate: {masked}"
    );
}

#[test]
fn github_app_token_families_are_masked() {
    // ghu_ (user-to-server), ghs_ (server-to-server), and ghr_ (refresh)
    // GitHub App tokens are real credential families. They are
    // context-free: no trigger word needed.
    let cases = [
        "ghu_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", // gitleaks:allow
        "ghs_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",  // gitleaks:allow
        "ghr_CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC",  // gitleaks:allow
    ];
    for token in &cases {
        assert!(
            check(token).is_err(),
            "gate must hard-block GitHub App token {token}"
        );
        let line = format!("auth: {token} trailing");
        let masked = mask_secrets(&line).into_owned();
        assert!(
            !masked.contains(token),
            "GitHub App token must not survive masking: {masked}"
        );
        assert!(
            check(&masked).is_ok(),
            "masked output must pass the gate: {masked}"
        );
    }
}

#[test]
fn mask_secrets_redacts_entropy_token_whose_trigger_is_left_of_earlier_secret() {
    // The entropy detector only fires near a
    // trigger word. When the trigger (`api_key`) sits to the LEFT of an
    // earlier known-prefix secret (`ghp_…`), a masker that rescans only the
    // suffix after each redaction loses that context and leaks the later
    // high-entropy token. Spans must be discovered against the ORIGINAL text.
    let line = "api_key ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvM1"; // gitleaks:allow
    let masked = mask_secrets(line).into_owned();
    assert!(
        !masked.contains("ghp_AAAA"),
        "the known secret must be redacted: {masked}"
    );
    assert!(
        !masked.contains("Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvM1"),
        "the later entropy token must be redacted even though its trigger \
             word sits left of the earlier redaction: {masked}"
    );
    assert_eq!(
        masked.matches(REDACTION_MARKER).count(),
        2,
        "both secrets must be redacted exactly once: {masked}"
    );
    assert!(
        check(&masked).is_ok(),
        "masked output must pass the gate: {masked}"
    );
}

// ── Structured identifiers: file paths / branch names ───────────────────

#[test]
fn allows_high_entropy_file_path_near_secret_word() {
    let content = "workspace path fable-ops/ADR-DRAFT-adr079-slices234.md for the secret gate bug";
    assert!(
        check(content).is_ok(),
        "structured file path in technical prose must pass; got {:?}",
        scan(content)
    );
}

#[test]
fn allows_high_entropy_workspace_path_before_later_key_word() {
    let content =
        "see internal/workspaces/20260701/adr079-slices234/PACKET.md for the key behavior";
    assert!(
        check(content).is_ok(),
        "a path before a later topical key word must pass; got {:?}",
        scan(content)
    );
}

#[test]
fn allows_high_entropy_short_run_path_near_auth_word() {
    let content = "auth work saved at internal/workspaces/20260701/cloud-rebuild/R1-repo-audit.md";
    assert!(
        check(content).is_ok(),
        "path with a short run in technical prose must pass; got {:?}",
        scan(content)
    );
}

#[test]
fn allows_branch_and_review_filename_near_key_word() {
    let content = "branch feat-session-mirror pushed, see release_notes_v2.md for the key findings";
    assert!(
        check(content).is_ok(),
        "branch name and review filename near 'key' must not be blocked; fired: {:?}",
        scan(content)
    );
}

#[test]
fn allows_adr_doc_path_near_password_word() {
    let content = "password reset doc: docs/adr/ADR-055-epistemic-edge-relations.md";
    assert!(
        check(content).is_ok(),
        "ADR doc path near 'password' must not be blocked; fired: {:?}",
        scan(content)
    );
}

#[test]
fn allows_source_file_path_near_credential_word() {
    let content = "credential handling code crates/khive-pack-session/src/mirror/ingest.rs";
    assert!(
        check(content).is_ok(),
        "source file path near 'credential' must not be blocked; fired: {:?}",
        scan(content)
    );
}

#[test]
fn allows_long_snake_case_identifier_near_key_word() {
    let content = "api key handling lives in check_entropy_heuristic_impl";
    assert!(
        check(content).is_ok(),
        "snake_case identifier near 'key' must not be blocked; fired: {:?}",
        scan(content)
    );
}

// ── Structured-identifier exemption: catch-suite regression ─────────────

#[test]
fn hyphenated_random_secret_is_not_a_structured_identifier() {
    // Same token as `blocks_bare_base64url_43chars_near_key`: hyphenated
    // but not word-shaped. The second run exceeds the 24-char run cap,
    // and the first run's case-transition density (~0.42) exceeds the
    // 0.3 threshold on its own, so this must not be exempted and the
    // existing catch-suite test must keep blocking it.
    assert!(!is_structured_identifier(
        "wJalrXUtnFEMI-K7MDENGbPxRfiCYEXAMPLEKEYX123"
    ));
    let line = "api key wJalrXUtnFEMI-K7MDENGbPxRfiCYEXAMPLEKEYX123";
    assert!(
        scan(line).is_some(),
        "hyphenated random secret must still be blocked; got: {:?}",
        scan(line)
    );
}

// ── Structured-identifier exemption: direct unit tests ───────────────────

#[test]
fn structured_identifier_true_for_repro_paths() {
    let paths = [
        "fable-ops/ADR-DRAFT-adr079-slices234.md",
        "internal/workspaces/20260701/adr079-slices234/PACKET.md",
        "internal/workspaces/20260701/cloud-rebuild/R1-repo-audit.md",
        "release_notes_v2.md",
        "docs/adr/ADR-055-epistemic-edge-relations.md",
        "crates/khive-pack-session/src/mirror/ingest.rs",
        "check_entropy_heuristic_impl",
    ];
    for p in paths {
        assert!(
            is_structured_identifier(p),
            "expected structured identifier: {p}"
        );
    }
}

#[test]
fn structured_identifier_false_without_separator() {
    // No `/`, `-`, `_`, or `.` present — fails rule 1 outright.
    assert!(!is_structured_identifier(
        "Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvM"
    ));
}

#[test]
fn structured_identifier_false_for_leetspeak_digit_interleaving() {
    // Digits interleaved with letters within a run (not a trailing digit
    // suffix) fail the `[A-Za-z]+[0-9]*` / `[0-9]+` shape check.
    assert!(!is_structured_identifier("S3cr3t-P4ssw0rd-t0ken-here!"));
}

#[test]
fn structured_identifier_false_for_run_over_length_cap() {
    // A 26-char single alphabetic run between separators fails the
    // 24-char per-run length cap even though it is otherwise trivially
    // word-shaped (uniform lowercase, zero case transitions).
    let long_run = "a".repeat(26);
    let token = format!("prefix-{long_run}-suffix");
    assert!(!is_structured_identifier(&token));
}

// ── Credential-labeled structured identifiers remain blocked ────────────
// Shannon entropy over an attacker-chosen run boundary cannot distinguish an
// English word from adversarially-chosen letters at the same length ceiling
// (log2(run_len)), so no run-shape exemption is sound near a trigger. File paths
// are exempt only after their runs pass the entropy/hex checks AND the path is
// not immediately labeled as a credential value. The tests below guard that
// credential-label boundary.

#[test]
fn blocks_separator_secret_access_key_bypass() {
    // Adversarial bypass shape: an AWS-secret-key-like value split into
    // separator-delimited word-shaped runs to dodge the entropy check.
    let content = "secret_access_key abcdefghij/klmnopqrst/uvwxyzabcd/efghijk";
    assert!(
        check(content).is_err(),
        "AWS Secret Access Key shaped bypass must be blocked: {:?}",
        scan(content)
    );
}

#[test]
fn blocks_adversarial_lowercase_only_separator_token_near_access_key() {
    let content = "access_key qrstuvwxyz/abcdefghij/klmnopqrst/uvwxyzab";
    assert!(
        check(content).is_err(),
        "lowercase-only separator-delimited high-entropy token near \
             'access_key' must be blocked: {:?}",
        scan(content)
    );
}

#[test]
fn blocks_adversarial_digit_and_word_mixed_token_near_api_key() {
    // A mix of pure-digit runs and letters-then-digits runs (both
    // individually word-shaped) whose combined alphabet diversity crosses
    // the entropy threshold.
    let content = "api_key attaycofrsm827/festwqjhc493/8261947350/qwikjzx982";
    assert!(
        check(content).is_err(),
        "digit-and-word-mixed high-entropy token near 'api_key' must be blocked: {:?}",
        scan(content)
    );
}

#[test]
fn blocks_adversarial_token_assignment_separator_delimited_secret() {
    let content = "token=zxkqwmvbpl/trfhysjgnc/dweiaoutkz-mnbvcxzlk";
    assert!(
        check(content).is_err(),
        "token= with lowercase-only separator-delimited high-entropy value \
             must be blocked: {:?}",
        scan(content)
    );
}

#[test]
fn blocks_extension_suffix_bypass_secret_access_key() {
    // A file-extension check alone would exempt this: appending `.md`
    // to a random credential must not bypass detection.
    let content = "secret_access_key abcdefghij/klmnopqrst/uvwxyzabcd/efghijk.md";
    assert!(
        check(content).is_err(),
        "extension-suffixed AWS Secret Access Key shaped bypass must be blocked: {:?}",
        scan(content)
    );
}

#[test]
fn blocks_extension_suffix_bypass_token_assignment() {
    let content = "token=zxkqwmvbpl/trfhysjgnc/dweiaoutkz-mnbvcxzlk.rs";
    assert!(
        check(content).is_err(),
        "extension-suffixed token= bypass must be blocked: {:?}",
        scan(content)
    );
}

#[test]
fn blocks_unsuffixed_separator_split_credential_bypasses() {
    let cases = [
        "secret_access_key abcdefghij/klmnopqrst/uvwxyzabcd/efghijk",
        "token=zxkqwmvbpl/trfhysjgnc/dweiaoutkz-mnbvcxzlk",
    ];
    for content in cases {
        assert!(
            check(content).is_err(),
            "bypass string must still be blocked: {content:?}, got: {:?}",
            scan(content)
        );
    }
}

#[test]
fn blocks_digit_run_suffix_bypass_attempt() {
    let cases = [
        "secret_access_key abcdefghij/klmnopqrst/uvwxyzabcd/efghijk2024",
        "secret_access_key abcdefghij2024/klmnopqrst/uvwxyzabcd/efghijk.md",
    ];
    for content in cases {
        assert!(
            check(content).is_err(),
            "digit-run-suffixed bypass attempt must be blocked: {content:?}, got: {:?}",
            scan(content)
        );
    }
}

#[test]
fn blocks_low_entropy_padding_run_bypass_attempts() {
    // A low-entropy padding run (`aaaa`) inserted before short/digit-shaped
    // runs would drag any AVERAGE per-run entropy signal below its
    // threshold. With the exemption dropped entirely, these must be
    // blocked purely on full-token entropy, same as any other
    // near-trigger high-entropy token.
    let cases = [
        "secret_access_key abcdefghij/klmnopqrst/uvwxyzabcd/efghijk/aaaa/R1.md",
        "token=zxkqwmvbpl/trfhysjgnc/dweiaoutkz/mnbvcxzlk/aaaa/R1.rs",
        "secret_access_key abcdefghij/klmnopqrst/uvwxyzabcd/efghijk/aaaa/bbbb/R1.md",
        "token=zxkqwmvbpl/trfhysjgnc/dweiaoutkz/mnbvcxzlk/aaaa/bbbb/R1.rs",
    ];
    for content in cases {
        assert!(
            check(content).is_err(),
            "padding-run bypass attempt must be blocked: {content:?}, got: {:?}",
            scan(content)
        );
    }
}

#[test]
fn blocks_run_splitting_bypass_attempts() {
    // Splitting a credential into short (4-6 char) runs drives EVERY
    // run's own letters-only entropy toward log2(run_len), which ordinary
    // short English path words already sit at or near: this is exactly
    // why any per-run entropy ceiling is unsound as an exemption signal.
    // With the exemption dropped, these are blocked on full-token entropy
    // regardless of run shape.
    let cases = [
        "secret_access_key abcd/efgh/ijkl/mnop/qrst/uvwx/yzab/cdef.md",
        "secret_access_key abcde/fghij/klmno/pqrst/uvwxy/zabcd.md",
        "secret_access_key abcdef/ghijkl/mnopqr/stuvwx/yzabcd.md",
    ];
    for content in cases {
        assert!(
            check(content).is_err(),
            "run-splitting bypass attempt must be blocked: {content:?}, got: {:?}",
            scan(content)
        );
    }
}

#[test]
fn blocks_separator_split_generic_hex_credential_ascii() {
    // Two 20-char hex runs joined by `/` — individually
    // below MIN_ENTROPY_LEN (24) and neither is a HEX_CREDENTIAL_LENGTHS
    // length on its own; the whole token is not pure hex (the `/` breaks
    // it) and its entropy is capped below ENTROPY_THRESHOLD by the
    // 17-symbol hex-plus-separator alphabet, so none of the prior checks
    // caught it. Concatenating the two runs (dropping the separator)
    // normalizes to one 40-char hex sequence — a HEX_CREDENTIAL_LENGTHS
    // value.
    let content = "api key 0123456789abcdef0123/456789abcdef01234567";
    assert!(
        check(content).is_err(),
        "separator-split hex credential must be blocked: got {:?}",
        scan(content)
    );
}

#[test]
fn blocks_separator_split_generic_hex_credential_unicode_separator() {
    // Same shape as above, but the separator is a non-ASCII character
    // (U+200B zero-width space) instead of `/`: the tokenizer treats
    // every non-ASCII character as a delimiter (see the tokenizer
    // comment in `check_entropy_heuristic`), so this splits the payload
    // into TWO tokens rather than leaving it inside one — the
    // intra-token concatenation above never sees both halves together.
    // The adjacent-token bridge must catch it the same way.
    let content = "api key 0123456789abcdef0123\u{200B}456789abcdef01234567";
    assert!(
        check(content).is_err(),
        "Unicode-separator split hex credential must be blocked: got {:?}",
        scan(content)
    );
}

#[test]
fn blocks_unicode_split_hex_credential_with_path_shaped_anchor() {
    let content = "api key handling uses source/x/0123456789abcdef0123\u{200B}456789abcdef01234567";
    let tokens: Vec<(usize, &str)> = content
        .split(|c: char| c.is_ascii_whitespace() || !c.is_ascii())
        .filter(|token| !token.is_empty())
        .map(|token| (token.as_ptr() as usize - content.as_ptr() as usize, token))
        .collect();
    let anchor = tokens
        .iter()
        .position(|(_, token)| token.starts_with("source/"))
        .expect("path anchor must be tokenized");
    assert!(is_plausible_file_path(tokens[anchor].1));
    let fragments = bridge_fragment_chain(&tokens, content, anchor);
    assert_eq!(fragments.len(), 2);
    assert!(normalized_hex_credential_span(&fragments.join(" ")).is_some());
    assert!(
        check(content).is_err(),
        "path-shaped anchor must not bypass fragment reconstruction: got {:?}",
        scan(content)
    );
}

#[test]
fn blocks_separator_split_hex_credential_repeated_unicode_gap() {
    // Three U+200B zero-width spaces in a row
    // (9 bytes) would exceed a fixed byte-length gap bound (e.g. 8 bytes),
    // leaving the two 20-char hex halves unbridged. The fragment-chain
    // bridge is bounded by fragment COUNT (MAX_BRIDGE_FRAGMENTS), not
    // gap byte length, so repeating the delimiter buys an attacker
    // nothing: the gap between the two fragments still contains zero
    // ASCII alphanumeric characters, so it is still one bridgeable gap
    // regardless of how many times the delimiter repeats inside it.
    let content = "api key 0123456789abcdef0123\u{200B}\u{200B}\u{200B}456789abcdef01234567";
    assert!(
        check(content).is_err(),
        "repeated-Unicode-gap split hex credential must be blocked: got {:?}",
        scan(content)
    );
}

#[test]
fn blocks_separator_split_three_way_hex_credential_mixed_case() {
    // A 40-char mixed-case hex credential split
    // into THREE tokens by two single-U+200B gaps. Bridging only one
    // adjacent pair (`idx` with `idx + 1`, or `idx` with
    // `idx - 1`) would never reach the full
    // 40 chars. `bridge_fragment_chain` walks a bounded chain in both
    // directions, so starting from the first fragment reconstructs all
    // three. `is_ascii_hexdigit` accepts both cases, so the mixed-case
    // split (`AAAA...` alongside lowercase `cccc...`) must still
    // normalize to one 40-char hex sequence.
    let content = "api key AAAA1111bbbb22\u{200B}22cccc3333ddd\u{200B}d4444eeee5555";
    assert!(
        check(content).is_err(),
        "three-way mixed-case Unicode-split hex credential must be blocked: got {:?}",
        scan(content)
    );
}

#[test]
fn blocks_separator_split_base64_like_unicode_credential() {
    // A base64-like credential (mixed-case
    // alphanumeric, not hex) split by one U+200B into two 20-char
    // halves. A hex-only bridge candidacy gate would only admit pure-hex
    // short tokens, so neither half here (mixed-case, non-hex letters
    // like `X`, `k`, `Z`) ever reached the near-trigger bridge checks at
    // all. `is_bridge_candidate` now admits any short alphanumeric
    // token, and the reconstructed chain is checked against the SAME
    // whole-token entropy decision a genuine single-token high-entropy
    // candidate must clear — closing the hex-only gap without widening
    // detection to non-alphanumeric noise.
    let content = "api key Xk9mZ2vQpLrT8nJwYuAe\u{200B}HfBsDcGiONvMabcdefgh";
    assert!(
        check(content).is_err(),
        "base64-like Unicode-split credential must be blocked: got {:?}",
        scan(content)
    );
}

#[test]
fn blocks_punctuation_glue_between_two_unicode_gaps() {
    // Two 20-char hex fragments separated by a punctuation-only token (`---`)
    // sandwiched between two U+200B gaps: `is_delimiter_only_token` must let the
    // walk absorb `---` as glue (without counting it against MAX_BRIDGE_FRAGMENTS)
    // rather than stopping at it as a non-fragment token.
    let content = "api key 0123456789abcdef0123\u{200B}---\u{200B}456789abcdef01234567";
    assert!(
        check(content).is_err(),
        "punctuation-glue split hex credential between two Unicode gaps must be \
             blocked: got {:?}",
        scan(content)
    );
}

#[test]
fn allows_seven_way_hex_split_beyond_fragment_cap_documented_limitation() {
    // A 64-hex credential split into SEVEN
    // Unicode-separated fragments (each meeting MIN_BRIDGE_FRAGMENT_LEN)
    // exceeds MAX_BRIDGE_FRAGMENTS (6), so no chain the walk can build
    // ever reconstructs the full 64 chars. This is an ACCEPTED RESIDUAL
    // of the local-neighborhood bound, not a defect to fix here: per
    // ADR-096 / ADR-115 the secret gate is accidental-persistence
    // hygiene on a single-principal same-uid host, not defense against a
    // same-uid adversary hand-splitting a credential to evade it — that
    // adversary could write the DB directly instead. This test pins the
    // boundary so a future reader does not mistake it for an
    // unaddressed bypass.
    let content = "api key 012345678\u{200B}9abcdef01\u{200B}23456789a\u{200B}bcdef0123\u{200B}456789abc\u{200B}def012345\u{200B}6789abcdef";
    assert!(
        check(content).is_ok(),
        "seven-way hex split beyond MAX_BRIDGE_FRAGMENTS is a documented residual \
             limitation and must stay allowed: got {:?}",
        scan(content)
    );
}

#[test]
fn allows_six_way_sub_floor_hex_split_documented_limitation() {
    // A 40-hex credential split into six
    // Unicode-separated fragments each individually below
    // MIN_BRIDGE_FRAGMENT_LEN (8) — 7/7/7/7/6/6 characters. Every
    // fragment fails `is_bridge_candidate`, so none ever reaches
    // `bridge_fragment_chain` in the first place. Same accepted-residual
    // rationale as the seven-way split above: a same-uid adversary
    // splitting fragments this small to evade the gate can equally
    // write the DB directly. This test pins the boundary.
    let content =
        "api key 0123456\u{200B}789abcd\u{200B}ef01234\u{200B}56789ab\u{200B}cdef01\u{200B}234567";
    assert!(
        check(content).is_ok(),
        "six-way sub-MIN_BRIDGE_FRAGMENT_LEN hex split is a documented residual \
             limitation and must stay allowed: got {:?}",
        scan(content)
    );
}

#[test]
fn allows_unrelated_short_fragments_cited_near_a_trigger_word() {
    // False-positive guard: ordinary prose
    // citing two SEPARATE short hex/base64-ish identifiers (e.g. two
    // unrelated git SHA prefixes) near a trigger word must NOT combine
    // into a block just because a delimiter-only gap between them makes
    // them bridge-eligible. Each fragment is well under
    // MIN_BRIDGE_FRAGMENT_LEN's credential-length neighborhood, and the
    // reconstructed concatenation (16 chars) is neither a
    // HEX_CREDENTIAL_LENGTHS value nor at MIN_ENTROPY_LEN (24), so it
    // must stay allowed exactly like a real single fragment that short
    // would.
    let content = "api key: see commits abc12345, def67890 for the fix";
    assert!(
        check(content).is_ok(),
        "unrelated short fragments cited near a trigger word must stay allowed: \
             fired {:?}",
        scan(content)
    );
}

#[test]
fn allows_unrelated_short_base64_like_fragments_cited_near_a_trigger_word() {
    // Same guard as above, for the newly-widened non-hex/base64-like
    // bridge path specifically: two short mixed-case alphanumeric build
    // identifiers separated by a plain space near a trigger word.
    // Reconstructed length (12 chars) is far under MIN_ENTROPY_LEN (24),
    // so the generic entropy reconstruction must not fire.
    let content = "api key: build ids Ab3Kf9 and Xy7Lm2 do not match";
    assert!(
        check(content).is_ok(),
        "unrelated short base64-like fragments cited near a trigger word must \
             stay allowed: fired {:?}",
        scan(content)
    );
}

#[test]
fn allows_scattered_short_hex_runs_that_do_not_sum_to_a_credential_length() {
    // False-positive guard: short hex-looking runs
    // that happen to sit near a trigger word must NOT be flagged just
    // because they exist — only when their normalized concatenation
    // actually lands on a HEX_CREDENTIAL_LENGTHS value. Three
    // independent 8-char runs (running total 8, 16, 24) never hit
    // 32/40/64/128, and the whole-token entropy check that follows stays
    // below ENTROPY_THRESHOLD for this path-shaped content.
    let content = "auth config lives in abc12345/de678901/fa234567.md";
    assert!(
        check(content).is_ok(),
        "scattered short hex runs that never sum to a credential length \
             must stay allowed: fired {:?}",
        scan(content)
    );
}

#[test]
fn allows_fp_paths_whose_full_token_entropy_is_already_below_threshold() {
    // 4 of the 7 original FP-repro paths stay OK near a trigger word even
    // with NO structured-identifier exemption at all, because their own
    // full-token Shannon entropy already reads below ENTROPY_THRESHOLD
    // (4.5) — the exemption was never load-bearing for these regardless
    // of which version of it existed.
    let paths = [
        "release_notes_v2.md",
        "docs/adr/ADR-055-epistemic-edge-relations.md",
        "crates/khive-pack-session/src/mirror/ingest.rs",
        "check_entropy_heuristic_impl",
    ];
    for p in paths {
        let content = format!("api_key handling in {p}");
        assert!(
            check(&content).is_ok(),
            "{p} must stay allowed near 'api_key' (full-token entropy already \
                 below threshold): fired {:?}",
            scan(&content)
        );
    }
}

#[test]
fn allows_adr_draft_path_near_trigger() {
    let content = "api_key handling in fable-ops/ADR-DRAFT-adr079-slices234.md";
    assert!(
        check(content).is_ok(),
        "ADR-DRAFT path in technical prose must pass; got {:?}",
        scan(content)
    );
}

#[test]
fn allows_workspace_packet_path_near_trigger() {
    let content = "api_key handling in internal/workspaces/20260701/adr079-slices234/PACKET.md";
    assert!(
        check(content).is_ok(),
        "workspace path in technical prose must pass; got {:?}",
        scan(content)
    );
}

#[test]
fn allows_high_entropy_repo_audit_path_near_api_key() {
    let content = "api_key handling in internal/workspaces/20260701/cloud-rebuild/R1-repo-audit.md";
    assert!(
        check(content).is_ok(),
        "repository path in technical prose must pass; got {:?}",
        scan(content)
    );
}

#[test]
fn allows_source_paths_near_ordinary_key_and_token_prose() {
    let contents = [
        "see <a/path/to/file.py>:~97-103 lists it as a real checkpoint-supplied key",
        "see <a/path/to/file.py>:~97-103 emits one extra token",
        "see /workspace/src/checkpoint_loader.rs:97-103\n\nconfiguration key behavior",
        "the token behavior is implemented in src/runtime/secret_gate.rs:618-914.",
    ];
    for content in contents {
        assert!(
            check(content).is_ok(),
            "source path in technical prose must pass: {content:?}, got {:?}",
            scan(content)
        );
    }
}

#[test]
fn allows_adr_authorization_path_in_markdown_ingest() {
    let content =
        "- Evidence: `docs/adr/C-ADR-007-authorization-server.md:70` defines token handling.";
    assert!(
        check(content).is_ok(),
        "ADR path citation in technical markdown must pass: got {:?}",
        scan(content)
    );
}

#[test]
fn blocks_markdown_ingest_bare_opaque_value_near_token() {
    let content = concat!(
        "- Review evidence for token handling: ",
        "Xk9mZ2vQpLrT8nJwYuAeHfBs",
        "DcGiONvM1qPrStUvWxYz23456789"
    );
    assert_eq!(
        scan(content).map(|matched| matched.detector),
        Some("high-entropy-token"),
        "bare opaque value near token must remain blocked"
    );
}

// ── UUID / content-hash allowlists are prose-context only ───────────────

#[test]
fn blocks_uuid_directly_labeled_as_api_key() {
    let content = "api_key 550e8400-e29b-41d4-a716-446655440000";
    assert!(
        check(content).is_err(),
        "UUID-shaped token labeled api_key must be blocked; got {:?}",
        scan(content)
    );
}

#[test]
fn blocks_sha256_content_hash_labeled_as_secret() {
    let content = "secret sha256-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopq";
    assert!(
        check(content).is_err(),
        "sha256-prefixed hash labeled secret must be blocked; got {:?}",
        scan(content)
    );
}

#[test]
fn blocks_sha384_content_hash_labeled_as_api_key() {
    let content = "api_key sha384-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    assert!(
        check(content).is_err(),
        "sha384-prefixed hash labeled api_key must be blocked; got {:?}",
        scan(content)
    );
}

#[test]
fn blocks_sha512_content_hash_labeled_as_auth() {
    let content = "auth sha512-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/ABCDEFGHIJKLMNOPQRSTUV";
    assert!(
        check(content).is_err(),
        "sha512-prefixed hash labeled auth must be blocked; got {:?}",
        scan(content)
    );
}

#[test]
fn allows_uuid_with_no_trigger_within_window() {
    // Common benign shape: a UUID (e.g. an internal record id) with no
    // credential trigger word anywhere in the surrounding window stays
    // allowed — the allowlist still applies outside trigger context.
    let content = "task 550e8400-e29b-41d4-a716-446655440000 was created and assigned to the team";
    assert!(
        check(content).is_ok(),
        "UUID with no nearby trigger word must stay allowed; got {:?}",
        scan(content)
    );
}

#[test]
fn allows_area_id_uuid_near_authorized_substring() {
    // An internal task `area_id` UUID field sitting within the trigger
    // window of the SUBSTRING "auth" inside
    // `authorized_write_requires_dominance` is not a genuine mention of
    // the word "auth": it is a pure substring collision with
    // "authorized". Bare trigger words match at a word boundary (see
    // `contains_bounded_word`), so `auth` does not match inside
    // `authorized`; this UUID has no trigger in its window and passes via
    // the ordinary out-of-context UUID allowlist.
    let content = "area_id: cfcea31d-6f50-4fd1-ad6d-5f160de1694c\n\n## Problem\nReduce Lion microkernel axioms. Converted authorized_write_requires_dominance from axiom to theorem.";
    assert!(
        check(content).is_ok(),
        "internal area_id UUID near the 'authorized' substring \
             (not a genuine 'auth' mention) must now pass; got {:?}",
        scan(content)
    );
}

#[test]
fn allows_uuid_on_line_after_benign_token_contract_title() {
    let content = "Design language and token contract\n550e8400-e29b-41d4-a716-446655440000";
    assert!(
        check(content).is_ok(),
        "a generic token-contract title must not make a next-line UUID look like a secret; \
             got {:?}",
        scan(content)
    );
}

#[test]
fn generic_token_uuid_exemption_keeps_strong_credential_controls() {
    let opaque = "Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvMabcdef"; // gitleaks:allow
    let uuid = "550e8400-e29b-41d4-a716-446655440000";
    let cases = [
        (format!("service token {opaque}"), "high-entropy-token"),
        (format!("token={opaque}"), "high-entropy-token"),
        (format!("token={uuid}"), "uuid-near-trigger"),
        (format!("api_key {uuid}"), "uuid-near-trigger"),
    ];

    for (content, detector) in cases {
        assert_eq!(
            scan(&content).map(|matched| matched.detector),
            Some(detector),
            "credential-shaped control must remain blocked: {content:?}"
        );
    }
}

// ── UUID/hash value extraction from assignment and wrapper syntax ───────

#[test]
fn blocks_uuid_glued_to_assignment_equals() {
    let content = "api_key=550e8400-e29b-41d4-a716-446655440000";
    assert!(
        check(content).is_err(),
        "UUID glued via '=' to a trigger word must be blocked; got {:?}",
        scan(content)
    );
}

#[test]
fn blocks_uuid_with_trailing_sentence_period() {
    let content = "api_key 550e8400-e29b-41d4-a716-446655440000.";
    assert!(
        check(content).is_err(),
        "UUID with a trailing sentence period near a trigger must be blocked; got {:?}",
        scan(content)
    );
}

#[test]
fn blocks_uuid_wrapped_in_parens() {
    let content = "api_key (550e8400-e29b-41d4-a716-446655440000)";
    assert!(
        check(content).is_err(),
        "UUID wrapped in parens near a trigger must be blocked; got {:?}",
        scan(content)
    );
}

#[test]
fn blocks_uuid_in_json_object() {
    let content = "{\"api_key\":\"550e8400-e29b-41d4-a716-446655440000\"}";
    assert!(
        check(content).is_err(),
        "UUID in a JSON-ish object near a trigger key must be blocked; got {:?}",
        scan(content)
    );
}

#[test]
fn blocks_content_hash_glued_to_assignment_equals() {
    let content = "secret=sha256-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopq";
    assert!(
        check(content).is_err(),
        "sha256-prefixed hash glued via '=' to a trigger word must be blocked; \
             got {:?}",
        scan(content)
    );
}

#[test]
fn blocks_content_hash_with_trailing_sentence_period() {
    let content = "secret sha256-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopq.";
    assert!(
        check(content).is_err(),
        "sha256-prefixed hash with a trailing period near a trigger must be \
             blocked; got {:?}",
        scan(content)
    );
}

#[test]
fn blocks_content_hash_wrapped_in_parens() {
    let content = "secret (sha256-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopq)";
    assert!(
        check(content).is_err(),
        "sha256-prefixed hash wrapped in parens near a trigger must be blocked; \
             got {:?}",
        scan(content)
    );
}

#[test]
fn blocks_content_hash_in_json_object() {
    let content = "{\"secret\":\"sha256-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopq\"}";
    assert!(
        check(content).is_err(),
        "sha256-prefixed hash in a JSON-ish object near a trigger key must be \
             blocked; got {:?}",
        scan(content)
    );
}

#[test]
fn allows_uuid_wrapped_in_parens_with_no_trigger_nearby() {
    // Control: the prose allowlist must survive for wrapper syntax when
    // there is no credential trigger word anywhere in the window — only
    // the trigger-context extraction changed, not the outside-context
    // allowlist itself.
    let content = "wrapper (550e8400-e29b-41d4-a716-446655440000) present";
    assert!(
        check(content).is_ok(),
        "UUID wrapped in parens with no trigger word nearby must stay allowed; \
             got {:?}",
        scan(content)
    );
}

#[test]
fn blocks_padded_content_hash_glued_to_assignment_with_trailing_period() {
    // A padded base64 value ends in its own `=`, which is also a valid
    // separator character — `value_candidates` must enumerate the
    // suffix after every `=`/`:`, not assume any single separator
    // position, so the true value is recovered regardless of which
    // separator happens to sit where.
    let content = "secret=sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=.";
    assert!(
        check(content).is_err(),
        "padded sha256 hash glued via '=' with a trailing period must be \
             blocked; got {:?}",
        scan(content)
    );
}

#[test]
fn blocks_padded_content_hash_in_json_object() {
    let content = "{\"secret\":\"sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\"}";
    assert!(
        check(content).is_err(),
        "padded sha256 hash in a JSON-ish object near a trigger key must be \
             blocked; got {:?}",
        scan(content)
    );
}

#[test]
fn blocks_uuid_when_json_label_itself_contains_colon() {
    // The label can itself contain the separator character
    // (`"api:key"` rather than `"api_key"`); the first `:` after
    // wrapper-stripping then lands inside the label, not at the
    // label/value boundary. value_candidates must still surface the
    // bare UUID as a later suffix candidate.
    let content = "{\"api:key\":\"550e8400-e29b-41d4-a716-446655440000\"}";
    assert!(
        check(content).is_err(),
        "UUID must be blocked even when the JSON label contains ':'; got {:?}",
        scan(content)
    );
}

#[test]
fn blocks_uuid_when_json_label_itself_contains_equals() {
    let content = "{\"api=key\":\"550e8400-e29b-41d4-a716-446655440000\"}";
    assert!(
        check(content).is_err(),
        "UUID must be blocked even when the JSON label contains '='; got {:?}",
        scan(content)
    );
}

#[test]
fn blocks_uuid_behind_doubled_assignment() {
    // key=label=value: the first `=` lands between two labels, not at
    // the true value boundary.
    let content = "api_key=label=550e8400-e29b-41d4-a716-446655440000"; // gitleaks:allow
    assert!(
        check(content).is_err(),
        "UUID must be blocked behind a doubled assignment; got {:?}",
        scan(content)
    );
}

#[test]
fn blocks_padded_content_hash_behind_doubled_assignment_equals() {
    let content = "secret=label=sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=.";
    assert!(
        check(content).is_err(),
        "padded content hash must be blocked behind a doubled '=' assignment; \
             got {:?}",
        scan(content)
    );
}

#[test]
fn blocks_padded_content_hash_behind_doubled_assignment_colon() {
    let content = "secret:label=sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=.";
    assert!(
        check(content).is_err(),
        "padded content hash must be blocked behind a doubled ':'+'=' \
             assignment; got {:?}",
        scan(content)
    );
}

#[test]
fn allows_benign_url_with_scheme_and_path_separators() {
    // `value_candidates`'s any-suffix semantics must not block ordinary
    // URLs, whose `://` and `/` characters produce several suffix
    // candidates but none of them are UUID- or content-hash-shaped.
    // Placed near a real trigger word ("key") so the check actually
    // exercises the trigger-context path rather than being skipped
    // outright.
    let content = "api_key endpoint=https://example.test/resource/for/testing";
    assert!(
        check(content).is_ok(),
        "a benign URL near a trigger word must stay allowed; got {:?}",
        scan(content)
    );
}

// ── Trigger word-boundary matching ──────────────────────────────────────

#[test]
fn allows_trigger_substrings_inside_benign_path_slugs() {
    let paths = [
        "docs/_archive/adr_v0/ADR-051-cli-auth-and-kg-git-workflow.md",
        "docs/platform/oauth-callback-docs-and-redirect-handling-v2.md",
        "docs/research/author-attribution-and-collaboration-notes.md",
        "docs/security/passwordless-authentication-overview-v3.md",
        "docs/platform/private_keynote-authoring-guide-v2.md",
    ];
    for path in paths {
        assert!(
            check(path).is_ok(),
            "trigger substring inside a benign path slug must not make the path \
                 its own credential context: {path:?}, got {:?}",
            scan(path)
        );
    }
}

#[test]
fn blocks_inline_auth_assignment_with_high_entropy_value() {
    let content = "auth=Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvMabcdef"; // gitleaks:allow
    assert!(
        check(content).is_err(),
        "auth=<high-entropy-value> must still be blocked; got {:?}",
        scan(content)
    );
}

#[test]
fn blocks_suffix_bearing_compound_credential_assignments() {
    let opaque = "Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvMabcdef"; // gitleaks:allow
    let cases = [
        format!("api_keyv2={opaque}"),
        format!("access_keyv2={opaque}"),
        format!("private_keyv2={opaque}"),
        format!("API_KEYV2={opaque}"),
        format!(r#"{{"private_keyv2":"{opaque}"}}"#),
    ];
    for content in &cases {
        assert!(
            check(content).is_err(),
            "suffix-bearing compound credential assignment must be blocked: \
                 {content:?}, got {:?}",
            scan(content)
        );
    }
}

#[test]
fn blocks_spaced_suffix_bearing_compound_credential_assignments() {
    let opaque = "Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvMabcdef"; // gitleaks:allow
    for label in ["api_keyv2", "access_keyv2", "private_keyv2"] {
        let cases = [
            format!("{label} = {opaque}"),
            format!("{label} : {opaque}"),
            format!(r#"{{"{label}": "{opaque}"}}"#),
        ];
        for content in &cases {
            assert!(
                check(content).is_err(),
                "spaced suffix-bearing compound credential assignment must be \
                     blocked: {content:?}, got {:?}",
                scan(content)
            );
        }
    }
}

#[test]
fn blocks_suffix_bearing_compound_credentials_without_assignment_separator() {
    let opaque = "Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvMabcdef"; // gitleaks:allow
    for label in ["api_keyv2", "access_keyv2", "private_keyv2"] {
        let cases = [format!("{label} {opaque}"), format!("{label}{opaque}")];
        for content in &cases {
            assert!(
                check(content).is_err(),
                "suffix-bearing compound credential without an assignment separator must be \
                     blocked: {content:?}, got {:?}",
                scan(content)
            );
            assert!(
                mask_secrets(content).contains(REDACTION_MARKER),
                "shared secret masker must redact separator-free compound credential: \
                     {content:?}"
            );
        }
    }

    let prefixed = "xapi_keyv2=Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvMabcdef"; // gitleaks:allow
    assert!(
        check(prefixed).is_err(),
        "prefix-bearing compound credential assignment must be blocked: \
             {prefixed:?}, got {:?}",
        scan(prefixed)
    );
}

#[test]
fn allows_authorized_and_authentication_prose_near_uuid() {
    // The word-boundary fix directly: "auth" no longer matches the
    // substring inside "authorized"/"authentication", so ordinary prose
    // using those words does not poison the trigger window for a nearby
    // UUID or other allowlisted shape.
    let cases = [
        "authorized_write_requires_dominance was converted from axiom to theorem, id 550e8400-e29b-41d4-a716-446655440000",
        "authentication flow diagram lives at 550e8400-e29b-41d4-a716-446655440000",
    ];
    for content in cases {
        assert!(
            check(content).is_ok(),
            "'authorized'/'authentication' substring must not trigger the \
                 entropy heuristic: {content:?}, got {:?}",
            scan(content)
        );
    }
}

#[test]
fn allows_turkey_monkey_keyword_prose_near_uuid() {
    // Other bare-word substring collisions in TRIGGER_WORDS ("key") must
    // likewise not fire on ordinary English words that merely contain it.
    let cases = [
        "the turkey and monkey story references id 550e8400-e29b-41d4-a716-446655440000",
        "keyword research doc: 550e8400-e29b-41d4-a716-446655440000",
    ];
    for content in cases {
        assert!(
            check(content).is_ok(),
            "'turkey'/'monkey'/'keyword' substring must not trigger the \
                 entropy heuristic: {content:?}, got {:?}",
            scan(content)
        );
    }
}

#[test]
fn blocks_opaque_tokens_near_standalone_trigger_words() {
    // Word-boundary matching only removes SUBSTRING collisions; a genuine
    // standalone trigger word must still dominate exactly as before.
    let opaque = "Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvMabcdef"; // gitleaks:allow
    let cases = [
        format!("auth header {opaque}"),
        format!("the key is {opaque}"),
        format!("secret value: {opaque}"),
    ];
    for content in &cases {
        assert!(
            check(content).is_err(),
            "a genuine standalone trigger word must still block: {content:?}, \
                 got {:?}",
            scan(content)
        );
    }
}

#[test]
fn issue_2654_lookup_members_accept_record_references() {
    let id = "550e8400-e29b-41d4-a716-446655440000";
    for label in [
        "association_key",
        "partition_key",
        "sort_key",
        "cache_key",
        "idempotency_key",
        "primary_key",
    ] {
        for value in [id, "", "runtime/current", "record-slug"] {
            for separator in [",", ", "] {
                let content = format!(r#"{{"{label}":"{value}"{separator}"neighbor":"{id}"}}"#);
                assert!(check(&content).is_ok(), "{content}: {:?}", check(&content));
                assert_eq!(mask_secrets(&content), content);
            }
        }
    }
    let renamed = format!(r#"{{"association_ref":"{id}","neighbor":"{id}"}}"#);
    assert!(check(&renamed).is_ok());
}

#[test]
fn issue_2654_credential_compounds_and_natural_assignments_stay_refused() {
    let id = "550e8400-e29b-41d4-a716-446655440000";
    for label in [
        "key",
        "api_key",
        "secret_key",
        "private_key",
        "access_key",
        "signing_key",
        "encryption_key",
        "auth_key",
        "service_signing_key",
    ] {
        for content in [
            format!("{label}={id}"),
            format!("{label}: {id}"),
            format!(r#"{{"{label}":"{id}"}}"#),
        ] {
            assert!(check(&content).is_err(), "{content}");
            assert!(!mask_secrets(&content).contains(id), "{content}");
        }
    }
    let opaque = "Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvMabcdef"; // gitleaks:allow
    for content in [
        format!("the key is {opaque}"),
        format!("api key {opaque}"),
        format!("association_key {id}"),
        format!("_key_ = {id}"),
        format!("association_key=x key={id}"),
    ] {
        assert!(check(&content).is_err(), "{content}");
    }
    assert!(check("secret docs/guide.md").is_ok());
    assert!(check("auth release-slug").is_ok());
}

#[test]
fn issue_2654_lookup_exception_requires_an_assignment_gap() {
    let id = "550e8400-e29b-41d4-a716-446655440000";
    for gap in ["***", ")", "}", "\\", "!", " ", "\" "] {
        let content = format!("association_key{gap}:{id}");
        assert!(check(&content).is_err(), "{content}");
        assert!(!mask_secrets(&content).contains(id), "{content}");
    }
    for gap in ["", "\"", "'", "`"] {
        let content = format!("association_key{gap}:{id}");
        assert!(check(&content).is_ok(), "{content}");
    }
}

#[test]
fn issue_2654_repeated_key_identifier_is_scanned_once() {
    let content = format!("{}key=x", "key_".repeat(262_144));
    assert!(check(&content).is_ok());
}

#[test]
fn issue_2654_unicode_before_lookup_member_keeps_boundaries() {
    let content = "記録association_key: 550e8400-e29b-41d4-a716-446655440000";
    assert!(check(content).is_ok(), "{:?}", check(content));
}

#[test]
fn issue_2654_unlisted_key_compounds_stay_credential_labels() {
    // The lookup exception is a closed allowlist: a `*_key` label it does
    // not name keeps `key` as a credential trigger for every value shape.
    let hex = "0123456789abcdef".repeat(4);
    let id = "550e8400-e29b-41d4-a716-446655440000";
    let neighbor = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";
    let opaque = "Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvMabcdef"; // gitleaks:allow
    for label in [
        "hmac_key",
        "master_key",
        "ssh_key",
        "jwt_key",
        "webhook_key",
        "license_key",
        "gpg_key",
        "service_key",
        "client_key",
        "service_hmac_key",
    ] {
        for value in [hex.as_str(), id, opaque] {
            for content in [
                format!("{label}={value}"),
                format!("{label}: {value}"),
                format!(r#"{{"{label}":"{value}"}}"#),
                format!(r#"{{"neighbor":"{neighbor}","{label}":"{value}"}}"#),
            ] {
                assert!(check(&content).is_err(), "{content}");
                assert!(!mask_secrets(&content).contains(value), "{content}");
            }
        }
    }
}

#[test]
fn issue_2654_listed_lookup_labels_accept_identifier_shapes() {
    let hex = "0123456789abcdef".repeat(4);
    let id = "550e8400-e29b-41d4-a716-446655440000";
    for label in LOOKUP_KEY_LABELS {
        for value in [id, hex.as_str(), "runtime/current", "record-slug"] {
            for content in [
                format!("{label}={value}"),
                format!(r#"{{"{label}":"{value}","neighbor":"{id}"}}"#),
            ] {
                assert!(check(&content).is_ok(), "{content}: {:?}", scan(&content));
                assert_eq!(mask_secrets(&content), content);
            }
        }
    }
    // A credential word in the prefix is its own trigger.
    assert!(check(&format!("secret_partition_key={id}")).is_err());
    assert!(check(&format!("api_key_cache_key={id}")).is_err());
}

#[test]
fn issue_2654_qualified_lookup_labels_stay_refused() {
    // The vocabulary matches whole labels. A prefix rule ("anything ending
    // in `_` before a listed suffix") re-opens exactly the compounds the
    // list closes, because stems such as `hmac` or `jwt` are not trigger
    // words of their own. The cost is that a qualified lookup spelling
    // (`left_association_key`) is refused too; that is the fail-closed side.
    let hex = "0123456789abcdef".repeat(4);
    let id = "550e8400-e29b-41d4-a716-446655440000";
    for label in [
        "hmac_cache_key",
        "master_routing_key",
        "jwt_lookup_key",
        "ssh_row_key",
        "webhook_search_key",
        "license_index_key",
        "left_association_key",
        "record_partition_key",
    ] {
        for content in [
            format!("{label}={hex}"),
            format!(r#"{{"{label}":"{hex}","neighbor":"{id}"}}"#),
        ] {
            assert!(check(&content).is_err(), "{content}");
            assert!(!mask_secrets(&content).contains(hex.as_str()), "{content}");
        }
    }
}

#[test]
fn accepted_false_positive_workspace_artifact_path_behind_attributive_trigger() {
    // "secret gate false positive repro: <path>" carries a trigger word
    // in clause range ahead of a value delimiter. The clause walk no
    // longer caps content words after a delimiter (any cap re-admits the
    // chained-qualifier labeled-value bypass), so this meta-prose shape
    // blocks. Accepted false positive — same attributive-trigger class
    // as the auth-setup docs path; documented in docs/api/secret_gate.md.
    let content = "writing up the secret gate false positive repro: \
             .workspace/20260101/fix-secret-gate-trigger-false-positive/MEASUREMENT_REPORT.md";
    assert!(
        check(content).is_err(),
        "accepted-FP contract changed: attributive trigger before a \
             delimited path no longer blocks — update the docs if deliberate"
    );
}

#[test]
fn accepted_false_positive_archive_doc_path_behind_attributive_trigger() {
    // "secret scanner archive notes: <path>" — attributive trigger two
    // qualifiers ahead of the delimiter. Same accepted-FP class as
    // above; the walk cannot tell an attributive trigger from a label
    // head without reopening the chained-qualifier bypass.
    let content = "secret scanner archive notes: docs/_archive/ADR051-TenantEncryption-v2Notes.md";
    assert!(
        check(content).is_err(),
        "accepted-FP contract changed: attributive trigger before a \
             delimited path no longer blocks — update the docs if deliberate"
    );
}

#[test]
fn allows_absolute_path_near_standalone_auth() {
    let content = "the auth scanner flagged this file: /home/user/projects/workspace/SessionNotes20260107/AuthGateFollowup2.md";
    assert!(
        check(content).is_ok(),
        "absolute path in technical prose must pass; got {:?}",
        scan(content)
    );
}

#[test]
fn blocks_assignment_shaped_credential_disguised_as_path_near_api_key() {
    // Adversarial negative: a credential-shaped value glued via '='
    // directly to a trigger word must not be exempted just because it is
    // path-shaped (separator-delimited, word-shaped runs) and looks
    // superficially like the technical paths above. The compound label
    // `api_key` makes this a credential value, so it must block.
    let content = "api_key=/home/user/workspaces/2026/topic-name-example/SECRET_VALUE_HERE.md";
    assert!(
        check(content).is_err(),
        "assignment-shaped credential disguised as a path must still be \
             blocked: {content:?}, got {:?}",
        scan(content)
    );
}

#[test]
fn blocks_separator_split_secret_access_key_compound() {
    // Adversarial negative: a separator-split bypass shape must still be
    // blocked. The `secret` and `key` entries both match because underscore
    // is a boundary for bare `TRIGGER_WORDS`. This asserts the end-to-end
    // outcome.
    let content = "secret_access_key abcdefghij/klmnopqrst/uvwxyzabcd/efghijk.md";
    assert!(
        check(content).is_err(),
        "secret_access_key bypass shape must still be blocked: {content:?}, \
             got {:?}",
        scan(content)
    );
}

// ── Underscore is a BOUNDARY for bare TRIGGER_WORDS, not a continuation ─
// Opposite of `has_standalone_token`'s rule for `token`: treating underscore as a
// boundary here is what keeps `SECRET_KEY=`/`auth_token=`/`signing_key=` detected
// (`contains_word`'s `underscore_is_word_char` parameter controls this per caller).

#[test]
fn blocks_secret_key_assignment_when_underscore_bounds_trigger() {
    // `SECRET_KEY=<value>` must block via the plain-substring `secret`
    // trigger even though `secret` is followed by `_` rather than a
    // non-word-char boundary.
    let content = "SECRET_KEY=dGhpc2lzYXNlY3JldGtleXZhbHVlMTIzNDU2Nzg5MA=="; // gitleaks:allow
    assert!(
        check(content).is_err(),
        "SECRET_KEY=<value> (Django/Flask-style config) must still be \
             blocked: {content:?}, got {:?}",
        scan(content)
    );
}

#[test]
fn blocks_auth_token_assignment_when_underscore_bounds_trigger() {
    let opaque = "Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvMabcdef"; // gitleaks:allow
    let content = format!("auth_token={opaque}");
    assert!(
        check(&content).is_err(),
        "auth_token=<value> must still be blocked: {content:?}, got {:?}",
        scan(&content)
    );
}

#[test]
fn blocks_underscore_joined_session_secret_and_signing_key_compounds() {
    let opaque = "Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvMabcdef"; // gitleaks:allow
    let cases = [
        format!("session_secret_{opaque}"),
        format!("signing_key={opaque}"),
    ];
    for content in &cases {
        assert!(
            check(content).is_err(),
            "underscore-joined credential compound must still be blocked: \
                 {content:?}, got {:?}",
            scan(content)
        );
    }
}

#[test]
fn allows_letter_joined_trigger_substrings_in_benign_prose() {
    // The letter-joined substring-collision exemption must survive the
    // underscore-as-boundary change, since that change only affects the
    // underscore character, not letter-joined words.
    let cases = [
        "authorized_write_requires_dominance was converted from axiom to theorem, id 550e8400-e29b-41d4-a716-446655440000",
        "authentication flow diagram lives at 550e8400-e29b-41d4-a716-446655440000",
        "the turkey and monkey story references id 550e8400-e29b-41d4-a716-446655440000",
        "keyword research doc: 550e8400-e29b-41d4-a716-446655440000",
    ];
    for content in cases {
        assert!(
            check(content).is_ok(),
            "letter-joined substring collision must stay exempt: \
                 {content:?}, got {:?}",
            scan(content)
        );
    }
}

#[test]
fn block_message_carries_actionable_guidance() {
    let fake = "AKIAFAKEKEY1234567890";
    let m = scan(fake).unwrap();
    let rendered = m.to_string();
    assert!(
        rendered.contains("real credential"),
        "block message must carry actionable guidance: {rendered}"
    );
}

#[test]
fn block_message_shape_guidance_names_effective_boundary() {
    let opaque = "Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvMabcdef"; // gitleaks:allow
    let content = format!("auth header {opaque}");
    let m = scan(&content).unwrap();
    let rendered = m.to_string();
    assert!(
        rendered.contains("sentence") || rendered.contains("paragraph"),
        "shape-based detector guidance must name an effective context boundary: {rendered}"
    );
}
