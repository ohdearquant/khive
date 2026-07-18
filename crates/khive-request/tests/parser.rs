//! Integration tests for the `khive-request` DSL parser.
//!
//! All tests exercise the public API (`parse_request`, public types). Tests that
//! require access to `pub(crate)` helpers (`check_write_key_conflicts`) live in
//! `src/conflict.rs` under `#[cfg(test)] mod tests`.

use khive_request::{
    parse_request, ArgValue, DslError, ExecutionMode, MAX_OPS, MAX_OPS_INPUT_LEN,
    NESTING_DEPTH_LIMIT,
};
use serde_json::json;

fn req(s: &str) -> khive_request::ParsedRequest {
    parse_request(s).unwrap_or_else(|e| panic!("parse({s:?}) failed: {e}"))
}

fn ops(s: &str) -> Vec<khive_request::ParsedOp> {
    req(s).ops
}

/// Extract the concrete `Value` from an `ArgValue::Value`, panicking on dynamic variants.
fn val(arg: &ArgValue) -> &serde_json::Value {
    match arg {
        ArgValue::Value(v) => v,
        ArgValue::PrevRef { path } => {
            panic!("expected Value, got PrevRef {{ path: {path:?} }}")
        }
        ArgValue::Array(els) => {
            panic!("expected Value, got Array with {} elements", els.len())
        }
        ArgValue::Object(pairs) => {
            panic!("expected Value, got Object with {} keys", pairs.len())
        }
    }
}

// ── Basic single op ───────────────────────────────────────────────────────────

#[test]
fn single_op_no_args() {
    let r = req("gtd.next()");
    assert_eq!(r.mode, ExecutionMode::Single);
    assert_eq!(r.ops.len(), 1);
    assert_eq!(r.ops[0].tool, "gtd.next");
    assert!(r.ops[0].args.is_empty());
}

