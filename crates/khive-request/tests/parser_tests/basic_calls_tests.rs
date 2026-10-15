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

#[test]
fn function_form_integer_boundaries_match_serde_json_value_representation() {
    // Mirrors `crates/khive-wire-protocol/src/codec.rs`'s boundary test for
    // `decode_payload`'s `serde_json::from_slice` seam, but exercises the
    // request parser's own scalar decode instead (`parser_impl.rs::parse_value`'s
    // `serde_json::from_str(trimmed)` call) — a literal within
    // `[i64::MIN, u64::MAX]` decodes as an exact integer there too; past
    // either endpoint it silently becomes `f64`, changing the JSON type.
    let min_i64 = ops("v(a=-9223372036854775808)");
    assert!(val(&min_i64[0].args["a"]).is_i64());

    let max_u64 = ops("v(a=18446744073709551615)");
    assert!(val(&max_u64[0].args["a"]).is_u64());

    let below_min = ops("v(a=-9223372036854775809)");
    assert!(val(&below_min[0].args["a"]).is_f64());
    assert!(!val(&below_min[0].args["a"]).is_i64());

    let above_max = ops("v(a=18446744073709551616)");
    assert!(val(&above_max[0].args["a"]).is_f64());
    assert!(!val(&above_max[0].args["a"]).is_u64());
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
