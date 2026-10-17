#[test]
fn invalid_backslash_escape_in_quoted_string_rejected() {
    // A negative invalid-escape case: `\q` is not a JSON escape sequence.
    // This must fail regardless of the literal-newline carve-out.
    let src = r#"gtd.assign(title="bad \q escape")"#;
    let err = parse_request(src).unwrap_err();
    assert!(matches!(err, DslError::InvalidValue { .. }));
}

#[test]
fn invalid_escape_with_unrelated_control_byte_not_misattributed() {
    // #491 round-2 blocking fix 1(a): a `\q` invalid escape occurs before an
    // unrelated form-feed later in the same span. The failure is the `\q`,
    // not the control byte, so the control-char teaching diagnostic must NOT
    // fire — misattributing the error to the wrong byte would send a caller
    // chasing the wrong fix.
    let src = format!("gtd.assign(title=\"bad \\q escape{}tail\")", '\u{c}');
    let err = parse_request(&src).unwrap_err();
    let msg = err.to_string();
    assert!(matches!(err, DslError::InvalidValue { .. }));
    assert!(
        !msg.contains("DSL string escapes follow JSON"),
        "an invalid \\q escape must not be enriched with the control-char diagnostic, got: {msg}"
    );
}

#[test]
fn malformed_unicode_escape_adjacent_to_control_byte_not_misattributed() {
    // A malformed `\u` escape (only 3 hex digits) followed immediately by a
    // raw control byte fails with serde's `InvalidEscape`, not a
    // control-character error — but the failure's byte offset can coincide
    // with the recorded control-byte hit (the 4th hex-digit slot serde reads
    // is the control byte itself). Offset alone is not enough to gate the
    // enrichment; the error's kind must also be checked, or this lands the
    // control-character escape guidance on an invalid-escape failure.
    let src = format!("gtd.assign(title=\"bad \\u123{}tail\")", '\u{c}');
    let err = parse_request(&src).unwrap_err();
    let msg = err.to_string();
    assert!(matches!(err, DslError::InvalidValue { .. }));
    assert!(
        msg.contains("invalid escape"),
        "expected the plain serde invalid-escape message, got: {msg}"
    );
    assert!(
        !msg.contains("DSL string escapes follow JSON"),
        "a malformed \\u escape must not be enriched with the control-char diagnostic, got: {msg}"
    );
}

#[test]
fn short_unicode_escape_backslash_adjacent_control_byte_not_misattributed() {
    // A `\u` escape short by exactly 2 hex digits has its 3rd slot land on a
    // raw backslash and its 4th slot land on a raw control byte — so the
    // control byte IS physically adjacent to a backslash, unlike the
    // 3-hex-digit case above. That adjacency is spurious: both bytes are
    // consumed as `\u`'s own hex-digit slots, never reinterpreted as a fresh
    // `\<ctrl>` escape pair. The failure must stay the plain malformed
    // unicode-escape message with no control-character escape guidance.
    let src = format!("gtd.assign(title=\"bad \\u12\\{}tail\")", '\u{c}');
    let err = parse_request(&src).unwrap_err();
    let msg = err.to_string();
    assert!(matches!(err, DslError::InvalidValue { .. }));
    assert!(
        msg.contains("invalid escape"),
        "expected the plain serde invalid-escape message, got: {msg}"
    );
    assert!(
        !msg.contains("DSL string escapes follow JSON"),
        "a short \\u escape landing on a backslash+control-byte pair must not be \
         enriched with the control-char diagnostic, got: {msg}"
    );

    // A genuine broken `\<ctrl>` pair with no preceding `\u` run still gets
    // the teaching diagnostic — the fix must not blind the guard to the
    // legitimate case.
    let genuine_src = format!("gtd.assign(title=\"before\\{}after\")", '\u{c}');
    let genuine_err = parse_request(&genuine_src).unwrap_err();
    let genuine_msg = genuine_err.to_string();
    assert!(matches!(genuine_err, DslError::InvalidValue { .. }));
    assert!(
        genuine_msg.contains("DSL string escapes follow JSON"),
        "a genuine broken \\<ctrl> pair must still get the escape guidance, got: {genuine_msg}"
    );
}

