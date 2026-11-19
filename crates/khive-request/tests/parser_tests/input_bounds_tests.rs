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
fn typed_json_batch_uses_transport_bounds_instead_of_raw_dsl_cap() {
    let mut args = serde_json::Map::new();
    args.insert(
        "payload".to_string(),
        json!("x".repeat(MAX_OPS_INPUT_LEN + 1)),
    );

    let parsed = parse_typed_json_batch(vec![TypedJsonOp {
        tool: "blob.put".to_string(),
        args,
    }])
    .expect("the bounded typed transport must not reapply the raw DSL byte cap");

    assert_eq!(parsed.mode, ExecutionMode::Parallel);
    assert_eq!(parsed.ops.len(), 1);
    assert_eq!(parsed.ops[0].tool, "blob.put");
    assert_eq!(
        val(&parsed.ops[0].args["payload"]).as_str().unwrap().len(),
        MAX_OPS_INPUT_LEN + 1
    );
}

#[test]
fn typed_json_batch_preserves_json_form_structural_guards() {
    assert!(matches!(
        parse_typed_json_batch(Vec::new()),
        Err(DslError::EmptyBatch)
    ));
    let over_count = (0..=MAX_OPS)
        .map(|index| TypedJsonOp {
            tool: format!("probe_{index}"),
            args: serde_json::Map::new(),
        })
        .collect();
    assert!(matches!(
        parse_typed_json_batch(over_count),
        Err(DslError::TooManyOps { count, max }) if count == MAX_OPS + 1 && max == MAX_OPS
    ));

    let reserved = TypedJsonOp {
        tool: "list".to_string(),
        args: serde_json::Map::from_iter([
            ("kind".to_string(), json!("concept")),
            ("presentation".to_string(), json!("verbose")),
        ]),
    };
    assert!(matches!(
        parse_typed_json_batch(vec![reserved]),
        Err(DslError::ReservedEnvelopeArg { arg_name, .. }) if arg_name == "presentation"
    ));

    let exposure = TypedJsonOp {
        tool: "moodboard.serve".to_string(),
        args: serde_json::Map::from_iter([(
            "exposure".to_string(),
            json!({"preference_probability_shown": false, "source_rank_shown": true}),
        )]),
    };
    let parsed = parse_typed_json_batch(vec![exposure])
        .expect("business exposure provenance must not collide with the request envelope");
    assert_eq!(
        val(&parsed.ops[0].args["exposure"])["source_rank_shown"],
        true
    );

    let prev = TypedJsonOp {
        tool: "get".to_string(),
        args: serde_json::Map::from_iter([("id".to_string(), json!({"nested": "$prev.id"}))]),
    };
    assert!(matches!(
        parse_typed_json_batch(vec![prev]),
        Err(DslError::PrevRefInJsonForm { arg_name }) if arg_name == "id"
    ));

    let mut at_limit = json!(null);
    for _ in 0..(NESTING_DEPTH_LIMIT - 3) {
        at_limit = json!([at_limit]);
    }
    let at_limit = TypedJsonOp {
        tool: "stats".to_string(),
        args: serde_json::Map::from_iter([("payload".to_string(), at_limit)]),
    };
    assert!(parse_typed_json_batch(vec![at_limit]).is_ok());

    let mut over_limit = json!(null);
    for _ in 0..(NESTING_DEPTH_LIMIT - 2) {
        over_limit = json!([over_limit]);
    }
    let over_limit = TypedJsonOp {
        tool: "stats".to_string(),
        args: serde_json::Map::from_iter([("payload".to_string(), over_limit)]),
    };
    assert!(matches!(
        parse_typed_json_batch(vec![over_limit]),
        Err(DslError::NestingTooDeep { max, .. }) if max == NESTING_DEPTH_LIMIT
    ));
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
