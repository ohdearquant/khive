#[test]
fn raw_control_char_in_bareword_position_rejected() {
    // The literal-control-char exception only applies inside a
    // double-quoted string value (`parse_value` only calls
    // `normalize_quoted_string` when the trimmed slice starts with
    // `"`). A raw NUL embedded in an unquoted (bareword) numeric value must
    // still fail to parse as JSON, exactly as before this PR.
    let src = format!("gtd.assign(weight=1{}0)", '\u{0}');
    let err = parse_request(&src).unwrap_err();
    assert!(
        matches!(err, DslError::InvalidValue { .. }),
        "expected InvalidValue for a raw control byte in bareword position, got {err:?}"
    );
}

#[test]
fn other_raw_control_chars_in_quoted_string_still_rejected() {
    // Only `\n`, `\r`, and `\t` are exempted (ADR-016). Every other raw
    // U+0000-U+001F byte inside a quoted value must remain a parse error,
    // matching pre-PR behavior — NUL, form feed, and ESC here.
    for raw in ['\u{0}', '\u{c}', '\u{1b}'] {
        let src = format!("gtd.assign(title=\"a{raw}b\")");
        let err = parse_request(&src).unwrap_err();
        assert!(
            matches!(err, DslError::InvalidValue { .. }),
            "expected InvalidValue for raw control byte {:#04x}, got {err:?}",
            raw as u32
        );
    }
}

#[test]
fn control_char_error_teaches_escape_syntax_without_transport_guidance() {
    // #491: the bare serde message ("control character ... found while
    // parsing a string") teaches nothing. The wrapped error must name the
    // JSON escape grammar while remaining transport-neutral; wire encoding
    // guidance belongs to the transport boundary, not this parser.
    let src = format!("gtd.assign(title=\"a{}b\")", '\u{c}');
    let err = parse_request(&src).unwrap_err();
    let msg = err.to_string();
    assert!(
        matches!(err, DslError::InvalidValue { .. }),
        "expected InvalidValue, got {err:?}"
    );
    assert!(
        msg.contains(r#"\n"#) && msg.contains(r#"\t"#) && msg.contains(r#"\""#),
        "error should name the JSON escape grammar, got: {msg}"
    );
    assert!(
        !msg.contains("MCP") && !msg.to_lowercase().contains("transport"),
        "generic parser error should stay transport-neutral, got: {msg}"
    );
}

#[test]
fn control_char_error_reports_value_relative_byte_index() {
    // #491: the diagnostic byte index is relative to the value, not the raw
    // quoted span; it must not count the opening quote. A control byte that is
    // the first byte of the value is reported as "byte 0 of the value", not
    // "byte 1".
    let src = format!("gtd.assign(title=\"{}b\")", '\u{c}');
    let err = parse_request(&src).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("byte 0 of the value"),
        "byte index must be value-relative (exclude the opening quote), got: {msg}"
    );
}

#[test]
fn control_char_in_object_key_rejected_with_plain_serde_message() {
    // #491: quoted object KEYS decode through a separate, strict path from
    // quoted values (`parse_object_arg_body` vs `parse_value`) — the
    // value-path escape-grammar teaching diagnostic is scoped to values only
    // (ADR-016, PR #957). A raw control byte in a key still fails to parse,
    // but with the plain serde message rather than the value-path
    // enrichment.
    let src = format!("gtd.assign(properties={{\"a{}b\":\"x\"}})", '\u{c}');
    let err = parse_request(&src).unwrap_err();
    let msg = err.to_string();
    assert!(
        matches!(err, DslError::InvalidValue { .. }),
        "expected InvalidValue, got {err:?}"
    );
    assert!(
        !msg.contains("DSL string escapes follow JSON"),
        "key-path error should not carry the value-path escape teaching, got: {msg}"
    );
}

#[test]
fn raw_control_byte_in_object_key_rejected() {
    // #491: the literal-newline/CR/tab carve-out (ADR-016, PR #957) is scoped
    // to quoted argument VALUES only. A quoted object KEY containing one of
    // these bytes raw must still be rejected — only the escaped form (`\n`,
    // `\r`, `\t`) is legal inside a key.
    for raw in ['\n', '\r', '\t'] {
        let src = format!("gtd.assign(properties={{\"a{raw}b\":\"x\"}})");
        let err = parse_request(&src).unwrap_err();
        assert!(
            matches!(err, DslError::InvalidValue { .. }),
            "expected InvalidValue for raw control byte {:#04x} in an object key, got {err:?}",
            raw as u32
        );
    }
}
