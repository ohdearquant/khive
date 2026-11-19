// ── CC-3: Quoted "$prev.id" substitutes the same as unquoted $prev.id ─────────

#[test]
fn quoted_prev_ref_chain_parses_as_prev_ref() {
    // CC-3: `get(id="$prev.id")` must produce PrevRef, not Value("$prev.id").
    let r = req(r#"create(kind="concept", name="A") | get(id="$prev.id")"#);
    assert_eq!(r.mode, ExecutionMode::Chain);
    assert_eq!(r.ops[1].args["id"], ArgValue::PrevRef { path: "id".into() });
}

#[test]
fn quoted_bare_prev_ref_parses_as_prev_ref() {
    // CC-3: `"$prev"` (no path) must also promote.
    let r = req(r#"next() | update(id="$prev")"#);
    assert_eq!(r.mode, ExecutionMode::Chain);
    assert_eq!(r.ops[1].args["id"], ArgValue::PrevRef { path: "".into() });
}

#[test]
fn quoted_prev_ref_deep_path_parses_as_prev_ref() {
    // CC-3: `"$prev.result.id"` — multi-segment dotted quoted ref.
    let r = req(r#"create(kind="concept", name="A") | get(id="$prev.result.id")"#);
    assert_eq!(r.mode, ExecutionMode::Chain);
    assert_eq!(
        r.ops[1].args["id"],
        ArgValue::PrevRef {
            path: "result.id".into()
        }
    );
}

#[test]
fn escaped_dollar_prev_stays_literal() {
    // CC-3 escape: `"\\$prev.id"` produces the literal string `$prev.id`.
    // The DSL source `"\\$prev.id"` deserializes to `\$prev.id` (one backslash).
    // string_as_prev_ref strips the leading `\` and returns Value("$prev.id").
    let r = req(r#"create(kind="concept", name="A") | get(id="\\$prev.id")"#);
    assert_eq!(r.mode, ExecutionMode::Chain);
    // After stripping the escape marker the handler sees the clean literal.
    assert_eq!(r.ops[1].args["id"], ArgValue::Value(json!("$prev.id")));
}

// ── ue-dsl-chain C1: JSON-form with $prev string is rejected clearly ───────────

#[test]
fn json_form_with_prev_ref_string_is_rejected() {
    // ue-dsl-chain C1: JSON form `[{...}, {"args":{"id":"$prev.id"}}]` must
    // be rejected with PrevRefInJsonForm, not silently passed through.
    let err = parse_request(
        r#"[{"tool":"create","args":{"kind":"concept","name":"A"}},{"tool":"get","args":{"id":"$prev.id"}}]"#,
    )
    .unwrap_err();
    assert!(
        matches!(err, DslError::PrevRefInJsonForm { ref arg_name } if arg_name == "id"),
        "expected PrevRefInJsonForm, got {err:?}"
    );
}

#[test]
fn json_form_with_bare_prev_string_is_rejected() {
    // ue-dsl-chain C1: bare `"$prev"` in JSON form is also rejected.
    let err = parse_request(r#"[{"tool":"get","args":{"id":"$prev"}}]"#).unwrap_err();
    assert!(
        matches!(err, DslError::PrevRefInJsonForm { ref arg_name } if arg_name == "id"),
        "expected PrevRefInJsonForm, got {err:?}"
    );
}

#[test]
fn json_form_without_prev_ref_still_works() {
    // ue-dsl-chain C1 guard: make sure normal JSON form is not broken.
    let r = req(r#"[{"tool":"next","args":{}}, {"tool":"complete","args":{"id":"abc"}}]"#);
    assert_eq!(r.mode, ExecutionMode::Parallel);
    assert_eq!(r.ops.len(), 2);
}

// ── PrevRefOutsideChain emitted at parse time ─────────────────────────────────

#[test]
fn prev_ref_in_single_op_is_rejected() {
    // `get(id=$prev.id)` without chain must be rejected.
    let err = parse_request(r#"get(id=$prev.id)"#).unwrap_err();
    assert!(
        matches!(err, DslError::PrevRefOutsideChain { .. }),
        "expected PrevRefOutsideChain, got {err:?}"
    );
}

#[test]
fn prev_ref_in_fn_batch_is_rejected() {
    // PrevRef inside `[create(...), get(id=$prev.id)]` is
    // parallel (no `|`) — must be rejected at parse time.
    let err = parse_request(r#"[create(kind="concept", name="A"), get(id=$prev.id)]"#).unwrap_err();
    assert!(
        matches!(err, DslError::PrevRefOutsideChain { .. }),
        "expected PrevRefOutsideChain, got {err:?}"
    );
}
