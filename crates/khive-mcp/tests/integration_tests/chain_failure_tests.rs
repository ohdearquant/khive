/// A `$prev` reference in the FIRST op of a chain has no preceding op to
/// resolve against. This must not reuse the "$prev reference in non-chain
/// context" wording (misleading — this IS a chain) and must teach the
/// one-op-back rule instead of failing silently confusing.
#[tokio::test]
async fn test_prev_in_first_op_of_chain_names_no_preceding_op() -> anyhow::Result<()> {
    let client = connect().await?;

    let ops =
        r#"get(id=$prev.id) | create(kind="entity", entity_kind="concept", name="NeverReached")"#;
    let result = call(
        &client,
        "request",
        json!({"ops": ops, "presentation": "verbose"}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let results = body["results"].as_array().expect("results array");

    assert_eq!(results.len(), 2, "expected 2 ops");
    assert_eq!(
        results[0]["ok"],
        json!(false),
        "first op referencing $prev must fail: {}",
        results[0]
    );
    let err = &results[0]["error"];
    let err_msg = err["message"]
        .as_str()
        .unwrap_or_else(|| err.as_str().unwrap_or(""));
    assert!(
        err_msg.contains("no preceding op") || err_msg.contains("first operation"),
        "must name the missing-preceding-op condition, not a generic non-chain message; got: {err_msg}"
    );
    assert!(
        !err_msg.contains("non-chain context"),
        "stale wording ('non-chain context') must not survive — this op IS in a chain, \
         it just has nothing before it; got: {err_msg}"
    );
    assert!(
        err_msg.contains("immediately preceding op"),
        "must teach the one-op-back rule; got: {err_msg}"
    );

    // Op 1 never runs — chain aborted at op 0 — and its aborted marker must
    // say plainly that op 0 is the one to fix, not itself.
    assert_eq!(results[1]["aborted"], json!(true));
    let aborted_msg = results[1]["message"].as_str().unwrap_or("");
    assert!(
        aborted_msg.contains("op #0"),
        "aborted marker must point at the failed op, not itself; got: {aborted_msg}"
    );

    Ok(())
}

/// A path segment that names a field which does exist, but on a value that
/// is not an object (or an index into something that is not an array), is a
/// different mistake than "field not found" and must say so — the caller
/// went looking for a sub-field on a scalar, not a missing name.
#[tokio::test]
async fn test_prev_path_wrong_type_names_the_type_mismatch() -> anyhow::Result<()> {
    let client = connect().await?;

    // `name` resolves to a string; `.foo` on a string is a type mismatch,
    // not a missing-field lookup.
    let ops = r#"create(kind="entity", entity_kind="concept", name="WrongTypeSource") | get(id=$prev.name.foo)"#;
    let result = call(
        &client,
        "request",
        json!({"ops": ops, "presentation": "verbose"}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let results = body["results"].as_array().expect("results array");

    assert_eq!(results[0]["ok"], json!(true), "create must succeed");
    assert_eq!(
        results[1]["ok"],
        json!(false),
        "get with a field-on-a-scalar path must fail: {}",
        results[1]
    );
    let err = &results[1]["error"];
    let err_msg = err["message"]
        .as_str()
        .unwrap_or_else(|| err.as_str().unwrap_or(""));
    assert!(
        err_msg.contains("is a string, not an"),
        "must name the actual JSON type found and what was expected; got: {err_msg}"
    );
    assert!(
        err_msg.contains("\"foo\""),
        "must name the segment that could not be applied; got: {err_msg}"
    );
    assert_eq!(err["kind"], json!("substitution_error"));
    assert_eq!(err["reason"], json!("path_wrong_type"));

    Ok(())
}

/// Bracket syntax that isn't a valid non-negative-integer index (e.g.
/// `[bad]`) never reaches the substitution layer at all: `khive-request`'s
/// own DSL parser rejects it up front, both for the unquoted form (here) and
/// for a quoted `"$prev[bad]"` string (which simply fails promotion to a
/// `$prev` reference and is treated as a literal string — see
/// `previous-result.md`). The substituter's own defense against this form
/// (`PrevFailure::Unsupported`, exercised directly in
/// `khive-request`'s test suite since it is otherwise unreachable through
/// this public surface) never fires here; what the caller actually sees is
/// this parse-time rejection.
#[tokio::test]
async fn test_prev_malformed_index_rejected_at_parse_time() -> anyhow::Result<()> {
    let client = connect().await?;

    let ops = r#"create(kind="entity", entity_kind="concept", name="MalformedIndexSource") | get(id=$prev[bad])"#;
    let err = call(
        &client,
        "request",
        json!({"ops": ops, "presentation": "verbose"}),
    )
    .await
    .expect_err("malformed bracket syntax must be rejected before any op runs");
    let err_msg = err.to_string();
    assert!(
        err_msg.contains("non-negative integer"),
        "parse-time rejection must explain what index syntax IS supported; got: {err_msg}"
    );

    Ok(())
}

/// A nested `$prev` reference inside an object-literal argument must resolve
/// (and fail) exactly as clearly as a bare `$prev.field` argument — the
/// error must still name the missing field, not just the outer arg name.
#[tokio::test]
async fn test_prev_nested_in_object_literal_errors_as_clearly_as_bare() -> anyhow::Result<()> {
    let client = connect().await?;

    let ops = r#"create(kind="entity", entity_kind="concept", name="NestedPrevSource") | create(kind="entity", entity_kind="concept", name="NestedPrevSink", properties={"linked": $prev.bogus_nested_field})"#;
    let result = call(
        &client,
        "request",
        json!({"ops": ops, "presentation": "verbose"}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let results = body["results"].as_array().expect("results array");

    assert_eq!(results[0]["ok"], json!(true), "first create must succeed");
    assert_eq!(
        results[1]["ok"],
        json!(false),
        "create with an unresolvable nested $prev inside properties must fail: {}",
        results[1]
    );
    let err = &results[1]["error"];
    let err_msg = err["message"]
        .as_str()
        .unwrap_or_else(|| err.as_str().unwrap_or(""));
    assert!(
        err_msg.contains("bogus_nested_field"),
        "nested $prev failure must name the unresolvable field just as a bare \
         reference would; got: {err_msg}"
    );

    Ok(())
}

/// When op N fails for a reason unrelated to substitution, downstream ops
/// referencing `$prev` are never dispatched at all (the chain aborts first).
/// The aborted marker must say plainly that an earlier op is the actual
/// problem, so the caller doesn't go looking at the wrong line.
#[tokio::test]
async fn test_prev_after_unrelated_failure_points_at_the_failed_op() -> anyhow::Result<()> {
    let client = connect().await?;

    // `get` on a well-formed but nonexistent id fails for reasons that have
    // nothing to do with $prev; op 1's own $prev.id reference must never be
    // attempted.
    let ops = r#"get(id="00000000-0000-0000-0000-000000000000") | create(kind="entity", entity_kind="concept", name=$prev.id)"#;
    let result = call(
        &client,
        "request",
        json!({"ops": ops, "presentation": "verbose"}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let results = body["results"].as_array().expect("results array");

    assert_eq!(results.len(), 2, "expected 2 ops");
    assert_eq!(
        results[0]["ok"],
        json!(false),
        "get on a nonexistent id must fail: {}",
        results[0]
    );
    assert_ne!(
        results[0]["aborted"],
        json!(true),
        "the actually-failing op must not itself be marked aborted: {}",
        results[0]
    );

    assert_eq!(results[1]["ok"], json!(false));
    assert_eq!(results[1]["aborted"], json!(true));
    let aborted_msg = results[1]["message"].as_str().unwrap_or("");
    assert!(
        aborted_msg.contains("op #0") && aborted_msg.contains("\"get\""),
        "aborted marker must name the earlier failed op, not describe op 1's own \
         (never-attempted) $prev reference; got: {aborted_msg}"
    );

    Ok(())
}
