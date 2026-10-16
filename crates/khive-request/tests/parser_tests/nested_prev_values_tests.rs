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
