// ── JSON-form recursive $prev detection ──────────────────────────────────────

#[test]
fn json_form_nested_array_with_prev_ref_is_rejected() {
    // `$prev` inside an array arg must be detected.
    let err = parse_request(
        r#"[{"tool":"create","args":{"kind":"concept","name":"A"}},{"tool":"search","args":{"ids":["$prev.id"]}}]"#,
    )
    .unwrap_err();
    assert!(
        matches!(err, DslError::PrevRefInJsonForm { ref arg_name } if arg_name == "ids"),
        "expected PrevRefInJsonForm for nested array, got {err:?}"
    );
}

#[test]
fn json_form_nested_object_with_prev_ref_is_rejected() {
    // `$prev` inside an object arg must be detected.
    let err = parse_request(
        r#"[{"tool":"create","args":{"kind":"concept","name":"A"}},{"tool":"search","args":{"filter":{"id":"$prev.id"}}}]"#,
    )
    .unwrap_err();
    assert!(
        matches!(err, DslError::PrevRefInJsonForm { ref arg_name } if arg_name == "filter"),
        "expected PrevRefInJsonForm for nested object, got {err:?}"
    );
}

#[test]
fn json_form_bracket_prev_ref_is_rejected() {
    // Bracket-index form must also be detected.
    let err = parse_request(
        r#"[{"tool":"create","args":{"kind":"concept","name":"A"}},{"tool":"get","args":{"id":"$prev[0].id"}}]"#,
    )
    .unwrap_err();
    assert!(
        matches!(err, DslError::PrevRefInJsonForm { ref arg_name } if arg_name == "id"),
        "expected PrevRefInJsonForm for $prev[0].id, got {err:?}"
    );
}