#[test]
fn single_op_with_string_arg() {
    let v = ops(r#"gtd.assign(title="ship release")"#);
    assert_eq!(v[0].tool, "gtd.assign");
    assert_eq!(val(&v[0].args["title"]), &json!("ship release"));
}

#[test]
fn single_op_with_multiple_typed_args() {
    let v = ops(
        r#"create(kind="entity", entity_kind="concept", name="LoRA", weight=0.9, active=true)"#,
    );
    assert_eq!(v[0].tool, "create");
    assert_eq!(val(&v[0].args["kind"]), &json!("entity"));
    assert_eq!(val(&v[0].args["weight"]), &json!(0.9));
    assert_eq!(val(&v[0].args["active"]), &json!(true));
}

// ── Batch ─────────────────────────────────────────────────────────────────────

#[test]
fn batch_three_ops() {
    let r = req(
        r#"[create(kind="entity", name="A"), create(kind="entity", name="B"), link(source_id="x", target_id="y", relation="extends")]"#,
    );
    assert_eq!(r.mode, ExecutionMode::Parallel);
    assert_eq!(r.ops.len(), 3);
    assert_eq!(r.ops[0].tool, "create");
    assert_eq!(r.ops[2].tool, "link");
    assert_eq!(val(&r.ops[2].args["relation"]), &json!("extends"));
}

#[test]
fn empty_batch_rejected() {
    // UE4-H2: empty batch must be rejected with EmptyBatch error.
    let err = parse_request("[]").unwrap_err();
    assert!(
        matches!(err, DslError::EmptyBatch),
        "expected EmptyBatch, got {err:?}"
    );
    // JSON form empty array is also rejected.
    let err2 = parse_request("[]").unwrap_err();
    assert!(matches!(err2, DslError::EmptyBatch));
}

#[test]
fn nested_array_and_object_values() {
    let v = ops(r#"gtd.assign(title="x", tags=["a","b"], properties={"k":"v","n":1})"#);
    assert_eq!(val(&v[0].args["tags"]), &json!(["a", "b"]));
    assert_eq!(val(&v[0].args["properties"]), &json!({"k": "v", "n": 1}));
}

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
    assert!(matches!(err, DslError::UnclosedString));
}

#[test]
fn json_form_rejects_literal_newline_in_string() {
    // JSON form stays strict JSON: a raw control character inside a quoted
    // string is a parse error there, unlike the function-call form above.
    let src = "[{\"tool\":\"gtd.next\",\"args\":{\"note\":\"line one\nline two\"}}]";
    let err = parse_request(src).unwrap_err();
    assert!(matches!(err, DslError::InvalidJson { .. }));
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

#[test]
fn raw_control_char_in_bareword_position_rejected() {
    // The literal-control-char exception only applies inside a
    // double-quoted string value (`parse_value` only calls
    // `escape_literal_control_chars` when the trimmed slice starts with
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
fn control_char_error_teaches_escape_syntax_and_mcp_double_escape() {
    // #491: the bare serde message ("control character ... found while
    // parsing a string") teaches nothing. The wrapped error must name the
    // JSON escape grammar and the MCP-transport double-escape gotcha, so a
    // caller can actually fix the `ops` string instead of landing on the
    // wrong "switch to JSON op form" workaround.
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
        msg.to_lowercase().contains("double"),
        "error should call out the MCP double-escape requirement, got: {msg}"
    );
}

#[test]
fn control_char_in_object_key_teaches_escape_syntax_and_mcp_double_escape() {
    // #491 follow-up: quoted object KEYS decode through a separate path from
    // quoted values (`parse_object_arg_body` vs `parse_value`) and used to
    // bypass the enrichment entirely, returning the bare serde message. Both
    // paths now share `decode_quoted_json_string`, so a raw control byte in
    // a key gets the same teaching diagnostic as one in a value.
    let src = format!("gtd.assign(properties={{\"a{}b\":\"x\"}})", '\u{c}');
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
        msg.to_lowercase().contains("double"),
        "error should call out the MCP double-escape requirement, got: {msg}"
    );
}

#[test]
fn invalid_backslash_escape_in_quoted_string_rejected() {
    // A negative invalid-escape case: `\q` is not a JSON escape sequence.
    // This must fail regardless of the literal-newline carve-out.
    let src = r#"gtd.assign(title="bad \q escape")"#;
    let err = parse_request(src).unwrap_err();
    assert!(matches!(err, DslError::InvalidValue { .. }));
}

#[test]
fn batch_ops_separated_by_raw_newlines() {
    // The top-level batch `,` separator may be split across literal
    // newlines between ops, since `skip_ws` treats `\n` as ordinary
    // whitespace around the separator.
    let src = "[create(kind=\"entity\", name=\"A\"),\ncreate(kind=\"entity\", name=\"B\")]";
    let r = req(src);
    assert_eq!(r.mode, ExecutionMode::Parallel);
    assert_eq!(r.ops.len(), 2);
    assert_eq!(val(&r.ops[0].args["name"]), &json!("A"));
    assert_eq!(val(&r.ops[1].args["name"]), &json!("B"));
}

#[test]
fn chain_ops_separated_by_raw_newlines() {
    // The chain `|` separator may likewise be split across literal
    // newlines between ops.
    let src = "create(kind=\"entity\", name=\"A\")\n|\ncreate(kind=\"entity\", name=\"B\")";
    let r = req(src);
    assert_eq!(r.mode, ExecutionMode::Chain);
    assert_eq!(r.ops.len(), 2);
    assert_eq!(val(&r.ops[0].args["name"]), &json!("A"));
    assert_eq!(val(&r.ops[1].args["name"]), &json!("B"));
}

#[test]
fn null_and_negative_number() {
    let v = ops(r#"update(id="x", description=null, weight=-0.5)"#);
    assert_eq!(val(&v[0].args["description"]), &json!(null));
    assert_eq!(val(&v[0].args["weight"]), &json!(-0.5));
}

// ── JSON form ─────────────────────────────────────────────────────────────────

#[test]
fn json_form_batch_parses() {
    let r = req(r#"[{"tool":"gtd.next","args":{}}, {"tool":"gtd.complete","args":{"id":"abc"}}]"#);
    assert_eq!(r.mode, ExecutionMode::Parallel);
    assert_eq!(r.ops.len(), 2);
    assert_eq!(r.ops[1].tool, "gtd.complete");
    assert_eq!(val(&r.ops[1].args["id"]), &json!("abc"));
}

#[test]
fn json_form_with_leading_whitespace_inside_array_parses() {
    // Pretty-printers commonly emit `[ {...} ]` with spaces or newlines after `[`.
    // The whitespace is legal JSON, so the parser must route this to the JSON
    // path rather than the function-call batch parser.
    let v = ops(r#"[  {"tool":"gtd.next","args":{}} ]"#);
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].tool, "gtd.next");

    let v = ops("[\n  {\"tool\":\"gtd.next\",\"args\":{}},\n  {\"tool\":\"gtd.complete\",\"args\":{\"id\":\"x\"}}\n]");
    assert_eq!(v.len(), 2);
    assert_eq!(v[1].tool, "gtd.complete");
}

#[test]
fn json_form_single_object_is_treated_as_one_op() {
    let r = req(r#"{"tool":"gtd.next","args":{}}"#);
    assert_eq!(r.mode, ExecutionMode::Single);
    assert_eq!(r.ops.len(), 1);
    assert_eq!(r.ops[0].tool, "gtd.next");
}

// ── Error cases ───────────────────────────────────────────────────────────────

#[test]
fn duplicate_arg_rejected() {
    let err = parse_request(r#"gtd.assign(title="a", title="b")"#).unwrap_err();
    assert!(matches!(err, DslError::DuplicateArg { ref name } if name == "title"));
}

#[test]
fn unknown_token_after_op_rejected() {
    let err = parse_request(r#"gtd.next() garbage"#).unwrap_err();
    assert!(matches!(err, DslError::UnexpectedChar { .. }));
}

#[test]
fn unclosed_paren_rejected() {
    let err = parse_request(r#"gtd.assign(title="a""#).unwrap_err();
    // The string is closed; the args list isn't.
    assert!(matches!(err, DslError::UnexpectedEof { .. }));
}

#[test]
fn unterminated_string_rejected() {
    let err = parse_request(r#"gtd.assign(title="oops)"#).unwrap_err();
    assert!(matches!(err, DslError::UnclosedString));
}

#[test]
fn too_many_ops_rejected() {
    let one = r#"gtd.next(),"#;
    let mut s = String::from("[");
    for _ in 0..MAX_OPS + 1 {
        s.push_str(one);
    }
    s.push_str("gtd.next()]");
    let err = parse_request(&s).unwrap_err();
    assert!(matches!(err, DslError::TooManyOps { .. }));
}

#[test]
fn empty_request_rejected() {
    let err = parse_request("   ").unwrap_err();
    assert!(matches!(err, DslError::Empty));
}

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

// ── Chain mode ───────────────────────────────────────────────────────────────

#[test]
fn chain_two_ops_with_prev_ref() {
    let r = req(
        r#"create(kind="entity", entity_kind="concept", name="A") | link(source_id=$prev.id, target_id="abc", relation="extends")"#,
    );
    assert_eq!(r.mode, ExecutionMode::Chain);
    assert_eq!(r.ops.len(), 2);
    assert_eq!(r.ops[0].tool, "create");
    assert_eq!(r.ops[1].tool, "link");
    // The second op's source_id should be a PrevRef
    assert_eq!(
        r.ops[1].args["source_id"],
        ArgValue::PrevRef { path: "id".into() }
    );
    // target_id is a concrete value
    assert_eq!(val(&r.ops[1].args["target_id"]), &json!("abc"));
}

#[test]
fn chain_three_ops_mode() {
    let r = req(
        r#"create(kind="entity", name="A") | link(source_id=$prev.id, target_id="b", relation="extends") | update(id=$prev.id, description="desc")"#,
    );
    assert_eq!(r.mode, ExecutionMode::Chain);
    assert_eq!(r.ops.len(), 3);
    assert_eq!(r.ops[2].args["id"], ArgValue::PrevRef { path: "id".into() });
}

#[test]
fn chain_prev_no_field_selector() {
    // $prev alone (no dot path) refers to the whole prior result.
    let r = req(r#"gtd.next() | update(id=$prev)"#);
    assert_eq!(r.mode, ExecutionMode::Chain);
    assert_eq!(r.ops[1].args["id"], ArgValue::PrevRef { path: "".into() });
}

#[test]
fn chain_prev_deep_path() {
    let r = req(
        r#"create(kind="entity", name="A") | link(source_id=$prev.result.id, target_id="b", relation="extends")"#,
    );
    assert_eq!(r.mode, ExecutionMode::Chain);
    assert_eq!(
        r.ops[1].args["source_id"],
        ArgValue::PrevRef {
            path: "result.id".into()
        }
    );
}

#[test]
fn single_op_mode() {
    let r = req("gtd.next()");
    assert_eq!(r.mode, ExecutionMode::Single);
}

#[test]
fn chain_too_many_ops_rejected() {
    let mut s = String::from("gtd.next()");
    for _ in 0..MAX_OPS {
        s.push_str(" | gtd.next()");
    }
    let err = parse_request(&s).unwrap_err();
    assert!(matches!(err, DslError::TooManyOps { .. }));
}

// ── ArgValue helpers ──────────────────────────────────────────────────────────

#[test]
fn arg_value_resolve_prev_simple() {
    let prev = json!({"id": "abc-123", "name": "A"});
    let r = ArgValue::PrevRef { path: "id".into() };
    assert_eq!(r.resolve_prev(&prev), Some(&json!("abc-123")));
}

#[test]
fn arg_value_resolve_prev_empty_path() {
    let prev = json!({"id": "x"});
    let r = ArgValue::PrevRef { path: "".into() };
    assert_eq!(r.resolve_prev(&prev), Some(&prev));
}

#[test]
fn arg_value_resolve_prev_nested_path() {
    let prev = json!({"result": {"id": "nested-id"}});
    let r = ArgValue::PrevRef {
        path: "result.id".into(),
    };
    assert_eq!(r.resolve_prev(&prev), Some(&json!("nested-id")));
}

#[test]
fn arg_value_resolve_prev_missing_field_returns_none() {
    let prev = json!({"id": "x"});
    let r = ArgValue::PrevRef {
        path: "nonexistent".into(),
    };
    assert_eq!(r.resolve_prev(&prev), None);
}

#[test]
fn arg_value_value_returns_none_for_resolve_prev() {
    let r = ArgValue::Value(json!("hello"));
    assert_eq!(r.resolve_prev(&json!({})), None);
}

// ── G-C1: $prev inside array / object literals (regression) ──────────────────

#[test]
fn chain_prev_in_single_element_array() {
    // `gtd.assign(title="root") | gtd.assign(title="dep", depends_on=[$prev.full_id])`
    let r =
        req(r#"gtd.assign(title="root") | gtd.assign(title="dep", depends_on=[$prev.full_id])"#);
    assert_eq!(r.mode, ExecutionMode::Chain);
    assert_eq!(r.ops.len(), 2);
    match &r.ops[1].args["depends_on"] {
        ArgValue::Array(els) => {
            assert_eq!(els.len(), 1);
            assert_eq!(
                els[0],
                ArgValue::PrevRef {
                    path: "full_id".into()
                }
            );
        }
        other => panic!("expected Array, got {other:?}"),
    }
}

#[test]
fn chain_prev_in_mixed_array() {
    // `[$prev.id, "literal-uuid"]` — first element is PrevRef, second is literal.
    let r = req(
        r#"gtd.assign(title="root") | gtd.assign(title="dep", depends_on=[$prev.id, "literal-uuid"])"#,
    );
    assert_eq!(r.mode, ExecutionMode::Chain);
    match &r.ops[1].args["depends_on"] {
        ArgValue::Array(els) => {
            assert_eq!(els.len(), 2);
            assert_eq!(els[0], ArgValue::PrevRef { path: "id".into() });
            assert_eq!(els[1], ArgValue::Value(json!("literal-uuid")));
        }
        other => panic!("expected Array, got {other:?}"),
    }
}

#[test]
fn chain_prev_multiple_in_array() {
    // `depends_on=[$prev.field.deep, $prev.other]`
    let r = req(
        r#"gtd.assign(title="root") | gtd.assign(title="dep", depends_on=[$prev.field.deep, $prev.other])"#,
    );
    assert_eq!(r.mode, ExecutionMode::Chain);
    match &r.ops[1].args["depends_on"] {
        ArgValue::Array(els) => {
            assert_eq!(els.len(), 2);
            assert_eq!(
                els[0],
                ArgValue::PrevRef {
                    path: "field.deep".into()
                }
            );
            assert_eq!(
                els[1],
                ArgValue::PrevRef {
                    path: "other".into()
                }
            );
        }
        other => panic!("expected Array, got {other:?}"),
    }
}

#[test]
fn chain_prev_inside_object_inside_array() {
    // `properties={"refs":[$prev.id]}` — nested: object containing array containing PrevRef
    let r = req(
        r#"gtd.assign(title="root") | gtd.assign(title="dep", properties={"refs": [$prev.id]})"#,
    );
    assert_eq!(r.mode, ExecutionMode::Chain);
    match &r.ops[1].args["properties"] {
        ArgValue::Object(pairs) => {
            assert_eq!(pairs.len(), 1);
            assert_eq!(pairs[0].0, "refs");
            match &pairs[0].1 {
                ArgValue::Array(els) => {
                    assert_eq!(els.len(), 1);
                    assert_eq!(els[0], ArgValue::PrevRef { path: "id".into() });
                }
                other => panic!("expected inner Array, got {other:?}"),
            }
        }
        other => panic!("expected Object, got {other:?}"),
    }
}

#[test]
fn pure_json_array_folds_to_value() {
    // An array with no $prev refs should still produce ArgValue::Value(Array(...))
    let v = ops(r#"gtd.assign(title="x", depends_on=["a", "b"])"#);
    assert_eq!(val(&v[0].args["depends_on"]), &json!(["a", "b"]));
}

#[test]
fn pure_json_object_folds_to_value() {
    // An object with no $prev refs should still produce ArgValue::Value(Object(...))
    let v = ops(r#"gtd.assign(title="x", properties={"k": "v"})"#);
    assert_eq!(val(&v[0].args["properties"]), &json!({"k": "v"}));
}

#[test]
fn resolve_all_on_array_with_prev_ref() {
    let prev = json!({"full_id": "abc-def-123"});
    let arr = ArgValue::Array(vec![ArgValue::PrevRef {
        path: "full_id".into(),
    }]);
    assert_eq!(arr.resolve_all(&prev), Some(json!(["abc-def-123"])));
}

#[test]
fn resolve_all_on_mixed_array() {
    let prev = json!({"id": "x"});
    let arr = ArgValue::Array(vec![
        ArgValue::PrevRef { path: "id".into() },
        ArgValue::Value(json!("literal")),
    ]);
    assert_eq!(arr.resolve_all(&prev), Some(json!(["x", "literal"])));
}

#[test]
fn resolve_all_on_nested_object() {
    let prev = json!({"id": "obj-id"});
    let obj = ArgValue::Object(vec![(
        "refs".into(),
        ArgValue::Array(vec![ArgValue::PrevRef { path: "id".into() }]),
    )]);
    assert_eq!(obj.resolve_all(&prev), Some(json!({"refs": ["obj-id"]})));
}

#[test]
fn resolve_all_missing_path_returns_none() {
    let prev = json!({"id": "x"});
    let arr = ArgValue::Array(vec![ArgValue::PrevRef {
        path: "missing".into(),
    }]);
    assert_eq!(arr.resolve_all(&prev), None);
}

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

// ── MixedSeparators emitted at parse time ─────────────────────────────────────

#[test]
fn mixed_separators_in_fn_batch_rejected() {
    // `[a() | b(), c()]` mixes `|` and `,` at top level.
    let err = parse_request("[a() | b(), c()]").unwrap_err();
    assert!(
        matches!(err, DslError::MixedSeparators),
        "expected MixedSeparators, got {err:?}"
    );
}

#[test]
fn mixed_separator_after_chain_rejected() {
    // `a() | b(), c()` mixes `|` chain with trailing `,`.
    let err = parse_request("a() | b(), c()").unwrap_err();
    assert!(
        matches!(err, DslError::MixedSeparators),
        "expected MixedSeparators, got {err:?}"
    );
}

#[test]
fn comma_only_parallel_accepted() {
    // `[a(), b(), c()]` is valid comma-only parallel batch.
    let r = parse_request("[a(), b(), c()]").unwrap();
    assert_eq!(r.mode, ExecutionMode::Parallel);
    assert_eq!(r.ops.len(), 3);
}

#[test]
fn pipe_only_chain_accepted() {
    // `a() | b() | c()` is valid pipe-only chain.
    let r = parse_request("a() | b() | c()").unwrap();
    assert_eq!(r.mode, ExecutionMode::Chain);
    assert_eq!(r.ops.len(), 3);
}

// ── multi-segment dotted verb → clear error ───────────────────────────────────

#[test]
fn three_segment_verb_name_rejected() {
    // `brain.state.debug()` must produce UnsupportedVerbNesting,
    // not the misleading "expected '|' or end of input, found '.'".
    let err = parse_request("brain.state.debug()").unwrap_err();
    assert!(
        matches!(err, DslError::UnsupportedVerbNesting { .. }),
        "expected UnsupportedVerbNesting, got {err:?}"
    );
}

// ── ue-dsl-chain H1: array indexing in $prev paths ────────────────────────────

#[test]
fn chain_prev_array_index_at_root() {
    // `$prev[0].id` — index at the root of a prev result.
    let r = req(r#"list(kind="concept") | get(id=$prev[0].id)"#);
    assert_eq!(r.mode, ExecutionMode::Chain);
    assert_eq!(
        r.ops[1].args["id"],
        ArgValue::PrevRef {
            path: "[0].id".into()
        }
    );
}

#[test]
fn chain_prev_array_index_nested() {
    // `$prev.items[2].name` — index inside a named field.
    let r = req(r#"list(kind="concept") | get(id=$prev.items[2].name)"#);
    assert_eq!(r.mode, ExecutionMode::Chain);
    assert_eq!(
        r.ops[1].args["id"],
        ArgValue::PrevRef {
            path: "items.[2].name".into()
        }
    );
}

#[test]
fn resolve_prev_array_index_at_root() {
    let prev = json!([{"id": "first"}, {"id": "second"}]);
    let r = ArgValue::PrevRef {
        path: "[0].id".into(),
    };
    assert_eq!(r.resolve_prev(&prev), Some(&json!("first")));
}

#[test]
fn resolve_prev_array_index_nested() {
    let prev = json!({"items": [{"name": "alpha"}, {"name": "beta"}]});
    let r = ArgValue::PrevRef {
        path: "items.[1].name".into(),
    };
    assert_eq!(r.resolve_prev(&prev), Some(&json!("beta")));
}

#[test]
fn resolve_prev_array_index_out_of_bounds_returns_none() {
    let prev = json!([{"id": "only"}]);
    let r = ArgValue::PrevRef {
        path: "[5].id".into(),
    };
    assert_eq!(r.resolve_prev(&prev), None);
}

// ── CC-3 + H1: quoted prev ref with array index ───────────────────────────────

#[test]
fn quoted_prev_ref_with_array_index_parses() {
    // `"$prev[0].id"` quoted with bracket index should also promote.
    let r = req(r#"list(kind="concept") | get(id="$prev[0].id")"#);
    assert_eq!(r.mode, ExecutionMode::Chain);
    assert_eq!(
        r.ops[1].args["id"],
        ArgValue::PrevRef {
            path: "[0].id".into()
        }
    );
}

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

// ── write_keys_for_op_pub (public extraction helper for MCP server) ────────────

#[test]
fn write_keys_for_op_pub_update() {
    use khive_request::write_keys_for_op_pub;
    use std::collections::BTreeMap;
    let op = khive_request::ParsedOp {
        tool: "update".into(),
        args: {
            let mut m = BTreeMap::new();
            m.insert("id".into(), ArgValue::Value(json!("some-uuid")));
            m
        },
    };
    assert_eq!(write_keys_for_op_pub(&op), vec!["entity:some-uuid"]);
}

#[test]
fn write_keys_for_op_pub_link() {
    use khive_request::write_keys_for_op_pub;
    use std::collections::BTreeMap;
    let op = khive_request::ParsedOp {
        tool: "link".into(),
        args: {
            let mut m = BTreeMap::new();
            m.insert("source_id".into(), ArgValue::Value(json!("a")));
            m.insert("target_id".into(), ArgValue::Value(json!("b")));
            m.insert("relation".into(), ArgValue::Value(json!("extends")));
            m
        },
    };
    assert_eq!(write_keys_for_op_pub(&op), vec!["edge-natural:a:b:extends"]);
}

// ── ADR-045: reserved envelope args rejected inside verb args ─────────────────

#[test]
fn presentation_in_fn_call_args_rejected() {
    let err = parse_request(r#"list(kind="task", presentation="agent")"#).unwrap_err();
    assert!(
        matches!(
            &err,
            DslError::ReservedEnvelopeArg { arg_name, verb }
            if arg_name == "presentation" && verb == "list"
        ),
        "expected ReservedEnvelopeArg, got {err:?}"
    );
}

#[test]
fn presentation_per_op_in_fn_call_args_rejected() {
    let err = parse_request(r#"list(kind="task", presentation_per_op="verbose")"#).unwrap_err();
    assert!(
        matches!(
            &err,
            DslError::ReservedEnvelopeArg { arg_name, verb }
            if arg_name == "presentation_per_op" && verb == "list"
        ),
        "expected ReservedEnvelopeArg, got {err:?}"
    );
}

#[test]
fn presentation_in_json_form_args_rejected() {
    let err = parse_request(r#"{"tool":"list","args":{"kind":"task","presentation":"agent"}}"#)
        .unwrap_err();
    assert!(
        matches!(
            &err,
            DslError::ReservedEnvelopeArg { arg_name, verb }
            if arg_name == "presentation" && verb == "list"
        ),
        "expected ReservedEnvelopeArg, got {err:?}"
    );
}

#[test]
fn presentation_in_fn_batch_args_rejected() {
    let err = parse_request(r#"[list(kind="task", presentation="agent"), search(query="x")]"#)
        .unwrap_err();
    assert!(
        matches!(
            &err,
            DslError::ReservedEnvelopeArg { arg_name, verb }
            if arg_name == "presentation" && verb == "list"
        ),
        "expected ReservedEnvelopeArg, got {err:?}"
    );
}

#[test]
fn presentation_in_chain_args_rejected() {
    let err = parse_request(r#"list(kind="task") | get(id=$prev.id, presentation="verbose")"#)
        .unwrap_err();
    assert!(
        matches!(
            &err,
            DslError::ReservedEnvelopeArg { arg_name, verb }
            if arg_name == "presentation" && verb == "get"
        ),
        "expected ReservedEnvelopeArg, got {err:?}"
    );
}

#[test]
fn non_reserved_presentation_like_arg_accepted() {
    let r = req(r#"list(kind="task", present="yes")"#);
    assert_eq!(r.mode, ExecutionMode::Single);
    assert_eq!(val(&r.ops[0].args["present"]), &json!("yes"));
}

/// RUNTIME-AUD-002 (#433): the JSON-form parser must preserve a present-but
/// non-string `namespace` verbatim (as `ArgValue::Value`), never dropping or
/// coercing it. This is the ingress link that carries the malformed value into
/// `VerbRegistry::dispatch`, where `namespace_null_rejected_not_coerced` proves
/// it is rejected with `InvalidInput` before the gate is ever consulted.
/// Together the two tests cover the full MCP JSON-form path end to end.
#[test]
fn json_form_namespace_non_string_preserved_for_dispatch_rejection() {
    let cases = [
        json!(null),
        json!(42),
        json!(true),
        json!(["local"]),
        json!({ "ns": "local" }),
    ];
    for ns in cases {
        let body = json!([{
            "tool": "create",
            "args": { "kind": "entity", "entity_kind": "concept", "name": "N", "namespace": ns },
        }]);
        let r = req(&body.to_string());
        assert_eq!(r.ops.len(), 1, "namespace={ns} should parse to one op");
        // `namespace` is NOT a reserved envelope arg — it survives parsing as a
        // raw Value and reaches the runtime dispatch chokepoint unchanged.
        assert_eq!(
            val(&r.ops[0].args["namespace"]),
            &ns,
            "namespace={ns} must be preserved verbatim, not dropped or coerced"
        );
    }
}

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

#[test]
fn oversized_ops_input_rejected_before_parsing() {
    let huge = format!("verb(x=\"{}\")", "a".repeat(MAX_OPS_INPUT_LEN + 1));
    let err = parse_request(&huge).unwrap_err();
    assert!(
        matches!(
            &err,
            DslError::InputTooLarge { len, max }
            if *len > MAX_OPS_INPUT_LEN && *max == MAX_OPS_INPUT_LEN
        ),
        "expected InputTooLarge, got {err:?}"
    );
}

#[test]
fn ops_input_at_max_len_boundary_still_parses() {
    // Pad a trivially valid op with a comment-free string arg up to exactly
    // MAX_OPS_INPUT_LEN bytes, proving the length cap does not reject
    // legitimate input sitting right at the boundary.
    let prefix = "verb(x=\"";
    let suffix = "\")";
    let pad_len = MAX_OPS_INPUT_LEN - prefix.len() - suffix.len();
    let input = format!("{prefix}{}{suffix}", "a".repeat(pad_len));
    assert_eq!(input.len(), MAX_OPS_INPUT_LEN);
    let r = parse_request(&input);
    assert!(
        r.is_ok(),
        "input at exactly the length cap must parse: {r:?}"
    );
}

/// Build a JSON-form `create` op carrying `count` bulk `items`, each with a
/// `description` of `desc_len` bytes, mirrors the ADR-038 1000-item bulk
/// contract (`crates/khive-pack-kg/src/handler_defs.rs` `items` param).
fn bulk_create_payload(count: usize, desc_len: usize) -> String {
    let items: Vec<serde_json::Value> = (0..count)
        .map(|i| {
            json!({
                "kind": "concept",
                "name": format!("item-{i}"),
                "description": "d".repeat(desc_len),
            })
        })
        .collect();
    let payload = json!([{ "tool": "create", "args": { "kind": "concept", "items": items } }]);
    payload.to_string()
}

#[test]
fn bulk_create_1000_items_at_220_byte_descriptions_parses() {
    // ADR-038 promises up to 1000 items per bulk `create`; 220-byte
    // descriptions land the realistic payload around 276 KB, comfortably
    // under MAX_OPS_INPUT_LEN (1 MiB) but well over the old 256 KiB cap this
    // finding raised the limit to fix.
    let input = bulk_create_payload(1000, 220);
    assert!(
        input.len() > 256 * 1024 && input.len() < MAX_OPS_INPUT_LEN,
        "expected a realistic ~276 KB bulk payload, got {} bytes",
        input.len()
    );
    let parsed = parse_request(&input)
        .unwrap_or_else(|e| panic!("realistic 1000-item bulk create must parse: {e}"));
    assert_eq!(parsed.ops.len(), 1);
    assert_eq!(parsed.ops[0].tool, "create");
}

#[test]
fn oversized_bulk_create_payload_rejected_before_parsing() {
    // Same bulk-create shape, but padded well past MAX_OPS_INPUT_LEN, the
    // cap must still reject pathological input even in the documented-contract
    // shape it was raised to accommodate.
    let input = bulk_create_payload(1000, 2000);
    assert!(
        input.len() > MAX_OPS_INPUT_LEN,
        "test payload must exceed the cap, got {} bytes",
        input.len()
    );
    let err = parse_request(&input).unwrap_err();
    assert!(
        matches!(
            &err,
            DslError::InputTooLarge { len, max }
            if *len > MAX_OPS_INPUT_LEN && *max == MAX_OPS_INPUT_LEN
        ),
        "expected InputTooLarge, got {err:?}"
    );
}

#[test]
fn realistic_nested_payload_well_under_limit_still_parses() {
    // Regression guard (test plan item 5): a `kg.propose`-shaped nested
    // changeset payload, several levels deep, must keep parsing under the
    // new depth guard exactly as it did before this change.
    let r = req(
        r#"propose(title="t", description="d", changeset=[{"op":"create","fields":{"tags":["a","b"],"props":{"nested":{"deep":[1,2,3]}}}}])"#,
    );
    assert_eq!(r.mode, ExecutionMode::Single);
}
