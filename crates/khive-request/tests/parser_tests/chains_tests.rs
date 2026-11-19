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