#[test]
fn json_form_prevish_id_stays_literal() {
    // Boundary: `$prevish.id` is NOT a $prev ref and must pass through.
    let r = req(r#"[{"tool":"get","args":{"id":"$prevish.id"}}]"#);
    assert_eq!(r.mode, ExecutionMode::Parallel);
    assert_eq!(r.ops[0].args["id"], ArgValue::Value(json!("$prevish.id")));
}

// ── Escape semantics produce clean literal ───────────────────────────────────

#[test]
fn escaped_dollar_prev_without_path_stays_literal() {
    // `"\\$prev"` produces literal `$prev` (no path), not a PrevRef.
    let r = req(r#"create(kind="concept", name="A") | get(id="\\$prev")"#);
    assert_eq!(r.mode, ExecutionMode::Chain);
    assert_eq!(r.ops[1].args["id"], ArgValue::Value(json!("$prev")));
}

#[test]
fn escaped_dollar_prev_bracket_stays_literal() {
    // `"\\$prev[0].id"` produces literal `$prev[0].id`, not a PrevRef.
    let r = req(r#"create(kind="concept", name="A") | get(id="\\$prev[0].id")"#);
    assert_eq!(r.mode, ExecutionMode::Chain);
    assert_eq!(r.ops[1].args["id"], ArgValue::Value(json!("$prev[0].id")));
}

// ── Quoted bracket index validation ──────────────────────────────────────────

#[test]
fn quoted_prev_ref_negative_index_treated_as_literal() {
    // `"$prev[-1].id"`: negative index is invalid in bracket grammar.
    // string_as_prev_ref returns None → stored as literal Value, not PrevRef.
    // In a chain, the value is a concrete string (no $prev substitution needed).
    let r = req(r#"list(kind="concept") | get(id="$prev[-1].id")"#);
    assert_eq!(r.mode, ExecutionMode::Chain);
    // Should be a Value (literal), NOT a PrevRef.
    assert!(
        matches!(r.ops[1].args["id"], ArgValue::Value(_)),
        "negative index quoted ref must be literal Value, not PrevRef; got {:?}",
        r.ops[1].args["id"]
    );
}

#[test]
fn quoted_prev_ref_missing_close_bracket_treated_as_literal() {
    // `"$prev[0.id"`: missing ']' is a malformed bracket.
    let r = req(r#"list(kind="concept") | get(id="$prev[0.id")"#);
    assert_eq!(r.mode, ExecutionMode::Chain);
    assert!(
        matches!(r.ops[1].args["id"], ArgValue::Value(_)),
        "unclosed bracket quoted ref must be literal Value, not PrevRef; got {:?}",
        r.ops[1].args["id"]
    );
}

#[test]
fn quoted_prev_ref_non_numeric_index_treated_as_literal() {
    // `"$prev[abc].id"`: a non-numeric index is invalid.
    let r = req(r#"list(kind="concept") | get(id="$prev[abc].id")"#);
    assert_eq!(r.mode, ExecutionMode::Chain);
    assert!(
        matches!(r.ops[1].args["id"], ArgValue::Value(_)),
        "non-numeric bracket index quoted ref must be literal Value; got {:?}",
        r.ops[1].args["id"]
    );
}

// ── #484 regression: nested malformed bracket paths in a quoted dotted ref ────

#[test]
fn quoted_prev_ref_nested_negative_index_treated_as_literal() {
    // #484: `"$prev.items[-1].id"` — malformed bracket after a field segment
    // must stay a literal Value, not be promoted to PrevRef.
    let r = req(r#"list(kind="concept") | get(id="$prev.items[-1].id")"#);
    assert_eq!(r.mode, ExecutionMode::Chain);
    assert_eq!(
        r.ops[1].args["id"],
        ArgValue::Value(json!("$prev.items[-1].id"))
    );
}

#[test]
fn quoted_prev_ref_nested_non_numeric_index_treated_as_literal() {
    // #484: `"$prev.items[abc].id"` — non-numeric nested index is malformed.
    let r = req(r#"list(kind="concept") | get(id="$prev.items[abc].id")"#);
    assert_eq!(r.mode, ExecutionMode::Chain);
    assert_eq!(
        r.ops[1].args["id"],
        ArgValue::Value(json!("$prev.items[abc].id"))
    );
}

#[test]
fn quoted_prev_ref_nested_missing_close_bracket_treated_as_literal() {
    // #484: `"$prev.items[0.id"` — unclosed nested bracket is malformed.
    let r = req(r#"list(kind="concept") | get(id="$prev.items[0.id")"#);
    assert_eq!(r.mode, ExecutionMode::Chain);
    assert_eq!(
        r.ops[1].args["id"],
        ArgValue::Value(json!("$prev.items[0.id"))
    );
}

#[test]
fn quoted_prev_ref_nested_valid_index_still_promotes_and_resolves() {
    // #484 non-regression: a well-formed nested bracket path must still
    // promote to PrevRef and resolve correctly.
    let r = req(r#"list(kind="concept") | get(id="$prev.items[0].id")"#);
    assert_eq!(r.mode, ExecutionMode::Chain);
    assert_eq!(
        r.ops[1].args["id"],
        ArgValue::PrevRef {
            path: "items[0].id".into()
        }
    );
    let prev = json!({"items": [{"id": "ok"}]});
    assert_eq!(r.ops[1].args["id"].resolve_prev(&prev), Some(&json!("ok")));
}

#[test]
fn quoted_prev_ref_root_bracket_with_malformed_nested_tail_treated_as_literal() {
    // #484 risk flag: the root `$prev[N]...` branch must also validate
    // malformed brackets in its tail, e.g. `"$prev[0].items[-1].id"`.
    let r = req(r#"list(kind="concept") | get(id="$prev[0].items[-1].id")"#);
    assert_eq!(r.mode, ExecutionMode::Chain);
    assert_eq!(
        r.ops[1].args["id"],
        ArgValue::Value(json!("$prev[0].items[-1].id"))
    );
}

#[test]
fn unquoted_negative_index_rejected_at_parse_time() {
    // Regression: unquoted `$prev[-1].id` — the `-` is not a digit, so the
    // digit-reader finds an empty index string and returns InvalidValue.
    let err = parse_request(r#"list(kind="concept") | get(id=$prev[-1].id)"#).unwrap_err();
    assert!(
        matches!(err, DslError::InvalidValue { .. }),
        "expected InvalidValue for negative index in unquoted ref, got {err:?}"
    );
}
