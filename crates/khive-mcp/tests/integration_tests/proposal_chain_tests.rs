// ── PR #121: proposal_id → id wire-key — DSL chain tests ─────────────────────
//
// These tests prove that `$prev.id` substitution works end-to-end through the
// request envelope for the proposal lifecycle.  Direct handler dispatch in
// crates/khive-pack-kg/tests/ does NOT prove this path — it bypasses the MCP
// request envelope, presentation mode selection, and $prev resolver.
//
// Each test uses the `|` DSL chain operator so the MCP server exercises the
// full substitution path defined in ADR-016.

/// Chain: propose(...) | review(id=$prev.id, decision="reject").
///
/// Asserts that $prev.id from the propose result is correctly substituted into
/// the review call, and that the resulting proposal status is "rejected".
/// Also asserts the old `proposal_id` wire key is absent from both results.
#[tokio::test]
async fn test_propose_pipe_review_reject_chain() -> anyhow::Result<()> {
    let client = connect().await?;

    // DSL chain: propose then immediately review with $prev.id.
    // The changeset uses DSL object syntax (JSON keys: "key":"value").
    let ops = r#"propose(title="ChainReviewRejectTest", description="pr121 chain test", changeset={"kind":"add_note","note":{"kind":"observation","content":"chain-review-reject"}}) | review(id=$prev.id, decision="reject")"#;
    let result = call(
        &client,
        "request",
        json!({"ops": ops, "presentation": "verbose"}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let results = body["results"].as_array().expect("results array");

    assert_eq!(results.len(), 2, "expected 2 ops in chain");
    assert_eq!(
        results[0]["ok"],
        json!(true),
        "propose (op 0) must succeed: {}",
        results[0]
    );
    assert_eq!(
        results[1]["ok"],
        json!(true),
        "review(id=$prev.id) (op 1) must succeed — $prev.id from propose not resolved: {}",
        results[1]
    );
    assert_eq!(
        body["summary"]["succeeded"],
        json!(2),
        "both ops must succeed"
    );
    assert_eq!(body["summary"]["aborted"], json!(0));

    // The review result must report status "rejected".
    let review_result = &results[1]["result"];
    assert_eq!(
        review_result["status"].as_str().unwrap_or(""),
        "rejected",
        "review(decision=reject) must yield status rejected; got: {review_result}"
    );

    // Clean-break: neither result must expose the old wire key.
    assert!(
        results[0]["result"].get("proposal_id").is_none(),
        "propose result must NOT contain proposal_id; got: {}",
        results[0]["result"]
    );
    assert!(
        results[1]["result"].get("proposal_id").is_none(),
        "review result must NOT contain proposal_id; got: {}",
        results[1]["result"]
    );

    Ok(())
}

/// Chain: propose(...) | withdraw(id=$prev.id).
///
/// Asserts that $prev.id from the propose result is correctly substituted into
/// the withdraw call, and that the resulting proposal status is "withdrawn".
/// Also asserts the old `proposal_id` wire key is absent from both results.
#[tokio::test]
async fn test_propose_pipe_withdraw_chain() -> anyhow::Result<()> {
    let client = connect().await?;

    // DSL chain: propose then immediately withdraw with $prev.id.
    let ops = r#"propose(title="ChainWithdrawTest", description="pr121 chain test", changeset={"kind":"add_note","note":{"kind":"observation","content":"chain-withdraw"}}) | withdraw(id=$prev.id)"#;
    let result = call(
        &client,
        "request",
        json!({"ops": ops, "presentation": "verbose"}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let results = body["results"].as_array().expect("results array");

    assert_eq!(results.len(), 2, "expected 2 ops in chain");
    assert_eq!(
        results[0]["ok"],
        json!(true),
        "propose (op 0) must succeed: {}",
        results[0]
    );
    assert_eq!(
        results[1]["ok"],
        json!(true),
        "withdraw(id=$prev.id) (op 1) must succeed — $prev.id from propose not resolved: {}",
        results[1]
    );
    assert_eq!(
        body["summary"]["succeeded"],
        json!(2),
        "both ops must succeed"
    );
    assert_eq!(body["summary"]["aborted"], json!(0));

    // The withdraw result must report status "withdrawn".
    let withdraw_result = &results[1]["result"];
    assert_eq!(
        withdraw_result["status"].as_str().unwrap_or(""),
        "withdrawn",
        "withdraw must yield status withdrawn; got: {withdraw_result}"
    );

    // Clean-break: neither result must expose the old wire key.
    assert!(
        results[0]["result"].get("proposal_id").is_none(),
        "propose result must NOT contain proposal_id; got: {}",
        results[0]["result"]
    );
    assert!(
        results[1]["result"].get("proposal_id").is_none(),
        "withdraw result must NOT contain proposal_id; got: {}",
        results[1]["result"]
    );

    Ok(())
}
