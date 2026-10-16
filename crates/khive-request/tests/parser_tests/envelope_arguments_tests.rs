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

#[test]
fn preference_verbs_share_the_envelope_reserved_argument_contract() {
    for verb in [
        "moodboard.serve",
        "moodboard.judge",
        "moodboard.train_preference",
        "moodboard.preference",
    ] {
        for argument in RESERVED_ENVELOPE_ARGS {
            let error = parse_typed_json_batch(vec![TypedJsonOp {
                tool: verb.to_string(),
                args: serde_json::Map::from_iter([((*argument).to_string(), json!("verbose"))]),
            }])
            .expect_err("envelope-owned names must fail before every preference handler");
            assert!(
                matches!(
                    error,
                    DslError::ReservedEnvelopeArg { ref arg_name, verb: ref error_verb }
                        if arg_name == argument && error_verb == verb
                ),
                "unexpected error for {verb}.{argument}: {error:?}"
            );
        }
    }
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
