// --- #771: bounded input length + bounded container-nesting depth ---
//
// Regression coverage for CWE-674 (unbounded recursive descent). Three
// independent recursion sites are guarded: the function-call array/object
// parser (`parse_array_arg`/`parse_object_arg`), the JSON-form pre-pass scan
// ahead of `serde_json::from_str`, and `$prev` nesting inside chain mode
// (covered for free since `ArgValue::Array`/`Object` are only ever
// constructed inside the same depth-guarded parser functions).

fn nested_array_arg(depth: usize) -> String {
    format!("verb(x={}{})", "[".repeat(depth), "]".repeat(depth))
}

fn nested_object_arg(depth: usize) -> String {
    format!("verb(x={}1{})", "{\"a\":".repeat(depth), "}".repeat(depth))
}

/// JSON-form envelope overhead before the user's own nesting begins: the
/// batch array `[`, the op object `{`, and the `args` object `{`, 3 levels total.
const JSON_FORM_ENVELOPE_DEPTH: usize = 3;

fn nested_json_form(inner_depth: usize) -> String {
    format!(
        "[{{\"tool\":\"x\",\"args\":{{\"a\":{}{}}}}}]",
        "[".repeat(inner_depth),
        "]".repeat(inner_depth)
    )
}

fn nested_prev_ref_in_chain(depth: usize) -> String {
    format!(
        "op1() | op2(x={}$prev{})",
        "[".repeat(depth),
        "]".repeat(depth)
    )
}

#[test]
fn array_nesting_at_limit_succeeds() {
    let r = parse_request(&nested_array_arg(NESTING_DEPTH_LIMIT));
    assert!(
        r.is_ok(),
        "depth {NESTING_DEPTH_LIMIT} (at limit) must parse: {r:?}"
    );
}

#[test]
fn array_nesting_over_limit_rejects_cleanly() {
    let err = parse_request(&nested_array_arg(NESTING_DEPTH_LIMIT + 1)).unwrap_err();
    assert!(
        matches!(
            &err,
            DslError::NestingTooDeep { depth, max, .. }
            if *depth == NESTING_DEPTH_LIMIT + 1 && *max == NESTING_DEPTH_LIMIT
        ),
        "expected NestingTooDeep, got {err:?}"
    );
}

#[test]
fn object_nesting_at_limit_succeeds() {
    let r = parse_request(&nested_object_arg(NESTING_DEPTH_LIMIT));
    assert!(
        r.is_ok(),
        "depth {NESTING_DEPTH_LIMIT} (at limit) must parse: {r:?}"
    );
}

#[test]
fn object_nesting_over_limit_rejects_cleanly() {
    let err = parse_request(&nested_object_arg(NESTING_DEPTH_LIMIT + 1)).unwrap_err();
    assert!(
        matches!(
            &err,
            DslError::NestingTooDeep { depth, max, .. }
            if *depth == NESTING_DEPTH_LIMIT + 1 && *max == NESTING_DEPTH_LIMIT
        ),
        "expected NestingTooDeep, got {err:?}"
    );
}

#[test]
fn json_form_nesting_at_limit_succeeds() {
    let inner = NESTING_DEPTH_LIMIT - JSON_FORM_ENVELOPE_DEPTH;
    let r = parse_request(&nested_json_form(inner));
    assert!(r.is_ok(), "at-limit JSON-form nesting must parse: {r:?}");
}

#[test]
fn json_form_nesting_over_limit_rejects_cleanly() {
    let inner = NESTING_DEPTH_LIMIT - JSON_FORM_ENVELOPE_DEPTH + 1;
    let err = parse_request(&nested_json_form(inner)).unwrap_err();
    assert!(
        matches!(
            &err,
            DslError::NestingTooDeep { depth, max, .. }
            if *depth == NESTING_DEPTH_LIMIT + 1 && *max == NESTING_DEPTH_LIMIT
        ),
        "expected NestingTooDeep, got {err:?}"
    );
}

#[test]
fn prev_ref_nesting_in_chain_at_limit_succeeds() {
    let r = parse_request(&nested_prev_ref_in_chain(NESTING_DEPTH_LIMIT));
    assert!(
        r.is_ok(),
        "at-limit $prev nesting in chain mode must parse: {r:?}"
    );
    assert_eq!(r.unwrap().mode, ExecutionMode::Chain);
}

#[test]
fn prev_ref_nesting_in_chain_over_limit_rejects_cleanly() {
    let err = parse_request(&nested_prev_ref_in_chain(NESTING_DEPTH_LIMIT + 1)).unwrap_err();
    assert!(
        matches!(
            &err,
            DslError::NestingTooDeep { depth, max, .. }
            if *depth == NESTING_DEPTH_LIMIT + 1 && *max == NESTING_DEPTH_LIMIT
        ),
        "expected NestingTooDeep, got {err:?}"
    );
}
