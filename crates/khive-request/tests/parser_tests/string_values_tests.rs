#[test]
fn string_with_comma_and_paren_inside() {
    let v = ops(r#"gtd.assign(title="hello, world (now)")"#);
    assert_eq!(val(&v[0].args["title"]), &json!("hello, world (now)"));
}

#[test]
fn string_with_escaped_quote() {
    let v = ops(r#"gtd.assign(title="he said \"hi\"")"#);
    assert_eq!(val(&v[0].args["title"]), &json!("he said \"hi\""));
}

#[test]
fn multiline_string_value_round_trips() {
    // A literal raw newline (0x0A) inside a double-quoted value, as an agent
    // client would send it directly rather than escaping it to `\n` text.
    let src = "gtd.assign(title=\"line one\nline two\")";
    let v = ops(src);
    assert_eq!(val(&v[0].args["title"]), &json!("line one\nline two"));
}

#[test]
fn tab_and_carriage_return_literal_in_string() {
    let src = "gtd.assign(title=\"a\tb\rc\")";
    let v = ops(src);
    assert_eq!(val(&v[0].args["title"]), &json!("a\tb\rc"));
}

#[test]
fn mixed_literal_newline_and_escaped_sequences() {
    // A literal raw newline sits next to escaped-quote and escaped-backslash
    // pairs in the same value, proving the control-char rewrite does not
    // disturb adjacent backslash-escape pairs.
    let src = "comm.reply(content=\"line one \\\"quoted\\\"\nline two a\\\\b\")";
    let v = ops(src);
    assert_eq!(
        val(&v[0].args["content"]),
        &json!("line one \"quoted\"\nline two a\\b")
    );
}

#[test]
fn unterminated_multiline_string_rejected() {
    // A raw newline mid-string does not make an unclosed string legal; the
    // closing quote is still required somewhere in the input.
    let src = "gtd.assign(title=\"line one\nline two)";
    let err = parse_request(src).unwrap_err();
    assert!(matches!(err, DslError::UnclosedString { .. }));
}

#[test]
fn json_form_rejects_literal_newline_in_string() {
    // JSON form stays strict JSON: a raw control character inside a quoted
    // string is a parse error there, unlike the function-call form above.
    let src = "[{\"tool\":\"gtd.next\",\"args\":{\"note\":\"line one\nline two\"}}]";
    let err = parse_request(src).unwrap_err();
    assert!(matches!(err, DslError::InvalidJson { .. }));
}

#[test]
fn json_form_rejects_unknown_entry_keys_before_defaulting_args() {
    for src in [
        r#"{"tool":"memory.prune","arg":{"dry_run":true}}"#,
        r#"[{"tool":"get","args":{"id":"known"}},{"tool":"memory.prune","arg":{}}]"#,
    ] {
        let error = parse_request(src).unwrap_err();
        assert!(
            matches!(error, DslError::InvalidJson { ref error } if error.contains("arg")),
            "misspelled operation key must be refused: {error:?}"
        );
    }
}

// ── ADR-016 grammar-boundary matrix (round-1 review, PR #957) ────────────────

#[test]
fn crlf_preserved_exact_in_quoted_value() {
    // A literal CRLF pair (not just LF or CR alone) inside a quoted value
    // round-trips byte-for-byte, proving both bytes are rewritten
    // independently rather than the pair being collapsed or reordered.
    let src = "gtd.assign(title=\"line one\r\nline two\")";
    let v = ops(src);
    assert_eq!(val(&v[0].args["title"]), &json!("line one\r\nline two"));
}

#[test]
fn literal_newline_immediately_before_closing_quote() {
    // The raw newline is the very last byte before the closing `"`, with no
    // trailing content — the scanner must not mistake it for trailing
    // whitespace to trim, nor the rewrite skip the final byte.
    let src = "gtd.assign(title=\"abc\n\")";
    let v = ops(src);
    assert_eq!(val(&v[0].args["title"]), &json!("abc\n"));
}

#[test]
fn raw_newline_immediately_after_escaped_quote_boundary() {
    // A `\"` escape pair is immediately followed by a raw literal newline.
    // `scan_string_end` must consume the escape pair as a unit (not treat
    // its second byte as the closing quote), and the rewrite must copy the
    // escape pair through untouched before rewriting the adjacent raw LF.
    let src = "gtd.assign(title=\"a\\\"\nb\")";
    let v = ops(src);
    assert_eq!(val(&v[0].args["title"]), &json!("a\"\nb"));
}
