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
    assert!(matches!(err, DslError::UnclosedCall { .. }));
}

#[test]
fn object_key_position_distinguishes_end_of_input_from_a_present_byte() {
    // Only true end-of-input right after `{` reaches the object-key `None` arm.
    let err = parse_request("v(a={").unwrap_err();
    assert!(
        matches!(
            err,
            DslError::UnexpectedEof {
                expected: "object key"
            }
        ),
        "{err:?}"
    );
    // Any present byte, `)` or `,` alike, takes the `Some(c)` arm instead.
    for input in ["v(a={)", "v(a={, b=1)"] {
        let err = parse_request(input).unwrap_err();
        assert!(
            matches!(
                err,
                DslError::UnexpectedChar {
                    expected: "quoted string key",
                    ..
                }
            ),
            "{input}: {err:?}"
        );
    }
    // The empty object closes only when `}` is the very next byte.
    let parsed = ops("v(a={})");
    assert_eq!(val(&parsed[0].args["a"]), &json!({}));
}

#[test]
fn a_closing_brace_inside_an_open_local_bracket_is_rejected_at_once() {
    // `[` is still open when `}` arrives, so the value scan rejects immediately.
    let err = parse_request("v(a=1[})").unwrap_err();
    assert!(
        matches!(err, DslError::UnclosedBracket { kind: '{' }),
        "{err:?}"
    );
    // With no local container open, `}` ends the value and the arg list rejects it.
    let err = parse_request("v(a=1})").unwrap_err();
    assert!(
        matches!(err, DslError::UnexpectedChar { found: '}', .. }),
        "{err:?}"
    );
    // Inside a quoted span the `}` is string content; the malformed scalar fails as a value.
    let err = parse_request(r#"v(a=1["x}"])"#).unwrap_err();
    assert!(matches!(err, DslError::InvalidValue { .. }), "{err:?}");
}

#[test]
fn unclosed_call_reports_its_opening_byte_after_long_input() {
    let padding = "x".repeat(1_024);
    let src = format!(r#"comm.send(to="x", content="{padding}""#);
    let opening = src.find('(').expect("call opening delimiter");

    let message = parse_request(&src).unwrap_err().to_string();
    assert_eq!(
        message,
        format!("unclosed call starting at byte {opening}; expected ')'"),
    );
}

#[test]
fn unclosed_call_reports_the_delimiter_not_preceding_whitespace() {
    // Whitespace is permitted between the verb and its opening delimiter. The reported
    // offset must be the delimiter's own byte, not the first byte skipped to reach it.
    let src = "comm.send  \n (to=\"x\", content=\"ok\"";
    let opening = src.find('(').expect("call opening delimiter");
    assert!(opening > src.find("send").expect("verb") + "send".len());

    let message = parse_request(src).unwrap_err().to_string();
    assert_eq!(
        message,
        format!("unclosed call starting at byte {opening}; expected ')'"),
    );
}

#[test]
fn unterminated_string_rejected() {
    let err = parse_request(r#"gtd.assign(title="oops)"#).unwrap_err();
    assert!(matches!(err, DslError::UnclosedString { .. }));
}

#[test]
fn unterminated_string_reports_its_opening_byte_after_long_input() {
    let padding = "x".repeat(1_024);
    let src = format!(r#"comm.send(to="x", content="{padding}"#);
    let opening = src.rfind('"').expect("string opening delimiter");

    let message = parse_request(&src).unwrap_err().to_string();
    assert_eq!(
        message,
        format!("unterminated string literal starting at byte {opening}"),
    );
}

#[test]
fn positioned_error_labels_utf8_byte_offset() {
    let src = r#"comm.send(to="x", content="→→→") garbage"#;
    let garbage_byte = src.find("garbage").expect("trailing token");
    assert_ne!(
        garbage_byte,
        src[..garbage_byte].chars().count(),
        "multibyte precondition"
    );

    let message = parse_request(src).unwrap_err().to_string();
    assert!(
        message.starts_with(&format!("at byte {garbage_byte}:")),
        "position basis must be explicit: {message}"
    );
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