#[test]
fn leading_surrogate_escape_followed_by_control_byte_not_misattributed() {
    // #491 round-5: a well-formed high-surrogate `\u` escape (U+D800-U+DBFF)
    // must be followed by a `\u` low-surrogate escape. When the byte after the
    // continuation backslash is a raw control byte instead, `serde_json` fails
    // on the malformed surrogate continuation, not on a broken `\<ctrl>`
    // escape. Scan's general backslash-pair walk resets after the high
    // surrogate's 4 hex slots, so without the surrogate-continuation guard it
    // would record that `\<ctrl>` as a genuine broken pair
    // (`preceded_by_backslash: true`) and land the escape teaching on a
    // failure it does not explain. The continuation control byte must not be
    // recorded, so the error stays serde's own surrogate message.
    let src = format!("gtd.assign(title=\"bad \\uD800\\{}tail\")", '\u{c}');
    let err = parse_request(&src).unwrap_err();
    let msg = err.to_string();
    assert!(matches!(err, DslError::InvalidValue { .. }));
    assert!(
        !msg.contains("DSL string escapes follow JSON"),
        "a control byte on a surrogate-continuation path must not be enriched \
         with the control-char escape diagnostic, got: {msg}"
    );

    // A broken `\<ctrl>` pair AFTER a complete surrogate pair (high + low) is a
    // real broken escape and still gets the teaching; the suppression is scoped
    // to the immediate continuation, not to everything after any `\u`.
    let after_pair = format!("gtd.assign(title=\"ok \\uD800\\uDC00\\{}tail\")", '\u{c}');
    let after_pair_err = parse_request(&after_pair).unwrap_err();
    let after_pair_msg = after_pair_err.to_string();
    assert!(matches!(after_pair_err, DslError::InvalidValue { .. }));
    assert!(
        after_pair_msg.contains("DSL string escapes follow JSON"),
        "a broken \\<ctrl> pair after a complete surrogate pair must still get \
         the escape guidance, got: {after_pair_msg}"
    );
}

#[test]
fn raw_newline_immediately_after_backslash_caught_as_control_char_cause() {
    // #491 round-2 blocking fix 1(b): a raw LF directly following a
    // backslash is not a valid two-byte JSON escape (valid escapes are
    // backslash + an ASCII letter, never backslash + a literal control
    // byte). This must be caught as a control-char cause and receive the
    // teaching diagnostic, not silently fall through to the bare serde
    // message.
    let src = "gtd.assign(title=\"before\\\nafter\")";
    let err = parse_request(src).unwrap_err();
    let msg = err.to_string();
    assert!(matches!(err, DslError::InvalidValue { .. }));
    assert!(
        msg.contains(r#"\n"#) && msg.contains(r#"\t"#) && msg.contains(r#"\""#),
        "error should name the JSON escape grammar, got: {msg}"
    );
    assert!(msg.contains("DSL string escapes follow JSON"));
}

#[test]
fn raw_carriage_return_and_tab_after_backslash_also_caught() {
    // Same defect as the LF case above, for CR and TAB.
    for raw in ['\r', '\t'] {
        let src = format!("gtd.assign(title=\"before\\{raw}after\")");
        let err = parse_request(&src).unwrap_err();
        assert!(
            matches!(err, DslError::InvalidValue { .. }),
            "expected InvalidValue for backslash + raw {:#04x}, got {err:?}",
            raw as u32
        );
        let msg = err.to_string();
        assert!(
            msg.contains("DSL string escapes follow JSON"),
            "error should explain DSL escape syntax for {:#04x}, got: {msg}",
            raw as u32
        );
    }
}

#[test]
fn backslash_followed_by_other_raw_control_bytes_also_caught() {
    // #491 round-2 blocking fix 2: the post-backslash branch previously
    // recorded a hit only for `\n`/`\r`/`\t`. A backslash followed by any
    // OTHER raw U+0000-U+001F byte — NUL, form feed, and ESC here — is
    // equally not a valid JSON escape and must reach the same teaching
    // diagnostic instead of the bare serde message.
    for raw in ['\u{0}', '\u{c}', '\u{1b}'] {
        let src = format!("gtd.assign(title=\"before\\{raw}after\")");
        let err = parse_request(&src).unwrap_err();
        assert!(
            matches!(err, DslError::InvalidValue { .. }),
            "expected InvalidValue for backslash + raw {:#04x}, got {err:?}",
            raw as u32
        );
        let msg = err.to_string();
        assert!(
            msg.contains("DSL string escapes follow JSON"),
            "error should explain DSL escape syntax for {:#04x}, got: {msg}",
            raw as u32
        );
    }
}
