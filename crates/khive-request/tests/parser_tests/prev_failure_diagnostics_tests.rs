fn prev_ref(path: &str) -> ArgValue {
    ArgValue::PrevRef {
        path: path.to_string(),
    }
}

#[test]
fn find_prev_failure_reports_not_found_for_missing_field() {
    let prev: JsonValue = json!({"id": "abc", "kind": "concept"});
    let failure = prev_ref("bogus").find_prev_failure(&prev);
    match failure {
        Some(PrevFailure::NotFound {
            missing, available, ..
        }) => {
            assert_eq!(missing, "bogus");
            assert!(available.contains(&"id".to_string()));
        }
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[test]
fn find_prev_failure_renders_root_index_as_valid_dsl_path() {
    let parsed = req(r#"search(kind="concept", query="missing root index") | get(id=$prev[2])"#);
    let failure = parsed.ops[1].args["id"]
        .find_prev_failure(&json!([]))
        .expect("out-of-range index must report a substitution failure");
    let rendered = failure.to_string();
    assert_eq!(
        rendered.split_once(':').map(|(path, _)| path),
        Some("$prev[2]")
    );
}

#[test]
fn find_prev_failure_renders_nested_index_as_valid_dsl_path() {
    let parsed = req(r#"list(kind="concept") | get(id=$prev.items[0].id)"#);
    let failure = parsed.ops[1].args["id"]
        .find_prev_failure(&json!({"items": []}))
        .expect("out-of-range index must report a substitution failure");
    let rendered = failure.to_string();
    assert_eq!(
        rendered.split_once(':').map(|(path, _)| path),
        Some("$prev.items[0].id")
    );
}

#[test]
fn find_prev_failure_reports_wrong_type_for_field_on_scalar() {
    let prev: JsonValue = json!({"name": "concept-1"});
    let failure = prev_ref("name.sub").find_prev_failure(&prev);
    match failure {
        Some(PrevFailure::WrongType {
            expected, found, ..
        }) => {
            assert_eq!(expected, "object");
            assert_eq!(found, "string");
        }
        other => panic!("expected WrongType, got {other:?}"),
    }
}

#[test]
fn find_prev_failure_reports_wrong_type_for_index_on_object() {
    let prev: JsonValue = json!({"items": {"not": "an array"}});
    let failure = prev_ref("items[0]").find_prev_failure(&prev);
    match failure {
        Some(PrevFailure::WrongType {
            expected, found, ..
        }) => {
            assert_eq!(expected, "array");
            assert_eq!(found, "object");
        }
        other => panic!("expected WrongType, got {other:?}"),
    }
}

#[test]
fn find_prev_failure_reports_unsupported_for_malformed_bracket() {
    // `ArgValue::PrevRef` built directly (bypassing the parser, which never
    // produces this shape) to exercise the substituter's own defense.
    let prev: JsonValue = json!({"id": "abc"});
    let failure = prev_ref("[bad]").find_prev_failure(&prev);
    match failure {
        Some(PrevFailure::Unsupported { segment, .. }) => {
            assert_eq!(segment, "[bad]");
        }
        other => panic!("expected Unsupported, got {other:?}"),
    }
}

#[test]
fn find_prev_failure_none_when_resolve_all_succeeds() {
    let prev: JsonValue = json!({"id": "abc", "nested": {"x": 1}});
    let arg = prev_ref("nested.x");
    assert!(arg.resolve_all(&prev).is_some());
    assert!(arg.find_prev_failure(&prev).is_none());
}

#[test]
fn find_prev_failure_wrong_type_prefix_uses_valid_dsl_path() {
    for (path, previous, prefix) in [
        ("$prev[0].id", json!([7]), "$prev[0]"),
        ("$prev.items[0].id", json!({"items": [7]}), "$prev.items[0]"),
        ("$prev.id", json!(7), "$prev"),
    ] {
        let parsed = parse_request_unplanned(&format!("source() | use(value={path})"))
            .expect("valid reference path");
        let arg = &parsed.ops[1].args["value"];
        assert_eq!(arg.resolve_all(&previous), None);
        let failure = arg.find_prev_failure(&previous).expect("wrong type");
        assert_eq!(
            failure,
            PrevFailure::WrongType {
                arg_path: String::new(),
                prev_path: path.to_string(),
                resolved_prefix: prefix.to_string(),
                segment: "id".to_string(),
                expected: "object",
                found: "number",
            }
        );
        assert_eq!(
            failure.to_string(),
            format!(
                "{path}: {prefix} is a number, not an object, so \
                 \"id\" cannot be applied to it"
            )
        );
    }
}

#[test]
fn find_prev_failure_not_found_prefix_uses_valid_dsl_path() {
    for (path, previous, prefix, missing, available) in [
        (
            "$prev[0].missing",
            json!([{"id": "x"}]),
            "$prev[0]",
            "missing",
            "id",
        ),
        (
            "$prev[0][1]",
            json!([[0]]),
            "$prev[0]",
            "[1]",
            "array has 1 element(s)",
        ),
        (
            "$prev.items[0].missing",
            json!({"items": [{"id": "x"}]}),
            "$prev.items[0]",
            "missing",
            "id",
        ),
        (
            "$prev[2]",
            json!([]),
            "$prev",
            "[2]",
            "array has 0 element(s)",
        ),
    ] {
        let parsed = parse_request_unplanned(&format!("source() | use(value={path})"))
            .expect("valid reference path");
        let arg = &parsed.ops[1].args["value"];
        assert_eq!(arg.resolve_all(&previous), None);
        let failure = arg.find_prev_failure(&previous).expect("missing path");
        assert_eq!(
            failure,
            PrevFailure::NotFound {
                arg_path: String::new(),
                prev_path: path.to_string(),
                resolved_prefix: prefix.to_string(),
                missing: missing.to_string(),
                available: vec![available.to_string()],
            }
        );
        assert_eq!(
            failure.to_string(),
            format!(
                "{path}: {prefix} has no {missing:?}. Available top-level fields: [{available}]"
            )
        );
    }
}
