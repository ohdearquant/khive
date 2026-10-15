// ── ue-dsl-chain H1: array indexing in $prev paths ────────────────────────────

#[test]
fn chain_prev_array_index_at_root() {
    // `$prev[0].id` — index at the root of a prev result.
    let r = req(r#"search(kind="concept", query="root index") | get(id=$prev[0].id)"#);
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
    let r = req(r#"search(kind="concept", query="quoted root index") | get(id="$prev[0].id")"#);
    assert_eq!(r.mode, ExecutionMode::Chain);
    assert_eq!(
        r.ops[1].args["id"],
        ArgValue::PrevRef {
            path: "[0].id".into()
        }
    );
}
