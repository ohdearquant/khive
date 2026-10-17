// ── Required prompt examples ──────────────────────────────────────────────────

#[test]
fn recall_with_query_arg() {
    let v = ops(r#"memory.recall(query="test")"#);
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].tool, "memory.recall");
    assert_eq!(val(&v[0].args["query"]), &json!("test"));
}

#[test]
fn search_with_query_and_limit() {
    let v = ops(r#"search(query="test", limit=5)"#);
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].tool, "search");
    assert_eq!(val(&v[0].args["query"]), &json!("test"));
    assert_eq!(val(&v[0].args["limit"]), &json!(5));
}

#[test]
fn parallel_recall_and_inbox() {
    let r = req(r#"[memory.recall(query="x"), comm.inbox()]"#);
    assert_eq!(r.mode, ExecutionMode::Parallel);
    assert_eq!(r.ops.len(), 2);
    assert_eq!(r.ops[0].tool, "memory.recall");
    assert_eq!(val(&r.ops[0].args["query"]), &json!("x"));
    assert_eq!(r.ops[1].tool, "comm.inbox");
    assert!(r.ops[1].args.is_empty());
}

// ── JSON form edge cases ──────────────────────────────────────────────────────

#[test]
fn json_missing_args_defaults_to_empty_map() {
    let v = ops(r#"{"tool":"comm.inbox"}"#);
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].tool, "comm.inbox");
    assert!(v[0].args.is_empty());
}

#[test]
fn json_args_as_array_rejected() {
    let err = parse_request(r#"{"tool":"x","args":[]}"#).unwrap_err();
    assert!(matches!(err, DslError::InvalidJson { .. }));
}

// ── Identifier grammar ────────────────────────────────────────────────────────

#[test]
fn dotted_tool_name_parsed() {
    let v = ops("brain.state()");
    assert_eq!(v[0].tool, "brain.state");
    assert!(v[0].args.is_empty());
}

#[test]
fn dotted_tool_with_args() {
    let v = ops(r#"memory.recall_candidates(query="test", limit=5)"#);
    assert_eq!(v[0].tool, "memory.recall_candidates");
    assert_eq!(val(&v[0].args["query"]), &json!("test"));
    assert_eq!(val(&v[0].args["limit"]), &json!(5));
}

#[test]
fn dotted_tool_in_batch() {
    let v = ops(r#"[brain.state(), memory.recall_fuse(query="x")]"#);
    assert_eq!(v.len(), 2);
    assert_eq!(v[0].tool, "brain.state");
    assert_eq!(v[1].tool, "memory.recall_fuse");
}

#[test]
fn leading_underscore_identifier_is_valid() {
    let v = ops("_internal()");
    assert_eq!(v[0].tool, "_internal");
    assert!(v[0].args.is_empty());
}

#[test]
fn identifier_starting_with_digit_rejected() {
    let err = parse_request("1bad()").unwrap_err();
    assert!(matches!(err, DslError::InvalidIdentifier { pos: 0 }));
}

// ── Argument value edge cases ─────────────────────────────────────────────────

#[test]
fn boolean_false_as_arg_value() {
    let v = ops("flag(active=false)");
    assert_eq!(val(&v[0].args["active"]), &json!(false));
}

#[test]
fn unicode_string_arg_preserved() {
    let v = ops(r#"gtd.assign(title="café")"#);
    assert_eq!(val(&v[0].args["title"]), &json!("café"));
}
