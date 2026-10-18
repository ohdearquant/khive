// ── Chain $prev dispatch tests (ADR-016) ─────────────────────────────────────
//
// These tests verify that $prev / $prev.dotted.path references in chain ops are
// resolved against the prior op's canonical result BEFORE dispatch — not passed
// through as literal strings.  The four cases mirror the UE4 DSL critical finding.

/// Chain: assign a task then complete it using $prev.id.
///
/// The canonical result of `assign` contains an `id` field (short UUID).
/// `$prev.id` must resolve to that value so `complete` receives a valid ID.
#[tokio::test]
async fn test_prev_dot_id_resolves() -> anyhow::Result<()> {
    let client = connect().await?;

    let result = call(
        &client,
        "request",
        json!({
            "ops": r#"gtd.assign(title="chain-prev-id-test", status="next") | gtd.complete(id=$prev.id)"#,
            "presentation": "verbose"
        }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let results = body["results"].as_array().expect("results array");

    assert_eq!(results.len(), 2, "expected 2 ops in chain result");
    assert_eq!(
        results[0]["ok"],
        json!(true),
        "gtd.assign (op 0) must succeed: {}",
        results[0]
    );
    assert_eq!(
        results[1]["ok"],
        json!(true),
        "gtd.complete (op 1) must succeed — $prev.id was not resolved: {}",
        results[1]
    );
    assert_eq!(body["summary"]["succeeded"], json!(2));
    assert_eq!(body["summary"]["failed"], json!(0));
    assert_eq!(body["summary"]["aborted"], json!(0));

    // The completed task must have status "done".
    let complete_result = &results[1]["result"];
    assert_eq!(
        complete_result["to"].as_str().unwrap_or(""),
        "done",
        "completed task must have to=done: {complete_result}"
    );
    Ok(())
}

/// Chain: create a concept entity, then link it to a pre-created target using
/// $prev.id (op 0 result), then fetch the link using $prev.id (op 1 result).
///
/// This verifies that $prev.field correctly walks single-level dotted paths in
/// a 3-op chain, and that $prev always refers to the IMMEDIATELY preceding op.
#[tokio::test]
async fn test_prev_dotted_path_resolves() -> anyhow::Result<()> {
    let client = connect().await?;

    // Create a target entity first (outside the chain — we need its id).
    // Entity create results expose "id" (short 8-char form); full UUID is not
    // separately aliased for entities (unlike task notes which use "full_id").
    let target = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="PrevDottedTarget")"#,
    )
    .await?;
    let target_id = target["id"]
        .as_str()
        .expect("id field on entity result")
        .to_string();

    // Chain: create source | link (uses $prev.id from create) | get (uses $prev.id from link)
    let ops = format!(
        r#"create(kind="entity", entity_kind="concept", name="PrevDottedSource") | link(source_id=$prev.id, target_id="{target_id}", relation="extends") | get(id=$prev.id)"#
    );
    let result = call(
        &client,
        "request",
        json!({"ops": ops, "presentation": "verbose"}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let results = body["results"].as_array().expect("results array");

    assert_eq!(results.len(), 3, "expected 3 ops");
    assert_eq!(
        results[0]["ok"],
        json!(true),
        "create failed: {}",
        results[0]
    );
    assert_eq!(
        results[1]["ok"],
        json!(true),
        "link failed — $prev.id (create result) not resolved: {}",
        results[1]
    );
    assert_eq!(
        results[2]["ok"],
        json!(true),
        "get failed — $prev.id (link result) not resolved: {}",
        results[2]
    );
    assert_eq!(body["summary"]["succeeded"], json!(3));
    assert_eq!(body["summary"]["aborted"], json!(0));

    // The link result should have source_id matching the created entity.
    let source_id = results[0]["result"]["id"]
        .as_str()
        .unwrap_or_else(|| results[0]["result"]["full_id"].as_str().unwrap_or(""));
    let link_source = results[1]["result"]["source_id"].as_str().unwrap_or("");
    assert!(
        link_source.starts_with(source_id) || source_id.starts_with(link_source),
        "link.source_id {link_source:?} should match created entity {source_id:?}"
    );
    Ok(())
}

/// Chain abort: second op references a non-existent $prev field.
///
/// The failing op must have ok=false with an error message referencing the
/// unavailable path.  All subsequent ops must be marked aborted (ok=false,
/// aborted=true).  Summary: succeeded=1, failed=1, aborted=1.
#[tokio::test]
async fn test_prev_unresolvable_aborts_chain() -> anyhow::Result<()> {
    let client = connect().await?;

    let ops = r#"create(kind="entity", entity_kind="concept", name="AbortSource") | get(id=$prev.bogus_field_xyz) | create(kind="entity", entity_kind="concept", name="AbortSink")"#;
    let result = call(
        &client,
        "request",
        json!({"ops": ops, "presentation": "verbose"}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let results = body["results"].as_array().expect("results array");

    assert_eq!(results.len(), 3, "expected 3 ops in chain result");

    // Op 0: create must succeed.
    assert_eq!(
        results[0]["ok"],
        json!(true),
        "create (op 0) must succeed: {}",
        results[0]
    );

    // Op 1: get with unresolvable $prev path must fail (not be silently ok).
    assert_eq!(
        results[1]["ok"],
        json!(false),
        "get with bogus $prev path (op 1) must fail: {}",
        results[1]
    );
    // The error message must reference the path that could not be resolved.
    let err_obj = &results[1]["error"];
    let err_str = err_obj
        .as_str()
        .unwrap_or_else(|| err_obj["message"].as_str().unwrap_or(""));
    assert!(
        err_str.contains("bogus_field_xyz") || err_str.contains("not found"),
        "error must mention the unresolvable path; got: {err_str}"
    );
    // The failing op itself must NOT be marked aborted.
    assert_ne!(
        results[1]["aborted"],
        json!(true),
        "the failing op (op 1) must not be marked aborted: {}",
        results[1]
    );

    // Op 2: must be aborted because op 1 failed.
    assert_eq!(
        results[2]["ok"],
        json!(false),
        "aborted op (op 2) must have ok=false: {}",
        results[2]
    );
    assert_eq!(
        results[2]["aborted"],
        json!(true),
        "aborted op (op 2) must have aborted=true: {}",
        results[2]
    );

    assert_eq!(body["summary"]["total"], json!(3));
    assert_eq!(body["summary"]["succeeded"], json!(1));
    assert_eq!(body["summary"]["failed"], json!(1));
    assert_eq!(body["summary"]["aborted"], json!(1));
    Ok(())
}

/// UE4-H1: Chain bare `$prev` (no dot path) when the prior result is a map
/// must be rejected with a clear substitution error that lists available fields.
///
/// `gtd.assign | gtd.complete(id=$prev.id, result=$prev)` — `$prev.id` resolves fine
/// (scalar), but `result=$prev` resolves to the whole assign result map.
/// The dispatcher must catch the bare map substitution and return a per-op error
/// with `kind=substitution_error` and a message listing the available fields —
/// instead of silently passing the map downstream where the handler emits a
/// confusing "invalid type: map, expected a string".
#[tokio::test]
async fn test_ue4_h1_bare_prev_map_produces_clear_substitution_error() -> anyhow::Result<()> {
    let client = connect().await?;

    let result = call(
        &client,
        "request",
        json!({
            "ops": r#"gtd.assign(title="bare-prev-test") | gtd.complete(id=$prev.id, result=$prev)"#,
            "presentation": "verbose"
        }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let results = body["results"].as_array().expect("results array");

    assert_eq!(results.len(), 2, "expected 2 ops");
    assert_eq!(
        results[0]["ok"],
        json!(true),
        "assign must succeed: {}",
        results[0]
    );

    // Op 1: result=$prev resolves to the whole assign result map.
    // UE4-H1: the dispatcher must detect this and return a substitution_error
    // rather than passing the map through to the handler.
    assert_eq!(
        results[1]["ok"],
        json!(false),
        "bare $prev -> map must cause op 1 to fail: {}",
        results[1]
    );
    let error = &results[1]["error"];
    let err_msg = error["message"]
        .as_str()
        .unwrap_or_else(|| error.as_str().unwrap_or(""));
    assert!(
        err_msg.contains("dotted path") || err_msg.contains("$prev"),
        "UE4-H1: error must mention dotted path or $prev; got: {err_msg}"
    );
    assert!(
        err_msg.contains("result") || error["kind"].as_str() == Some("substitution_error"),
        "UE4-H1: error must reference the offending arg or be a substitution_error; got: {error}"
    );
    // The error must list at least one available field from the prior result.
    // assign result includes fields like id/full_id/title/kind.
    let mentions_field = err_msg.contains("id")
        || err_msg.contains("title")
        || err_msg.contains("kind")
        || err_msg.contains("full_id");
    assert!(
        mentions_field,
        "UE4-H1: error must list available top-level fields from prior result; got: {err_msg}"
    );

    // Chain is aborted: op 1 fails, no op 2 here (only 2 ops total).
    assert_eq!(body["summary"]["failed"], json!(1));
    Ok(())
}

/// ADR-016 H3 regression: `$prev.nonexistent_field` error must list the
/// available top-level fields from the prior result.
///
/// This test specifically covers the "H3: available fields hint" claim from the
/// PR — that `$prev.bogus` returns an error message containing
/// "Available top-level fields" plus at least one known field name.
/// The existing `test_prev_unresolvable_aborts_chain` only checked that the
/// path name appears in the error; this test asserts the field-hint clause.
#[tokio::test]
async fn test_h3_prev_nonexistent_field_error_lists_available_fields() -> anyhow::Result<()> {
    let client = connect().await?;

    // Create a concept so $prev has known fields (id, full_id, kind, name, …).
    let ops = r#"create(kind="entity", entity_kind="concept", name="H3Test") | get(id=$prev.nonexistent_field)"#;
    let result = call(
        &client,
        "request",
        json!({"ops": ops, "presentation": "verbose"}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let results = body["results"].as_array().expect("results array");

    assert_eq!(results.len(), 2, "expected 2 ops");
    assert_eq!(results[0]["ok"], json!(true), "create must succeed");

    // Op 1 (get) must fail because $prev.nonexistent_field doesn't exist.
    assert_eq!(
        results[1]["ok"],
        json!(false),
        "get with nonexistent field must fail: {}",
        results[1]
    );

    // The error message must contain the "Available top-level fields" hint.
    let err_obj = &results[1]["error"];
    let err_msg = err_obj
        .as_str()
        .unwrap_or_else(|| err_obj["message"].as_str().unwrap_or(""));
    assert!(
        err_msg.contains("Available top-level fields"),
        "H3: error must contain 'Available top-level fields'; got: {err_msg}"
    );
    // The hint must list at least one known field from the create result.
    let mentions_field =
        err_msg.contains("id") || err_msg.contains("kind") || err_msg.contains("full_id");
    assert!(
        mentions_field,
        "H3: available-fields hint must name at least one known field; got: {err_msg}"
    );

    Ok(())
}
